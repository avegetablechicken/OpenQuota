use std::{collections::HashMap, sync::Mutex};

use super::{ModelRates, PricingCatalog, PricingSupplement, ServiceTier, TokenBreakdown};

pub struct ModelPricing {
    pub supplement: PricingSupplement,
    pub primary: PricingCatalog,
    pub secondary: PricingCatalog,
    memo: Mutex<HashMap<String, Option<ModelRates>>>,
}

impl ModelPricing {
    pub fn new(
        supplement: PricingSupplement,
        primary: PricingCatalog,
        secondary: PricingCatalog,
    ) -> Self {
        Self {
            supplement,
            primary,
            secondary,
            memo: Mutex::new(HashMap::new()),
        }
    }

    pub fn resolve(&self, model: &str) -> Option<ModelRates> {
        let model = model.trim();
        if let Some(cached) = self
            .memo
            .lock()
            .ok()
            .and_then(|memo| memo.get(model).copied())
        {
            return cached;
        }
        let normalized = model.to_ascii_lowercase();
        let resolved = self.resolve_uncached(model).or_else(|| {
            (normalized != model)
                .then(|| self.resolve_uncached(&normalized))
                .flatten()
        });
        if let Ok(mut memo) = self.memo.lock() {
            memo.insert(model.to_owned(), resolved);
        }
        resolved
    }

    /// An empty match for a tracked model means its price is unknown on this date.
    /// An untracked model can still be estimated from the ordinary catalog.
    pub(crate) fn historical_rates(
        &self,
        provider: &str,
        model: &str,
        at: chrono::DateTime<chrono::Utc>,
        tier: ServiceTier,
        prompt: u64,
    ) -> Option<Option<ModelRates>> {
        let normalized = model.trim().to_ascii_lowercase();
        // Foreign models in native logs use upstream API-equivalent pricing. A reseller export
        // keeps its own provider scope.
        let provider = match provider {
            "codex" | "claude" if normalized.starts_with("deepseek-") => "deepseek",
            _ => provider,
        };
        let models = self.supplement.history.get(provider)?;
        let name = match (provider, normalized.split_once('/')) {
            ("codex", Some(("openai" | "openai-codex", id)))
            | ("claude", Some(("anthropic", id)))
            | ("grok", Some(("xai", id)))
            | ("antigravity", Some(("google" | "gemini" | "google-antigravity", id))) => id,
            _ => &normalized,
        };
        let lookup = |candidate: &str| {
            if let Some(rows) = models.get(candidate) {
                return Some((rows, false));
            }
            let (base, suffix_fast) = candidate
                .strip_suffix("-fast")
                .map_or((candidate, false), |base| (base, true));
            let base = base
                .rsplit_once('-')
                .filter(|(_, effort)| {
                    matches!(
                        *effort,
                        "minimal" | "none" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                    )
                })
                .map_or(base, |(model, _)| model);
            let base = base.strip_suffix("-thinking").unwrap_or(base);
            let base = base.strip_suffix("[1m]").unwrap_or(base);
            models
                .get(super::history::strip_snapshot(base))
                .map(|rows| (rows, suffix_fast))
        };
        let (rows, suffix_fast) =
            lookup(name).or_else(|| self.supplement.canonical_name(name).and_then(lookup))?;
        let tier = if suffix_fast && tier == ServiceTier::Standard {
            ServiceTier::Fast
        } else {
            tier
        };
        Some(super::history::active(rows, at).and_then(|row| row.rates(tier, prompt, at)))
    }

    pub fn estimated_cost_at(
        &self,
        provider: &str,
        model: &str,
        at: chrono::DateTime<chrono::Utc>,
        tokens: TokenBreakdown,
        apply_long_context_rates: bool,
    ) -> Option<f64> {
        let tier = if tokens.is_fast {
            ServiceTier::Fast
        } else {
            ServiceTier::Standard
        };
        match self.historical_rates(provider, model, at, tier, tokens.prompt_tokens()) {
            Some(rates) => Some(rates?.cost_dollars(tokens, apply_long_context_rates)),
            None => self.estimated_cost_dollars(model, tokens, apply_long_context_rates),
        }
    }

    pub fn estimated_cost_dollars(
        &self,
        model: &str,
        tokens: TokenBreakdown,
        apply_long_context_rates: bool,
    ) -> Option<f64> {
        Some(
            self.resolve(model)?
                .cost_dollars(tokens, apply_long_context_rates),
        )
    }

    /// Returns the stable display family used for provider exports that contain one slug per model
    /// variant. Alias rules are the same source of truth used for pricing; fast variants fold into
    /// their base family without guessing at otherwise unknown names.
    pub fn display_family(&self, model: &str) -> String {
        let canonical = self.supplement.canonical_name(model).unwrap_or(model);
        canonical
            .strip_suffix("-fast")
            .filter(|base| !base.is_empty())
            .unwrap_or(canonical)
            .to_owned()
    }

    fn resolve_uncached(&self, model: &str) -> Option<ModelRates> {
        if let Some(canonical) = self.supplement.canonical_name(model) {
            if canonical != model {
                return self.lookup(canonical).or_else(|| self.lookup(model));
            }
        }
        self.lookup(model)
    }

    fn lookup(&self, name: &str) -> Option<ModelRates> {
        if let Some(rates) = self.supplement.pricing.get(name) {
            return Some(*rates);
        }
        if let Some((_, rates)) = self.primary.find_exact(name) {
            return Some(rates);
        }
        if let Some(rates) = self.fast_variant(name) {
            return Some(rates);
        }
        if name.ends_with("-fast") {
            return self.secondary.find_exact(name).map(|(_, rates)| rates);
        }
        if let Some((_, rates)) = self.primary.find_fuzzy(name) {
            return Some(rates);
        }
        self.secondary.find_exact(name).map(|(_, rates)| rates)
    }

    fn fast_variant(&self, name: &str) -> Option<ModelRates> {
        let base = name.strip_suffix("-fast")?;
        if base.is_empty() {
            return None;
        }
        let (key, rates) = self.base_entry(base)?;
        let multiplier = if rates.fast_multiplier != 1.0 {
            rates.fast_multiplier
        } else {
            self.supplement
                .fast_multiplier(key)
                .or_else(|| self.supplement.fast_multiplier(base))?
        };
        Some(rates.scaled(multiplier))
    }

    fn base_entry<'a>(&'a self, base: &'a str) -> Option<(&'a str, ModelRates)> {
        if let Some(rates) = self.supplement.pricing.get(base) {
            return Some((base, *rates));
        }
        self.primary
            .find_exact(base)
            .or_else(|| self.primary.find_fuzzy(base))
            .or_else(|| self.secondary.find_exact(base))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::ModelPricing;
    use crate::pricing::{ModelRates, PricingCatalog, PricingSupplement, TokenBreakdown};

    fn rates(input: f64, output: f64) -> ModelRates {
        ModelRates::new(input, output)
    }

    fn pricing(
        supplement: Option<&str>,
        primary: &[(&str, ModelRates)],
        secondary: &[(&str, ModelRates)],
    ) -> ModelPricing {
        ModelPricing::new(
            supplement
                .map(|json| PricingSupplement::decode(json.as_bytes()).unwrap())
                .unwrap_or_default(),
            PricingCatalog {
                entries: primary
                    .iter()
                    .map(|(name, rates)| ((*name).to_owned(), *rates))
                    .collect::<HashMap<_, _>>(),
                retrieved_at: None,
            },
            PricingCatalog {
                entries: secondary
                    .iter()
                    .map(|(name, rates)| ((*name).to_owned(), *rates))
                    .collect::<HashMap<_, _>>(),
                retrieved_at: None,
            },
        )
    }

    #[test]
    fn supplement_alias_and_precedence_follow_contract() {
        let supplement = r#"{
          "pricing":{"auto":{"input_per_million":1.25,"output_per_million":6}},
          "alias_rules":[{"pattern":"^claude-4\\.5-sonnet(?:-thinking)?$","canonical":"claude-sonnet-4-5"}]
        }"#;
        let model_pricing = pricing(
            Some(supplement),
            &[
                ("auto", rates(99.0, 99.0)),
                ("claude-sonnet-4-5", rates(3.0, 15.0)),
            ],
            &[],
        );
        assert_eq!(
            model_pricing.resolve("auto").unwrap().input_per_million,
            1.25
        );
        assert_eq!(
            model_pricing
                .resolve("claude-4.5-sonnet-thinking")
                .unwrap()
                .input_per_million,
            3.0
        );
    }

    #[test]
    fn alias_miss_falls_back_to_raw_name() {
        let supplement =
            r#"{"pricing":{},"alias_rules":[{"pattern":"^gpt-x$","canonical":"missing"}]}"#;
        let model_pricing = pricing(Some(supplement), &[("gpt-x", rates(1.0, 2.0))], &[]);
        assert_eq!(
            model_pricing.resolve("gpt-x").unwrap().input_per_million,
            1.0
        );
    }

    #[test]
    fn fast_variant_requires_a_multiplier_or_secondary_exact_rate() {
        let no_multiplier = pricing(None, &[("gpt-9", rates(1.0, 2.0))], &[]);
        assert!(no_multiplier.resolve("gpt-9-fast").is_none());

        let supplement = r#"{"pricing":{},"fast_multipliers":{"gpt-5.5":2.5},"alias_rules":[]}"#;
        let with_multiplier = pricing(
            Some(supplement),
            &[("gpt-5.5-20260423", rates(5.0, 30.0))],
            &[],
        );
        assert_eq!(
            with_multiplier
                .resolve("gpt-5.5-fast")
                .unwrap()
                .input_per_million,
            12.5
        );

        let secondary = pricing(
            None,
            &[("gpt-9", rates(1.0, 2.0))],
            &[("gpt-9-fast", rates(2.5, 5.0))],
        );
        assert_eq!(
            secondary.resolve("gpt-9-fast").unwrap().input_per_million,
            2.5
        );
    }

    #[test]
    fn unknown_model_cost_is_none() {
        let model_pricing = pricing(None, &[], &[]);
        assert!(model_pricing
            .estimated_cost_dollars(
                "mystery",
                TokenBreakdown {
                    input: 100,
                    ..TokenBreakdown::default()
                },
                true,
            )
            .is_none());
    }

    #[test]
    fn model_resolution_ignores_ascii_case() {
        let model_pricing = pricing(None, &[("gpt-5.5", rates(5.0, 30.0))], &[]);

        assert_eq!(
            model_pricing.resolve("GPT-5.5").unwrap().input_per_million,
            5.0
        );
    }

    #[test]
    fn secondary_catalog_is_exact_only() {
        let model_pricing = pricing(
            None,
            &[],
            &[("provider/secondary-model-20260715", rates(1.0, 2.0))],
        );
        assert!(model_pricing
            .resolve("provider/secondary-model-20260715")
            .is_some());
        assert!(model_pricing.resolve("secondary-model").is_none());
    }
}
