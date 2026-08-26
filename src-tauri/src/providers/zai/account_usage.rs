use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde_json::Value;

use crate::models::{
    DailyUsage, MetricValue, MetricValueKind, ModelUsageBreakdown, ModelUsageEntry, UsageHistory,
    UsagePeriod, ValueMetric,
};

use super::ZaiError;

const SOURCE_NOTE: &str = "From your Z.ai account usage history";
const CREDITS_METRIC_ID: &str = "credits30";
pub const HISTORY_DAYS: u64 = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct ZaiAccountUsage {
    pub history: UsageHistory,
    pub credits: Option<f64>,
}

impl ZaiAccountUsage {
    pub fn credits_metric(&self) -> Option<ValueMetric> {
        self.credits.map(|credits| ValueMetric {
            id: CREDITS_METRIC_ID.into(),
            label: "Last 30 Days Credits".into(),
            values: vec![MetricValue {
                number: credits,
                kind: MetricValueKind::Count,
                label: Some("credits".into()),
                estimated: false,
            }],
            expiries_at: Vec::new(),
        })
    }
}

#[derive(Default)]
struct AccountUsageAccumulator {
    days: BTreeMap<NaiveDate, AccountDay>,
}

#[derive(Default)]
struct AccountDay {
    tokens: u64,
    models: HashMap<String, ModelTotal>,
}

struct ModelTotal {
    display_name: String,
    tokens: u64,
}

impl AccountUsageAccumulator {
    fn add(&mut self, date: NaiveDate, model: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let model = normalized_model(model);
        let day = self.days.entry(date).or_default();
        day.tokens = day.tokens.saturating_add(tokens);
        let total = day
            .models
            .entry(model.to_ascii_lowercase())
            .or_insert_with(|| ModelTotal {
                display_name: model.to_owned(),
                tokens: 0,
            });
        total.tokens = total.tokens.saturating_add(tokens);
    }

    fn build(self, now: DateTime<Utc>) -> UsageHistory {
        let today = now.with_timezone(&Local).date_naive();
        let yesterday = today.checked_sub_days(Days::new(1));
        let daily = self
            .days
            .iter()
            .rev()
            .filter(|(_, day)| day.tokens > 0)
            .map(|(date, day)| DailyUsage {
                date: date.to_string(),
                tokens: day.tokens,
                estimated_cost_usd: None,
                estimate_complete: true,
            })
            .collect();

        UsageHistory {
            today: usage_period(&self.days, |date| *date == today),
            yesterday: yesterday
                .and_then(|yesterday| usage_period(&self.days, |date| *date == yesterday)),
            last_30_days: usage_period(&self.days, |_| true),
            daily,
            unknown_models: Vec::new(),
            other_usage: None,
        }
    }
}

pub fn map_credit_usage(body: &Value, now: DateTime<Utc>) -> Result<ZaiAccountUsage, ZaiError> {
    let data = response_data(body)?;
    let usage = data
        .get("modelUsage")
        .and_then(Value::as_object)
        .ok_or(ZaiError::InvalidResponse)?;
    let dates = bucket_dates(usage.get("xTime").or_else(|| usage.get("x_time")))?;
    let models = usage
        .get("modelDataList")
        .and_then(Value::as_array)
        .ok_or(ZaiError::InvalidResponse)?;
    let (mut accumulator, series_credits) = map_models(models, &dates, true, now)?;
    if models.is_empty() {
        add_unattributed_series(&mut accumulator, usage, &dates, now)?;
    }
    let summary_credits = data
        .get("summary")
        .and_then(|summary| summary.get("totalCredits"))
        .and_then(|total| total.get("value"))
        .and_then(number)
        .filter(|value| *value >= 0.0);
    let credits = (series_credits > 0.0)
        .then_some(series_credits)
        .or(summary_credits);
    Ok(ZaiAccountUsage {
        history: accumulator.build(now),
        credits,
    })
}

pub fn map_legacy_usage(body: &Value, now: DateTime<Utc>) -> Result<ZaiAccountUsage, ZaiError> {
    let data = response_data(body)?;
    let dates = bucket_dates(data.get("x_time").or_else(|| data.get("xTime")))?;
    let models = data
        .get("modelDataList")
        .and_then(Value::as_array)
        .ok_or(ZaiError::InvalidResponse)?;
    let (mut accumulator, _) = map_models(models, &dates, false, now)?;
    if models.is_empty() {
        add_unattributed_series(&mut accumulator, data, &dates, now)?;
    }
    Ok(ZaiAccountUsage {
        history: accumulator.build(now),
        credits: None,
    })
}

fn map_models(
    models: &[Value],
    dates: &[NaiveDate],
    credit_response: bool,
    now: DateTime<Utc>,
) -> Result<(AccountUsageAccumulator, f64), ZaiError> {
    let mut accumulator = AccountUsageAccumulator::default();
    let mut total_credits = 0.0;
    for model in models {
        let model = model.as_object().ok_or(ZaiError::InvalidResponse)?;
        let name = [model.get("modelName"), model.get("modelCode")]
            .into_iter()
            .flatten()
            .find_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(ZaiError::InvalidResponse)?;
        let total_tokens = token_series(
            model
                .get("totalTokensUsage")
                .or_else(|| model.get("tokensUsage")),
        )?;
        let cached_input = token_series(model.get("cachedInputTokensUsage"))?;
        let uncached_input = token_series(model.get("uncachedInputTokensUsage"))?;
        let output = token_series(model.get("outputTokensUsage"))?;
        let total_credit_series = numeric_series(model.get("totalCreditsUsage"))?;
        let cached_credits = numeric_series(model.get("cachedInputCreditsUsage"))?;
        let uncached_credits = numeric_series(model.get("uncachedInputCreditsUsage"))?;
        let output_credits = numeric_series(model.get("outputCreditsUsage"))?;

        for (index, date) in dates.iter().copied().enumerate() {
            if !date_in_range(date, now) {
                continue;
            }
            let components = value_at(&cached_input, index)
                .saturating_add(value_at(&uncached_input, index))
                .saturating_add(value_at(&output, index));
            let tokens = value_at(&total_tokens, index).max(components);
            accumulator.add(date, name, tokens);

            if credit_response {
                let component_credits = numeric_at(&cached_credits, index)
                    + numeric_at(&uncached_credits, index)
                    + numeric_at(&output_credits, index);
                total_credits += numeric_at(&total_credit_series, index).max(component_credits);
            }
        }
    }
    if !total_credits.is_finite() || total_credits < 0.0 {
        return Err(ZaiError::InvalidResponse);
    }
    Ok((accumulator, total_credits))
}

fn add_unattributed_series(
    accumulator: &mut AccountUsageAccumulator,
    usage: &serde_json::Map<String, Value>,
    dates: &[NaiveDate],
    now: DateTime<Utc>,
) -> Result<(), ZaiError> {
    let tokens = token_series(
        usage
            .get("totalTokensUsage")
            .or_else(|| usage.get("tokensUsage")),
    )?;
    for (index, date) in dates.iter().copied().enumerate() {
        if date_in_range(date, now) {
            accumulator.add(date, "Unattributed", value_at(&tokens, index));
        }
    }
    Ok(())
}

fn usage_period(
    days: &BTreeMap<NaiveDate, AccountDay>,
    include: impl Fn(&NaiveDate) -> bool,
) -> Option<UsagePeriod> {
    let selected = days
        .iter()
        .filter(|(date, day)| include(date) && day.tokens > 0)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return None;
    }
    let tokens = selected
        .iter()
        .fold(0_u64, |total, (_, day)| total.saturating_add(day.tokens));
    let mut models = HashMap::<String, ModelTotal>::new();
    for (_, day) in selected {
        for (key, model) in &day.models {
            let total = models.entry(key.clone()).or_insert_with(|| ModelTotal {
                display_name: model.display_name.clone(),
                tokens: 0,
            });
            total.tokens = total.tokens.saturating_add(model.tokens);
        }
    }
    let mut models = models
        .into_values()
        .map(|model| ModelUsageEntry {
            model: model.display_name,
            total_tokens: model.tokens,
            cost_usd: None,
            variants: None,
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .total_tokens
            .cmp(&left.total_tokens)
            .then_with(|| left.model.cmp(&right.model))
    });
    Some(UsagePeriod {
        tokens,
        estimated_cost_usd: None,
        cost_estimated: false,
        estimate_complete: true,
        model_breakdown: Some(ModelUsageBreakdown {
            models,
            source_note: SOURCE_NOTE.into(),
        }),
        unknown_models: Vec::new(),
    })
}

fn response_data(body: &Value) -> Result<&serde_json::Map<String, Value>, ZaiError> {
    if body.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(ZaiError::InvalidResponse);
    }
    body.get("data")
        .and_then(Value::as_object)
        .ok_or(ZaiError::InvalidResponse)
}

fn bucket_dates(value: Option<&Value>) -> Result<Vec<NaiveDate>, ZaiError> {
    value
        .and_then(Value::as_array)
        .ok_or(ZaiError::InvalidResponse)?
        .iter()
        .map(|value| {
            let value = value.as_str().ok_or(ZaiError::InvalidResponse)?;
            let date = value.get(..10).ok_or(ZaiError::InvalidResponse)?;
            NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|_| ZaiError::InvalidResponse)
        })
        .collect()
}

fn token_series(value: Option<&Value>) -> Result<Vec<u64>, ZaiError> {
    numeric_series(value)?
        .into_iter()
        .map(|value| {
            if value < 0.0 || value > u64::MAX as f64 {
                Err(ZaiError::InvalidResponse)
            } else {
                Ok(value.trunc() as u64)
            }
        })
        .collect()
}

fn numeric_series(value: Option<&Value>) -> Result<Vec<f64>, ZaiError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or(ZaiError::InvalidResponse)?
        .iter()
        .map(|value| number(value).ok_or(ZaiError::InvalidResponse))
        .collect()
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .filter(|value: &f64| value.is_finite())
}

fn value_at(values: &[u64], index: usize) -> u64 {
    values.get(index).copied().unwrap_or_default()
}

fn numeric_at(values: &[f64], index: usize) -> f64 {
    values.get(index).copied().unwrap_or_default()
}

fn date_in_range(date: NaiveDate, now: DateTime<Utc>) -> bool {
    let today = now.with_timezone(&Local).date_naive();
    let since = today
        .checked_sub_days(Days::new(HISTORY_DAYS.saturating_sub(1)))
        .unwrap_or(today);
    date >= since && date <= today
}

fn normalized_model(model: &str) -> &str {
    let model = model.trim();
    if model.is_empty() {
        "Unattributed"
    } else {
        model
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::{map_credit_usage, map_legacy_usage};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap()
    }

    #[test]
    fn credit_history_maps_string_series_models_and_exact_credits() {
        let mapped = map_credit_usage(
            &json!({
                "success": true,
                "data": {
                    "summary": {"totalCredits": {"value": "1.2345"}},
                    "modelUsage": {
                        "xTime": ["2026-08-26 00:00", "2026-08-27 00:00"],
                        "modelDataList": [{
                            "modelCode": "glm-5.3-flash",
                            "modelName": "GLM-5.3-Flash",
                            "cachedInputTokensUsage": ["100", "10"],
                            "uncachedInputTokensUsage": ["20", "5"],
                            "outputTokensUsage": ["5", "2"],
                            "totalTokensUsage": ["125", "17"],
                            "cachedInputCreditsUsage": ["0.6000", "0.1000"],
                            "uncachedInputCreditsUsage": ["0.3000", "0.0500"],
                            "outputCreditsUsage": ["0.1000", "0.0845"],
                            "totalCreditsUsage": ["1.0000", "0.2345"]
                        }]
                    }
                }
            }),
            now(),
        )
        .unwrap();

        assert_eq!(mapped.history.today.as_ref().unwrap().tokens, 17);
        assert_eq!(mapped.history.yesterday.as_ref().unwrap().tokens, 125);
        let period = mapped.history.last_30_days.as_ref().unwrap();
        assert_eq!(period.tokens, 142);
        assert_eq!(period.estimated_cost_usd, None);
        assert_eq!(
            period.model_breakdown.as_ref().unwrap().models[0].model,
            "GLM-5.3-Flash"
        );
        assert!((mapped.credits.unwrap() - 1.2345).abs() < 0.000_001);
        assert_eq!(
            mapped.credits_metric().unwrap().values[0].label.as_deref(),
            Some("credits")
        );
    }

    #[test]
    fn legacy_history_maps_daily_tokens_without_inventing_cost() {
        let mapped = map_legacy_usage(
            &json!({
                "success": true,
                "data": {
                    "x_time": ["2026-08-27 00:00"],
                    "tokensUsage": [100],
                    "modelDataList": [{
                        "modelCode": "glm-4.7",
                        "modelName": "GLM-4.7",
                        "tokensUsage": [100]
                    }]
                }
            }),
            now(),
        )
        .unwrap();

        let today = mapped.history.today.unwrap();
        assert_eq!(today.tokens, 100);
        assert_eq!(today.estimated_cost_usd, None);
        assert_eq!(today.model_breakdown.unwrap().models[0].model, "GLM-4.7");
        assert_eq!(mapped.credits, None);
    }

    #[test]
    fn empty_history_is_valid_but_changed_contract_is_not() {
        let empty = map_credit_usage(
            &json!({
                "success": true,
                "data": {
                    "summary": {"totalCredits": {"value": "0.0000"}},
                    "modelUsage": {"xTime": [], "modelDataList": []}
                }
            }),
            now(),
        )
        .unwrap();
        assert_eq!(empty.history, crate::models::UsageHistory::default());
        assert_eq!(empty.credits, Some(0.0));

        assert!(map_credit_usage(&json!({"success": true, "data": {}}), now()).is_err());
        assert!(map_legacy_usage(
            &json!({"success": false, "data": {"x_time": [], "modelDataList": []}}),
            now()
        )
        .is_err());
    }
}
