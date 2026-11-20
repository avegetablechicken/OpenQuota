use std::collections::HashMap;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};

use super::ModelRates;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub(crate) enum ServiceTier {
    Standard,
    Fast,
    UltraFast,
    Flex,
    Unknown,
}

impl ServiceTier {
    pub(crate) fn parse(raw: &str) -> Self {
        match raw {
            "auto" | "default" | "standard" => Self::Standard,
            "fast" | "priority" => Self::Fast,
            "ultrafast" => Self::UltraFast,
            "flex" => Self::Flex,
            _ => Self::Unknown,
        }
    }
}

/// Keys are billing providers, then model IDs. A later start supersedes an earlier row.
pub(super) type PriceHistory = HashMap<String, HashMap<String, Vec<PricePeriod>>>;

#[derive(Debug, Deserialize)]
pub struct PricePeriod {
    pub from: DateTime<Utc>,
    pub until: Option<DateTime<Utc>>,
    input: f64,
    output: f64,
    read: f64,
    write: f64,
    fast: Option<f64>,
    ultrafast: Option<f64>,
    fast_up_to: Option<u64>,
    flex: Option<f64>,
    long: Option<LongContext>,
    weekday_peak: Option<WeekdayPeak>,
    one_hour_write_multiplier: Option<f64>,
    source: String,
}

#[derive(Debug, Deserialize)]
struct LongContext {
    above: u64,
    input_multiplier: f64,
    output_multiplier: f64,
}

#[derive(Debug, Deserialize)]
struct WeekdayPeak {
    hours_utc: Vec<[u32; 2]>,
    holidays: Vec<[NaiveDate; 2]>,
    multiplier: f64,
}

impl WeekdayPeak {
    fn applies(&self, at: DateTime<Utc>) -> bool {
        let date = at.date_naive();
        !matches!(at.weekday(), Weekday::Sat | Weekday::Sun)
            && self
                .hours_utc
                .iter()
                .any(|&[start, end]| (start..end).contains(&at.hour()))
            && !self
                .holidays
                .iter()
                .any(|&[start, end]| (start..=end).contains(&date))
    }

    fn valid(&self) -> bool {
        self.multiplier.is_finite()
            && self.multiplier > 0.0
            && !self.hours_utc.is_empty()
            && self
                .hours_utc
                .iter()
                .all(|&[start, end]| start < end && end <= 24)
            && self.holidays.iter().all(|&[start, end]| start <= end)
    }
}

impl PricePeriod {
    pub fn rates(&self, tier: ServiceTier, prompt: u64, at: DateTime<Utc>) -> Option<ModelRates> {
        let mut rates = ModelRates::new(self.input, self.output);
        rates.cache_read_per_million = self.read;
        rates.cache_write_per_million = self.write;
        if let Some(long) = &self.long {
            rates.long_context_threshold_tokens = long.above;
            rates.input_above_200k_per_million = Some(self.input * long.input_multiplier);
            rates.cache_write_above_200k_per_million = Some(self.write * long.input_multiplier);
            rates.cache_read_above_200k_per_million = Some(self.read * long.input_multiplier);
            rates.output_above_200k_per_million = Some(self.output * long.output_multiplier);
        }
        if let Some(multiplier) = self.one_hour_write_multiplier {
            rates.one_hour_write_multiplier = multiplier;
        }
        if let Some(peak) = self.weekday_peak.as_ref().filter(|peak| peak.applies(at)) {
            rates = rates.scaled(peak.multiplier);
        }
        match tier {
            ServiceTier::Standard => Some(rates),
            ServiceTier::Fast if self.fast_up_to.is_none_or(|max| prompt <= max) => {
                Some(rates.scaled(self.fast?))
            }
            ServiceTier::UltraFast => Some(rates.scaled(self.ultrafast?)),
            ServiceTier::Flex => Some(rates.scaled(self.flex?)),
            _ => None,
        }
    }

    fn valid(&self) -> bool {
        [self.input, self.output, self.read, self.write]
            .into_iter()
            .all(|rate| rate.is_finite() && rate >= 0.0)
            && self.until.is_none_or(|end| end > self.from)
            && self.source.starts_with("https://")
            && self.weekday_peak.as_ref().is_none_or(WeekdayPeak::valid)
            && self
                .one_hour_write_multiplier
                .is_none_or(|m| m.is_finite() && m >= 0.0)
            && self
                .fast
                .into_iter()
                .chain(self.ultrafast)
                .chain(self.flex)
                .all(|n| n.is_finite() && n > 0.0)
            && self.long.as_ref().is_none_or(|long| {
                long.above > 0
                    && [long.input_multiplier, long.output_multiplier]
                        .into_iter()
                        .all(|n| n.is_finite() && n > 0.0)
            })
    }
}

pub(super) fn valid(history: &PriceHistory) -> bool {
    history
        .values()
        .flat_map(|models| models.values())
        .all(|rows| {
            !rows.is_empty()
                && rows.iter().all(PricePeriod::valid)
                && rows.windows(2).all(|pair| {
                    pair[0].from < pair[1].from
                        && pair[0].until.is_none_or(|end| end <= pair[1].from)
                })
        })
}

pub(super) fn active(rows: &[PricePeriod], at: DateTime<Utc>) -> Option<&PricePeriod> {
    rows.iter()
        .rev()
        .find(|row| row.from <= at)
        .filter(|row| row.until.is_none_or(|end| at < end))
}

pub(super) fn strip_snapshot(model: &str) -> &str {
    for len in [10, 8] {
        if let Some((base, suffix)) = model
            .len()
            .checked_sub(len)
            .and_then(|i| model.get(..i).zip(model.get(i..)))
        {
            if let Some(base) = base.strip_suffix('-') {
                let format = if len == 10 { "%Y-%m-%d" } else { "%Y%m%d" };
                if chrono::NaiveDate::parse_from_str(suffix, format).is_ok() {
                    return base;
                }
            }
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use crate::pricing::{test_bundled_pricing, TokenBreakdown};

    #[test]
    fn deepseek_peak_uses_utc_weekdays_and_chinese_public_holidays() {
        let pricing = test_bundled_pricing();
        let cost = |at: &str| {
            pricing.estimated_cost_at(
                "claude",
                "deepseek-flash",
                at.parse().unwrap(),
                TokenBreakdown {
                    cache_read: 1_000_000,
                    ..Default::default()
                },
                true,
            )
        };
        for (timestamp, rate) in [
            ("2026-11-20T00:59:59Z", 0.003),
            ("2026-11-20T09:00:00+08:00", 0.006),
            ("2026-11-20T04:00:00Z", 0.003),
            ("2026-11-20T06:00:00Z", 0.006),
            ("2026-11-20T10:00:00Z", 0.003),
            ("2026-11-21T02:00:00Z", 0.003),
            ("2026-10-01T02:00:00Z", 0.003),
        ] {
            assert_eq!(cost(timestamp), Some(rate), "{timestamp}");
        }
    }

    #[test]
    fn claude_cache_cutover_uses_request_time_including_offset() {
        let pricing = test_bundled_pricing();
        let cost = |at: &str| {
            pricing.estimated_cost_at(
                "claude",
                "anthropic/claude-sonnet-5-5[1m]",
                at.parse().unwrap(),
                TokenBreakdown {
                    cache_read: 1_000_000,
                    ..Default::default()
                },
                true,
            )
        };
        assert_eq!(cost("2026-10-07T07:59:59+08:00"), Some(0.20));
        assert_eq!(cost("2026-10-07T08:00:00+08:00"), Some(0.10));
        assert_eq!(cost("2026-09-27T00:00:00Z"), None);
    }

    #[test]
    fn prompt_threshold_includes_read_and_write_buckets() {
        let pricing = test_bundled_pricing();
        let cost = |model, at: &str, read| {
            pricing
                .estimated_cost_at(
                    "claude",
                    model,
                    at.parse().unwrap(),
                    TokenBreakdown {
                        cache_write_1h: 20_000,
                        cache_read: read,
                        output: 1_000,
                        ..Default::default()
                    },
                    true,
                )
                .unwrap()
        };
        assert!((cost("claude-haiku-5-5", "2026-11-10T00:00:00Z", 80_000) - 0.0053).abs() < 1e-10);
        assert!(
            (cost("claude-haiku-5-5", "2026-11-10T00:00:00Z", 80_001) - 0.02650005).abs() < 1e-10
        );
        assert!((cost("claude-opus-4-6", "2026-03-12T23:59:59Z", 200_000) - 0.6375).abs() < 1e-10);
        assert!((cost("claude-opus-4-6", "2026-03-13T00:00:00Z", 200_000) - 0.325).abs() < 1e-10);
    }

    #[test]
    fn codex_alias_and_long_context_follow_both_sides_of_a_price_cut() {
        let pricing = test_bundled_pricing();
        let cost = |at: &str| {
            pricing.estimated_cost_at(
                "codex",
                "gpt-5.6-high-fast",
                at.parse().unwrap(),
                TokenBreakdown {
                    input: 300_000,
                    output: 10_000,
                    is_fast: true,
                    ..Default::default()
                },
                true,
            )
        };
        assert_eq!(cost("2026-08-20T23:59:59Z"), Some(6.9));
        assert_eq!(cost("2026-08-21T00:00:00Z"), Some(5.4));
        assert_eq!(cost("2026-12-01T00:00:00Z"), None);
    }

    #[test]
    fn scoped_rates_preserve_the_grok_slug_before_migration() {
        let pricing = test_bundled_pricing();
        let cost = |at: &str| {
            pricing.estimated_cost_at(
                "grok",
                "grok-code-fast-1",
                at.parse().unwrap(),
                TokenBreakdown {
                    input: 100_000,
                    ..Default::default()
                },
                true,
            )
        };
        assert_eq!(cost("2026-05-15T18:59:59Z"), Some(0.02));
        assert_eq!(cost("2026-05-15T19:00:00Z"), Some(0.10));
    }
}
