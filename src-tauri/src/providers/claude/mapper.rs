use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::Value;

use crate::models::{
    MetricValue, MetricValueKind, ProviderSnapshot, QuotaFormat, QuotaWindow, ResetCreditDetail,
    ResetCreditKind, ValueMetric,
};

use super::{auth::ClaudeOAuth, ClaudeError};

/// Cap on listed resets so a malformed or hostile upstream count cannot exhaust memory.
const MAX_RESET_CREDITS: u64 = 1000;

pub(crate) struct ClaudeMappedUsage {
    pub plan: Option<String>,
    pub quotas: Vec<QuotaWindow>,
    pub value_metrics: Vec<ValueMetric>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn map_usage(
    status: StatusCode,
    body: &Value,
    credentials: &ClaudeOAuth,
) -> Result<ClaudeMappedUsage, ClaudeError> {
    map_usage_at(status, body, credentials, Utc::now())
}

pub fn map_usage_at(
    status: StatusCode,
    body: &Value,
    credentials: &ClaudeOAuth,
    now: DateTime<Utc>,
) -> Result<ClaudeMappedUsage, ClaudeError> {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(ClaudeError::TokenExpired);
    }
    if !status.is_success() {
        return Err(ClaudeError::RequestFailed(status.as_u16()));
    }
    let object = body.as_object().ok_or(ClaudeError::InvalidResponse)?;
    let mut quotas = Vec::new();
    let mut value_metrics = Vec::new();
    append_window(
        &mut quotas,
        "session",
        "Session",
        object.get("five_hour"),
        5 * 60 * 60,
    );
    append_window(
        &mut quotas,
        "weekly",
        "Weekly",
        object.get("seven_day"),
        7 * 24 * 60 * 60,
    );
    append_window(
        &mut quotas,
        "sonnet",
        "Sonnet",
        object.get("seven_day_sonnet"),
        7 * 24 * 60 * 60,
    );
    append_window(
        &mut quotas,
        "fable",
        "Fable",
        object.get("seven_day_fable"),
        7 * 24 * 60 * 60,
    );
    if !quotas.iter().any(|quota| quota.id == "fable") {
        if let Some(limits) = object.get("limits").and_then(Value::as_array) {
            if let Some(limit) = limits.iter().find(|limit| {
                limit.get("kind").and_then(Value::as_str) == Some("weekly_scoped")
                    && limit
                        .pointer("/scope/model/display_name")
                        .and_then(Value::as_str)
                        == Some("Fable")
            }) {
                append_percent(
                    &mut quotas,
                    "fable",
                    "Fable",
                    limit.get("percent").and_then(number),
                    limit.get("resets_at").and_then(reset_date),
                    7 * 24 * 60 * 60,
                );
            }
        }
    }
    if let Some(extra) = object.get("extra_usage").and_then(Value::as_object) {
        if extra.get("is_enabled").and_then(Value::as_bool) == Some(true) {
            if let Some(used_cents) = extra.get("used_credits").and_then(number) {
                let used = used_cents / 100.0;
                let limit = extra
                    .get("monthly_limit")
                    .and_then(number)
                    .map(|v| v / 100.0);
                if limit.is_some_and(|value| value > 0.0) {
                    let limit_value = limit.unwrap_or_default();
                    quotas.push(QuotaWindow {
                        clock_offset_seconds: None,
                        id: "extra".into(),
                        label: "Extra Usage".into(),
                        used_percent: (used / limit_value * 100.0).clamp(0.0, 100.0),
                        resets_at: None,
                        period_seconds: 0,
                        format: QuotaFormat::Dollars,
                        used_value: Some(used),
                        limit_value: Some(limit_value),
                        unit: None,
                        estimated: false,
                        source_note: None,
                    });
                } else if used > 0.0 {
                    value_metrics.push(ValueMetric {
                        id: "extra".into(),
                        label: "Extra Usage".into(),
                        values: vec![MetricValue {
                            number: used,
                            kind: MetricValueKind::Dollars,
                            label: Some("spent".into()),
                            estimated: false,
                        }],
                        expiries_at: Vec::new(),
                        reset_credits: Vec::new(),
                    });
                }
            }
        }
    }
    if let Some(resets) = map_cedar_ember_resets(body, now) {
        value_metrics.push(resets);
    }
    Ok(ClaudeMappedUsage {
        plan: format_plan(
            credentials.subscription_type.as_deref(),
            credentials.rate_limit_tier.as_deref(),
        ),
        quotas,
        value_metrics,
    })
}

#[cfg(test)]
pub(crate) fn map_sub2api_usage(body: &Value) -> Result<ClaudeMappedUsage, ClaudeError> {
    map_sub2api_usage_at(body, Utc::now())
}

pub(crate) fn map_sub2api_usage_at(
    body: &Value,
    now: DateTime<Utc>,
) -> Result<ClaudeMappedUsage, ClaudeError> {
    let subscription_type = body
        .get("subscription_tier")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // An unused session (0%, no reset) is kept as not started, as for direct logins.
    map_usage_at(
        StatusCode::OK,
        body,
        &ClaudeOAuth {
            subscription_type,
            ..ClaudeOAuth::default()
        },
        now,
    )
}

pub(crate) fn is_valid_grant_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

pub(crate) fn classify_reset_kind(grant: &Value) -> ResetCreditKind {
    // Claude Code classifies Fable weekly-refund grants by `gives_back_weekly`.
    if grant.get("gives_back_weekly").and_then(Value::as_bool) == Some(true) {
        return ResetCreditKind::Fable;
    }
    if let Some(clears) = grant.get("clears").and_then(Value::as_array) {
        let has_five_hour = clears
            .iter()
            .any(|value| value.as_str() == Some("five_hour"));
        let has_seven_day = clears
            .iter()
            .any(|value| value.as_str() == Some("seven_day"));
        let has_fable_weekly = clears
            .iter()
            .any(|value| value.as_str() == Some("seven_day_overage_included"));
        if has_five_hour && has_seven_day {
            ResetCreditKind::Full
        } else if has_fable_weekly && !has_seven_day {
            ResetCreditKind::Fable
        } else {
            ResetCreditKind::Partial
        }
    } else {
        ResetCreditKind::Unknown
    }
}

pub(crate) fn map_cedar_ember_resets(body: &Value, now: DateTime<Utc>) -> Option<ValueMetric> {
    let block = body.get("cedar_ember").filter(|value| value.is_object())?;
    if block.get("eligible").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let grants = block.get("grants").and_then(Value::as_array)?;
    let at_limit = block
        .get("at_limit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next_grant_id = block
        .get("next_grant_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let cooldown_active = block
        .get("cooldown_until")
        .and_then(reset_date)
        .is_some_and(|until| until > now);

    let mut total: u64 = 0;
    let mut expiries_at = Vec::new();
    let mut reset_credits = Vec::new();
    for grant in grants {
        if grant.get("paused").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let starts_at = grant.get("starts_at").and_then(reset_date);
        if starts_at.is_some_and(|start| start > now) {
            continue;
        }
        let ends_at = grant.get("ends_at").and_then(reset_date);
        if ends_at.is_some_and(|end| end <= now) {
            continue;
        }
        let Some(left) = bounded_reset_count(grant.get("resets_left"), total) else {
            continue;
        };
        if left == 0 {
            continue;
        }
        let kind = classify_reset_kind(grant);
        let grant_id = grant
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| is_valid_grant_id(id));
        let usable_now = grant
            .get("usable_now")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let blocking_empty = grant
            .get("blocking")
            .and_then(Value::as_array)
            .is_none_or(|items| items.is_empty());
        let limit_satisfied =
            grant.get("use_requires_limit").and_then(Value::as_bool) != Some(true) || at_limit;
        let is_next = next_grant_id.is_none_or(|next| grant_id == Some(next));
        let redeemable = grant_id.is_some()
            && usable_now
            && blocking_empty
            && limit_satisfied
            && is_next
            && !cooldown_active;

        total += left;
        for _ in 0..left {
            if let Some(expiry) = ends_at {
                expiries_at.push(expiry);
            }
            reset_credits.push(ResetCreditDetail {
                claim_account: None,
                expires_at: ends_at,
                kind,
                redeemable,
            });
        }
    }
    expiries_at.sort();
    reset_credits.sort_by_key(|credit| (credit.expires_at.is_none(), credit.expires_at));
    Some(ValueMetric {
        id: "claudeRateLimitResets".into(),
        label: "Rate Limit Resets".into(),
        values: vec![MetricValue {
            number: total as f64,
            kind: MetricValueKind::Count,
            label: Some("available".into()),
            estimated: false,
        }],
        expiries_at,
        reset_credits,
    })
}

/// Maps Sub2API's `/api/v1/admin/accounts/:id/claude/reset-credits` payload as a read-only
/// `sub2apiRateLimitResets` value metric.
pub(crate) fn map_sub2api_claude_reset_credits(
    body: &Value,
    now: DateTime<Utc>,
) -> Option<ValueMetric> {
    if !body.is_object() {
        return None;
    }
    if body.get("eligible").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let mut total: u64 = 0;
    let mut expiries_at = Vec::new();
    let mut reset_credits = Vec::new();
    if let Some(credits) = body.get("credits").and_then(Value::as_array) {
        for credit in credits {
            let starts_at = credit.get("starts_at").and_then(reset_date);
            if starts_at.is_some_and(|start| start > now) {
                continue;
            }
            let expires_at = credit
                .get("expires_at")
                .or_else(|| credit.get("ends_at"))
                .and_then(reset_date);
            if expires_at.is_some_and(|end| end <= now) {
                continue;
            }
            let Some(left) = bounded_reset_count(credit.get("resets_left"), total) else {
                continue;
            };
            if left == 0 {
                continue;
            }
            let kind = classify_reset_kind(credit);
            total += left;
            for _ in 0..left {
                if let Some(expiry) = expires_at {
                    expiries_at.push(expiry);
                }
                reset_credits.push(ResetCreditDetail {
                    claim_account: None,
                    expires_at,
                    kind,
                    redeemable: false,
                });
            }
        }
    } else {
        total = bounded_reset_count(body.get("available_count"), 0)?;
    }
    expiries_at.sort();
    reset_credits.sort_by_key(|credit| (credit.expires_at.is_none(), credit.expires_at));
    Some(ValueMetric {
        id: "sub2apiRateLimitResets".into(),
        label: "Rate Limit Resets".into(),
        values: vec![MetricValue {
            number: total as f64,
            kind: MetricValueKind::Count,
            label: Some("available".into()),
            estimated: false,
        }],
        expiries_at,
        reset_credits,
    })
}

/// Retains unexpired reset credits from the last confirmed snapshot when a live usage response
/// omits `cedar_ember` (for example after falling back to the legacy User-Agent on HTTP 429).
pub(crate) fn retain_known_resets(
    value_metrics: &mut Vec<ValueMetric>,
    previous: Option<&ProviderSnapshot>,
    body: &Value,
    now: DateTime<Utc>,
) {
    if body.get("cedar_ember").is_some()
        || value_metrics
            .iter()
            .any(|metric| metric.id == "claudeRateLimitResets")
    {
        return;
    }
    let Some(prior) = previous.and_then(|snapshot| {
        snapshot
            .value_metrics
            .iter()
            .find(|metric| metric.id == "claudeRateLimitResets")
    }) else {
        return;
    };
    let mut retained = prior.clone();
    if !retained.reset_credits.is_empty() {
        retained
            .reset_credits
            .retain(|credit| credit.expires_at.is_none_or(|expiry| expiry > now));
        retained.expiries_at = retained
            .reset_credits
            .iter()
            .filter_map(|credit| credit.expires_at)
            .collect();
        retained.expiries_at.sort();
        let count = retained.reset_credits.len() as f64;
        if let Some(value) = retained.values.first_mut() {
            value.number = count;
        }
    } else {
        let before = retained.expiries_at.len();
        retained.expiries_at.retain(|expiry| *expiry > now);
        let expired = before.saturating_sub(retained.expiries_at.len()) as f64;
        if let Some(value) = retained.values.first_mut() {
            value.number = (value.number - expired).max(0.0);
        }
    }
    value_metrics.push(retained);
}

fn bounded_reset_count(value: Option<&Value>, current_total: u64) -> Option<u64> {
    let raw = number(value?)?.floor();
    if !raw.is_finite() || raw < 0.0 {
        return None;
    }
    let remaining_capacity = MAX_RESET_CREDITS.saturating_sub(current_total);
    let clamped = (raw.min(MAX_RESET_CREDITS as f64) as u64).min(remaining_capacity);
    Some(clamped)
}

fn append_window(
    output: &mut Vec<QuotaWindow>,
    id: &str,
    label: &str,
    value: Option<&Value>,
    period_seconds: u64,
) {
    let Some(object) = value.and_then(Value::as_object) else {
        return;
    };
    append_percent(
        output,
        id,
        label,
        object.get("utilization").and_then(number),
        object.get("resets_at").and_then(reset_date),
        period_seconds,
    );
}

fn append_percent(
    output: &mut Vec<QuotaWindow>,
    id: &str,
    label: &str,
    percent: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
    period_seconds: u64,
) {
    let Some(percent) = percent.filter(|value| value.is_finite()) else {
        return;
    };
    output.push(QuotaWindow {
        clock_offset_seconds: None,
        id: id.into(),
        label: label.into(),
        used_percent: percent,
        resets_at,
        period_seconds,
        format: QuotaFormat::Percent,
        used_value: None,
        limit_value: None,
        unit: None,
        estimated: false,
        source_note: None,
    });
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn reset_date(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|date| date.to_utc())
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|date| date.and_utc())
            });
    }
    let raw = number(value)?;
    let seconds = if raw.abs() < 10_000_000_000.0 {
        raw
    } else {
        raw / 1000.0
    };
    DateTime::from_timestamp_millis((seconds * 1000.0) as i64)
}

fn format_plan(subscription: Option<&str>, tier: Option<&str>) -> Option<String> {
    let subscription = subscription?.trim();
    if subscription.is_empty() {
        return None;
    }
    let mut plan = subscription
        .to_ascii_lowercase()
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(multiplier) = tier.and_then(|tier| {
        tier.split(|character: char| !character.is_ascii_alphanumeric())
            .find(|part| part.ends_with('x') && part[..part.len() - 1].parse::<u32>().is_ok())
    }) {
        plan.push(' ');
        plan.push_str(multiplier);
    }
    Some(plan)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use reqwest::StatusCode;
    use serde_json::Value;

    use super::{
        map_sub2api_claude_reset_credits, map_sub2api_usage, map_usage, map_usage_at,
        retain_known_resets,
    };
    use crate::{
        models::{ProviderSnapshot, ResetCreditKind},
        providers::claude::auth::ClaudeOAuth,
    };

    #[test]
    fn maps_live_windows_scoped_limit_and_extra_usage() {
        let body: Value =
            serde_json::from_str(include_str!("fixtures/usage.json")).expect("valid fixture");
        let mapped = map_usage(
            StatusCode::OK,
            &body,
            &ClaudeOAuth {
                subscription_type: Some("pro".into()),
                rate_limit_tier: Some("default_5x".into()),
                ..ClaudeOAuth::default()
            },
        )
        .unwrap();
        assert_eq!(mapped.plan.as_deref(), Some("Pro 5x"));
        assert_eq!(mapped.quotas.len(), 5);
        assert_eq!(mapped.quotas[4].used_percent, 25.0);
        assert_eq!(mapped.quotas[4].used_value, Some(12.5));
        assert!(mapped.value_metrics.is_empty());
    }

    #[test]
    fn maps_uncapped_extra_usage_as_an_unbounded_value() {
        let body = serde_json::json!({
            "extra_usage": {
                "is_enabled": true,
                "used_credits": 123456,
                "monthly_limit": null
            }
        });
        let mapped = map_usage(StatusCode::OK, &body, &ClaudeOAuth::default()).unwrap();
        assert!(mapped.quotas.is_empty());
        assert_eq!(mapped.value_metrics[0].id, "extra");
        assert_eq!(mapped.value_metrics[0].values[0].number, 1234.56);
        assert_eq!(
            mapped.value_metrics[0].values[0].label.as_deref(),
            Some("spent")
        );
    }

    #[test]
    fn maps_microsecond_reset_without_a_timezone_as_utc() {
        let body = serde_json::json!({
            "five_hour": {
                "utilization": 0,
                "resets_at": "2099-06-01T12:00:00.123456"
            }
        });
        let mapped = map_usage(StatusCode::OK, &body, &ClaudeOAuth::default()).unwrap();
        assert_eq!(
            mapped.quotas[0].resets_at.unwrap().to_rfc3339(),
            "2099-06-01T12:00:00.123456+00:00"
        );
    }

    #[test]
    fn sub2api_keeps_an_unused_session_as_not_started() {
        let body = serde_json::json!({
            "five_hour": {
                "utilization": 0,
                "resets_at": null,
                "remaining_seconds": 0,
                "window_stats": {
                    "requests": 0,
                    "tokens": 0,
                    "cost": 0
                }
            },
            "seven_day": {
                "utilization": 8,
                "resets_at": "2099-06-08T12:00:00Z"
            }
        });

        let mapped = map_sub2api_usage(&body).unwrap();
        let session = &mapped.quotas[0];
        assert_eq!(session.id, "session");
        assert_eq!(session.used_percent, 0.0);
        assert_eq!(session.resets_at, None);
        assert_eq!(mapped.quotas[1].id, "weekly");
    }

    #[test]
    fn sub2api_keeps_a_zero_session_with_a_real_reset() {
        let body = serde_json::json!({
            "five_hour": {
                "utilization": 0,
                "resets_at": "2099-06-01T12:00:00Z",
                "remaining_seconds": 18000
            }
        });

        let mapped = map_sub2api_usage(&body).unwrap();
        assert_eq!(mapped.quotas[0].id, "session");
        assert_eq!(mapped.quotas[0].used_percent, 0.0);
        assert!(mapped.quotas[0].resets_at.is_some());
    }

    #[test]
    fn maps_cedar_ember_resets_with_fable_and_full_classification() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap();
        let body = serde_json::json!({
            "cedar_ember": {
                "eligible": true,
                "at_limit": false,
                "next_grant_id": "fable_grant",
                "grants": [
                    {
                        "id": "full_grant",
                        "type": "full",
                        "resets_left": 1,
                        "ends_at": "2026-12-10T22:00:00+00:00",
                        "gives_back_weekly": false,
                        "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
                        "paused": false,
                        "usable_now": true,
                        "use_requires_limit": false,
                        "blocking": []
                    },
                    {
                        "id": "fable_grant",
                        "type": "full",
                        "resets_left": 2,
                        "ends_at": "2026-11-17T07:59:00+00:00",
                        "gives_back_weekly": true,
                        "clears": ["five_hour", "seven_day_overage_included"],
                        "paused": false,
                        "usable_now": true,
                        "use_requires_limit": false,
                        "blocking": []
                    },
                    {
                        "id": "expired_grant",
                        "resets_left": 3,
                        "ends_at": "2026-11-10T00:00:00Z",
                        "clears": ["five_hour", "seven_day"],
                        "usable_now": true
                    },
                    {
                        "id": "paused_grant",
                        "resets_left": 1,
                        "ends_at": "2026-12-01T00:00:00Z",
                        "clears": ["five_hour"],
                        "paused": true,
                        "usable_now": true
                    }
                ]
            }
        });
        let mapped = map_usage_at(StatusCode::OK, &body, &ClaudeOAuth::default(), now).unwrap();
        let resets = mapped
            .value_metrics
            .iter()
            .find(|metric| metric.id == "claudeRateLimitResets")
            .unwrap();
        assert_eq!(resets.values[0].number, 3.0);
        assert_eq!(resets.expiries_at.len(), 3);
        assert_eq!(resets.reset_credits.len(), 3);
        assert_eq!(resets.reset_credits[0].kind, ResetCreditKind::Fable);
        assert!(resets.reset_credits[0].redeemable);
        assert_eq!(resets.reset_credits[1].kind, ResetCreditKind::Fable);
        assert!(resets.reset_credits[1].redeemable);
        assert_eq!(resets.reset_credits[2].kind, ResetCreditKind::Full);
        assert!(!resets.reset_credits[2].redeemable);
    }

    #[test]
    fn maps_empty_eligible_grants_as_zero_and_ineligible_as_absent() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap();
        let zero = map_usage_at(
            StatusCode::OK,
            &serde_json::json!({"cedar_ember": {"eligible": true, "grants": []}}),
            &ClaudeOAuth::default(),
            now,
        )
        .unwrap();
        assert_eq!(zero.value_metrics[0].values[0].number, 0.0);

        for body in [
            serde_json::json!({"cedar_ember": {"eligible": false, "ineligible_reason": "cli_version"}}),
            serde_json::json!({"cedar_ember": {"eligible": true}}),
            serde_json::json!({}),
        ] {
            let mapped = map_usage_at(StatusCode::OK, &body, &ClaudeOAuth::default(), now).unwrap();
            assert!(mapped
                .value_metrics
                .iter()
                .all(|metric| metric.id != "claudeRateLimitResets"));
        }
    }

    #[test]
    fn retains_unexpired_resets_when_a_subsequent_poll_omits_cedar_ember() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 11, 20, 0, 0, 0).unwrap();
        let first = map_usage_at(
            StatusCode::OK,
            &serde_json::json!({
                "cedar_ember": {
                    "eligible": true,
                    "grants": [
                        {"id": "early", "resets_left": 1, "ends_at": "2026-11-15T00:00:00Z", "gives_back_weekly": true, "clears": ["five_hour"]},
                        {"id": "later", "resets_left": 1, "ends_at": "2026-12-01T00:00:00Z", "clears": ["five_hour", "seven_day"]}
                    ]
                }
            }),
            &ClaudeOAuth::default(),
            t0,
        )
        .unwrap();
        let previous = ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: "claude".into(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: first.value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: t0,
        };
        let mut metrics = Vec::new();
        retain_known_resets(&mut metrics, Some(&previous), &serde_json::json!({}), t1);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].values[0].number, 1.0);
        assert_eq!(metrics[0].reset_credits.len(), 1);
        assert_eq!(metrics[0].reset_credits[0].kind, ResetCreditKind::Full);

        // Explicit negative or malformed reset metadata must supersede the prior positive state.
        for body in [
            serde_json::json!({"cedar_ember": {"eligible": false}}),
            serde_json::json!({"cedar_ember": null}),
            serde_json::json!({"cedar_ember": {"eligible": true}}),
            serde_json::json!({"cedar_ember": {"eligible": true, "grants": []}}),
        ] {
            let mut mapped =
                map_usage_at(StatusCode::OK, &body, &ClaudeOAuth::default(), t1).unwrap();
            retain_known_resets(&mut mapped.value_metrics, Some(&previous), &body, t1);
            assert!(mapped
                .value_metrics
                .iter()
                .all(|metric| metric.values[0].number == 0.0));
        }
    }

    #[test]
    fn maps_sub2api_claude_reset_credits_using_total_held_resets_as_read_only() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap();
        let body = serde_json::json!({
            "eligible": true,
            "available_count": 0,
            "credits": [
                {
                    "label": "Fable reset",
                    "resets_left": 1,
                    "expires_at": "2026-11-17T07:59:00Z",
                    "clears": ["five_hour", "seven_day_overage_included"],
                    "redeemable": false
                },
                {
                    "label": "Full reset",
                    "resets_left": 2,
                    "expires_at": "2026-12-10T22:00:00Z",
                    "clears": ["five_hour", "seven_day", "seven_day_overage_included"],
                    "redeemable": true
                }
            ]
        });
        let metric = map_sub2api_claude_reset_credits(&body, now).unwrap();
        assert_eq!(metric.id, "sub2apiRateLimitResets");
        assert_eq!(metric.values[0].number, 3.0);
        assert_eq!(metric.expiries_at.len(), 3);
        assert_eq!(metric.reset_credits[0].kind, ResetCreditKind::Fable);
        assert_eq!(metric.reset_credits[1].kind, ResetCreditKind::Full);
        assert!(metric.reset_credits.iter().all(|credit| !credit.redeemable));

        assert!(map_sub2api_claude_reset_credits(
            &serde_json::json!({"eligible": false, "available_count": 0, "credits": []}),
            now
        )
        .is_none());
    }
}
