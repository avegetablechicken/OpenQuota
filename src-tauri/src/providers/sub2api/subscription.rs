use super::reported::{AccountStatsDay, AccountStatsModel, ReportedUsage};
use super::{
    classify_status, decode_response, transport_error, AccountStats, Sub2ApiClient, Sub2ApiError,
};
use crate::models::{MetricValueKind, ProviderSnapshot, QuotaFormat, QuotaWindow, UsageHistories};
use chrono::{DateTime, Days, Local, Utc};
use reqwest::StatusCode;
use serde::Deserialize;

#[derive(Deserialize)]
struct UsageResponse {
    #[serde(rename = "planName")]
    plan: Option<String>,
    subscription: Option<Subscription>,
    daily_usage: Option<Vec<AccountStatsDay>>,
    model_stats: Option<Vec<AccountStatsModel>>,
    usage: Option<KeyUsage>,
}

#[derive(Deserialize)]
struct KeyUsage {
    today: Option<ReportedUsage>,
}

#[derive(Deserialize)]
struct Subscription {
    daily_usage_usd: Option<f64>,
    daily_limit_usd: Option<f64>,
    weekly_usage_usd: Option<f64>,
    weekly_limit_usd: Option<f64>,
    monthly_usage_usd: Option<f64>,
    monthly_limit_usd: Option<f64>,
    weekly_window_start: Option<DateTime<Utc>>,
}

pub(super) struct CachedModelFamilies {
    endpoint: reqwest::Url,
    key_hash: String,
    checked: std::time::Instant,
    icon: Option<&'static str>,
}

#[derive(Deserialize)]
struct ModelList {
    #[serde(alias = "models")]
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    #[serde(alias = "slug")]
    id: String,
}

// An advertised model identifies a family. It need not identify the account that
// actually serves the request. Use an existing provider mark only for a single family.
fn model_icon(id: &str) -> Option<&'static str> {
    let id = id.trim().to_ascii_lowercase();
    let id = id.rsplit('/').next()?;
    if matches!(id, "o1" | "o3" | "o4") {
        return Some("codex");
    }
    for (icon, prefixes) in [
        (
            "codex",
            &["gpt-", "chatgpt-", "codex-", "o1-", "o3-", "o4-"][..],
        ),
        ("claude", &["claude-"][..]),
        ("grok", &["grok-"][..]),
        ("kimi", &["kimi-", "moonshot-"][..]),
        ("minimax", &["minimax-"][..]),
        ("zai", &["glm-"][..]),
    ] {
        if prefixes.iter().any(|prefix| id.starts_with(prefix)) {
            return Some(icon);
        }
    }
    None
}

fn catalog_icon(models: &[ListedModel]) -> Option<&'static str> {
    let (first, rest) = models.split_first()?;
    let icon = model_icon(&first.id)?;
    rest.iter()
        .all(|model| model_icon(&model.id) == Some(icon))
        .then_some(icon)
}

impl Sub2ApiClient {
    fn gateway_endpoint(&self, resource: &str) -> Result<reqwest::Url, Sub2ApiError> {
        if self.base_url.path().trim_end_matches('/').ends_with("/v1") {
            self.endpoint(&[resource])
        } else {
            self.endpoint(&["v1", resource])
        }
    }

    pub(super) fn subscription_icon(
        &self,
        key: &str,
        cache: &std::sync::Mutex<Option<CachedModelFamilies>>,
    ) -> Option<&'static str> {
        let key_hash = crate::hashing::sha256_hex(key.as_bytes());
        let mut cached = cache.lock().ok()?;
        if !cached.as_ref().is_some_and(|last| {
            last.endpoint == self.base_url
                && last.key_hash == key_hash
                && last.checked.elapsed()
                    < std::time::Duration::from_secs(if last.icon.is_some() { 3600 } else { 300 })
        }) {
            let icon = self.fetch_model_icon(key);
            *cached = Some(CachedModelFamilies {
                endpoint: self.base_url.clone(),
                key_hash,
                checked: std::time::Instant::now(),
                icon,
            });
        }
        cached.as_ref()?.icon
    }

    fn fetch_model_icon(&self, key: &str) -> Option<&'static str> {
        let list: ModelList = self
            .client
            .get(self.gateway_endpoint("models").ok()?)
            .bearer_auth(key)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .ok()?;
        catalog_icon(&list.data)
    }

    pub(super) fn subscription_snapshot(
        &self,
        key: &str,
        mut snapshot: ProviderSnapshot,
    ) -> Result<ProviderSnapshot, Sub2ApiError> {
        let today = snapshot.refreshed_at.with_timezone(&Local).date_naive();
        let start = today - Days::new(29);
        let mut url = self.gateway_endpoint("usage")?;
        url.query_pairs_mut()
            .append_pair("days", "30")
            .append_pair("start_date", &start.to_string())
            .append_pair("end_date", &today.to_string());
        let response = self
            .client
            .get(url)
            .bearer_auth(key)
            .header("Accept", "application/json")
            .send()
            .map_err(|error| transport_error("subscription", &error))?;
        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(Sub2ApiError::ApiKeyRejected)
            }
            status if !status.is_success() => return Err(classify_status(status)),
            _ => {}
        }
        let usage: UsageResponse = decode_response("subscription", response)?;
        let subscription = usage.subscription.ok_or(Sub2ApiError::NoSubscription)?;
        snapshot.plan = usage.plan;
        let mut history = AccountStats {
            history: usage.daily_usage.unwrap_or_default(),
            models: usage.model_stats.unwrap_or_default(),
        }
        .into_history(snapshot.refreshed_at);
        if history.today.is_none() {
            history.today = usage
                .usage
                .and_then(|usage| usage.today)
                .map(|today| today.period());
        }
        snapshot.usage_histories = UsageHistories::account(history);
        let weekly_reset = subscription
            .weekly_window_start
            .and_then(|start| start.checked_add_days(Days::new(7)))
            .filter(|end| *end > snapshot.refreshed_at);
        for ((id, label, days), (used, limit, reset)) in WINDOWS.into_iter().zip([
            (
                subscription.daily_usage_usd,
                subscription.daily_limit_usd,
                None,
            ),
            (
                subscription.weekly_usage_usd,
                subscription.weekly_limit_usd,
                weekly_reset,
            ),
            (
                subscription.monthly_usage_usd,
                subscription.monthly_limit_usd,
                None,
            ),
        ]) {
            if let Some(used) = used.filter(|n| *n >= 0.0) {
                snapshot
                    .quotas
                    .push(window(id, label, days, used, limit, reset));
            }
        }
        Ok(snapshot)
    }
}

const WINDOWS: [(&str, &str, u64); 3] = [
    ("daily", "Daily", 1),
    ("weekly", "Weekly", 7),
    ("monthly", "Monthly", 30),
];

fn window(
    id: &str,
    label: &str,
    days: u64,
    used: f64,
    limit: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
) -> QuotaWindow {
    let limit = limit.filter(|n| *n > 0.0);
    QuotaWindow {
        id: id.into(),
        label: label.into(),
        used_percent: limit.map_or(0.0, |limit| (used / limit * 100.0).clamp(0.0, 100.0)),
        used_value: Some(used),
        limit_value: limit,
        format: if limit.is_some() {
            QuotaFormat::Percent
        } else {
            QuotaFormat::UnlimitedDollars
        },
        period_seconds: days * 86400,
        resets_at,
        clock_offset_seconds: None,
        unit: None,
        estimated: false,
        source_note: None,
    }
}

/// Earlier subscription snapshots stored uncapped windows as dollar values.
pub(crate) fn restore_subscription_windows(snapshot: &mut ProviderSnapshot) {
    if !snapshot.is_sub2api_subscription() {
        return;
    }
    // Older subscription snapshots used the dollar display format for capped windows.
    for quota in &mut snapshot.quotas {
        if quota.id == "monthly" && quota.label != "Monthly" {
            quota.label = "Monthly".into();
        }
        if quota.format == QuotaFormat::Dollars
            && quota.limit_value.is_some_and(|limit| limit > 0.0)
        {
            quota.format = QuotaFormat::Percent;
        }
    }
    // Reuse model discovery from snapshots written before icons were stored directly.
    if snapshot.upstream_provider_id.is_none() {
        snapshot.upstream_provider_id = snapshot
            .status_metrics
            .iter()
            .find(|metric| metric.id == "modelFamilies")
            .and_then(|metric| match metric.text.as_str() {
                "OpenAI" => Some("codex"),
                "Claude" => Some("claude"),
                "Grok" => Some("grok"),
                _ => None,
            })
            .map(str::to_owned);
    }
    snapshot
        .status_metrics
        .retain(|metric| metric.id != "modelFamilies" && metric.id != "expires");
    for (id, label, days) in WINDOWS {
        if let Some(index) = snapshot
            .value_metrics
            .iter()
            .position(|metric| metric.id == id)
        {
            let metric = snapshot.value_metrics.remove(index);
            if let [used] = metric.values.as_slice() {
                if used.kind == MetricValueKind::Dollars
                    && used.number >= 0.0
                    && !snapshot.quotas.iter().any(|quota| quota.id == id)
                {
                    snapshot
                        .quotas
                        .push(window(id, label, days, used.number, None, None));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{catalog_icon, ModelList};

    #[test]
    fn composite_icons_require_an_unambiguous_supported_model_family() {
        for (ids, expected) in [
            (
                vec!["anthropic/claude-sonnet", "claude-opus"],
                Some("claude"),
            ),
            (vec!["xai/grok-test"], Some("grok")),
            (vec!["gpt-test", "claude-test"], None),
            (vec!["gpt-test", "custom-alias"], None),
            (vec!["custom-alias"], None),
            (vec![], None),
        ] {
            let list: ModelList = serde_json::from_value(serde_json::json!({
                "data": ids.iter().map(|id| serde_json::json!({"id":id})).collect::<Vec<_>>()
            }))
            .unwrap();
            assert_eq!(catalog_icon(&list.data), expected);
        }
    }
}
