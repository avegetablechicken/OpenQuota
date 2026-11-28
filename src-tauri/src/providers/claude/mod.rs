mod accounts;
pub mod auth;
mod client;
pub mod config;
pub(crate) mod local_usage;
mod mapper;
mod refresh_lock;
pub mod reset_claim;

pub(crate) use mapper::{map_sub2api_claude_reset_credits, map_sub2api_usage_at};

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
            MetricDefinition::value(
                "claude.rateLimitResets",
                "Rate Limit Resets",
                "claudeRateLimitResets",
                true,
                MetricSection::OnDemand,
                false,
                "R",
                Some("resets"),
            ),
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
        ClaudeCredentialScope, UnsavedRenewal,
    },
    client::ClaudeClient,
    local_usage::scan_local_usage,
    mapper::{map_usage_at, retain_known_resets},
};

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
    #[error("Claude Code is renewing its login. Refresh again in a moment.")]
    ClaudeCodeRenewing,
    #[error("Claude Code's login refresh lock could not be taken.")]
    RefreshLockUnavailable,
    #[error("The Claude account changed while usage was refreshing. Refresh again.")]
    AccountChanged,
    #[error("Claude usage request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("Claude returned an invalid usage response.")]
    InvalidResponse,
    #[error("Could not connect to Claude. Check your internet connection.")]
    ConnectionFailed,
    #[error("{0}")]
    CertificateFailed(String),
    #[error("Local Claude usage logs could not be processed.")]
    LocalUsage,
    #[error("Claude account settings could not be loaded.")]
    AccountStore(#[from] crate::storage::StorageError),
}

impl From<crate::providers::local_usage::home_clients::FoldInUnavailable> for ClaudeError {
    fn from(_: crate::providers::local_usage::home_clients::FoldInUnavailable) -> Self {
        Self::LocalUsage
    }
}

/// Reads this device's local usage history for a refresh.
type LocalUsageScan =
    fn(
        &Storage,
        chrono::DateTime<Utc>,
        &ModelPricing,
    ) -> Result<crate::providers::local_usage::log_usage::ScannedUsage, ClaudeError>;

pub struct ClaudeProvider {
    credential_scope: ClaudeCredentialScope,
    /// `scan_local_usage` outside tests, which read the logs under the real home directory.
    local_usage_scan: LocalUsageScan,
    account_identity: Option<String>,
    storage: Arc<Storage>,
    pricing: Arc<PricingStore>,
    client: ClaudeClient,
    cached_credential_fingerprint: Mutex<Option<[u8; 32]>>,
    last_good: Mutex<Option<ProviderSnapshot>>,
    rate_limited_until: Mutex<Option<chrono::DateTime<Utc>>>,
    /// Renewals that could not be saved. Their refresh tokens were spent, so each is used in
    /// place of its saved login while that still holds the spent token.
    unsaved_renewals: Mutex<Vec<UnsavedRenewal>>,
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
            local_usage_scan: scan_local_usage,
            storage,
            pricing,
            client,
            cached_credential_fingerprint: Mutex::new(None),
            last_good: Mutex::new(None),
            rate_limited_until: Mutex::new(None),
            unsaved_renewals: Mutex::new(Vec::new()),
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
        // The generation describes the saved logins, so it is taken before unsaved renewals
        // replace them.
        let mut credential_generation = ClaudeCredentialGeneration::from_candidates(&candidates);
        let candidates = self.resume_unsaved_renewals(candidates);
        let mut last_auth_error = None;
        for mut credential in candidates {
            let inference_only = credential.inference_only;
            match self.refresh_candidate(
                &mut credential,
                config,
                now,
                &mut credential_generation,
                identity,
            ) {
                Ok(mut snapshot) => {
                    // The inference-only token shows no live limits; say why the stored login
                    // that would have shown them did not.
                    if let Some(error) = last_auth_error.filter(|_| inference_only) {
                        snapshot
                            .warnings
                            .push(format!("Live Claude limits are unavailable. {error}"));
                    }
                    return Ok(snapshot);
                }
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
        credential_generation: &mut ClaudeCredentialGeneration,
        identity: Option<&str>,
    ) -> Result<ProviderSnapshot, ClaudeError> {
        let mut warnings = Vec::new();

        if credential.inference_only {
            return Ok(ProviderSnapshot {
                upstream_provider_id: None,
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: Vec::new(),
                usage_histories: UsageHistories::default(),
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
                upstream_provider_id: None,
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: Vec::new(),
                usage_histories: UsageHistories::default(),
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
            self.remember_unsaved_renewal(credential);
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
                snapshot.usage_histories = UsageHistories::default();
                snapshot.warnings.push(
                    "Claude live usage is rate limited; showing the last successful limits.".into(),
                );
                snapshot.notices = vec![rate_limit_notice(retry, true)];
                return Ok(snapshot);
            }
            warnings.push(format!(
                "Claude live usage is rate limited; retrying in about {}.",
                retry_minutes(retry)
            ));
            return Ok(ProviderSnapshot {
                upstream_provider_id: None,
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: vec![rate_limit_notice(retry, false)],
                usage_histories: UsageHistories::default(),
                warnings,
                refreshed_at: now,
            });
        }

        let token = credential.access_token().ok_or(ClaudeError::NotLoggedIn)?;
        let (mut status, mut body, mut retry_after, mut clock_offset) =
            self.client.fetch_usage(token, config)?;
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
            self.remember_unsaved_renewal(credential);
            self.replace_live_usage_fingerprint(previous_fingerprint, credential.fingerprint());
            let token = credential.access_token().ok_or(ClaudeError::TokenExpired)?;
            (status, body, retry_after, clock_offset) = self.client.fetch_usage(token, config)?;
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
                snapshot.usage_histories = UsageHistories::default();
                snapshot.warnings.push(format!(
                    "Claude live usage is rate limited; retrying in about {}.",
                    retry_minutes(retry)
                ));
                snapshot.notices = vec![rate_limit_notice(retry, true)];
                return Ok(snapshot);
            }
            warnings.push(format!(
                "Claude live usage is rate limited; retrying in about {}.",
                retry_minutes(retry)
            ));
            return Ok(ProviderSnapshot {
                upstream_provider_id: None,
                accounts: None,
                provider_id: self.provider_id().into(),
                plan: plan_name(credential),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: vec![rate_limit_notice(retry, false)],
                usage_histories: UsageHistories::default(),
                warnings,
                refreshed_at: now,
            });
        }
        let mut mapped = map_usage_at(status, &body, &credential.oauth, now)?;
        let last_good = self.last_good.lock().ok().and_then(|value| value.clone());
        retain_known_resets(&mut mapped.value_metrics, last_good.as_ref(), &body, now);
        let has_resets = mapped
            .value_metrics
            .iter()
            .any(|metric| metric.id == "claudeRateLimitResets" && !metric.reset_credits.is_empty());
        // The local identity file and the first usable credential (e.g. Keychain) can belong
        // to different logins. Verify the very token that produced this usage before stamping
        // new or retained reset credits with the local account identity.
        let verified_owner = identity.filter(|expected| {
            has_resets
                && credential
                    .access_token()
                    .and_then(|token| self.client.fetch_profile(token, config).ok())
                    .is_some_and(|(status, profile)| {
                        status.is_success()
                            && accounts::profile_matches_identity(&profile, expected)
                    })
        });
        if has_resets && verified_owner.is_none() {
            warnings.push("Claude reset ownership could not be verified; claiming resets is disabled until a successful refresh.".into());
        }
        for metric in &mut mapped.value_metrics {
            if metric.id == "claudeRateLimitResets" {
                for credit in &mut metric.reset_credits {
                    credit.claim_account = verified_owner.map(str::to_owned);
                    credit.redeemable &= verified_owner.is_some();
                }
            }
        }
        for quota in &mut mapped.quotas {
            quota.clock_offset_seconds = clock_offset;
        }
        let snapshot = ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: self.provider_id().into(),
            plan: mapped.plan,
            quotas: mapped.quotas,
            value_metrics: mapped.value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories::default(),
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

    /// Replaces each saved login that still holds a refresh token spent by an unsaved renewal
    /// with that renewal, and forgets renewals whose saved login has since changed.
    fn resume_unsaved_renewals(
        &self,
        mut candidates: Vec<ClaudeCredential>,
    ) -> Vec<ClaudeCredential> {
        let Ok(mut renewals) = self.unsaved_renewals.lock() else {
            return candidates;
        };
        renewals.retain(|renewal| {
            candidates
                .iter_mut()
                .any(|candidate| candidate.resume_unsaved(renewal))
        });
        candidates
    }

    /// Keeps the renewal `credential` carries while its saved login could not take it.
    fn remember_unsaved_renewal(&self, credential: &ClaudeCredential) {
        let Ok(mut renewals) = self.unsaved_renewals.lock() else {
            return;
        };
        renewals.retain(|renewal| !credential.is_for_source(renewal));
        if let Some(renewal) = credential.unsaved_renewal() {
            renewals.push(renewal);
        }
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
    let spent = credential
        .oauth
        .refresh_token
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or(ClaudeError::TokenExpired)?;
    // Claude Code renews its login only while holding this lock and rereads the login inside
    // it. Holding it until the renewal is saved means no two programs spend one refresh token,
    // and neither writes a spent one over the other's renewal.
    let lock = auth::claude_code_lock(credential_scope)?;
    // A login renewed or switched while we waited is reloaded, not renewed again.
    if auth::credential_generation(credential_scope) != *credential_generation {
        return Err(ClaudeError::CredentialsChanged);
    }
    // Taken over as a dead holder's: another program may be renewing now.
    if lock.compromised() {
        return Err(ClaudeError::ClaudeCodeRenewing);
    }
    let refreshed = match client.refresh_token(&spent, config) {
        // A program that ignores the lock may have spent the token meanwhile.
        Err(ClaudeError::SessionExpired)
            if auth::credential_generation(credential_scope) != *credential_generation =>
        {
            return Err(ClaudeError::CredentialsChanged);
        }
        refreshed => refreshed?,
    };
    match credential.save_renewal(&refreshed, &spent, now.timestamp_millis()) {
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
        | ClaudeError::ClaudeCodeRenewing
        | ClaudeError::AccountChanged => Kind::Authentication,
        ClaudeError::InvalidOAuthUrl | ClaudeError::InvalidResponse => Kind::InvalidResponse,
        ClaudeError::AuthWrite | ClaudeError::RefreshLockUnavailable => Kind::CredentialStorage,
        ClaudeError::RequestFailed(429) => Kind::RateLimited,
        ClaudeError::RequestFailed(_)
        | ClaudeError::ConnectionFailed
        | ClaudeError::CertificateFailed(_) => Kind::Network,
        ClaudeError::LocalUsage => Kind::LocalData,
        ClaudeError::AccountStore(_) => Kind::Internal,
    };
    crate::providers::ProviderError::from_display(kind, error)
}

impl crate::providers::UsageProvider for ClaudeProvider {
    fn definition(&self) -> ProviderDefinition {
        definition()
    }

    fn supports_local_usage(&self) -> bool {
        true
    }

    fn refresh_local_usage(
        &self,
    ) -> Option<Result<crate::providers::ScannedUsage, crate::providers::ProviderError>> {
        Some(
            (self.local_usage_scan)(&self.storage, Utc::now(), &self.pricing.current())
                .map_err(provider_error),
        )
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

    /// Keeps a refresh off the local logs under the real home directory, whose scan on a
    /// machine with Claude Code history takes long enough to outlast the tests' timeouts.
    fn without_local_usage(mut provider: ClaudeProvider) -> ClaudeProvider {
        provider.local_usage_scan = |_, _, _| Ok(Default::default());
        provider
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
            upstream_provider_id: None,
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
        let provider = Arc::new(without_local_usage(ClaudeProvider::with_scope(
            credential_scope,
            storage,
            pricing,
            ClaudeClient::new().unwrap(),
        )));
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

    /// A provider over an isolated Claude configuration directory.
    fn isolated_provider(
        directory: &std::path::Path,
        account_root: &std::path::Path,
    ) -> Arc<ClaudeProvider> {
        let storage = Arc::new(Storage::open(&directory.join("openquota.db")).unwrap());
        let pricing = Arc::new(PricingStore::new(directory.join("pricing")).unwrap());
        Arc::new(without_local_usage(ClaudeProvider::with_scope(
            ClaudeCredentialScope::ConfigDir {
                path: account_root.to_path_buf(),
                keychain_literal: account_root.to_string_lossy().into_owned(),
            },
            storage,
            pricing,
            ClaudeClient::new().unwrap(),
        )))
    }

    #[test]
    fn reset_ownership_is_verified_with_the_usage_token_including_retained_credits() {
        let org = "550e8400-e29b-41d4-a716-446655440000";
        let matching =
            serde_json::json!({"account":{"uuid":"account-a"},"organization":{"uuid":org}});
        let mismatched =
            serde_json::json!({"account":{"uuid":"account-b"},"organization":{"uuid":org}});
        for (profile_status, profile, verified) in [
            (200, matching.clone(), true),
            (200, mismatched.clone(), false),
            (403, serde_json::Value::Null, false),
            (503, serde_json::Value::Null, false),
            (
                200,
                serde_json::json!({"account":{"uuid":"account-a"}}),
                false,
            ),
        ] {
            let directory = tempdir().unwrap();
            let root = directory.path().join("account");
            fs::create_dir_all(&root).unwrap();
            fs::write(
                root.join(".credentials.json"),
                credential_json("usage-token", "refresh-token", "pro"),
            )
            .unwrap();
            fs::write(root.join(".claude.json"), serde_json::json!({"oauthAccount":{"accountUuid":"account-a","organizationUuid":org}}).to_string()).unwrap();
            let provider = isolated_provider(directory.path(), &root);
            let expected_owner =
                super::accounts::identity_for_scope(&provider.credential_scope).unwrap();
            let usage = serde_json::json!({"five_hour":{"utilization":12},"cedar_ember":{
                "eligible":true,"next_grant_id":"grant","grants":[{"id":"grant","resets_left":2,"ends_at":"2099-12-01T00:00:00Z","usable_now":true,"clears":["five_hour","seven_day"]}]
            }});
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let mismatched = mismatched.clone();
            let server = thread::spawn(move || {
                for (status, body, path) in [
                    (200, usage, "/usage?cedar_ember=1"),
                    (profile_status, profile, "/api/oauth/profile"),
                    (
                        200,
                        serde_json::json!({"five_hour":{"utilization":13}}),
                        "/usage?cedar_ember=1",
                    ),
                    (200, mismatched.clone(), "/api/oauth/profile"),
                ] {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(StdDuration::from_secs(5)))
                        .unwrap();
                    let request = read_request(&mut stream);
                    assert!(request.starts_with(&format!("GET {path} ")), "{request}");
                    assert!(request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer usage-token"));
                    let body = body.to_string();
                    write!(stream,"HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
            });
            let config = ClaudeOAuthConfig {
                usage_url: format!("{base}/usage"),
                refresh_url: format!("{base}/token"),
                client_id: "test".into(),
            };
            let (snapshot, _) = provider.refresh_inner_with_config(&config).unwrap();
            let metric = snapshot
                .value_metrics
                .iter()
                .find(|metric| metric.id == "claudeRateLimitResets")
                .unwrap();
            assert_eq!(metric.values[0].number, 2.0);
            for credit in &metric.reset_credits {
                assert_eq!(
                    credit.claim_account.as_deref(),
                    verified.then_some(expected_owner.as_str())
                );
                assert_eq!(credit.redeemable, verified);
            }
            let (retained, _) = provider.refresh_inner_with_config(&config).unwrap();
            let metric = retained
                .value_metrics
                .iter()
                .find(|metric| metric.id == "claudeRateLimitResets")
                .unwrap();
            assert_eq!(metric.values[0].number, 2.0);
            assert!(metric
                .reset_credits
                .iter()
                .all(|credit| credit.claim_account.is_none() && !credit.redeemable));
            assert!(retained
                .warnings
                .iter()
                .any(|warning| warning.contains("ownership")));
            server.join().unwrap();
        }
    }

    #[test]
    fn fallback_capability_ineligibility_retains_verified_credits_while_real_revocation_clears_them(
    ) {
        let org = "550e8400-e29b-41d4-a716-446655440000";
        let matching =
            serde_json::json!({"account":{"uuid":"account-a"},"organization":{"uuid":org}});
        let mismatched =
            serde_json::json!({"account":{"uuid":"account-b"},"organization":{"uuid":org}});
        let directory = tempdir().unwrap();
        let root = directory.path().join("account");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(".credentials.json"),
            credential_json("usage-token", "refresh-token", "pro"),
        )
        .unwrap();
        fs::write(
            root.join(".claude.json"),
            serde_json::json!({"oauthAccount":{"accountUuid":"account-a","organizationUuid":org}})
                .to_string(),
        )
        .unwrap();
        let provider = isolated_provider(directory.path(), &root);
        let expected_owner =
            super::accounts::identity_for_scope(&provider.credential_scope).unwrap();
        let initial_usage = serde_json::json!({
            "five_hour": {"utilization": 12},
            "cedar_ember": {
                "eligible": true,
                "next_grant_id": "grant",
                "grants": [{
                    "id": "grant",
                    "resets_left": 2,
                    "ends_at": "2099-12-01T00:00:00Z",
                    "usable_now": true,
                    "clears": ["five_hour", "seven_day"]
                }]
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for (status, body, path) in [
                // 1. Primary UA succeeds with active reset credits.
                (200, initial_usage, "/usage?cedar_ember=1"),
                (200, matching.clone(), "/api/oauth/profile"),
                // 2. Primary UA hits 429, fallback returns `surface` ineligibility -> retain & verify.
                (429, serde_json::Value::Null, "/usage?cedar_ember=1"),
                (
                    200,
                    serde_json::json!({
                        "five_hour": {"utilization": 12},
                        "cedar_ember": {"eligible": false, "ineligible_reason": "surface"}
                    }),
                    "/usage?cedar_ember=1",
                ),
                (200, matching, "/api/oauth/profile"),
                // 3. During primary cooldown, fallback returns `cli_version` + mismatched profile ->
                //    credits retained, ownership verification clears claimability.
                (
                    200,
                    serde_json::json!({
                        "five_hour": {"utilization": 12},
                        "cedar_ember": {"eligible": false, "ineligible_reason": "cli_version"}
                    }),
                    "/usage?cedar_ember=1",
                ),
                (200, mismatched, "/api/oauth/profile"),
                // 4. Real eligibility revocation supersedes prior credits without calling profile.
                (
                    200,
                    serde_json::json!({
                        "five_hour": {"utilization": 12},
                        "cedar_ember": {"eligible": false, "ineligible_reason": "plan"}
                    }),
                    "/usage?cedar_ember=1",
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(StdDuration::from_secs(5)))
                    .unwrap();
                let request = read_request(&mut stream);
                assert!(request.starts_with(&format!("GET {path} ")), "{request}");
                let body = body.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let config = ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test".into(),
        };

        let (initial, _) = provider.refresh_inner_with_config(&config).unwrap();
        let (fallback_ok, _) = provider.refresh_inner_with_config(&config).unwrap();
        assert_eq!(fallback_ok.value_metrics, initial.value_metrics);
        let metric = fallback_ok
            .value_metrics
            .iter()
            .find(|metric| metric.id == "claudeRateLimitResets")
            .unwrap();
        assert_eq!(metric.values[0].number, 2.0);
        assert!(metric.reset_credits.iter().all(|credit| {
            credit.claim_account.as_deref() == Some(expected_owner.as_str()) && credit.redeemable
        }));

        let (fallback_mismatch, _) = provider.refresh_inner_with_config(&config).unwrap();
        let metric = fallback_mismatch
            .value_metrics
            .iter()
            .find(|metric| metric.id == "claudeRateLimitResets")
            .unwrap();
        assert_eq!(metric.values[0].number, 2.0);
        assert!(metric
            .reset_credits
            .iter()
            .all(|credit| credit.claim_account.is_none() && !credit.redeemable));
        assert!(fallback_mismatch
            .warnings
            .iter()
            .any(|warning| warning.contains("ownership")));

        let (revoked, _) = provider.refresh_inner_with_config(&config).unwrap();
        assert!(revoked
            .value_metrics
            .iter()
            .all(|metric| metric.id != "claudeRateLimitResets"));

        server.join().unwrap();
    }

    fn expired_login(account_root: &std::path::Path) -> std::path::PathBuf {
        fs::create_dir_all(account_root).unwrap();
        let path = account_root.join(".credentials.json");
        fs::write(
            &path,
            r#"{"mcpOAuth":{"server":{"accessToken":"mcp"}},"claudeAiOauth":{"accessToken":"old","refreshToken":"spent","expiresAt":1,"clientId":"code-client","subscriptionType":"max","scopes":["user:profile","user:inference"]}}"#,
        )
        .unwrap();
        path
    }

    /// One HTTP request: its head and body.
    fn read_request(stream: &mut impl Read) -> String {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let length = stream.read(&mut chunk).unwrap();
            bytes.extend_from_slice(&chunk[..length]);
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let expected = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if body.len() >= expected || length == 0 {
                    return text;
                }
            }
            if length == 0 {
                return text;
            }
        }
    }

    fn unreachable_config() -> ClaudeOAuthConfig {
        // Nothing listens here: any request fails as a connection error.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test-client".into(),
        }
    }

    #[test]
    fn refresh_waits_while_claude_code_renews_and_never_spends_its_token() {
        let directory = tempdir().unwrap();
        let account_root = directory.path().join("account");
        let credentials = expired_login(&account_root);
        let before = fs::read(&credentials).unwrap();
        let locks = super::refresh_lock::lock_paths(&account_root);
        for lock in &locks {
            fs::create_dir(lock).unwrap();
        }
        let provider = isolated_provider(directory.path(), &account_root);

        let error = provider
            .refresh_inner_with_config(&unreachable_config())
            .unwrap_err();

        // Nothing was sent: the refresh token is Claude Code's to spend.
        assert!(matches!(error, ClaudeError::ClaudeCodeRenewing), "{error}");
        assert_eq!(fs::read(&credentials).unwrap(), before);
        assert!(locks.iter().all(|lock| lock.is_dir()));
    }

    #[test]
    fn refresh_adopts_the_renewal_claude_code_finished_while_it_waited() {
        let directory = tempdir().unwrap();
        let account_root = directory.path().join("account");
        let credentials = expired_login(&account_root);
        let locks = super::refresh_lock::lock_paths(&account_root);
        for lock in &locks {
            fs::create_dir(lock).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            write_http_response(&mut stream, 25);
            request
        });
        let finish = {
            let (credentials, locks) = (credentials.clone(), locks.clone());
            thread::spawn(move || {
                thread::sleep(StdDuration::from_millis(150));
                fs::write(
                    &credentials,
                    credential_json("code-access", "code-refresh", "max"),
                )
                .unwrap();
                for lock in locks.iter().rev() {
                    fs::remove_dir(lock).unwrap();
                }
            })
        };
        let provider = isolated_provider(directory.path(), &account_root);
        let config = ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test-client".into(),
        };

        provider.refresh_inner_with_config(&config).unwrap();
        finish.join().unwrap();

        // The only request asks for usage with Claude Code's renewal.
        let request = server.join().unwrap();
        assert!(
            request.starts_with("GET /usage?cedar_ember=1 "),
            "{request}"
        );
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer code-access"));
        assert!(locks.iter().all(|lock| !lock.exists()));
    }

    #[cfg(unix)]
    #[test]
    fn refresh_takes_over_from_a_claude_code_that_died_holding_its_lock() {
        let directory = tempdir().unwrap();
        let account_root = directory.path().join("account");
        let credentials = expired_login(&account_root);
        // Exited mid-renewal: its locks are far from stale, but its owner record proves it gone.
        let locks = super::refresh_lock::hold_as_dead_claude_code(&account_root);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let refresh = read_request(&mut stream);
            let body =
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            write_http_response(&mut stream, 25);
            refresh
        });
        let provider = isolated_provider(directory.path(), &account_root);
        let config = ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test-client".into(),
        };

        provider.refresh_inner_with_config(&config).unwrap();

        assert!(server.join().unwrap().starts_with("POST /token "));
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&credentials).unwrap()).unwrap();
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "new-access");
        assert!(locks.iter().all(|lock| !lock.exists()));
        assert!(!account_root.join(super::refresh_lock::OWNER_FILE).exists());
    }

    #[test]
    fn renewal_is_saved_under_claude_codes_lock_before_usage_is_asked() {
        let directory = tempdir().unwrap();
        let account_root = directory.path().join("account");
        let credentials = expired_login(&account_root);
        let locks = super::refresh_lock::lock_paths(&account_root);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = {
            let (credentials, locks) = (credentials.clone(), locks.clone());
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let refresh = read_request(&mut stream);
                let locked_during_refresh = locks.iter().all(|lock| lock.is_dir());
                let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"refresh_token_expires_in":7200,"scope":"user:profile user:inference"}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                drop(stream);
                let (mut stream, _) = listener.accept().unwrap();
                let usage = read_request(&mut stream);
                let locked_during_usage = locks.iter().any(|lock| lock.exists());
                let saved_before_usage = fs::read_to_string(&credentials).unwrap();
                write_http_response(&mut stream, 25);
                (
                    refresh,
                    locked_during_refresh,
                    usage,
                    locked_during_usage,
                    saved_before_usage,
                )
            })
        };
        let provider = isolated_provider(directory.path(), &account_root);
        let config = ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test-client".into(),
        };

        provider.refresh_inner_with_config(&config).unwrap();

        let (refresh, locked_during_refresh, usage, locked_during_usage, saved_before_usage) =
            server.join().unwrap();
        assert!(refresh.starts_with("POST /token "), "{refresh}");
        assert!(refresh.contains("\"refresh_token\":\"spent\""));
        assert!(locked_during_refresh);
        assert!(usage.starts_with("GET /usage?cedar_ember=1 "), "{usage}");
        assert!(usage
            .to_ascii_lowercase()
            .contains("authorization: bearer new-access"));
        assert!(!locked_during_usage);
        assert!(saved_before_usage.contains("new-refresh"));
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&credentials).unwrap()).unwrap();
        let oauth = &saved["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], "new-access");
        assert_eq!(oauth["refreshToken"], "new-refresh");
        assert!(oauth["expiresAt"].is_i64());
        assert!(oauth["refreshTokenExpiresAt"].is_i64());
        assert_eq!(oauth["clientId"], "code-client");
        assert_eq!(oauth["subscriptionType"], "max");
        assert_eq!(saved["mcpOAuth"]["server"]["accessToken"], "mcp");
    }

    #[cfg(unix)]
    #[test]
    fn an_unsaved_renewal_is_reused_on_later_refreshes_instead_of_the_spent_token() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let account_root = directory.path().join("account");
        // The login is a link into a directory that cannot be written, as in a read-only
        // dotfiles checkout; Claude Code's lock directory stays writable.
        let target = expired_login(&directory.path().join("dotfiles"));
        fs::create_dir_all(&account_root).unwrap();
        std::os::unix::fs::symlink(&target, account_root.join(".credentials.json")).unwrap();
        let read_only = target.parent().unwrap().to_path_buf();
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o500)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("POST /token ") {
                    let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"scope":"user:profile user:inference"}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                } else {
                    write_http_response(&mut stream, 25);
                }
                requests.push(request);
            }
            requests
        });
        let provider = isolated_provider(directory.path(), &account_root);
        let config = ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test-client".into(),
        };

        let first = provider.refresh_inner_with_config(&config);
        let second = provider.refresh_inner_with_config(&config);
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o700)).unwrap();

        let (first, _) = first.unwrap();
        assert!(first
            .warnings
            .iter()
            .any(|warning| warning.contains("could not be saved")));
        second.unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /token "), "{}", requests[0]);
        for usage in &requests[1..] {
            assert!(usage.starts_with("GET /usage?cedar_ember=1 "), "{usage}");
            assert!(usage
                .to_ascii_lowercase()
                .contains("authorization: bearer new-access"));
        }
        assert!(fs::read_to_string(&target).unwrap().contains("\"spent\""));
    }
}
