mod accounts;
pub mod auth;
mod client;
pub mod config;
mod local_usage;
mod mapper;

pub(crate) use mapper::map_sub2api_usage;

use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use reqwest::StatusCode;
use thiserror::Error;

use crate::{
    models::{
        MetricDefinition, MetricSection, ProviderDefinition, ProviderLink, ProviderNotice,
        ProviderNoticeTone, ProviderSnapshot, UsageHistories, UsagePeriodSelection,
    },
    pricing::{ModelPricing, PricingStore},
    storage::Storage,
};

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "claude".into(),
        display_name: "Claude".into(),
        short_name: "Cl".into(),
        fallback_enabled: true,
        local_usage_source_note: Some("From your Claude usage history (estimated)".into()),
        links: vec![
            ProviderLink::new("Status", "https://status.anthropic.com/"),
            ProviderLink::new("Dashboard", "https://claude.ai/settings/usage"),
        ],
        metrics: vec![
            MetricDefinition::quota(
                "claude.session",
                "Session",
                "session",
                true,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::quota(
                "claude.weekly",
                "Weekly",
                "weekly",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "W",
            ),
            MetricDefinition::quota(
                "claude.sonnet",
                "Sonnet",
                "sonnet",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "Sn",
            ),
            MetricDefinition::quota(
                "claude.fable",
                "Fable",
                "fable",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "F",
            ),
            MetricDefinition::quota_or_value(
                "claude.extra",
                "Extra Usage",
                "extra",
                true,
                MetricSection::AlwaysVisible,
                false,
                "E",
            ),
            MetricDefinition::trend("claude.trend"),
            MetricDefinition::usage(
                "claude.today",
                "Today",
                UsagePeriodSelection::Today,
                MetricSection::OnDemand,
                "T",
            ),
            MetricDefinition::usage(
                "claude.yesterday",
                "Yesterday",
                UsagePeriodSelection::Yesterday,
                MetricSection::OnDemand,
                "Y",
            ),
            MetricDefinition::usage(
                "claude.last30",
                "Last 30 Days",
                UsagePeriodSelection::Last30Days,
                MetricSection::OnDemand,
                "M",
            ),
        ],
    }
}

use self::{
    auth::{
        load_candidates, oauth_config, ClaudeCredential, ClaudeCredentialGeneration,
        ClaudeCredentialScope,
    },
    client::ClaudeClient,
    local_usage::scan_local_usage,
    mapper::map_usage,
};
use crate::providers::log_usage::scan_or_cached_usage;

#[derive(Debug, Error)]
pub enum ClaudeError {
    #[error("Not logged in. Run `claude` to authenticate.")]
    NotLoggedIn,
    #[error(
        "Claude Desktop login found, but its macOS-only encrypted session cannot be reused safely. Run `claude` in a terminal and sign in once."
    )]
    DesktopAppOnly,
    #[error("Your Claude session expired. Run `claude` to sign in again.")]
    SessionExpired,
    #[error("Your Claude token expired. Run `claude` to sign in again.")]
    TokenExpired,
    #[error("Claude OAuth settings contain an invalid URL.")]
    InvalidOAuthUrl,
    #[error("Refreshed Claude credentials could not be saved.")]
    AuthWrite,
    #[error("Claude login changed during refresh. Refresh again.")]
    CredentialsChanged,
    #[error("The Claude account changed while usage was refreshing. Refresh again.")]
    AccountChanged,
    #[error("Claude usage request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("Claude returned an invalid usage response.")]
    InvalidResponse,
    #[error("Could not connect to Claude. Check your internet connection.")]
    ConnectionFailed,
    #[error("Local Claude usage logs could not be processed.")]
    LocalUsage,
    #[error("Claude account settings could not be loaded.")]
    AccountStore(#[from] crate::storage::StorageError),
}

pub struct ClaudeProvider {
    credential_scope: ClaudeCredentialScope,
    account_identity: Option<String>,
    storage: Arc<Storage>,
    pricing: Arc<PricingStore>,
    client: ClaudeClient,
    cached_credential_fingerprint: Mutex<Option<[u8; 32]>>,
    last_good: Mutex<Option<ProviderSnapshot>>,
    rate_limited_until: Mutex<Option<chrono::DateTime<Utc>>>,
}

impl ClaudeProvider {
    pub fn new(storage: Arc<Storage>, pricing: Arc<PricingStore>) -> Result<Self, ClaudeError> {
        let client = ClaudeClient::new()?.for_provider("claude", config::settings_path());
        let provider = Self::with_scope(ClaudeCredentialScope::Standard, storage, pricing, client);
        if let Some(identity) = provider.account_identity.as_deref() {
            crate::providers::remember_default_account(&provider.storage, "claude", identity)?;
        }
        Ok(provider)
    }

    fn with_scope(
        credential_scope: ClaudeCredentialScope,
        storage: Arc<Storage>,
        pricing: Arc<PricingStore>,
        client: ClaudeClient,
    ) -> Self {
        Self {
            account_identity: accounts::identity_for_scope(&credential_scope),
            credential_scope,
            storage,
            pricing,
            client,
            cached_credential_fingerprint: Mutex::new(None),
            last_good: Mutex::new(None),
            rate_limited_until: Mutex::new(None),
        }
    }

    fn provider_id(&self) -> &str {
        "claude"
    }

    fn refresh_inner(&self) -> Result<(ProviderSnapshot, Option<String>), ClaudeError> {
        let config = oauth_config()?;
        self.refresh_inner_with_config(&config)
    }

    fn refresh_inner_with_config(
        &self,
        config: &auth::ClaudeOAuthConfig,
    ) -> Result<(ProviderSnapshot, Option<String>), ClaudeError> {
        let identity = accounts::identity_for_scope(&self.credential_scope);
        // A login switched mid-refresh must not publish one account's usage under the identity
        // of another, nor reload credentials for the new login within this refresh.
        let ensure_identity_unchanged = || {
            (accounts::identity_for_scope(&self.credential_scope) == identity)
                .then_some(())
                .ok_or(ClaudeError::AccountChanged)
        };
        let mut credential_reloads_remaining = 1;
        loop {
            match self.refresh_inner_once(config, identity.as_deref()) {
                Err(ClaudeError::CredentialsChanged) if credential_reloads_remaining > 0 => {
                    ensure_identity_unchanged()?;
                    credential_reloads_remaining -= 1;
                    crate::app_info!(
                        "auth:claude",
                        "credential source changed during refresh; reloading current login"
                    );
                }
                Ok(snapshot) => {
                    ensure_identity_unchanged()?;
                    return Ok((snapshot, identity));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn refresh_inner_once(
        &self,
        config: &auth::ClaudeOAuthConfig,
        identity: Option<&str>,
    ) -> Result<ProviderSnapshot, ClaudeError> {
        let candidates = load_candidates(&self.credential_scope);
        if candidates.is_empty() {
            crate::app_info!("auth:claude", "no reusable CLI credentials found");
            return Err(
                if matches!(self.credential_scope, ClaudeCredentialScope::Standard)
                    && auth::has_desktop_app_data()
                {
                    ClaudeError::DesktopAppOnly
                } else {
                    ClaudeError::NotLoggedIn
                },
            );
        }
        crate::app_debug!(
            "auth:claude",
            "credential candidates loaded ({})",
            candidates.len()
        );
        let now = Utc::now();
        let pricing = self.pricing.current();
        let mut credential_generation = ClaudeCredentialGeneration::from_candidates(&candidates);
        let mut last_auth_error = None;
        for mut credential in candidates {
            match self.refresh_candidate(
                &mut credential,
                config,
                now,
                &pricing,
                &mut credential_generation,
                identity,
            ) {
                Ok(snapshot) => return Ok(snapshot),
                Err(error @ (ClaudeError::SessionExpired | ClaudeError::TokenExpired)) => {
                    last_auth_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_auth_error.unwrap_or(ClaudeError::NotLoggedIn))
    }

    fn refresh_candidate(
        &self,
        credential: &mut ClaudeCredential,
        config: &auth::ClaudeOAuthConfig,
        now: chrono::DateTime<Utc>,
        pricing: &ModelPricing,
        credential_generation: &mut ClaudeCredentialGeneration,
        identity: Option<&str>,
    ) -> Result<ProviderSnapshot, ClaudeError> {
        let mut warnings = Vec::new();
        let usage = scan_or_cached_usage(
            &self.storage,
            self.provider_id(),
            identity
                .map(crate::providers::CacheIdentity::Resolved)
                .unwrap_or(crate::providers::CacheIdentity::Unresolved),
            "Claude",
            || scan_local_usage(&self.storage, now, pricing),
            &mut warnings,
        );

        if credential.inference_only {
            return Ok(ProviderSnapshot {
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: Vec::new(),
                usage_histories: UsageHistories::local_device(usage),
                warnings,
                refreshed_at: now,
            });
        }
        if !credential.has_profile_scope() {
            warnings.push(
                "Re-login for live usage. Run `claude` and sign in again to restore subscription limits."
                    .into(),
            );
            return Ok(ProviderSnapshot {
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: Vec::new(),
                usage_histories: UsageHistories::local_device(usage),
                warnings,
                refreshed_at: now,
            });
        }
        self.activate_live_usage_cache(credential.fingerprint());
        if credential.needs_refresh(now.timestamp_millis()) {
            let previous_fingerprint = credential.fingerprint();
            refresh_credential(
                &self.client,
                credential,
                config,
                now,
                &mut warnings,
                credential_generation,
                &self.credential_scope,
            )?;
            self.replace_live_usage_fingerprint(previous_fingerprint, credential.fingerprint());
        }

        let cooldown_until = self
            .rate_limited_until
            .lock()
            .ok()
            .and_then(|value| *value)
            .filter(|until| now < *until);
        if let Some(until) = cooldown_until {
            let retry = until.signed_duration_since(now).num_seconds().max(0) as u64;
            if let Some(mut snapshot) = self.last_good.lock().ok().and_then(|value| value.clone()) {
                snapshot.usage_histories = UsageHistories::local_device(usage);
                snapshot.warnings.push(
                    "Claude live usage is rate limited; showing the last successful limits.".into(),
                );
                snapshot.notices = vec![rate_limit_notice(retry, true)];
                snapshot.refreshed_at = now;
                return Ok(snapshot);
            }
            warnings.push(format!(
                "Claude live usage is rate limited; retrying in about {}.",
                retry_minutes(retry)
            ));
            return Ok(ProviderSnapshot {
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: vec![rate_limit_notice(retry, false)],
                usage_histories: UsageHistories::local_device(usage),
                warnings,
                refreshed_at: now,
            });
        }

        let token = credential.access_token().ok_or(ClaudeError::NotLoggedIn)?;
        let (mut status, mut body, mut retry_after) = self.client.fetch_usage(token, config)?;
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            let previous_fingerprint = credential.fingerprint();
            refresh_credential(
                &self.client,
                credential,
                config,
                now,
                &mut warnings,
                credential_generation,
                &self.credential_scope,
            )?;
            self.replace_live_usage_fingerprint(previous_fingerprint, credential.fingerprint());
            let token = credential.access_token().ok_or(ClaudeError::TokenExpired)?;
            (status, body, retry_after) = self.client.fetch_usage(token, config)?;
        }
        if auth::credential_generation(&self.credential_scope) != *credential_generation {
            return Err(ClaudeError::CredentialsChanged);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry = retry_after.unwrap_or(5 * 60);
            if let Ok(mut until) = self.rate_limited_until.lock() {
                *until = Some(now + Duration::seconds(retry as i64));
            }
            if let Some(mut snapshot) = self.last_good.lock().ok().and_then(|value| value.clone()) {
                snapshot.usage_histories = UsageHistories::local_device(usage);
                snapshot.warnings.push(format!(
                    "Claude live usage is rate limited; retrying in about {}.",
                    retry_minutes(retry)
                ));
                snapshot.notices = vec![rate_limit_notice(retry, true)];
                snapshot.refreshed_at = now;
                return Ok(snapshot);
            }
            warnings.push(format!(
                "Claude live usage is rate limited; retrying in about {}.",
                retry_minutes(retry)
            ));
            return Ok(ProviderSnapshot {
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: vec![rate_limit_notice(retry, false)],
                usage_histories: UsageHistories::local_device(usage),
                warnings,
                refreshed_at: now,
            });
        }
        self.build_snapshot(status, &body, credential, usage, warnings, now)
    }

    fn build_snapshot(
        &self,
        status: StatusCode,
        body: &serde_json::Value,
        credential: &ClaudeCredential,
        usage: crate::models::UsageHistory,
        warnings: Vec<String>,
        now: chrono::DateTime<Utc>,
    ) -> Result<ProviderSnapshot, ClaudeError> {
        let mapped = map_usage(status, body, &credential.oauth)?;
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: self.provider_id().into(),
            plan: mapped.plan,
            quotas: mapped.quotas,
            value_metrics: mapped.value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories::local_device(usage),
            warnings,
            refreshed_at: now,
        };
        if let Ok(mut last) = self.last_good.lock() {
            *last = Some(snapshot.clone());
        }
        if let Ok(mut until) = self.rate_limited_until.lock() {
            *until = None;
        }
        Ok(snapshot)
    }

    fn activate_live_usage_cache(&self, fingerprint: [u8; 32]) {
        let changed = self
            .cached_credential_fingerprint
            .lock()
            .map(|mut active| {
                if active.as_ref() == Some(&fingerprint) {
                    false
                } else {
                    *active = Some(fingerprint);
                    true
                }
            })
            .unwrap_or(true);
        if !changed {
            return;
        }
        if let Ok(mut last) = self.last_good.lock() {
            *last = None;
        }
        if let Ok(mut until) = self.rate_limited_until.lock() {
            *until = None;
        }
    }

    fn replace_live_usage_fingerprint(&self, previous: [u8; 32], current: [u8; 32]) {
        if let Ok(mut active) = self.cached_credential_fingerprint.lock() {
            if active.as_ref() == Some(&previous) {
                *active = Some(current);
            }
        }
    }
}

fn rate_limit_notice(retry_seconds: u64, showing_stale_limits: bool) -> ProviderNotice {
    let retry = if retry_seconds == 0 {
        "Ready to retry".to_owned()
    } else {
        format!("Retrying in about {}", retry_minutes(retry_seconds))
    };
    ProviderNotice {
        id: "rateLimited".into(),
        title: "Live usage paused".into(),
        message: if showing_stale_limits {
            format!("Showing the last successful limits · {retry}")
        } else {
            retry
        },
        tone: ProviderNoticeTone::Warning,
    }
}

fn retry_minutes(retry_seconds: u64) -> String {
    let minutes = retry_seconds.div_ceil(60);
    format!(
        "{minutes} {}",
        if minutes == 1 { "minute" } else { "minutes" }
    )
}

fn refresh_credential(
    client: &ClaudeClient,
    credential: &mut ClaudeCredential,
    config: &auth::ClaudeOAuthConfig,
    now: chrono::DateTime<Utc>,
    warnings: &mut Vec<String>,
    credential_generation: &mut ClaudeCredentialGeneration,
    credential_scope: &ClaudeCredentialScope,
) -> Result<(), ClaudeError> {
    let refresh_token = credential
        .oauth
        .refresh_token
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or(ClaudeError::TokenExpired)?;
    let refreshed = client.refresh_token(refresh_token, config)?;
    match credential.update_and_save(
        refreshed.access_token,
        refreshed.refresh_token,
        refreshed.expires_in,
        now.timestamp_millis(),
        credential_generation,
        credential_scope,
    ) {
        Ok(true) => {
            *credential_generation = credential_generation
                .replacing(credential)
                .ok_or(ClaudeError::CredentialsChanged)?;
        }
        Ok(false) => {}
        Err(ClaudeError::CredentialsChanged) => return Err(ClaudeError::CredentialsChanged),
        Err(_) => {
            crate::app_error!(
                "auth:claude",
                "failed to persist rotated credentials; using them for this session only"
            );
            warnings.push(
                "The refreshed Claude login is active for this session but could not be saved."
                    .into(),
            );
        }
    }
    Ok(())
}

fn plan_name(credential: &ClaudeCredential) -> Option<String> {
    credential.oauth.subscription_type.as_ref().map(|value| {
        let mut chars = value.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
            .unwrap_or_default()
    })
}

fn provider_error(error: ClaudeError) -> crate::providers::ProviderError {
    use crate::models::ProviderErrorKind as Kind;

    let kind = match error {
        ClaudeError::NotLoggedIn
        | ClaudeError::DesktopAppOnly
        | ClaudeError::SessionExpired
        | ClaudeError::TokenExpired
        | ClaudeError::CredentialsChanged
        | ClaudeError::AccountChanged => Kind::Authentication,
        ClaudeError::InvalidOAuthUrl | ClaudeError::InvalidResponse => Kind::InvalidResponse,
        ClaudeError::AuthWrite => Kind::CredentialStorage,
        ClaudeError::RequestFailed(429) => Kind::RateLimited,
        ClaudeError::RequestFailed(_) | ClaudeError::ConnectionFailed => Kind::Network,
        ClaudeError::LocalUsage => Kind::LocalData,
        ClaudeError::AccountStore(_) => Kind::Internal,
    };
    crate::providers::ProviderError::from_display(kind, error)
}

impl crate::providers::UsageProvider for ClaudeProvider {
    fn definition(&self) -> ProviderDefinition {
        definition()
    }

    fn has_local_credentials(&self) -> bool {
        auth::has_local_credentials(&self.credential_scope)
    }

    fn cache_identity(&self) -> crate::providers::CacheIdentity<'_> {
        self.account_identity
            .as_deref()
            .map(crate::providers::CacheIdentity::Resolved)
            .unwrap_or(crate::providers::CacheIdentity::Unresolved)
    }

    fn supports_account_names(&self) -> bool {
        true
    }

    fn account_identity(&self) -> Option<&str> {
        self.account_identity.as_deref()
    }

    fn refresh(&self) -> Result<ProviderSnapshot, crate::providers::ProviderError> {
        self.refresh_inner()
            .map(|(snapshot, _)| snapshot)
            .map_err(provider_error)
    }

    fn refresh_for_service(
        &self,
    ) -> Result<crate::providers::ProviderRefresh, crate::providers::ProviderError> {
        let (snapshot, identity) = self.refresh_inner().map_err(provider_error)?;
        Ok(crate::providers::ProviderRefresh {
            snapshot,
            cache_identity: identity.clone(),
            account: identity.map(|identity| crate::providers::AccountRefresh {
                family: "claude",
                provider_id: "claude",
                identity,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        sync::{mpsc, Arc},
        thread,
        time::Duration as StdDuration,
    };

    use chrono::{Duration, Utc};
    use tempfile::tempdir;

    use crate::{
        models::{ProviderNoticeTone, ProviderSnapshot},
        pricing::PricingStore,
        storage::Storage,
    };

    use super::{
        auth::{ClaudeCredentialScope, ClaudeOAuthConfig},
        client::ClaudeClient,
        rate_limit_notice, ClaudeError, ClaudeProvider,
    };

    fn credential_json(access: &str, refresh: &str, plan: &str) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":4102444800000,"subscriptionType":"{plan}","scopes":["user:profile"]}}}}"#
        )
    }

    fn write_http_response(stream: &mut impl Write, utilization: u8) {
        let body = format!(
            r#"{{"five_hour":{{"utilization":{utilization},"resets_at":"2099-01-01T00:00:00Z"}}}}"#
        );
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    #[test]
    fn rate_limit_notice_distinguishes_empty_and_stale_live_usage() {
        let empty = rate_limit_notice(301, false);
        assert_eq!(empty.title, "Live usage paused");
        assert_eq!(empty.message, "Retrying in about 6 minutes");
        assert_eq!(empty.tone, ProviderNoticeTone::Warning);

        let stale = rate_limit_notice(60, true);
        assert_eq!(
            stale.message,
            "Showing the last successful limits · Retrying in about 1 minute"
        );
    }

    #[test]
    fn account_change_clears_live_usage_and_rate_limit_cache() {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let pricing = Arc::new(PricingStore::new(directory.path().join("pricing")).unwrap());
        let provider = ClaudeProvider::with_scope(
            ClaudeCredentialScope::Standard,
            storage,
            pricing,
            ClaudeClient::new().unwrap(),
        );
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: "claude".into(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        };

        provider.activate_live_usage_cache([1; 32]);
        *provider.last_good.lock().unwrap() = Some(snapshot);
        *provider.rate_limited_until.lock().unwrap() = Some(Utc::now() + Duration::minutes(5));
        provider.activate_live_usage_cache([1; 32]);
        assert!(provider.last_good.lock().unwrap().is_some());
        assert!(provider.rate_limited_until.lock().unwrap().is_some());

        provider.activate_live_usage_cache([2; 32]);
        assert!(provider.last_good.lock().unwrap().is_none());
        assert!(provider.rate_limited_until.lock().unwrap().is_none());
    }

    #[test]
    fn login_changed_during_usage_request_is_not_published_under_the_old_identity() {
        let directory = tempdir().unwrap();
        let account_root = directory.path().join("account");
        fs::create_dir_all(&account_root).unwrap();
        let credential_path = account_root.join(".credentials.json");
        let identity_path = account_root.join(".claude.json");
        fs::write(
            &credential_path,
            credential_json("account-a", "refresh-a", "pro"),
        )
        .unwrap();
        fs::write(
            &identity_path,
            r#"{"oauthAccount":{"accountUuid":"account-a"}}"#,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (first_request_tx, first_request_rx) = mpsc::sync_channel(0);
        let (first_response_tx, first_response_rx) = mpsc::sync_channel(0);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0_u8; 4096];
            let length = stream.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..length]);
            let authorization = request
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_owned())
                .unwrap();
            first_request_tx.send(()).unwrap();
            first_response_rx.recv().unwrap();
            write_http_response(&mut stream, 25);
            authorization
        });

        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let pricing = Arc::new(PricingStore::new(directory.path().join("pricing")).unwrap());
        let credential_scope = ClaudeCredentialScope::ConfigDir {
            path: account_root.clone(),
            keychain_literal: account_root.to_string_lossy().into_owned(),
        };
        let provider = Arc::new(ClaudeProvider::with_scope(
            credential_scope,
            storage,
            pricing,
            ClaudeClient::new().unwrap(),
        ));
        let config = ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test-client".into(),
        };
        let refresh = thread::spawn(move || provider.refresh_inner_with_config(&config));

        first_request_rx
            .recv_timeout(StdDuration::from_secs(2))
            .unwrap();
        fs::write(
            &credential_path,
            credential_json("account-b", "refresh-b", "max"),
        )
        .unwrap();
        fs::write(
            &identity_path,
            r#"{"oauthAccount":{"accountUuid":"account-b"}}"#,
        )
        .unwrap();
        first_response_tx.send(()).unwrap();

        let error = refresh.join().unwrap().unwrap_err();
        let authorization = server.join().unwrap();
        assert!(matches!(error, ClaudeError::AccountChanged));
        assert_eq!(authorization, "Bearer account-a");
    }
}
