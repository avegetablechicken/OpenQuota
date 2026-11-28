use crate::models::{DailyUsage, ModelUsageBreakdown, ModelUsageEntry, UsageHistory, UsagePeriod};
use chrono::{Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq)]
pub(super) struct AccountStats {
    #[serde(deserialize_with = "deserialize_null_default")]
    pub(super) history: Vec<AccountStatsDay>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub(super) models: Vec<AccountStatsModel>,
}

#[derive(Clone, Copy)]
enum CostBasis {
    Subscription,
    Standard,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub(super) struct AccountStatsDay {
    date: String,
    #[serde(flatten)]
    usage: ReportedUsage,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub(super) struct AccountStatsModel {
    model: String,
    #[serde(flatten)]
    usage: ReportedUsage,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
pub(super) struct ReportedUsage {
    #[serde(default, alias = "total_tokens")]
    tokens: u64,
    actual_cost: Option<f64>,
    cost: Option<f64>,
}

impl ReportedUsage {
    pub(super) fn period(&self) -> UsagePeriod {
        Total {
            tokens: self.tokens,
            amount: self.amount(),
        }
        .period(None)
    }

    fn amount(&self) -> Option<f64> {
        self.actual_cost
            .filter(|n| n.is_finite() && *n >= 0.0)
            .or_else(|| self.cost.filter(|n| n.is_finite() && *n >= 0.0))
    }

    fn amount_for(&self, basis: CostBasis) -> Option<f64> {
        match basis {
            CostBasis::Subscription => self.amount(),
            // Admin daily `actual_cost` is account-adjusted, while model
            // `actual_cost` is user-adjusted. Only `cost` has the same standard
            // meaning in both, so never substitute either adjusted amount.
            CostBasis::Standard => self.cost.filter(|n| n.is_finite() && *n >= 0.0),
        }
    }
}

#[derive(Clone, Copy)]
struct Total {
    tokens: u64,
    amount: Option<f64>,
}

impl Default for Total {
    fn default() -> Self {
        Self {
            tokens: 0,
            amount: Some(0.0),
        }
    }
}

impl Total {
    fn add(&mut self, tokens: u64, amount: Option<f64>) {
        self.tokens = self.tokens.saturating_add(tokens);
        self.amount = self.amount.zip(amount).map(|(a, b)| a + b);
    }

    fn period(self, breakdown: Option<ModelUsageBreakdown>) -> UsagePeriod {
        UsagePeriod {
            tokens: self.tokens,
            estimated_cost_usd: self.amount,
            cost_estimated: false,
            estimate_complete: self.amount.is_some(),
            model_breakdown: breakdown,
            unknown_models: Vec::new(),
        }
    }
}

impl AccountStats {
    pub(super) fn into_history(self, now: chrono::DateTime<Utc>) -> UsageHistory {
        self.into_history_with_basis(now, CostBasis::Subscription)
    }

    pub(super) fn into_standard_history(self, now: chrono::DateTime<Utc>) -> UsageHistory {
        self.into_history_with_basis(now, CostBasis::Standard)
    }

    fn into_history_with_basis(self, now: chrono::DateTime<Utc>, basis: CostBasis) -> UsageHistory {
        let today = now.with_timezone(&Local).date_naive();
        let mut days = std::collections::BTreeMap::<NaiveDate, Total>::new();
        let mut monthly = Total::default();
        for day in self.history {
            let Ok(date) = NaiveDate::parse_from_str(day.date.trim(), "%Y-%m-%d") else {
                continue;
            };
            let amount = day.usage.amount_for(basis);
            if day.usage.tokens == 0 && amount.unwrap_or_default() == 0.0 {
                continue;
            }
            days.entry(date).or_default().add(day.usage.tokens, amount);
            monthly.add(day.usage.tokens, amount);
        }
        let mut models = std::collections::BTreeMap::<String, Total>::new();
        for entry in self.models {
            let name = entry.model.trim();
            let amount = entry.usage.amount_for(basis);
            if !name.is_empty() && (entry.usage.tokens > 0 || amount.unwrap_or_default() > 0.0) {
                models
                    .entry(name.to_owned())
                    .or_default()
                    .add(entry.usage.tokens, amount);
            }
        }
        let mut models: Vec<_> = models
            .into_iter()
            .map(|(model, total)| ModelUsageEntry {
                model,
                total_tokens: total.tokens,
                cost_usd: total.amount,
                variants: None,
            })
            .collect();
        models.sort_by(|a, b| {
            b.cost_usd
                .partial_cmp(&a.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.total_tokens.cmp(&a.total_tokens))
                .then_with(|| a.model.cmp(&b.model))
        });
        let breakdown = (!models.is_empty()).then(|| ModelUsageBreakdown {
            models,
            source_note: match basis {
                CostBasis::Subscription => "Costs reported by Sub2API for this API key",
                CostBasis::Standard => {
                    "Standard costs reported by Sub2API for this upstream account"
                }
            }
            .into(),
        });
        UsageHistory {
            today: days.get(&today).map(|day| day.period(None)),
            yesterday: today
                .checked_sub_days(Days::new(1))
                .and_then(|date| days.get(&date))
                .map(|day| day.period(None)),
            last_30_days: (!days.is_empty()).then(|| monthly.period(breakdown)),
            daily: days
                .into_iter()
                .map(|(date, total)| DailyUsage {
                    date: date.to_string(),
                    tokens: total.tokens,
                    estimated_cost_usd: total.amount,
                    estimate_complete: total.amount.is_some(),
                })
                .collect(),
            ..UsageHistory::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    #[test]
    fn standard_summaries_never_substitute_adjusted_costs() {
        let now = Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap();
        let date = now.with_timezone(&Local).date_naive().to_string();
        let stats: AccountStats = serde_json::from_value(json!({
            "history":[{"date":date,"tokens":100,"cost":2.0,"actual_cost":9.0,"user_cost":8.0}],
            "models":[{"model":"model-a","total_tokens":100,"cost":2.0,"actual_cost":8.0,"account_cost":9.0}]
        })).unwrap();
        let standard = stats.clone().into_standard_history(now);
        assert_eq!(standard.today.unwrap().estimated_cost_usd, Some(2.0));
        let period = standard.last_30_days.unwrap();
        assert!(!period.cost_estimated);
        let breakdown = period.model_breakdown.unwrap();
        assert!(breakdown.source_note.contains("Standard costs"));
        assert_eq!(breakdown.models[0].cost_usd, Some(2.0));
        assert_eq!(
            stats.into_history(now).today.unwrap().estimated_cost_usd,
            Some(9.0)
        );

        for cost in [serde_json::Value::Null, json!(0)] {
            let stats: AccountStats = serde_json::from_value(json!({
                "history":[{"date":date,"tokens":100,"cost":cost,"actual_cost":9.0}],
                "models":[{"model":"model-a","total_tokens":100,"cost":cost,"actual_cost":8.0}]
            }))
            .unwrap();
            let period = stats.into_standard_history(now).last_30_days.unwrap();
            assert_eq!(period.estimated_cost_usd, cost.as_f64());
            assert_eq!(period.estimate_complete, !cost.is_null());
            assert_eq!(
                period.model_breakdown.unwrap().models[0].cost_usd,
                cost.as_f64()
            );
        }
    }

    #[test]
    fn into_history_trims_valid_dates_and_skips_malformed_rows_consistently() {
        let now = Utc.with_ymd_and_hms(2026, 11, 22, 12, 0, 0).unwrap();
        let today_str = now.with_timezone(&Local).date_naive().to_string();
        let stats: AccountStats = serde_json::from_value(json!({
            "history": [
                {"date": format!(" {today_str} "), "tokens": 100, "actual_cost": 1.25},
                {"date": today_str, "tokens": 50, "actual_cost": 0.25},
                {"date": "not-a-date", "tokens": 999, "actual_cost": null},
                {"date": "2026-02-30", "tokens": 500, "cost": 5.0},
                {"date": "", "tokens": 100, "cost": 1.0}
            ],
            "models": []
        }))
        .unwrap();

        let history = stats.into_history(now);
        let today = history.today.unwrap();
        assert_eq!(today.tokens, 150);
        assert_eq!(today.estimated_cost_usd, Some(1.5));
        assert!(today.estimate_complete);

        let monthly = history.last_30_days.unwrap();
        assert_eq!(monthly.tokens, 150);
        assert_eq!(monthly.estimated_cost_usd, Some(1.5));
        assert!(monthly.estimate_complete);

        assert_eq!(history.daily.len(), 1);
        assert_eq!(history.daily[0].tokens, 150);
    }

    #[test]
    fn all_malformed_history_dates_produce_no_spend_periods() {
        let now = Utc.with_ymd_and_hms(2026, 11, 22, 12, 0, 0).unwrap();
        let stats: AccountStats = serde_json::from_value(json!({
            "history": [
                {"date": "bad", "tokens": 100, "cost": 1.0}
            ],
            "models": []
        }))
        .unwrap();

        let history = stats.into_history(now);
        assert!(history.today.is_none());
        assert!(history.yesterday.is_none());
        assert!(history.last_30_days.is_none());
        assert!(history.daily.is_empty());
    }

    #[test]
    fn account_stats_requires_history_but_accepts_explicit_empty_slices() {
        assert!(serde_json::from_value::<AccountStats>(json!({})).is_err());
        let empty: AccountStats = serde_json::from_value(json!({"history":[]})).unwrap();
        assert!(empty.history.is_empty());

        let nulls: AccountStats =
            serde_json::from_value(json!({"history": null, "models": null})).unwrap();
        assert!(nulls.history.is_empty());
        assert!(nulls.models.is_empty());
    }
}
