use super::{classify_status, transport_error, Envelope, Sub2ApiClient, Sub2ApiError};
use super::{Sub2ApiAccount, UsageCache};
use crate::models::{DailyUsage, ModelUsageBreakdown, ModelUsageEntry, UsageHistory, UsagePeriod};
use chrono::{Days, Local, NaiveDate, Utc};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize, PartialEq)]
pub(super) struct AccountStats {
    #[serde(default)]
    history: Vec<AccountStatsDay>,
    #[serde(default)]
    models: Vec<AccountStatsModel>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
struct AccountStatsDay {
    date: String,
    #[serde(flatten)]
    usage: ReportedUsage,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
struct AccountStatsModel {
    model: String,
    #[serde(flatten)]
    usage: ReportedUsage,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
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
    fn stats(&self, token: &str, account_id: i64, days: u8) -> Result<AccountStats, Sub2ApiError> {
        let account_id = account_id.to_string();
        let mut url = self.endpoint(&["api", "v1", "admin", "accounts", &account_id, "stats"])?;
        url.query_pairs_mut().append_pair("days", &days.to_string());
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

#[derive(Serialize, Deserialize)]
struct CachedStats {
    checked: chrono::DateTime<Utc>,
    local_day: NaiveDate,
    utc_day: NaiveDate,
    activity: Option<String>,
    today: AccountStats,
    month: AccountStats,
    past: Option<AccountStats>,
}

impl Sub2ApiClient {
    pub(super) fn cached_stats(
        &self,
        token: &str,
        account: &Sub2ApiAccount,
        now: chrono::DateTime<Utc>,
        cache: &UsageCache,
    ) -> Result<AccountStats, Sub2ApiError> {
        let stored: Option<String> = cache
            .db
            .query_row(
                "SELECT payload FROM stats WHERE account=?1",
                [account.id],
                |r| r.get(0),
            )
            .optional()?;
        let local_day = now.with_timezone(&Local).date_naive();
        let utc_day = now.date_naive();
        let previous = stored
            .as_deref()
            .map(serde_json::from_str::<CachedStats>)
            .transpose()
            .map_err(|_| Sub2ApiError::HistoryCache)?;
        if let Some(previous) = &previous {
            if previous.local_day == local_day
                && previous.utc_day == utc_day
                && account.last_used_at.is_some()
                && previous.activity == account.last_used_at
                && (0..3600).contains(&(now - previous.checked).num_seconds())
            {
                return Ok(previous.month.clone());
            }
        }
        let today = self.stats(token, account.id, 1)?;
        let same_day = previous.as_ref().filter(|v| {
            let date = today.history.first().map(|d| &d.date);
            v.today.history.first().map(|d| &d.date) == date
                && (date.is_some() || (v.local_day == local_day && v.utc_day == utc_day))
        });
        let (month, past) = match same_day {
            Some(v) if v.today == today => (v.month.clone(), v.past.clone()),
            Some(v) if v.past.is_some() => {
                let mut month = v.past.clone().unwrap_or_default();
                month.extend(today.clone());
                (month, v.past.clone())
            }
            _ => {
                let month = self.stats(token, account.id, 30)?;
                let past = month.without_today(&today, local_day);
                (month, past)
            }
        };
        let entry = CachedStats {
            checked: now,
            local_day,
            utc_day,
            activity: account.last_used_at.clone(),
            today,
            month,
            past,
        };
        let payload = serde_json::to_string(&entry).map_err(|_| Sub2ApiError::HistoryCache)?;
        cache.db.execute(
            "INSERT OR REPLACE INTO stats(account,payload) VALUES(?1,?2)",
            rusqlite::params![account.id, payload],
        )?;
        Ok(entry.month)
    }
}

impl Sub2ApiClient {
    /// The server summary for days before today. Those days only change when the local
    /// or UTC calendar day changes, so one cached summary serves the whole day.
    pub(super) fn past_stats(
        &self,
        token: &str,
        account: &Sub2ApiAccount,
        now: chrono::DateTime<Utc>,
        cache: &UsageCache,
    ) -> Result<AccountStats, Sub2ApiError> {
        let stored: Option<String> = cache
            .db
            .query_row(
                "SELECT payload FROM stats WHERE account=?1",
                [account.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = stored
            .as_deref()
            .and_then(|payload| serde_json::from_str::<CachedStats>(payload).ok())
        {
            if previous.local_day == now.with_timezone(&Local).date_naive()
                && previous.utc_day == now.date_naive()
            {
                return Ok(previous.month);
            }
        }
        self.cached_stats(token, account, now, cache)
    }
}

impl AccountStats {
    // The server reports a single model breakdown for the entire requested period.
    // Remove the matching one-day breakdown once, then replace only the live day.
    // If usage changed between the two reads, keep the authoritative full response.
    fn without_today(&self, today: &Self, local_day: NaiveDate) -> Option<Self> {
        let mut past = self.clone();
        let [day] = today.history.as_slice() else {
            return (today.history.is_empty()
                && today.models.is_empty()
                && !self.history.iter().any(|d| d.date == local_day.to_string()))
            .then_some(past);
        };
        let index = past.history.iter().position(|d| d == day)?;
        past.history.remove(index);
        for model in &today.models {
            let previous = past.models.iter_mut().find(|m| m.model == model.model)?;
            let tokens = previous.usage.tokens.checked_sub(model.usage.tokens)?;
            let amount = previous.usage.amount()? - model.usage.amount()?;
            if amount < -1e-9 {
                return None;
            }
            previous.usage = ReportedUsage {
                tokens,
                actual_cost: Some(amount.max(0.0)),
                cost: None,
            };
        }
        Some(past)
    }

    /// Server-reported tokens and cost for each local date in `from..until`.
    pub(super) fn days_before(
        &self,
        from: NaiveDate,
        until: NaiveDate,
    ) -> Vec<(NaiveDate, u64, Option<f64>)> {
        let mut days = std::collections::BTreeMap::<NaiveDate, Total>::new();
        for day in &self.history {
            let Ok(date) = NaiveDate::parse_from_str(day.date.trim(), "%Y-%m-%d") else {
                continue;
            };
            let amount = day.usage.amount();
            if date < from
                || date >= until
                || (day.usage.tokens == 0 && amount.unwrap_or(0.0) == 0.0)
            {
                continue;
            }
            days.entry(date).or_default().add(day.usage.tokens, amount);
        }
        days.into_iter()
            .map(|(date, total)| (date, total.tokens, total.amount))
            .collect()
    }

    pub(super) fn extend(&mut self, other: Self) {
        self.history.extend(other.history);
        self.models.extend(other.models);
    }

    pub(super) fn into_history(self, now: chrono::DateTime<Utc>) -> UsageHistory {
        let today = now.with_timezone(&Local).date_naive();
        let mut days = std::collections::BTreeMap::<NaiveDate, Total>::new();
        let mut monthly = Total::default();
        for day in self.history {
            let Ok(date) = NaiveDate::parse_from_str(day.date.trim(), "%Y-%m-%d") else {
                continue;
            };
            let amount = day.usage.amount();
            if day.usage.tokens == 0 && amount.unwrap_or_default() == 0.0 {
                continue;
            }
            days.entry(date).or_default().add(day.usage.tokens, amount);
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

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
    fn caches_the_month_across_restarts_and_replaces_only_todays_stats() {
        let now = Utc.with_ymd_and_hms(2026, 11, 24, 12, 0, 0).unwrap();
        let today = now.with_timezone(&Local).date_naive();
        let yesterday = today.pred_opt().unwrap();
        let row = |date, tokens, cost| json!({"date":date,"tokens":tokens,"actual_cost":cost});
        let stats = |days: Vec<serde_json::Value>, tokens, cost| {
            json!({"code":0,"data":{
                "history":days,"models":[{"model":"x","tokens":tokens,"actual_cost":cost}]
            }})
            .to_string()
        };
        let (base, requests, worker) = super::super::tests::serve_sequence(vec![
            stats(vec![row(today, 2, 0.2)], 2, 0.2),
            stats(vec![row(yesterday, 3, 0.3), row(today, 2, 0.2)], 5, 0.5),
            stats(vec![row(today, 4, 0.4)], 4, 0.4),
            stats(vec![row(today, 4, 0.4)], 4, 0.4),
            stats(vec![], 0, 0.0),
            stats(vec![row(today, 4, 0.4)], 4, 0.4),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let client = Sub2ApiClient::new(&base).unwrap();
        let fetch = |activity: Option<&str>, time| {
            let cache = UsageCache::open(&path, "test").unwrap();
            let account = Sub2ApiAccount {
                id: 7,
                name: String::new(),
                last_used_at: activity.map(str::to_owned),
            };
            client
                .cached_stats("token", &account, time, &cache)
                .unwrap()
                .into_history(time)
        };
        assert_eq!(fetch(Some("1"), now).last_30_days.unwrap().tokens, 5);
        assert_eq!(fetch(Some("1"), now).last_30_days.unwrap().tokens, 5);
        for activity in [Some("2"), None] {
            let history = fetch(activity, now);
            assert_eq!(history.today.unwrap().tokens, 4);
            let month = history.last_30_days.unwrap();
            assert_eq!(month.tokens, 7);
            let model = &month.model_breakdown.unwrap().models[0];
            assert_eq!(model.total_tokens, 7);
            assert!((model.cost_usd.unwrap() - 0.7).abs() < 1e-12);
        }
        assert_eq!(
            fetch(None, now + chrono::Duration::days(1))
                .last_30_days
                .unwrap()
                .tokens,
            4
        );
        worker.join().unwrap();
        let days: Vec<_> = requests
            .try_iter()
            .map(|r| {
                r.split("days=")
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(days, ["1", "30", "1", "1", "1", "30"]);
    }
}
