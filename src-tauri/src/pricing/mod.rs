mod catalog;
mod codecs;
mod history;
mod model_pricing;
mod rates;
mod store;
mod supplement;

pub use catalog::PricingCatalog;
pub(crate) use history::ServiceTier;
pub use model_pricing::ModelPricing;
pub use rates::{ModelRates, TokenBreakdown};
pub use store::PricingStore;
pub use supplement::PricingSupplement;

#[cfg(test)]
pub fn test_bundled_pricing() -> ModelPricing {
    ModelPricing::new(
        PricingSupplement::decode(include_bytes!("../../resources/pricing_supplement.json"))
            .expect("bundled pricing supplement must be valid"),
        codecs::catalog_from_compact(include_bytes!(
            "../../resources/pricing_litellm_snapshot.json"
        ))
        .expect("bundled LiteLLM pricing must be valid"),
        codecs::catalog_from_compact(include_bytes!(
            "../../resources/pricing_models_dev_snapshot.json"
        ))
        .expect("bundled models.dev pricing must be valid"),
    )
}

#[cfg(test)]
mod bundled_resource_tests {
    use super::{test_bundled_pricing, TokenBreakdown};

    #[test]
    fn bundled_catalogs_are_non_trivial_and_all_rules_resolve() {
        let pricing = test_bundled_pricing();
        assert!(pricing.primary.entries.len() > 500);
        assert!(pricing.secondary.entries.len() > 500);
        assert!(!pricing.supplement.pricing.is_empty());
        assert!(!pricing.supplement.alias_rules.is_empty());
        let raw_supplement: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../resources/pricing_supplement.json"))
                .unwrap();
        assert_eq!(
            pricing.supplement.alias_rules.len(),
            raw_supplement["alias_rules"].as_array().unwrap().len(),
            "every bundled alias regex must compile in Rust"
        );
        for rule in &pricing.supplement.alias_rules {
            assert!(
                pricing.resolve(&rule.canonical).is_some(),
                "alias canonical '{}' resolves against no pricing source",
                rule.canonical
            );
        }
        for base in pricing.supplement.fast_multipliers.keys() {
            assert!(
                pricing.resolve(base).is_some(),
                "fast multiplier base '{base}' resolves against no pricing source"
            );
        }
    }

    #[test]
    fn known_log_models_and_aliases_match_expected_rates() {
        let pricing = test_bundled_pricing();
        let assert_rates = |model: &str, expected: [f64; 4]| {
            let rates = pricing.resolve(model).unwrap_or_else(|| panic!("{model}"));
            assert_eq!(
                [
                    rates.input_per_million,
                    rates.cache_write_per_million,
                    rates.cache_read_per_million,
                    rates.output_per_million,
                ],
                expected,
                "{model}"
            );
        };
        assert_eq!(
            pricing
                .resolve("claude-sonnet-4-5-20250929")
                .unwrap()
                .input_per_million,
            3.0
        );
        assert_eq!(
            pricing
                .resolve("gpt-5.6-sol-ultra-fast")
                .unwrap()
                .input_per_million,
            10.0
        );
        assert_eq!(
            pricing
                .resolve("claude-fable-5-thinking-xhigh")
                .unwrap()
                .output_per_million,
            50.0
        );
        assert_rates("gpt-5.6-high-fast", [10.0, 12.5, 1.0, 60.0]);
        assert_rates("gpt-daybreak-blue-latest", [5.0, 6.25, 0.5, 30.0]);
        assert_rates("daybreak-blue-latest", [5.0, 6.25, 0.5, 30.0]);
        assert_rates("gpt-5.6-terra-max-fast", [4.0, 5.0, 0.4, 24.0]);
        assert_rates("gpt-5.6-luna-high-fast", [0.4, 0.5, 0.04, 2.4]);
        assert_rates("claude-opus-5", [5.0, 6.25, 0.5, 25.0]);
        assert_rates(
            "claude-opus-5[1m]-thinking-max-fast",
            [10.0, 12.5, 1.0, 50.0],
        );
        assert_rates("claude-sonnet-5", [2.0, 2.5, 0.2, 10.0]);
        assert_rates("kimi-k3-code-max", [3.0, 3.0, 0.3, 15.0]);
        assert_rates("kimi-code/k3", [3.0, 3.0, 0.3, 15.0]);
        assert_rates("kimi-code/k3-max", [3.0, 3.0, 0.3, 15.0]);
        assert_rates("kimi-code/k3.1-pro", [2.0, 2.0, 0.2, 10.0]);
        assert_rates("kimi-code/k3.1-pro-highspeed", [4.0, 4.0, 0.2, 20.0]);
        assert_rates("kimi-code/k2.7-code", [0.95, 0.95, 0.19, 4.0]);
        assert_rates("moonshot-cn/kimi-k2.7-code", [0.95, 0.95, 0.19, 4.0]);
        assert_rates("kimi-coding/k2p5", [0.6, 0.6, 0.1, 3.0]);
        assert_rates("moonshot-cn/kimi-k2.6", [0.95, 0.95, 0.16, 4.0]);
        assert_rates("GLM-5.3-Flash-max", [0.15, 0.15, 0.03, 0.5]);
        assert_rates("glm-5.3-high", [1.4, 1.4, 0.26, 4.4]);
        assert_rates("cursor-grok-4.5-high-fast", [4.0, 4.0, 1.0, 12.0]);
        assert_rates("cursor-grok-4.6-medium", [2.0, 2.0, 0.5, 6.0]);
        assert_rates("grok-4.6-fast-high", [4.0, 4.0, 1.0, 12.0]);
        assert_rates("gemini-4-neon-high", [3.0, 3.0, 0.15, 15.0]);
        assert_rates("gemini-4-carbon-high", [2.0, 2.0, 0.1, 10.0]);
        assert_rates("gemini-3.6-flash-medium", [0.75, 0.75, 0.075, 3.75]);
        assert_rates("gemini-3.1-pro-high", [2.0, 2.0, 0.2, 12.0]);
        assert_eq!(
            pricing.resolve("grok-build-0.1").unwrap().input_per_million,
            1.0
        );
        assert_eq!(
            pricing
                .resolve("cursor-grok-4.5-xhigh")
                .unwrap()
                .input_per_million,
            pricing.resolve("grok-4.5").unwrap().input_per_million
        );
        assert_eq!(
            pricing
                .resolve("cursor-grok-4.5-fast-xhigh")
                .unwrap()
                .output_per_million,
            pricing.resolve("grok-4.5-fast").unwrap().output_per_million
        );
        assert!(pricing.resolve("glm-5.2-bogus").is_none());
    }

    #[test]
    fn bundled_engine_prices_cache_and_long_context_buckets() {
        let pricing = test_bundled_pricing();
        let cost = pricing
            .estimated_cost_dollars(
                "claude-sonnet-4-5-20250929",
                TokenBreakdown {
                    input: 100_000,
                    cache_write_5m: 60_000,
                    cache_read: 50_000,
                    output: 20_000,
                    ..TokenBreakdown::default()
                },
                true,
            )
            .unwrap();
        assert!(cost > 0.0);
    }

    #[test]
    fn namespaced_kimi_aliases_resolve_through_historical_schedules() {
        use chrono::{TimeZone, Utc};

        let pricing = test_bundled_pricing();
        let tokens = TokenBreakdown {
            input: 1_000_000,
            cache_read: 1_000_000,
            output: 100_000,
            ..TokenBreakdown::default()
        };
        let k3_date = Utc.with_ymd_and_hms(2026, 7, 20, 0, 0, 0).unwrap();
        let k3_cost = pricing
            .estimated_cost_at("kimi", "kimi-code/k3", k3_date, tokens, true)
            .unwrap();
        assert!((k3_cost - 4.8).abs() < 1e-6);

        let k31_date = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        let k31_cost = pricing
            .estimated_cost_at("kimi", "kimi-code/k3.1-pro", k31_date, tokens, true)
            .unwrap();
        assert!((k31_cost - 3.2).abs() < 1e-6);

        // Before K3.1 Pro's effective `from` date, the historical schedule reports unknown pricing.
        let before_k31 = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        assert!(pricing
            .estimated_cost_at("kimi", "kimi-code/k3.1-pro", before_k31, tokens, true)
            .is_none());
    }
}
