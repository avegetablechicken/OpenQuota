use std::collections::BTreeMap;

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::models::{DailyUsage, UsageHistory, UsagePeriod};

use super::client::UsageResponse;

#[derive(Deserialize)]
struct ProfileResponse {
    stats: ProfileStats,
}

#[derive(Deserialize)]
struct ProfileStats {
    daily_usage_buckets: Option<Vec<ProfileDailyBucket>>,
}

#[derive(Deserialize)]
struct ProfileDailyBucket {
    start_date: String,
    tokens: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountUsageOutcome {
    Available(Box<UsageHistory>),
    Unavailable,
    Failed,
}

pub fn classify_account_usage(response: &UsageResponse, now: DateTime<Utc>) -> AccountUsageOutcome {
    if response.status.is_success() {
        return map_account_usage(response, now)
            .map(Box::new)
            .map(AccountUsageOutcome::Available)
            .unwrap_or(AccountUsageOutcome::Unavailable);
    }
    if matches!(
        response.status,
        StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::GONE
            | StatusCode::NOT_IMPLEMENTED
    ) {
        AccountUsageOutcome::Unavailable
    } else {
        AccountUsageOutcome::Failed
    }
}

fn map_account_usage(response: &UsageResponse, now: DateTime<Utc>) -> Option<UsageHistory> {
    let profile: ProfileResponse = serde_json::from_value(response.body.clone()).ok()?;
    let buckets = profile.stats.daily_usage_buckets?;
    let today = now.with_timezone(&Local).date_naive();
    let yesterday = today.checked_sub_days(Days::new(1));
    let since = today
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);
    let mut tokens_by_date = BTreeMap::<NaiveDate, u64>::new();

    for bucket in buckets {
        let date = NaiveDate::parse_from_str(bucket.start_date.trim(), "%Y-%m-%d").ok()?;
        let tokens = u64::try_from(bucket.tokens).ok()?;
        if date < since || date > today {
            continue;
        }
        let total = tokens_by_date.entry(date).or_default();
        *total = total.saturating_add(tokens);
    }

    let daily = tokens_by_date
        .iter()
        .rev()
        .filter(|(_, tokens)| **tokens > 0)
        .map(|(date, tokens)| DailyUsage {
            date: date.to_string(),
            tokens: *tokens,
            estimated_cost_usd: None,
            estimate_complete: true,
        })
        .collect();
    let today_period = usage_period(tokens_by_date.get(&today).copied().unwrap_or_default());
    let yesterday_period = yesterday
        .and_then(|date| usage_period(tokens_by_date.get(&date).copied().unwrap_or_default()));
    let last_30_days = usage_period(
        tokens_by_date
            .values()
            .copied()
            .fold(0_u64, u64::saturating_add),
    );

    Some(UsageHistory {
        today: today_period,
        yesterday: yesterday_period,
        last_30_days,
        daily,
        unknown_models: Vec::new(),
        other_usage: None,
    })
}

fn usage_period(tokens: u64) -> Option<UsagePeriod> {
    (tokens > 0).then_some(UsagePeriod {
        tokens,
        estimated_cost_usd: None,
        cost_estimated: false,
        estimate_complete: true,
        model_breakdown: None,
        unknown_models: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{Days, Local, TimeZone, Utc};
    use reqwest::StatusCode;
    use serde_json::json;

    use super::{classify_account_usage, AccountUsageOutcome};
    use crate::providers::codex::client::UsageResponse;

    fn response(status: StatusCode, body: serde_json::Value) -> UsageResponse {
        UsageResponse {
            status,
            headers: HashMap::new(),
            body,
        }
    }

    #[test]
    fn maps_account_buckets_into_existing_usage_periods() {
        let now = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
        let today = now.with_timezone(&Local).date_naive();
        let yesterday = today.checked_sub_days(Days::new(1)).unwrap();
        let oldest = today.checked_sub_days(Days::new(30)).unwrap();
        let outside = today.checked_sub_days(Days::new(31)).unwrap();
        let outcome = classify_account_usage(
            &response(
                StatusCode::OK,
                json!({
                    "stats": {
                        "lifetime_tokens": 9999,
                        "daily_usage_buckets": [
                            {"start_date": outside.to_string(), "tokens": 1000},
                            {"start_date": yesterday.to_string(), "tokens": 50},
                            {"start_date": today.to_string(), "tokens": 100},
                            {"start_date": oldest.to_string(), "tokens": 25},
                            {"start_date": today.to_string(), "tokens": 20}
                        ]
                    }
                }),
            ),
            now,
        );
        let AccountUsageOutcome::Available(history) = outcome else {
            panic!("valid profile should map to account usage");
        };
        let history = *history;

        assert_eq!(history.today.unwrap().tokens, 120);
        assert_eq!(history.yesterday.unwrap().tokens, 50);
        assert_eq!(history.last_30_days.unwrap().tokens, 195);
        assert_eq!(history.daily.len(), 3);
        assert_eq!(history.daily[0].date, today.to_string());
        assert_eq!(history.daily[0].estimated_cost_usd, None);
        assert!(history.daily.iter().all(|day| day.estimate_complete));
    }

    #[test]
    fn empty_profile_history_is_a_valid_account_scope() {
        let outcome = classify_account_usage(
            &response(
                StatusCode::OK,
                json!({"stats": {"daily_usage_buckets": []}}),
            ),
            Utc::now(),
        );
        let AccountUsageOutcome::Available(history) = outcome else {
            panic!("empty profile should remain an available account scope");
        };
        let history = *history;

        assert_eq!(history, crate::models::UsageHistory::default());
    }

    #[test]
    fn missing_or_changed_profile_contract_is_unavailable() {
        let now = Utc::now();
        for response in [
            response(StatusCode::OK, json!({"stats": {}})),
            response(
                StatusCode::OK,
                json!({"stats": {"daily_usage_buckets": null}}),
            ),
            response(
                StatusCode::OK,
                json!({"stats": {"daily_usage_buckets": [{"start_date": "changed", "tokens": 1}]}}),
            ),
            response(
                StatusCode::OK,
                json!({"stats": {"daily_usage_buckets": [{"start_date": "2026-08-20", "tokens": -1}]}}),
            ),
        ] {
            assert_eq!(
                classify_account_usage(&response, now),
                AccountUsageOutcome::Unavailable
            );
        }

        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::GONE,
            StatusCode::NOT_IMPLEMENTED,
        ] {
            assert_eq!(
                classify_account_usage(
                    &response(status, json!({"stats": {"daily_usage_buckets": []}})),
                    now,
                ),
                AccountUsageOutcome::Unavailable
            );
        }
    }

    #[test]
    fn access_errors_are_distinct_from_an_unavailable_contract() {
        let now = Utc::now();
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert_eq!(
                classify_account_usage(&response(status, json!({"error": "unavailable"})), now,),
                AccountUsageOutcome::Failed
            );
        }
    }
}
