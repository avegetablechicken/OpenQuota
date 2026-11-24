//! Quotas of Antigravity and Grok upstream accounts, as Sub2API reports them from
//! `/api/v1/admin/accounts/:id/usage`. Sub2API polls each upstream itself and caches the
//! result, so readings are relative to the response's `updated_at` when it is present.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::models::{QuotaFormat, QuotaWindow};
use crate::providers::antigravity::started_reset;

const FIVE_HOURS: u64 = 5 * 60 * 60;
const ONE_WEEK: u64 = 7 * 24 * 60 * 60;
const ONE_MONTH: u64 = 30 * 24 * 60 * 60;

/// What one upstream account reports: its plan and quotas, or why Sub2API cannot use it.
#[derive(Debug, Default, PartialEq)]
pub(super) struct UpstreamQuota {
    pub plan: Option<String>,
    pub quotas: Vec<QuotaWindow>,
    pub warnings: Vec<String>,
}

/// Antigravity's shared pools (`antigravity_quota_groups`, Sub2API 0.2.20 and later): the
/// Gemini group's five-hour and weekly windows as Session and Weekly, and the third-party
/// (Claude and GPT-OSS) group's as Claude and Claude Weekly, as the native provider shows them.
/// Older instances report only each model's window, pooled by the most used model.
pub(super) fn map_antigravity(body: &Value, now: DateTime<Utc>) -> UpstreamQuota {
    if let Some(warning) = unusable(body, "Google") {
        return warning;
    }
    let observed = observed_at(body, now);
    let mut quotas = Vec::new();
    for group in array(body, "antigravity_quota_groups") {
        let third_party = match group.get("key").and_then(Value::as_str) {
            Some("gemini") => false,
            Some("3p") => true,
            _ => continue,
        };
        for entry in array(group, "windows") {
            let Some(used) = entry.get("utilization").and_then(number) else {
                continue;
            };
            let (id, label, period) =
                match (entry.get("window").and_then(Value::as_str), third_party) {
                    (Some("5h"), false) => ("session", "Session", FIVE_HOURS),
                    (Some("weekly"), false) => ("weekly", "Weekly", ONE_WEEK),
                    (Some("5h"), true) => ("claude", "Claude", FIVE_HOURS),
                    (Some("weekly"), true) => ("claudeWeekly", "Claude Weekly", ONE_WEEK),
                    _ => continue,
                };
            if quotas.iter().any(|quota: &QuotaWindow| quota.id == id) {
                continue;
            }
            let reset = entry.get("resets_at").and_then(time);
            quotas.push(window(id, label, used, reset, period, observed));
        }
    }
    if quotas.is_empty() {
        quotas = per_model_windows(body, observed);
    }
    let order = ["session", "weekly", "claude", "claudeWeekly"];
    quotas.sort_by_key(|quota| order.iter().position(|id| *id == quota.id));
    UpstreamQuota {
        plan: antigravity_plan(body),
        quotas,
        warnings: Vec::new(),
    }
}

/// Before quota groups, Sub2API reported each model's window (`antigravity_quota`). Gemini
/// models share the Session pool, Claude and GPT-OSS the Claude pool; the most used model of
/// each stands for its pool.
fn per_model_windows(body: &Value, observed: DateTime<Utc>) -> Vec<QuotaWindow> {
    let mut quotas: Vec<QuotaWindow> = Vec::new();
    let Some(models) = body.get("antigravity_quota").and_then(Value::as_object) else {
        return quotas;
    };
    for (model, quota) in models {
        let model = model.to_ascii_lowercase();
        let (id, label) = if model.contains("gemini") {
            ("session", "Session")
        } else if model.starts_with("claude") || model.starts_with("gpt") {
            ("claude", "Claude")
        } else {
            continue;
        };
        let Some(used) = quota.get("utilization").and_then(number) else {
            continue;
        };
        let reset = quota.get("reset_time").and_then(time);
        let candidate = window(id, label, used, reset, FIVE_HOURS, observed);
        match quotas.iter_mut().find(|quota| quota.id == id) {
            Some(existing) if existing.used_percent >= candidate.used_percent => {}
            Some(existing) => *existing = candidate,
            None => quotas.push(candidate),
        }
    }
    quotas
}

fn antigravity_plan(body: &Value) -> Option<String> {
    let tier = text(body, "subscription_tier")?;
    let lower = tier.to_ascii_lowercase();
    if matches!(lower.as_str(), "unknown" | "abnormal") {
        return None;
    }
    Some(title_case(&lower.replace(['_', '-'], " ")))
}

/// Grok's xAI billing period: the `seven_day` progress Sub2API keeps for it, else the billing
/// probe itself (`grok_billing`, Sub2API 0.1.155 and later). A monthly billing period is shown
/// as Monthly rather than Weekly. Instances that only saw xAI's rate-limit headers report no
/// window, which leaves the account without a quota.
pub(super) fn map_grok(body: &Value, now: DateTime<Utc>) -> UpstreamQuota {
    if let Some(warning) = unusable(body, "xAI") {
        return warning;
    }
    let billing = body.get("grok_billing").unwrap_or(&Value::Null);
    let start = billing.get("period_start").and_then(time);
    let end = billing.get("period_end").and_then(time);
    let monthly = billing.get("period_type").and_then(Value::as_str) == Some("monthly");
    let progress = body
        .get("seven_day")
        .filter(|window| {
            window.is_object()
                && window.get("utilization_unknown").and_then(Value::as_bool) != Some(true)
        })
        .and_then(|window| {
            let used = window.get("utilization").and_then(number)?;
            Some((used, window.get("resets_at").and_then(time).or(end)))
        })
        .or_else(|| {
            let used = billing.get("usage_percent").and_then(number)?;
            Some((used, end))
        });
    let mut warnings = Vec::new();
    let quotas = match progress {
        Some((used, reset)) => {
            let (id, label, fallback) = if monthly {
                ("monthly", "Monthly", ONE_MONTH)
            } else {
                ("weekly", "Weekly", ONE_WEEK)
            };
            let period = start
                .zip(end)
                .map(|(start, end)| (end - start).num_seconds())
                .filter(|seconds| *seconds > 0)
                .map_or(fallback, |seconds| seconds as u64);
            vec![QuotaWindow {
                resets_at: reset,
                ..window(id, label, used, None, period, now)
            }]
        }
        None => {
            let code = text(body, "error_code").unwrap_or_default();
            if !code.is_empty() && code != "quota_unknown" {
                warnings.push(text(body, "error").unwrap_or(code));
            }
            Vec::new()
        }
    };
    UpstreamQuota {
        plan: grok_plan(body),
        quotas,
        warnings,
    }
}

fn grok_plan(body: &Value) -> Option<String> {
    [
        body.get("subscription_tier"),
        body.get("subscription_tier_raw"),
        body.pointer("/grok_billing/plan"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .map(str::trim)
    .find(|tier| {
        !tier.is_empty() && !matches!(tier.to_ascii_lowercase().as_str(), "unknown" | "abnormal")
    })
    .map(|tier| tier.replace('_', " "))
}

/// An account Sub2API reports as refused upstream or signed out keeps its card, with the
/// reason, instead of failing the whole connection.
fn unusable(body: &Value, upstream: &str) -> Option<UpstreamQuota> {
    let warning = if body.get("is_forbidden").and_then(Value::as_bool) == Some(true) {
        text(body, "forbidden_reason")
            .unwrap_or_else(|| format!("{upstream} refuses this account in Sub2API."))
    } else if body.get("needs_reauth").and_then(Value::as_bool) == Some(true) {
        "This account must sign in to Sub2API again.".to_owned()
    } else {
        return None;
    };
    Some(UpstreamQuota {
        warnings: vec![warning],
        ..UpstreamQuota::default()
    })
}

/// An idle window reports its reset a whole window after each poll; it starts on first use.
fn window(
    id: &str,
    label: &str,
    used: f64,
    reset: Option<DateTime<Utc>>,
    period_seconds: u64,
    observed: DateTime<Utc>,
) -> QuotaWindow {
    let used = used.clamp(0.0, 100.0);
    QuotaWindow {
        clock_offset_seconds: None,
        id: id.into(),
        label: label.into(),
        used_percent: used,
        resets_at: started_reset(1.0 - used / 100.0, reset, period_seconds, observed),
        period_seconds,
        format: QuotaFormat::Percent,
        used_value: None,
        limit_value: None,
        unit: None,
        estimated: false,
        source_note: None,
    }
}

fn observed_at(body: &Value, now: DateTime<Utc>) -> DateTime<Utc> {
    body.get("updated_at")
        .and_then(time)
        .filter(|observed| *observed <= now)
        .unwrap_or(now)
}

fn array<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn time(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?.trim())
        .ok()
        .map(|time| time.to_utc())
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|value| value.is_finite())
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::json;

    use super::{map_antigravity, map_grok};

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    #[test]
    fn antigravity_groups_map_to_session_weekly_and_claude_pools() {
        let now = at("2026-11-24T16:45:00Z");
        let body = json!({
            "updated_at": "2026-11-24T17:41:08+01:00",
            "antigravity_quota": {"gemini-4-neon-high": {"utilization": 90, "reset_time": "2026-11-24T21:41:05Z"}},
            "antigravity_quota_groups": [
                {"key": "3p", "windows": [
                    {"window": "5h", "utilization": 0, "resets_at": "2026-11-24T21:41:08Z"},
                    {"window": "weekly", "utilization": 52.14592, "resets_at": "2026-11-27T18:45:34Z"}
                ]},
                {"key": "gemini", "windows": [
                    {"window": "5h", "utilization": 12.5, "resets_at": "2026-11-24T20:00:00Z"},
                    {"window": "weekly", "utilization": 4.0e-5, "resets_at": "2026-12-01T06:56:20Z"}
                ]},
                {"key": "other", "windows": [{"window": "5h", "utilization": 99}]}
            ],
            "subscription_tier": "PRO",
            "subscription_tier_raw": "g1-pro-tier"
        });

        let mapped = map_antigravity(&body, now);

        assert_eq!(mapped.plan.as_deref(), Some("Pro"));
        assert!(mapped.warnings.is_empty());
        let ids = mapped
            .quotas
            .iter()
            .map(|q| q.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["session", "weekly", "claude", "claudeWeekly"]);
        assert_eq!(mapped.quotas[0].used_percent, 12.5);
        assert_eq!(mapped.quotas[0].resets_at, Some(at("2026-11-24T20:00:00Z")));
        assert_eq!(mapped.quotas[0].period_seconds, 5 * 3600);
        assert_eq!(mapped.quotas[1].resets_at, Some(at("2026-12-01T06:56:20Z")));
        // The idle third-party session rolls its reset with each poll, so it has not started.
        assert_eq!(mapped.quotas[2].used_percent, 0.0);
        assert_eq!(mapped.quotas[2].resets_at, None);
        assert_eq!(mapped.quotas[3].label, "Claude Weekly");
        assert!((mapped.quotas[3].used_percent - 52.14592).abs() < 1e-9);
    }

    #[test]
    fn antigravity_without_groups_pools_the_most_used_model() {
        let now = at("2026-11-24T16:45:00Z");
        let body = json!({
            "antigravity_quota": {
                "gemini-3-flash": {"utilization": 10, "reset_time": "2026-11-24T18:00:00Z"},
                "gemini-4-neon-high": {"utilization": 40, "reset_time": "2026-11-24T19:00:00Z"},
                "claude-opus-5-5-high": {"utilization": 52, "reset_time": "2026-11-24T20:00:00Z"},
                "gpt-oss-120b-medium": {"utilization": 30, "reset_time": "2026-11-24T20:00:00Z"},
                "tab_flash_lite_preview": {"utilization": 100, "reset_time": ""}
            },
            "subscription_tier": "unknown"
        });

        let mapped = map_antigravity(&body, now);

        assert_eq!(mapped.plan, None);
        let readings = mapped
            .quotas
            .iter()
            .map(|q| (q.id.as_str(), q.used_percent))
            .collect::<Vec<_>>();
        assert_eq!(readings, [("session", 40.0), ("claude", 52.0)]);
        assert_eq!(mapped.quotas[0].resets_at, Some(at("2026-11-24T19:00:00Z")));
    }

    #[test]
    fn unusable_accounts_keep_a_card_with_the_reason() {
        let now = at("2026-11-24T16:45:00Z");
        let forbidden = map_antigravity(
            &json!({"is_forbidden": true, "forbidden_reason": " Account disabled "}),
            now,
        );
        assert!(forbidden.quotas.is_empty());
        assert_eq!(forbidden.warnings, ["Account disabled"]);

        let signed_out = map_grok(&json!({"needs_reauth": true}), now);
        assert_eq!(
            signed_out.warnings,
            ["This account must sign in to Sub2API again."]
        );
        let refused = map_grok(&json!({"is_forbidden": true}), now);
        assert_eq!(refused.warnings, ["xAI refuses this account in Sub2API."]);
    }

    #[test]
    fn grok_seven_day_progress_maps_to_the_billing_period() {
        let now = at("2026-11-24T16:45:00Z");
        let body = json!({
            "seven_day": {"utilization": 88, "resets_at": "2026-11-28T15:17:54.02Z"},
            "grok_billing": {
                "period_type": "weekly",
                "usage_percent": 80,
                "period_start": "2026-11-21T15:17:54.020+00:00",
                "period_end": "2026-11-28T15:17:54.020+00:00"
            }
        });

        let mapped = map_grok(&body, now);

        assert_eq!(mapped.quotas.len(), 1);
        let weekly = &mapped.quotas[0];
        assert_eq!(
            (weekly.id.as_str(), weekly.label.as_str()),
            ("weekly", "Weekly")
        );
        assert_eq!(weekly.used_percent, 88.0);
        assert_eq!(weekly.resets_at, Some(at("2026-11-28T15:17:54.02Z")));
        assert_eq!(weekly.period_seconds, 7 * 24 * 3600);
    }

    #[test]
    fn grok_falls_back_to_billing_and_labels_monthly_periods() {
        let now = at("2026-11-24T16:45:00Z");
        let body = json!({
            "seven_day": {"utilization": 0, "utilization_unknown": true},
            "grok_billing": {
                "period_type": "monthly",
                "usage_percent": "130",
                "period_start": "2026-11-01T00:00:00Z",
                "period_end": "2026-12-01T00:00:00Z",
                "plan": "SUPER_GROK"
            }
        });

        let mapped = map_grok(&body, now);

        assert_eq!(mapped.plan.as_deref(), Some("SUPER GROK"));
        let monthly = &mapped.quotas[0];
        assert_eq!(
            (monthly.id.as_str(), monthly.label.as_str()),
            ("monthly", "Monthly")
        );
        assert_eq!(monthly.used_percent, 100.0);
        assert_eq!(monthly.resets_at, Some(at("2026-12-01T00:00:00Z")));
        assert_eq!(monthly.period_seconds, 30 * 24 * 3600);
    }

    #[test]
    fn grok_without_billing_reports_no_window_and_surfaces_probe_errors() {
        let now = at("2026-11-24T16:45:00Z");
        let unprobed = map_grok(&json!({"error_code": "quota_unknown"}), now);
        assert!(unprobed.quotas.is_empty());
        assert!(unprobed.warnings.is_empty());

        let failed = map_grok(
            &json!({"error_code": "billing_failed", "error": "xAI billing returned 500"}),
            now,
        );
        assert_eq!(failed.warnings, ["xAI billing returned 500"]);
    }
}
