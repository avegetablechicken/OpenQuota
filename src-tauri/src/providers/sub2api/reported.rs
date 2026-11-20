use super::{classify_status, transport_error, Envelope, Sub2ApiClient, Sub2ApiError};
use crate::models::{DailyUsage, ModelUsageBreakdown, ModelUsageEntry, UsageHistory, UsagePeriod};
use chrono::{Days, Local, NaiveDate, Utc};
use serde::Deserialize;

#[derive(Clone, Default, Deserialize)]
pub(super) struct AccountStats {
    #[serde(default)]
    history: Vec<AccountStatsDay>,
    #[serde(default)]
    models: Vec<AccountStatsModel>,
}

#[derive(Clone, Deserialize)]
struct AccountStatsDay {
    date: NaiveDate,
    #[serde(flatten)]
    usage: ReportedUsage,
}

#[derive(Clone, Deserialize)]
struct AccountStatsModel {
    model: String,
    #[serde(flatten)]
    usage: ReportedUsage,
}

#[derive(Clone, Deserialize)]
struct ReportedUsage {
    #[serde(default, alias = "total_tokens")]
    tokens: u64,
    actual_cost: Option<f64>,
    cost: Option<f64>,
}

impl ReportedUsage {
    fn amount(&self) -> Option<f64> {
        self.actual_cost
            .filter(|n| n.is_finite() && *n >= 0.0)
            .or_else(|| self.cost.filter(|n| n.is_finite() && *n >= 0.0))
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

impl Sub2ApiClient {
    pub(super) fn stats(&self, token: &str, account_id: i64) -> Result<AccountStats, Sub2ApiError> {
        let account_id = account_id.to_string();
        let mut url = self.endpoint(&["api", "v1", "admin", "accounts", &account_id, "stats"])?;
        url.query_pairs_mut().append_pair("days", "30");
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .map_err(|error| transport_error("usage statistics", &error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }
        let envelope = response
            .json::<Envelope<AccountStats>>()
            .map_err(|_| Sub2ApiError::InvalidResponse)?;
        if envelope.code != 0 {
            return Err(Sub2ApiError::InvalidResponse);
        }
        envelope.data.ok_or(Sub2ApiError::InvalidResponse)
    }
}

impl AccountStats {
    pub(super) fn extend(&mut self, other: Self) {
        self.history.extend(other.history);
        self.models.extend(other.models);
    }

    pub(super) fn into_history(self, now: chrono::DateTime<Utc>) -> UsageHistory {
        let today = now.with_timezone(&Local).date_naive();
        let mut days = std::collections::BTreeMap::<NaiveDate, Total>::new();
        let mut monthly = Total::default();
        for day in self.history {
            let amount = day.usage.amount();
            if day.usage.tokens == 0 && amount.unwrap_or_default() == 0.0 {
                continue;
            }
            days.entry(day.date)
                .or_default()
                .add(day.usage.tokens, amount);
            monthly.add(day.usage.tokens, amount);
        }
        let mut models = std::collections::BTreeMap::<String, Total>::new();
        for entry in self.models {
            let name = entry.model.trim();
            let amount = entry.usage.amount();
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
            source_note: "Account costs reported by Sub2API".into(),
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
