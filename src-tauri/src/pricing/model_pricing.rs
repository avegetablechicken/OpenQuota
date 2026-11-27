use std::{collections::HashMap, sync::Mutex};

use super::{ModelRates, PricingCatalog, PricingSupplement, ServiceTier, TokenBreakdown};

pub struct ModelPricing {
    pub supplement: PricingSupplement,
    pub primary: PricingCatalog,
    pub secondary: PricingCatalog,
    memo: Mutex<HashMap<String, Option<ModelRates>>>,
}

/// Models named for a Local runtime (`ollama/*`) are free, even when named after a cloud model.
pub fn is_local_runtime_model(model: &str) -> bool {
    model
        .trim()
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ollama/"))
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
        if is_local_runtime_model(model) {
            return None;
        }
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
        if is_local_runtime_model(&normalized) {
            return Some(None);
        }
        // Foreign models in native logs use upstream API-equivalent pricing. A reseller export
        // keeps its own provider scope.
        let provider = match provider {
            "codex" | "claude" | "antigravity" | "grok" | "kimi" | "minimax" | "zai"
            | "opencode"
                if normalized.starts_with("deepseek-")
                    || normalized.ends_with("/deepseek-flash") =>
            {
                "deepseek"
            }
            _ => provider,
        };
        let models = self.supplement.history.get(provider)?;
        let name = match (provider, normalized.split_once('/')) {
            ("codex", Some(("openai" | "openai-codex", id)))
            | ("claude", Some(("anthropic", id)))
            | ("grok", Some(("xai" | "x-ai" | "grok", id)))
            | (
                "kimi",
                Some((
                    "moonshot" | "moonshot-cn" | "moonshotai" | "kimi" | "kimi-code"
                    | "kimi-coding",
                    id,
                )),
            )
            | ("minimax", Some(("minimax", id)))
            | ("zai", Some(("zai" | "z-ai" | "zhipu" | "zhipuai" | "bigmodel", id)))
            | (
                "antigravity",
                Some((
                    "google" | "gemini" | "google-antigravity" | "google-gemini-cli"
                    | "google-vertex" | "vertex_ai" | "vertex" | "models",
                    id,
                )),
            )
            | ("deepseek", Some(("deepseek", id))) => id,
            ("deepseek", Some(_)) if normalized.ends_with("/deepseek-flash") => "deepseek-flash",
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
        let (rows, suffix_fast) = lookup(name)
            .or_else(|| self.supplement.canonical_name(name).and_then(lookup))
            .or_else(|| self.supplement.canonical_name(&normalized).and_then(lookup))
            .or_else(|| {
                (provider == "antigravity").then(|| {
                    gemini_candidates(&normalized).into_iter().find_map(|cand| {
                        lookup(&cand)
                            .or_else(|| self.supplement.canonical_name(&cand).and_then(lookup))
                    })
                })?
            })?;
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
        if let Some(canonical) = self.supplement.canonical_name(model) {
            return strip_fast_family(canonical);
        }
        let normalized = model.trim().to_ascii_lowercase();
        if normalized != model {
            if let Some(canonical) = self.supplement.canonical_name(&normalized) {
                return strip_fast_family(canonical);
            }
        }
        if let Some(family) = self.gemini_display_family(&normalized) {
            return family;
        }
        strip_fast_family(model)
    }

    fn resolve_uncached(&self, model: &str) -> Option<ModelRates> {
        if let Some(canonical) = self.supplement.canonical_name(model) {
            if canonical != model {
                return self.lookup(canonical).or_else(|| self.lookup(model));
            }
        }
        if let Some(rates) = self.lookup_exact(model) {
            return Some(rates);
        }
        if let Some(rates) = self.lookup_gemini_normalized(model) {
            return Some(rates);
        }
        self.lookup_fallback(model)
    }

    fn lookup(&self, name: &str) -> Option<ModelRates> {
        self.lookup_exact(name)
            .or_else(|| self.lookup_fallback(name))
    }

    fn lookup_exact(&self, name: &str) -> Option<ModelRates> {
        if let Some(rates) = self.supplement.pricing.get(name) {
            return Some(*rates);
        }
        if let Some((_, rates)) = self.primary.find_exact(name) {
            return Some(rates);
        }
        if let Some(rates) = self.fast_variant(name) {
            return Some(rates);
        }
        None
    }

    fn lookup_fallback(&self, name: &str) -> Option<ModelRates> {
        if name.ends_with("-fast") {
            return self.secondary.find_exact(name).map(|(_, rates)| rates);
        }
        if let Some((_, rates)) = self.primary.find_fuzzy(name) {
            return Some(rates);
        }
        self.secondary.find_exact(name).map(|(_, rates)| rates)
    }

    fn lookup_gemini_normalized(&self, model: &str) -> Option<ModelRates> {
        let normalized = model.to_ascii_lowercase();
        let candidates = gemini_candidates(&normalized);
        if candidates.is_empty() {
            return None;
        }
        for candidate in &candidates {
            if let Some(rates) = self.supplement.pricing.get(candidate.as_str()) {
                return Some(*rates);
            }
            if let Some(canonical) = self.supplement.canonical_name(candidate) {
                if let Some(rates) = self.lookup(canonical) {
                    return Some(rates);
                }
            }
        }
        for candidate in &candidates {
            if let Some((_, rates)) = self.primary.find_exact(candidate) {
                return Some(rates);
            }
        }
        for candidate in &candidates {
            if let Some((_, rates)) = self.secondary.find_exact(candidate) {
                return Some(rates);
            }
        }
        None
    }

    fn gemini_display_family(&self, normalized: &str) -> Option<String> {
        let candidates = gemini_candidates(normalized);
        if candidates.is_empty() {
            return None;
        }
        for candidate in &candidates {
            if let Some(canonical) = self.supplement.canonical_name(candidate) {
                return Some(strip_fast_family(canonical));
            }
            if self.supplement.pricing.contains_key(candidate.as_str()) {
                return Some(strip_fast_family(candidate));
            }
        }
        for candidate in &candidates {
            if self.primary.find_exact(candidate).is_some() {
                return Some(strip_fast_family(candidate));
            }
        }
        for candidate in &candidates {
            if self.secondary.find_exact(candidate).is_some() {
                return Some(strip_fast_family(candidate));
            }
        }
        None
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

fn strip_fast_family(model: &str) -> String {
    model
        .strip_suffix("-fast")
        .filter(|base| !base.is_empty())
        .unwrap_or(model)
        .to_owned()
}

fn strip_gemini_namespace(mut slug: &str) -> &str {
    slug = slug.strip_prefix("openrouter/").unwrap_or(slug);
    slug = slug.strip_prefix("publishers/google/").unwrap_or(slug);
    for prefix in [
        "google-antigravity/",
        "google-gemini-cli/",
        "google-vertex/",
        "vertex_ai/",
        "vertex/",
        "google/",
        "gemini/",
    ] {
        if let Some(rest) = slug.strip_prefix(prefix) {
            slug = rest;
            break;
        }
    }
    slug.strip_prefix("models/").unwrap_or(slug)
}

fn normalize_gemini_minor_version(slug: &str) -> String {
    let Some(rest) = slug.strip_prefix("gemini-") else {
        return slug.to_owned();
    };
    let bytes = rest.as_bytes();
    if bytes.len() >= 5
        && bytes[0].is_ascii_digit()
        && bytes[1] == b'-'
        && bytes[2].is_ascii_digit()
        && bytes[3] == b'-'
        && !bytes[4].is_ascii_digit()
    {
        return format!(
            "gemini-{}.{}-{}",
            bytes[0] as char,
            bytes[2] as char,
            &rest[4..]
        );
    }
    slug.to_owned()
}

fn strip_gemini_variant_modifiers(mut slug: &str) -> &str {
    loop {
        let stripped = [
            "[1m]",
            "-extra-high",
            "-minimal",
            "-none",
            "-low",
            "-medium",
            "-high",
            "-xhigh",
            "-max",
            "-ultra",
            "-tiered",
            "-thinking",
            "-customtools",
        ]
        .into_iter()
        .find_map(|suffix| slug.strip_suffix(suffix));
        match stripped {
            Some(next) if !next.is_empty() => slug = next,
            _ => return slug,
        }
    }
}

fn strip_gemini_release_suffix(slug: &str) -> Option<&str> {
    let without_snapshot = super::history::strip_snapshot(slug);
    if without_snapshot != slug {
        return Some(without_snapshot);
    }
    // Trailing `-MM-YYYY` (e.g. `-09-2025`)
    if let Some((base, suffix)) = slug
        .len()
        .checked_sub(8)
        .and_then(|i| slug.get(..i).zip(slug.get(i..)))
    {
        let bytes = suffix.as_bytes();
        if bytes[0] == b'-'
            && bytes[1].is_ascii_digit()
            && bytes[2].is_ascii_digit()
            && bytes[3] == b'-'
            && bytes[4] == b'2'
            && bytes[5] == b'0'
            && bytes[6].is_ascii_digit()
            && bytes[7].is_ascii_digit()
        {
            return Some(base);
        }
    }
    // Trailing `-MM-DD` (e.g. `-05-06`, `-06-17`)
    if let Some((base, suffix)) = slug
        .len()
        .checked_sub(6)
        .and_then(|i| slug.get(..i).zip(slug.get(i..)))
    {
        let bytes = suffix.as_bytes();
        if bytes[0] == b'-'
            && bytes[1].is_ascii_digit()
            && bytes[2].is_ascii_digit()
            && bytes[3] == b'-'
            && bytes[4].is_ascii_digit()
            && bytes[5].is_ascii_digit()
        {
            let month = (bytes[1] - b'0') * 10 + (bytes[2] - b'0');
            let day = (bytes[4] - b'0') * 10 + (bytes[5] - b'0');
            if (1..=12).contains(&month) && (1..=31).contains(&day) {
                return Some(base);
            }
        }
    }
    for suffix in ["-preview", "-exp", "-latest"] {
        if let Some(base) = slug.strip_suffix(suffix) {
            if !base.is_empty() && base != "gemini" {
                return Some(base);
            }
        }
    }
    None
}

fn gemini_candidates(model: &str) -> Vec<String> {
    let bare = strip_gemini_namespace(model.trim());
    if !bare.starts_with("gemini-") {
        return Vec::new();
    }
    let versioned = normalize_gemini_minor_version(bare);
    let base = strip_gemini_variant_modifiers(&versioned);
    if base.is_empty() || base == "gemini" {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    let mut push_unique = |value: String| {
        if !candidates.contains(&value) {
            candidates.push(value);
        }
    };

    push_unique(base.to_owned());
    let mut current = base;
    while let Some(shorter) = strip_gemini_release_suffix(current) {
        let shorter = strip_gemini_variant_modifiers(shorter);
        if shorter.is_empty() || shorter == "gemini" {
            break;
        }
        push_unique(shorter.to_owned());
        current = shorter;
    }

    if let Some(root) = candidates.last().cloned() {
        if !root.ends_with("-preview") {
            let preview = format!("{root}-preview");
            if !candidates.contains(&preview) {
                candidates.push(preview);
            }
        }
    }
    candidates
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

    #[test]
    fn gemini_normalization_handles_future_catalog_models_without_conflating_variants() {
        let supplement = r#"{
          "pricing": {
            "gemini-3.5-flash-lite": {"input_per_million": 0.3, "output_per_million": 2.5}
          },
          "alias_rules": []
        }"#;
        let model_pricing = pricing(
            Some(supplement),
            &[
                ("gemini-3.5-flash", rates(1.5, 9.0)),
                ("gemini-4-helium-preview", rates(1.25, 6.0)),
            ],
            &[],
        );

        // Supplement flash-lite must beat primary fuzzy match on flash (including mixed case).
        assert_eq!(
            model_pricing
                .resolve("gemini-3.5-flash-lite-tiered-high")
                .unwrap()
                .input_per_million,
            0.3
        );
        assert_eq!(
            model_pricing
                .resolve("Google/gemini-3.5-flash-lite-tiered-high")
                .unwrap()
                .input_per_million,
            0.3
        );
        assert_eq!(
            model_pricing
                .resolve("gemini-3.5-flash-lite-HIGH")
                .unwrap()
                .input_per_million,
            0.3
        );
        // Brand-new primary catalog Gemini model resolves across namespaces/modifiers/snapshots.
        assert_eq!(
            model_pricing
                .resolve("google-antigravity/gemini-4-helium-tiered-high")
                .unwrap()
                .input_per_million,
            1.25
        );
        assert_eq!(
            model_pricing
                .resolve("models/gemini-4-helium-2026-12-01-thinking-xhigh")
                .unwrap()
                .output_per_million,
            6.0
        );
        assert_eq!(
            model_pricing.display_family("google-antigravity/gemini-4-helium-tiered-high"),
            "gemini-4-helium-preview"
        );
        // Unknown Gemini slugs still fail closed.
        assert!(model_pricing
            .resolve("google/gemini-4-unknown-tiered-high")
            .is_none());
        assert_eq!(
            model_pricing.display_family("google/gemini-4-unknown-tiered-high"),
            "google/gemini-4-unknown-tiered-high"
        );

        let bundled = crate::pricing::test_bundled_pricing();
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z")
            .unwrap()
            .to_utc();
        assert!(bundled
            .historical_rates(
                "antigravity",
                "deepseek-flash",
                at,
                crate::pricing::ServiceTier::Standard,
                0
            )
            .flatten()
            .is_some());
        assert!(bundled
            .historical_rates(
                "opencode",
                "openrouter/deepseek-flash",
                at,
                crate::pricing::ServiceTier::Standard,
                0
            )
            .flatten()
            .is_some());
    }

    #[test]
    fn local_runtime_models_stay_unpriced_even_when_named_after_cloud_models() {
        let bundled = crate::pricing::test_bundled_pricing();
        let tokens = TokenBreakdown {
            input: 1_000_000,
            output: 1_000_000,
            ..TokenBreakdown::default()
        };
        let at = chrono::Utc::now();
        for model in [
            "ollama/deepseek-flash",
            "ollama/claude-sonnet-4-6",
            "Ollama/gemini-2.5-pro",
            "ollama/gpt-5.4",
        ] {
            assert!(bundled.resolve(model).is_none(), "{model}");
            for provider in ["antigravity", "codex", "claude", "grok", "kimi", "opencode"] {
                assert_eq!(
                    bundled.estimated_cost_at(provider, model, at, tokens, true),
                    None,
                    "{provider} {model}"
                );
            }
        }
        assert!(bundled
            .estimated_cost_at("claude", "claude-sonnet-4-6", at, tokens, true)
            .is_some());
    }
}
