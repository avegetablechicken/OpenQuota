mod account_usage;
mod auth;
mod client;
mod mapper;

use std::sync::Arc;

use chrono::{DateTime, Days, Local, Utc};
use reqwest::StatusCode;
use thiserror::Error;

use crate::models::{
    ApiKeyStatus, MetricDefinition, MetricSection, ProviderDefinition, ProviderErrorKind,
    ProviderLink, ProviderSnapshot, UsageHistories, UsagePeriodSelection,
};

use self::{
    account_usage::{map_credit_usage, map_legacy_usage, ZaiAccountUsage, HISTORY_DAYS},
    auth::ZaiAuthStore,
    client::{AccountUsageKind, ZaiClient, ZaiResponse},
    mapper::{is_no_coding_plan, map_usage},
};

use super::{ProviderError, UsageProvider};

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "zai".into(),
        display_name: "Z.ai".into(),
        short_name: "Z".into(),
        fallback_enabled: false,
        local_usage_source_note: None,
        links: vec![
            ProviderLink::new(
                "Dashboard",
                "https://z.ai/manage-apikey/coding-plan/personal/my-plan",
            ),
            ProviderLink::new("API Keys", "https://z.ai/manage-apikey/apikey-list"),
        ],
        metrics: vec![
            MetricDefinition::quota(
                "zai.session",
                "Session",
                "session",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::quota(
                "zai.weekly",
                "Weekly",
                "weekly",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "W",
            ),
            MetricDefinition::quota(
                "zai.webSearches",
                "Web Searches",
                "webSearches",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "Search",
            ),
            MetricDefinition::trend("zai.trend"),
            MetricDefinition::usage(
                "zai.today",
                "Today",
                UsagePeriodSelection::Today,
                MetricSection::OnDemand,
                "T",
            ),
            MetricDefinition::usage(
                "zai.yesterday",
                "Yesterday",
                UsagePeriodSelection::Yesterday,
                MetricSection::OnDemand,
                "Y",
            ),
            MetricDefinition::usage(
                "zai.last30",
                "Last 30 Days",
                UsagePeriodSelection::Last30Days,
                MetricSection::OnDemand,
                "30",
            ),
            MetricDefinition::value(
                "zai.credits30",
                "Last 30 Days Credits",
                "credits30",
                true,
                MetricSection::OnDemand,
                false,
                "Cr",
                None,
            ),
        ],
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum ZaiError {
    #[error(
        "Add a Z.ai API key in Customize, set ZAI_API_KEY, ZHIPUAI_API_KEY, or GLM_API_KEY, or configure ~/.config/openquota/zai.json."
    )]
    MissingKey,
    #[error("The Z.ai API key is invalid. Check it at z.ai/manage-apikey/apikey-list.")]
    InvalidKey,
    #[error("Could not reach Z.ai. Check your internet connection.")]
    ConnectionFailed,
    #[error("Z.ai usage data is temporarily unavailable.")]
    InvalidResponse,
    #[error("Z.ai request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("No active GLM Coding Plan. Subscribe at z.ai/subscribe to view usage.")]
    NoCodingPlan,
    #[error("The Z.ai API key could not be read or updated.")]
    CredentialStorage,
}

impl From<ZaiError> for ProviderError {
    fn from(error: ZaiError) -> Self {
        let kind = match error {
            ZaiError::MissingKey | ZaiError::InvalidKey => ProviderErrorKind::Authentication,
            ZaiError::ConnectionFailed => ProviderErrorKind::Network,
            ZaiError::RequestFailed(429) => ProviderErrorKind::RateLimited,
            ZaiError::RequestFailed(401 | 403) => ProviderErrorKind::Authentication,
            ZaiError::NoCodingPlan => ProviderErrorKind::Permission,
            ZaiError::RequestFailed(_) | ZaiError::InvalidResponse => {
                ProviderErrorKind::InvalidResponse
            }
            ZaiError::CredentialStorage => ProviderErrorKind::CredentialStorage,
        };
        ProviderError::new(kind, error.to_string())
    }
}

pub struct ZaiProvider {
    auth: ZaiAuthStore,
    client: Arc<ZaiClient>,
}

impl ZaiProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            auth: ZaiAuthStore::new(),
            client: Arc::new(ZaiClient::new().map_err(ProviderError::from)?),
        })
    }

    #[cfg(test)]
    fn with_dependencies(auth: ZaiAuthStore, client: ZaiClient) -> Self {
        Self {
            auth,
            client: Arc::new(client),
        }
    }

    fn refresh_snapshot(&self, api_key: &str) -> Result<ProviderSnapshot, ProviderError> {
        let now = Utc::now();
        let quota = required_response(self.client.fetch_quota(api_key))?;
        if is_no_coding_plan(&quota.body) {
            return Err(ZaiError::NoCodingPlan.into());
        }
        let subscription = self
            .client
            .fetch_subscription(api_key)
            .ok()
            .filter(|response| response.status.is_success());
        let mapped = map_usage(
            &quota.body,
            subscription.as_ref().map(|response| &response.body),
        )?;
        let mut warnings = Vec::new();
        let account_usage = self.fetch_account_usage(api_key, mapped.uses_credits, now);
        if account_usage.is_none() {
            warnings.push("Z.ai account usage history is temporarily unavailable.".into());
        }
        let value_metrics = account_usage
            .as_ref()
            .and_then(ZaiAccountUsage::credits_metric)
            .into_iter()
            .collect();
        let usage_histories = account_usage
            .map(|usage| UsageHistories::account(usage.history))
            .unwrap_or_default();
        Ok(ProviderSnapshot {
            provider_id: "zai".into(),
            plan: mapped.plan,
            quotas: mapped.quotas,
            value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories,
            warnings,
            refreshed_at: now,
        })
    }

    fn fetch_account_usage(
        &self,
        api_key: &str,
        uses_credits: bool,
        now: DateTime<Utc>,
    ) -> Option<ZaiAccountUsage> {
        let primary = if uses_credits {
            AccountUsageKind::Credits
        } else {
            AccountUsageKind::Legacy
        };
        let fallback = if uses_credits {
            AccountUsageKind::Legacy
        } else {
            AccountUsageKind::Credits
        };
        let primary_usage = self.fetch_account_usage_kind(api_key, primary, now);
        if primary_usage.is_some() {
            return primary_usage;
        }
        self.fetch_account_usage_kind(api_key, fallback, now)
    }

    fn fetch_account_usage_kind(
        &self,
        api_key: &str,
        kind: AccountUsageKind,
        now: DateTime<Utc>,
    ) -> Option<ZaiAccountUsage> {
        let (start_time, end_time) = account_usage_range(now);
        let response = required_response(self.client.fetch_account_usage(
            kind,
            api_key,
            &start_time,
            &end_time,
        ))
        .ok()?;
        match kind {
            AccountUsageKind::Legacy => map_legacy_usage(&response.body, now),
            AccountUsageKind::Credits => map_credit_usage(&response.body, now),
        }
        .ok()
    }
}

fn account_usage_range(now: DateTime<Utc>) -> (String, String) {
    let local_now = now.with_timezone(&Local);
    let start = local_now
        .date_naive()
        .checked_sub_days(Days::new(HISTORY_DAYS.saturating_sub(1)))
        .unwrap_or(local_now.date_naive())
        .and_hms_opt(0, 0, 0)
        .unwrap_or(local_now.naive_local());
    (
        start.format("%Y-%m-%d %H:%M:%S").to_string(),
        local_now.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

impl UsageProvider for ZaiProvider {
    fn definition(&self) -> ProviderDefinition {
        definition()
    }

    fn has_local_credentials(&self) -> bool {
        self.auth.has_local_credentials()
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        let api_key = self
            .auth
            .load()
            .map_err(ProviderError::from)?
            .ok_or_else(|| ProviderError::from(ZaiError::MissingKey))?;
        self.refresh_snapshot(api_key.as_str())
    }

    fn api_key_status(&self) -> Option<Result<ApiKeyStatus, ProviderError>> {
        Some(self.auth.status().map_err(ProviderError::from))
    }

    fn supports_api_key_configuration(&self) -> bool {
        true
    }

    fn save_api_key(&self, value: &str) -> Result<(), ProviderError> {
        self.auth.save(value).map_err(ProviderError::from)
    }

    fn delete_api_key(&self) -> Result<(), ProviderError> {
        self.auth.delete().map_err(ProviderError::from)
    }
}

fn required_response(response: Result<ZaiResponse, ZaiError>) -> Result<ZaiResponse, ZaiError> {
    let response = response?;
    if matches!(
        response.status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Err(ZaiError::InvalidKey);
    }
    if !response.status.is_success() {
        return Err(ZaiError::RequestFailed(response.status.as_u16()));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use chrono::{Local, NaiveDate, TimeZone, Utc};

    use crate::{
        models::{ApiKeyStatus, MetricSection, ProviderErrorKind, QuotaFormat},
        providers::{
            api_key::{ApiKeyStore, EnvironmentReader, SecretBackend, SecretBytes},
            test_http, UsageProvider,
        },
    };

    use super::{
        account_usage_range, auth::ZaiAuthStore, client::ZaiClient, definition, ZaiProvider,
    };

    #[derive(Default)]
    struct MemorySecrets(Mutex<HashMap<String, Vec<u8>>>);

    impl SecretBackend for MemorySecrets {
        fn read(&self, account: &str) -> Result<Option<SecretBytes>, String> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(account)
                .cloned()
                .map(SecretBytes::new))
        }

        fn write(&self, account: &str, value: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(account.to_owned(), value.to_vec());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    struct Environment(HashMap<String, String>);

    impl EnvironmentReader for Environment {
        fn value(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn auth(key: Option<&str>) -> ZaiAuthStore {
        ZaiAuthStore::with_store(ApiKeyStore::with_backends(
            "zai",
            "ZAI_API_KEY",
            Arc::new(MemorySecrets::default()),
            Arc::new(Environment(
                key.map(|value| HashMap::from([("ZAI_API_KEY".into(), value.into())]))
                    .unwrap_or_default(),
            )),
        ))
    }

    fn provider(
        key: Option<&str>,
        quota_status: u16,
        quota_body: &str,
        subscription_status: u16,
        subscription_body: &str,
    ) -> ZaiProvider {
        let quota_url = test_http::serve_once(quota_status, &[], quota_body);
        let subscription_url = test_http::serve_once(subscription_status, &[], subscription_body);
        let legacy_usage_url = test_http::serve_once(
            200,
            &[],
            r#"{"success":true,"data":{"x_time":[],"modelDataList":[]}}"#,
        );
        let credit_usage_url = test_http::serve_once(
            200,
            &[],
            r#"{"success":true,"data":{"summary":{"totalCredits":{"value":"0"}},"modelUsage":{"xTime":[],"modelDataList":[]}}}"#,
        );
        ZaiProvider::with_dependencies(
            auth(key),
            ZaiClient::for_test(
                &subscription_url,
                &quota_url,
                &legacy_usage_url,
                &credit_usage_url,
                Duration::from_secs(1),
            ),
        )
    }

    fn provider_with_history(
        quota_body: &str,
        legacy_usage_body: &str,
        credit_usage_body: &str,
    ) -> ZaiProvider {
        let quota_url = test_http::serve_once(200, &[], quota_body);
        let subscription_url =
            test_http::serve_once(200, &[], include_str!("fixtures/subscription.json"));
        let legacy_usage_url = test_http::serve_once(200, &[], legacy_usage_body);
        let credit_usage_url = test_http::serve_once(200, &[], credit_usage_body);
        ZaiProvider::with_dependencies(
            auth(Some("secret")),
            ZaiClient::for_test(
                &subscription_url,
                &quota_url,
                &legacy_usage_url,
                &credit_usage_url,
                Duration::from_secs(1),
            ),
        )
    }

    #[test]
    fn refresh_maps_required_quota_and_optional_subscription() {
        let snapshot = provider(
            Some("secret"),
            200,
            include_str!("fixtures/quota.json"),
            200,
            include_str!("fixtures/subscription.json"),
        )
        .refresh()
        .unwrap();

        assert_eq!(snapshot.plan.as_deref(), Some("GLM Coding Pro"));
        assert_eq!(
            snapshot
                .quotas
                .iter()
                .map(|quota| quota.id.as_str())
                .collect::<Vec<_>>(),
            ["session", "weekly", "webSearches"]
        );
        assert_eq!(snapshot.quotas[2].format, QuotaFormat::Count);
        assert_eq!(snapshot.quotas[2].unit.as_deref(), Some("searches"));
        assert!(snapshot.status_metrics.is_empty());
        assert!(snapshot.warnings.is_empty());
    }

    #[test]
    fn credit_plan_populates_account_history_and_exact_credit_metric() {
        let today = chrono::Local::now().date_naive().to_string();
        let credit_usage = serde_json::json!({
            "success": true,
            "data": {
                "summary": {"totalCredits": {"value": "1.2345"}},
                "modelUsage": {
                    "xTime": [format!("{today} 00:00")],
                    "modelDataList": [{
                        "modelCode": "glm-5.3-flash",
                        "modelName": "GLM-5.3-Flash",
                        "cachedInputTokensUsage": ["100"],
                        "uncachedInputTokensUsage": ["20"],
                        "outputTokensUsage": ["5"],
                        "totalTokensUsage": ["125"],
                        "cachedInputCreditsUsage": ["0.6000"],
                        "uncachedInputCreditsUsage": ["0.4000"],
                        "outputCreditsUsage": ["0.2345"],
                        "totalCreditsUsage": ["1.2345"]
                    }]
                }
            }
        })
        .to_string();
        let snapshot = provider_with_history(
            r#"{"success":true,"data":{"limits":[
                {"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":25},
                {"type":"CREDIT_LIMIT","unit":6,"number":1,"percentage":10}
            ]}}"#,
            r#"{"success":true,"data":{"x_time":[],"modelDataList":[]}}"#,
            &credit_usage,
        )
        .refresh()
        .unwrap();

        assert_eq!(snapshot.quotas[0].used_percent, 25.0);
        let account = snapshot.usage_histories.account.unwrap();
        let today = account.today.unwrap();
        assert_eq!(today.tokens, 125);
        assert_eq!(today.estimated_cost_usd, None);
        assert_eq!(
            today.model_breakdown.unwrap().models[0].model,
            "GLM-5.3-Flash"
        );
        let credits = &snapshot.value_metrics[0];
        assert_eq!(credits.id, "credits30");
        assert!((credits.values[0].number - 1.2345).abs() < 0.000_001);
        assert_eq!(credits.values[0].label.as_deref(), Some("credits"));
        assert!(snapshot.warnings.is_empty());
    }

    #[test]
    fn account_history_request_covers_exactly_thirty_calendar_days() {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 34, 56).unwrap();
        let (start, end) = account_usage_range(now);
        let start_date = NaiveDate::parse_from_str(&start[..10], "%Y-%m-%d").unwrap();
        let end_date = NaiveDate::parse_from_str(&end[..10], "%Y-%m-%d").unwrap();

        assert_eq!(end_date.signed_duration_since(start_date).num_days(), 29);
        assert!(start.ends_with("00:00:00"));
        assert_eq!(
            end,
            now.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        );
    }

    #[test]
    fn subscription_failure_does_not_blank_required_quota() {
        let snapshot = provider(
            Some("secret"),
            200,
            include_str!("fixtures/quota.json"),
            503,
            "{}",
        )
        .refresh()
        .unwrap();

        assert_eq!(snapshot.plan, None);
        assert_eq!(snapshot.quotas.len(), 3);
    }

    #[test]
    fn missing_invalid_and_rate_limited_keys_are_distinct() {
        let missing = provider(None, 200, "{}", 200, "{}").refresh().unwrap_err();
        assert_eq!(missing.kind(), ProviderErrorKind::Authentication);
        assert!(missing.to_string().contains("Add a Z.ai API key"));

        for status in [401, 403] {
            let invalid = provider(Some("bad-key"), status, "{}", 200, "{}")
                .refresh()
                .unwrap_err();
            assert_eq!(invalid.kind(), ProviderErrorKind::Authentication);
            assert!(invalid.to_string().contains("invalid"));
            assert!(!invalid.to_string().contains("bad-key"));
        }

        let rate_limited = provider(Some("secret"), 429, "{}", 200, "{}")
            .refresh()
            .unwrap_err();
        assert_eq!(rate_limited.kind(), ProviderErrorKind::RateLimited);
    }

    #[test]
    fn no_coding_plan_and_malformed_payloads_are_typed() {
        let no_plan = provider(
            Some("secret"),
            200,
            r#"{"code":500,"msg":"Current user has no coding plan","success":false}"#,
            200,
            include_str!("fixtures/subscription.json"),
        )
        .refresh()
        .unwrap_err();
        assert_eq!(no_plan.kind(), ProviderErrorKind::Permission);
        assert!(no_plan.to_string().contains("GLM Coding Plan"));

        let malformed = provider(
            Some("secret"),
            200,
            r#"{"data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5}]}}"#,
            200,
            include_str!("fixtures/subscription.json"),
        )
        .refresh()
        .unwrap_err();
        assert_eq!(malformed.kind(), ProviderErrorKind::InvalidResponse);
    }

    #[test]
    fn transport_and_timeout_errors_do_not_expose_the_key() {
        let subscription_url = test_http::serve_once(200, &[], "{}");
        let provider = ZaiProvider::with_dependencies(
            auth(Some("super-secret-key")),
            ZaiClient::for_test(
                &subscription_url,
                "http://127.0.0.1:1",
                "http://127.0.0.1:1",
                "http://127.0.0.1:1",
                Duration::from_millis(100),
            ),
        );
        let error = provider.refresh().unwrap_err();
        assert_eq!(error.kind(), ProviderErrorKind::Network);
        assert!(!error.to_string().contains("super-secret-key"));

        let delayed = test_http::serve_once_after(
            test_http::TIMEOUT_TEST_RESPONSE_DELAY,
            200,
            &[],
            include_str!("fixtures/quota.json"),
        );
        let subscription_url = test_http::serve_once(200, &[], "{}");
        let timeout = ZaiProvider::with_dependencies(
            auth(Some("another-secret")),
            ZaiClient::for_test(
                &subscription_url,
                &delayed,
                "http://127.0.0.1:1",
                "http://127.0.0.1:1",
                test_http::TIMEOUT_TEST_CLIENT_LIMIT,
            ),
        )
        .refresh()
        .unwrap_err();
        assert_eq!(timeout.kind(), ProviderErrorKind::Network);
        assert!(!timeout.to_string().contains("another-secret"));
    }

    #[test]
    fn api_key_capability_uses_the_secure_vault_and_falls_back_to_environment() {
        let provider = provider(
            Some("environment-key"),
            200,
            include_str!("fixtures/quota.json"),
            200,
            include_str!("fixtures/subscription.json"),
        );
        assert_eq!(
            provider.api_key_status().unwrap().unwrap(),
            ApiKeyStatus::FromEnvironment
        );

        provider.save_api_key("saved-key").unwrap();
        assert_eq!(
            provider.api_key_status().unwrap().unwrap(),
            ApiKeyStatus::OverrideActive
        );

        provider.delete_api_key().unwrap();
        assert_eq!(
            provider.api_key_status().unwrap().unwrap(),
            ApiKeyStatus::FromEnvironment
        );
    }

    #[test]
    fn definition_exposes_expected_links_and_default_metric_layout() {
        let definition = definition();
        assert_eq!(definition.id, "zai");
        assert_eq!(definition.display_name, "Z.ai");
        assert_eq!(
            definition
                .links
                .iter()
                .map(|link| link.label.as_str())
                .collect::<Vec<_>>(),
            ["Dashboard", "API Keys"]
        );

        let metric = |id: &str| {
            definition
                .metrics
                .iter()
                .find(|metric| metric.id == id)
                .unwrap()
        };
        assert_eq!(
            metric("zai.session").default_section,
            MetricSection::AlwaysVisible
        );
        assert!(metric("zai.session").default_pinned);
        assert!(!metric("zai.session").source.session_window());
        assert_eq!(
            metric("zai.weekly").default_section,
            MetricSection::AlwaysVisible
        );
        assert!(metric("zai.weekly").default_pinned);
        assert_eq!(
            metric("zai.webSearches").default_section,
            MetricSection::OnDemand
        );
        assert!(!metric("zai.webSearches").default_pinned);
        assert_eq!(
            metric("zai.credits30").default_section,
            MetricSection::OnDemand
        );
        assert_eq!(
            definition
                .metrics
                .iter()
                .filter(|metric| matches!(metric.source, crate::models::MetricSource::Usage { .. }))
                .count(),
            3
        );
    }
}
