use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

use super::{catalog::normalized_key, ModelRates};

#[derive(Debug, Default)]
pub struct PricingSupplement {
    pub(super) history: super::history::PriceHistory,
    pub pricing: HashMap<String, ModelRates>,
    pub fast_multipliers: HashMap<String, f64>,
    pub alias_rules: Vec<AliasRule>,
    pub updated_at: Option<String>,
}

#[derive(Debug)]
pub struct AliasRule {
    pub pattern: Regex,
    pub canonical: String,
}

impl PricingSupplement {
    pub fn decode(data: &[u8]) -> Result<Self, SupplementError> {
        let file = serde_json::from_slice::<SupplementFile>(data)?;
        if !super::history::valid(&file.history) {
            return Err(SupplementError::InvalidHistory);
        }
        let pricing = file
            .pricing
            .into_iter()
            .map(|(model, entry)| {
                let input = entry.input_per_million;
                let mut rates = ModelRates::new(input, entry.output_per_million);
                rates.cache_write_per_million = entry.cache_write_per_million.unwrap_or(input);
                rates.cache_read_per_million = entry.cache_read_per_million.unwrap_or(input * 0.1);
                rates.cache_read_is_explicit = entry.cache_read_per_million.is_some();
                (model, rates)
            })
            .collect();
        let alias_rules = file
            .alias_rules
            .into_iter()
            .filter_map(|rule| match Regex::new(&rule.pattern) {
                Ok(pattern) => Some(AliasRule {
                    pattern,
                    canonical: rule.canonical,
                }),
                Err(error) => {
                    crate::app_warn!(
                        "pricing",
                        "pricing supplement: invalid alias pattern '{}' skipped: {error}",
                        rule.pattern
                    );
                    None
                }
            })
            .collect();
        Ok(Self {
            history: file.history,
            pricing,
            fast_multipliers: file.fast_multipliers,
            alias_rules,
            updated_at: file.updated_at,
        })
    }

    pub fn canonical_name<'a>(&'a self, model: &str) -> Option<&'a str> {
        self.alias_rules
            .iter()
            .find(|rule| rule.pattern.is_match(model))
            .map(|rule| rule.canonical.as_str())
    }

    pub fn fast_multiplier(&self, model: &str) -> Option<f64> {
        if let Some(exact) = self.fast_multipliers.get(model) {
            return Some(*exact);
        }
        let normalized = normalized_key(model);
        // Several bases can match one part ("gpt-5" and "gpt-5.5" both match
        // "gpt-5-5"), so prefer the longest, then the smallest key, to keep the
        // result independent of HashMap iteration order.
        normalized.split(['/', ':']).find_map(|part| {
            self.fast_multipliers
                .iter()
                .map(|(base, multiplier)| (normalized_key(base), base, *multiplier))
                .filter(|(normalized_base, _, _)| matches_model_suffix(part, normalized_base))
                .max_by(|(left, left_key, _), (right, right_key, _)| {
                    left.len()
                        .cmp(&right.len())
                        .then_with(|| right_key.cmp(left_key))
                })
                .map(|(_, _, multiplier)| multiplier)
        })
    }
}

fn matches_model_suffix(part: &str, base: &str) -> bool {
    !base.is_empty()
        && part.match_indices(base).any(|(start, _)| {
            let starts_token = part[..start]
                .chars()
                .next_back()
                .is_none_or(|previous| !previous.is_ascii_alphanumeric());
            let suffix = &part[start + base.len()..];
            starts_token && (suffix.is_empty() || suffix.starts_with('-'))
        })
}

#[derive(Debug, Error)]
pub enum SupplementError {
    #[error("Pricing history contains invalid rates or overlapping periods.")]
    InvalidHistory,
    #[error("Pricing supplement is invalid JSON.")]
    InvalidJson(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct SupplementFile {
    #[serde(default)]
    history: super::history::PriceHistory,
    #[serde(default)]
    updated_at: Option<String>,
    pricing: HashMap<String, SupplementEntry>,
    #[serde(default)]
    fast_multipliers: HashMap<String, f64>,
    #[serde(default)]
    alias_rules: Vec<SupplementRule>,
}

#[derive(Deserialize)]
struct SupplementEntry {
    input_per_million: f64,
    output_per_million: f64,
    #[serde(default)]
    cache_write_per_million: Option<f64>,
    #[serde(default)]
    cache_read_per_million: Option<f64>,
}

#[derive(Deserialize)]
struct SupplementRule {
    pattern: String,
    canonical: String,
}

#[cfg(test)]
mod tests {
    use super::PricingSupplement;

    fn supplement(multipliers: &str) -> PricingSupplement {
        PricingSupplement::decode(
            format!(r#"{{"pricing":{{}},"fast_multipliers":{multipliers}}}"#).as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn fast_multiplier_prefers_the_longest_matching_base() {
        let supplement = supplement(r#"{"gpt-5":2.0,"gpt-5.5":2.5}"#);
        assert_eq!(supplement.fast_multiplier("azure/gpt-5.5"), Some(2.5));
        assert_eq!(supplement.fast_multiplier("azure/gpt-5-mini"), Some(2.0));
    }

    #[test]
    fn fast_multiplier_requires_a_token_boundary_before_the_base() {
        let supplement = supplement(r#"{"gpt-5":2.0}"#);
        assert_eq!(supplement.fast_multiplier("chatgpt-5"), None);
        assert_eq!(supplement.fast_multiplier("openai/gpt-5"), Some(2.0));
        assert_eq!(supplement.fast_multiplier("x_gpt-5-pro"), Some(2.0));
    }
}
