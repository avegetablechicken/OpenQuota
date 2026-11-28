//! Estimates for usage priced from Codex's own logs: auto-review on a ChatGPT plan is priced
//! from ChatGPT-plan history, and Codex models get Codex's priority multipliers, long-context
//! rates and cache rules.

use crate::pricing::{HistoricalRates, ModelPricing, ModelRates, ServiceTier, TokenBreakdown};

use super::UsageObservation;

pub(super) fn estimate(
    pricing: &ModelPricing,
    model: &str,
    chatgpt_plan: bool,
    observation: &UsageObservation<'_>,
) -> Option<f64> {
    let normalized_model = model.to_ascii_lowercase();
    let model = normalized_model.as_str();
    let tokens = TokenBreakdown {
        is_fast: false,
        ..observation.tokens
    };
    if model == "codex-auto-review" {
        if !chatgpt_plan {
            return None;
        }
        // ChatGPT-plan auto-review has no catalog price to fall back on.
        return match pricing.historical_rates(
            "codex-chatgpt",
            model,
            observation.timestamp,
            ServiceTier::Standard,
            tokens.prompt_tokens(),
        ) {
            HistoricalRates::Priced(rates) => Some(rates.cost_dollars(tokens, true)),
            HistoricalRates::Unpriced | HistoricalRates::Untracked => None,
        };
    }
    let tier = observation.tier;
    if tier == ServiceTier::Unknown {
        return None;
    }
    match pricing.historical_rates(
        "codex",
        model,
        observation.timestamp,
        tier,
        tokens.prompt_tokens(),
    ) {
        HistoricalRates::Priced(rates) => return Some(rates.cost_dollars(tokens, true)),
        HistoricalRates::Unpriced => return None,
        // Untracked models fall back to catalog rates under Codex's own rules below.
        HistoricalRates::Untracked => {}
    }
    if !matches!(tier, ServiceTier::Standard | ServiceTier::Fast) {
        return None;
    }
    let canonical = pricing.supplement.canonical_name(model).unwrap_or(model);
    let fast_base = canonical
        .strip_suffix("-fast")
        .filter(|base| !base.is_empty());
    let rate_model = fast_base.unwrap_or(canonical);
    let base_rates = pricing.resolve(rate_model);
    let rates = base_rates.or_else(|| pricing.resolve(model))?;
    let applies_fast_tier = if fast_base.is_some() {
        base_rates.is_some()
    } else {
        tier == ServiceTier::Fast
    };
    Some(codex_cost(rates, tokens, rate_model, applies_fast_tier))
}

fn codex_cost(mut rates: ModelRates, tokens: TokenBreakdown, model: &str, fast_tier: bool) -> f64 {
    if let Some((input, output, cache_read)) = codex_long_context_rates(model) {
        rates.input_above_200k_per_million = Some(input);
        rates.output_above_200k_per_million = Some(output);
        rates.cache_read_above_200k_per_million = Some(cache_read);
        rates.long_context_threshold_tokens = 272_000;
    }
    if codex_model_has_no_cache_discount(model) || !rates.cache_read_is_explicit {
        rates.cache_read_per_million = rates.input_per_million;
        rates.cache_read_above_200k_per_million = rates.input_above_200k_per_million;
    }
    rates.fast_multiplier = codex_priority_multiplier(model, rates);
    rates.cost_dollars(
        TokenBreakdown {
            is_fast: fast_tier,
            ..tokens
        },
        true,
    )
}

pub(in crate::providers) fn codex_priority_multiplier(model: &str, rates: ModelRates) -> f64 {
    match dated_base_model(model) {
        "gpt-5.5" | "gpt-5.5-pro" => 2.5,
        "gpt-5.4" | "gpt-5.4-pro" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => 2.0,
        _ if rates.fast_multiplier == 1.0 => 2.0,
        _ => rates.fast_multiplier,
    }
}

fn codex_model_has_no_cache_discount(model: &str) -> bool {
    matches!(dated_base_model(model), "gpt-5.4-pro" | "gpt-5.5-pro")
}

pub(in crate::providers) fn codex_long_context_rates(model: &str) -> Option<(f64, f64, f64)> {
    match dated_base_model(model) {
        "gpt-5.4" => Some((5.0, 22.5, 0.5)),
        "gpt-5.4-pro" => Some((60.0, 270.0, 60.0)),
        "gpt-5.5" => Some((10.0, 45.0, 1.0)),
        "gpt-5.5-pro" => Some((60.0, 270.0, 60.0)),
        "gpt-5.6-sol" => Some((10.0, 45.0, 1.0)),
        "gpt-5.6-terra" => Some((4.0, 18.0, 0.4)),
        "gpt-5.6-luna" => Some((0.4, 1.8, 0.04)),
        _ => None,
    }
}

fn dated_base_model(model: &str) -> &str {
    let bytes = model.as_bytes();
    if bytes.len() >= 11 {
        let suffix = &bytes[bytes.len() - 11..];
        if suffix[0] == b'-'
            && suffix[1..5].iter().all(u8::is_ascii_digit)
            && suffix[5] == b'-'
            && suffix[6..8].iter().all(u8::is_ascii_digit)
            && suffix[8] == b'-'
            && suffix[9..11].iter().all(u8::is_ascii_digit)
        {
            return &model[..model.len() - 11];
        }
    }
    if bytes.len() >= 9 {
        let suffix = &bytes[bytes.len() - 9..];
        if suffix[0] == b'-' && suffix[1..].iter().all(u8::is_ascii_digit) {
            return &model[..model.len() - 9];
        }
    }
    model
}
