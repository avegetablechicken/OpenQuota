use serde_json::Value;

use super::OpenRouterError;
use crate::models::{MetricValue, MetricValueKind, QuotaFormat, QuotaWindow, ValueMetric};

#[derive(Debug, Default, PartialEq)]
pub struct CreditsMetrics {
    pub quota: Option<QuotaWindow>,
    pub balance: Option<ValueMetric>,
}

#[derive(Debug, Default, PartialEq)]
pub struct KeyMetrics {
    pub plan: Option<String>,
    pub quota: Option<QuotaWindow>,
    pub values: Vec<ValueMetric>,
}

pub fn data_object(body: &Value) -> Option<&serde_json::Map<String, Value>> {
    body.get("data")?.as_object()
}

pub fn map_credits(
    data: &serde_json::Map<String, Value>,
) -> Result<CreditsMetrics, OpenRouterError> {
    let total_usage = number(data.get("total_usage")).ok_or(OpenRouterError::InvalidResponse)?;
    let used = total_usage.max(0.0);
    let total = number(data.get("total_credits"))
        .ok_or(OpenRouterError::InvalidResponse)?
        .max(0.0);
    Ok(CreditsMetrics {
        quota: (total > 0.0).then(|| dollars_quota("credits", "Credits", used, total)),
        balance: Some(dollars_value("balance", "Balance", (total - used).max(0.0))),
    })
}

pub fn map_key(data: &serde_json::Map<String, Value>) -> Result<KeyMetrics, OpenRouterError> {
    if ![
        "usage",
        "usage_daily",
        "usage_weekly",
        "usage_monthly",
        "limit_remaining",
    ]
    .iter()
    .any(|field| number(data.get(*field)).is_some())
    {
        return Err(OpenRouterError::InvalidResponse);
    }
    let values = [
        ("today", "Today", "usage_daily"),
        ("week", "This Week", "usage_weekly"),
        ("month", "This Month", "usage_monthly"),
    ]
    .into_iter()
    .filter_map(|(id, label, field)| {
        number(data.get(field)).map(|value| dollars_value(id, label, value.max(0.0)))
    })
    .collect();
    let quota = number(data.get("limit"))
        .filter(|limit| *limit > 0.0)
        .map(|limit| {
            key_limit_used(data, limit)
                .map(|used| dollars_quota("keyLimit", "Key Limit", used, limit))
                .ok_or(OpenRouterError::InvalidResponse)
        })
        .transpose()?;
    let plan = data
        .get("is_free_tier")
        .and_then(Value::as_bool)
        .map(|free| {
            if free {
                "Free tier".to_owned()
            } else {
                "Pay as you go".to_owned()
            }
        });
    Ok(KeyMetrics {
        plan,
        quota,
        values,
    })
}

/// Spend counted against the key cap. `usage` is lifetime spend, while a cap
/// with `limit_reset` only counts the current window, so prefer the in-window
/// remainder and then the matching period spend.
fn key_limit_used(data: &serde_json::Map<String, Value>, limit: f64) -> Option<f64> {
    if let Some(remaining) = number(data.get("limit_remaining")) {
        return Some((limit - remaining).max(0.0));
    }
    let field = match data.get("limit_reset").and_then(Value::as_str) {
        Some("daily") => "usage_daily",
        Some("weekly") => "usage_weekly",
        Some("monthly") => "usage_monthly",
        _ => "usage",
    };
    number(data.get(field)).map(|used| used.max(0.0))
}

fn dollars_quota(id: &str, label: &str, used: f64, limit: f64) -> QuotaWindow {
    QuotaWindow {
        clock_offset_seconds: None,
        id: id.into(),
        label: label.into(),
        used_percent: if limit > 0.0 {
            (used / limit * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        },
        resets_at: None,
        period_seconds: 0,
        format: QuotaFormat::Dollars,
        used_value: Some(used),
        limit_value: Some(limit),
        unit: None,
        estimated: false,
        source_note: None,
    }
}

fn dollars_value(id: &str, label: &str, number: f64) -> ValueMetric {
    ValueMetric {
        id: id.into(),
        label: label.into(),
        values: vec![MetricValue {
            number,
            kind: MetricValueKind::Dollars,
            label: None,
            estimated: false,
        }],
        expiries_at: Vec::new(),
        reset_credits: Vec::new(),
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{data_object, map_credits, map_key};

    #[test]
    fn credits_map_to_a_dollar_meter_and_measured_balance() {
        let body = json!({"data":{"total_credits":277.47,"total_usage":178.20}});
        let mapped = map_credits(data_object(&body).unwrap()).unwrap();
        let quota = mapped.quota.unwrap();
        assert_eq!(quota.id, "credits");
        assert_eq!(quota.used_value, Some(178.20));
        assert_eq!(quota.limit_value, Some(277.47));
        assert!((mapped.balance.unwrap().values[0].number - 99.27).abs() < 0.001);
    }

    #[test]
    fn zero_purchased_credits_keep_balance_without_a_fake_meter() {
        let body = json!({"data":{"total_credits":0,"total_usage":0}});
        let mapped = map_credits(data_object(&body).unwrap()).unwrap();
        assert!(mapped.quota.is_none());
        assert_eq!(mapped.balance.unwrap().values[0].number, 0.0);
    }

    #[test]
    fn key_metadata_maps_period_spend_plan_and_optional_cap() {
        let body = json!({"data":{
            "is_free_tier":false,
            "usage_daily":0,
            "usage_weekly":1.25,
            "usage_monthly":"4.5",
            "usage":2,
            "limit":5
        }});
        let mapped = map_key(data_object(&body).unwrap()).unwrap();
        assert_eq!(mapped.plan.as_deref(), Some("Pay as you go"));
        assert_eq!(
            mapped
                .values
                .iter()
                .map(|metric| (metric.id.as_str(), metric.values[0].number))
                .collect::<Vec<_>>(),
            [("today", 0.0), ("week", 1.25), ("month", 4.5)]
        );
        assert_eq!(mapped.quota.unwrap().used_value, Some(2.0));
    }

    #[test]
    fn resetting_key_limit_counts_only_the_current_window() {
        let body = json!({"data":{
            "usage":50,
            "usage_monthly":2,
            "limit":10,
            "limit_reset":"monthly",
            "limit_remaining":7
        }});
        let quota = map_key(data_object(&body).unwrap()).unwrap().quota.unwrap();
        assert_eq!(quota.used_value, Some(3.0));
        assert!((quota.used_percent - 30.0).abs() < 1e-9);

        let body = json!({"data":{
            "usage":50,
            "usage_weekly":4,
            "limit":10,
            "limit_reset":"weekly"
        }});
        let quota = map_key(data_object(&body).unwrap()).unwrap().quota.unwrap();
        assert_eq!(quota.used_value, Some(4.0));
    }

    #[test]
    fn missing_balances_and_cap_usage_are_not_zero_measurements() {
        for body in [json!({"total_usage":10}), json!({"total_credits":100})] {
            assert!(map_credits(body.as_object().unwrap()).is_err());
        }
        for body in [
            json!({}),
            json!({"limit":100}),
            json!({"limit":100,"limit_reset":"weekly","usage":20}),
        ] {
            assert!(map_key(body.as_object().unwrap()).is_err());
        }
        assert_eq!(
            map_key(json!({"limit":100,"usage":0}).as_object().unwrap())
                .unwrap()
                .quota
                .unwrap()
                .used_percent,
            0.0
        );
    }
}
