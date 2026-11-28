pub mod account_usage;
pub mod auth;
pub mod client;
pub mod config;
pub mod local_usage;
pub mod mapper;
pub mod reset_claim;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Utc;
use reqwest::StatusCode;
use thiserror::Error;

use crate::{
    hashing::sha256_hex,
    models::{
        MetricDefinition, MetricSection, ProviderDefinition, ProviderLink, ProviderSnapshot,
        UsageHistories, UsageHistory, UsagePeriodSelection,
    },
    pricing::PricingStore,
    storage::Storage,
};

use self::{
    account_usage::{classify_account_usage, AccountUsageOutcome},
    auth::CodexAuthState,
    client::{CodexClient, UsageResponse},
    local_usage::scan_local_usage,
    mapper::{idle_session_candidates, map_usage, moving_idle_session, StartOnUseWindow},
};

const IDLE_SESSION_CONFIRMATION_DELAY: Duration = Duration::from_millis(1100);

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "codex".into(),
        display_name: "Codex".into(),
        short_name: "Cx".into(),
        fallback_enabled: true,
        local_usage_source_note: Some("From your Codex logs (estimated)".into()),
        links: vec![
            ProviderLink::new("Status", "https://status.openai.com/"),
            ProviderLink::new("Dashboard", "https://chatgpt.com/codex/settings/usage"),
        ],
        metrics: vec![
            MetricDefinition::quota(
                "codex.session",
                "Session",
                "session",
                true,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::quota(
                "codex.weekly",
                "Weekly",
                "weekly",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "W",
            ),
            MetricDefinition::quota(
                "codex.spark",
                "Spark",
                "spark",
                true,
                true,
                MetricSection::OnDemand,
                false,
                "Sp",
            ),
            MetricDefinition::quota(
                "codex.sparkWeekly",
                "Spark Weekly",
                "sparkWeekly",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "SW",
            ),
            MetricDefinition::trend("codex.trend"),
            MetricDefinition::value(
                "codex.credits",
                "Extra Usage",
                "credits",
                true,
                MetricSection::OnDemand,
                false,
                "E",
                None,
            ),
            MetricDefinition::value(
                "codex.rateLimitResets",
                "Rate Limit Resets",
                "rateLimitResets",
                true,
                MetricSection::OnDemand,
                false,
                "R",
                Some("resets"),
            ),
            MetricDefinition::usage(
                "codex.today",
                "Today",
                UsagePeriodSelection::Today,
                MetricSection::OnDemand,
                "T",
            ),
            MetricDefinition::usage(
                "codex.yesterday",
                "Yesterday",
                UsagePeriodSelection::Yesterday,
                MetricSection::OnDemand,
                "Y",
            ),
            MetricDefinition::usage(
                "codex.last30",
                "Last 30 Days",
                UsagePeriodSelection::Last30Days,
                MetricSection::OnDemand,
                "M",
            ),
        ],
    }
}

#[derive(Debug, Error)]
pub enum CodexError {
    #[error("Not logged in. Run `codex` to authenticate.")]
    NotLoggedIn,
    #[error(
        "Subscription usage is unavailable for API-key-only logins. Sign in to Codex with ChatGPT."
    )]
    ApiKeyOnly,
    #[error("Your Codex session expired. Run `codex` to sign in again.")]
    SessionExpired,
    #[error("Codex credentials changed while refreshing. Run `codex` to sign in again.")]
    TokenConflict,
    #[error("Your Codex session was revoked. Run `codex` to sign in again.")]
    TokenRevoked,
    #[error("Your Codex access token expired. Run `codex` to sign in again.")]
    TokenExpired,
    #[error("Codex auth data is invalid. Run `codex` to sign in again.")]
    InvalidAuth,
    #[error("The Codex account changed while usage was refreshing. Refresh again.")]
    AccountChanged,
    #[error("Refreshed Codex credentials could not be saved.")]
    AuthWrite,
    #[error("Codex usage request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("Codex returned an invalid usage response.")]
    InvalidResponse,
    #[error("Could not connect to Codex. Check your internet connection.")]
    ConnectionFailed,
    #[error("Local Codex usage logs could not be processed.")]
    LocalUsage,
    #[error("OpenQuota cache is unavailable.")]
    Storage,
}

impl From<crate::providers::local_usage::home_clients::FoldInUnavailable> for CodexError {
    fn from(_: crate::providers::local_usage::home_clients::FoldInUnavailable) -> Self {
        Self::LocalUsage
    }
}

impl From<crate::storage::StorageError> for CodexError {
    fn from(_: crate::storage::StorageError) -> Self {
        Self::Storage
    }
}

pub struct CodexProvider {
    account_identity: Option<String>,
    storage: Arc<Storage>,
    pricing: Arc<PricingStore>,
    client: CodexClient,
    /// Logins carrying a renewal that could not be saved. Their refresh tokens were spent, so
    /// each is used in place of its saved login while that is still the one it was made from.
    unsaved_renewals: Mutex<Vec<CodexAuthState>>,
}

impl CodexProvider {
    pub fn new(storage: Arc<Storage>, pricing: Arc<PricingStore>) -> Result<Self, CodexError> {
        let account_identity = CodexAuthState::observed_account_identity()
            .map(|identity| account_identity_key(&identity));
        if let Some(identity) = account_identity.as_deref() {
            crate::providers::remember_default_account(&storage, "codex", identity)?;
        }
        Ok(Self {
            account_identity,
            storage,
            pricing,
            client: CodexClient::new()?,
            unsaved_renewals: Mutex::new(Vec::new()),
        })
    }

    pub fn refresh(&self) -> Result<ProviderSnapshot, CodexError> {
        self.refresh_with_identity().map(|(snapshot, _)| snapshot)
    }

    fn refresh_with_identity(&self) -> Result<(ProviderSnapshot, Option<String>), CodexError> {
        let now = Utc::now();
        let candidates = self.resume_unsaved_renewals(CodexAuthState::load_candidates()?);
        crate::app_debug!(
            "auth:codex",
            "credential candidates loaded ({})",
            candidates.len()
        );
        let mut last_auth_error = None;
        for mut auth in candidates {
            let identity = auth
                .account_identity()
                .map(|identity| account_identity_key(&identity));
            match self.refresh_candidate(&mut auth, now, identity.as_deref()) {
                Ok(snapshot) => return Ok((snapshot, identity)),
                Err(
                    error @ (CodexError::SessionExpired
                    | CodexError::TokenConflict
                    | CodexError::TokenRevoked
                    | CodexError::TokenExpired),
                ) => last_auth_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_auth_error.unwrap_or(CodexError::NotLoggedIn))
    }

    fn ensure_candidate_identity(
        auth: &CodexAuthState,
        expected: Option<&str>,
    ) -> Result<(), CodexError> {
        let observed = auth
            .account_identity()
            .map(|identity| account_identity_key(&identity));
        validate_account_identity(expected, observed.as_deref())
    }

    fn ensure_candidate_source_current(
        auth: &CodexAuthState,
        expected: Option<&str>,
    ) -> Result<(), CodexError> {
        let current = auth.reload().map_err(|_| CodexError::AccountChanged)?;
        Self::ensure_candidate_identity(&current, expected)
    }

    fn refresh_candidate(
        &self,
        auth: &mut CodexAuthState,
        now: chrono::DateTime<Utc>,
        account_identity: Option<&str>,
    ) -> Result<ProviderSnapshot, CodexError> {
        let mut warnings = Vec::new();

        Self::ensure_candidate_identity(auth, account_identity)?;

        if auth.needs_refresh(now) {
            if let Ok(live) = auth.reload() {
                Self::ensure_candidate_identity(&live, account_identity)?;
                // An unchanged saved login is no newer than an unsaved renewal of it.
                if live.document != *auth.saved_document() {
                    *auth = live;
                }
            }
        }
        if auth.needs_refresh(now) {
            self.refresh_access_token(auth, now, &mut warnings)?;
            Self::ensure_candidate_identity(auth, account_identity)?;
        }

        let mut response = self
            .client
            .fetch_usage(&auth.access_token, auth.account_id.as_deref())?;
        if should_refresh_access_token(response.status) {
            self.refresh_access_token(auth, now, &mut warnings)?;
            Self::ensure_candidate_identity(auth, account_identity)?;
            response = self
                .client
                .fetch_usage(&auth.access_token, auth.account_id.as_deref())?;
        }
        let idle_windows = self.confirm_idle_windows(auth, &mut response);
        let reset_credits = if response.status.is_success() {
            self.client
                .fetch_reset_credits(&auth.access_token, auth.account_id.as_deref())
                .ok()
        } else {
            None
        };
        let mut mapped = map_usage(&response, reset_credits.as_ref(), now)?;
        // A rolling placeholder is not an actual reset. Keep the quota but omit its
        // timer so an unused window does not look started.
        for quota in &mut mapped.quotas {
            if idle_windows.iter().any(|kind| kind.quota_id() == quota.id) {
                quota.resets_at = None;
            }
        }
        let account_usage = self.fetch_account_usage(auth, now, account_identity, &mut warnings);
        Self::ensure_candidate_source_current(auth, account_identity)?;
        Ok(ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: "codex".into(),
            plan: mapped.plan,
            quotas: mapped.quotas,
            value_metrics: mapped.value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories {
                local_device_status: None,
                local_device: None,
                account: account_usage,
            },
            warnings,
            refreshed_at: now,
        })
    }

    /// Confirms possibly idle five-hour windows with a second reading on the same
    /// endpoint, returning the windows whose reset is only a moving placeholder. A failed
    /// confirmation keeps the first successful reading.
    fn confirm_idle_windows(
        &self,
        auth: &CodexAuthState,
        response: &mut UsageResponse,
    ) -> Vec<StartOnUseWindow> {
        let first_received_at = Utc::now();
        let candidates = idle_session_candidates(response, first_received_at);
        if candidates.is_empty() {
            return Vec::new();
        }
        // reset_at has whole-second precision. Cross a second boundary so the
        // confirmation can distinguish a moving placeholder from a fixed deadline.
        std::thread::sleep(IDLE_SESSION_CONFIRMATION_DELAY);
        let Ok(latest) = self
            .client
            .confirm_usage(&auth.access_token, auth.account_id.as_deref())
        else {
            return Vec::new();
        };
        let latest_received_at = Utc::now();
        if !latest.status.is_success() || !latest.body.is_object() {
            return Vec::new();
        }
        let idle = candidates
            .into_iter()
            .filter(|kind| {
                moving_idle_session(
                    *kind,
                    response,
                    first_received_at,
                    &latest,
                    latest_received_at,
                )
            })
            .collect();
        *response = latest;
        idle
    }

    fn fetch_account_usage(
        &self,
        auth: &CodexAuthState,
        now: chrono::DateTime<Utc>,
        account_identity: Option<&str>,
        warnings: &mut Vec<String>,
    ) -> Option<UsageHistory> {
        let outcome = self
            .client
            .fetch_profile(&auth.access_token, auth.account_id.as_deref())
            .map(|response| classify_account_usage(&response, now))
            .unwrap_or(AccountUsageOutcome::Failed);
        resolve_account_usage(&self.storage, account_identity, outcome, warnings)
    }

    /// Replaces each saved login an unsaved renewal was made from with that renewal, and
    /// forgets renewals whose saved login has since changed.
    fn resume_unsaved_renewals(&self, mut candidates: Vec<CodexAuthState>) -> Vec<CodexAuthState> {
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

    /// Keeps the renewal `auth` carries while its saved login could not take it.
    fn remember_unsaved_renewal(&self, auth: &CodexAuthState) {
        let Ok(mut renewals) = self.unsaved_renewals.lock() else {
            return;
        };
        renewals.retain(|renewal| !renewal.has_source_of(auth));
        if auth.has_unsaved_renewal() {
            renewals.push(auth.clone());
        }
    }

    fn refresh_access_token(
        &self,
        auth: &mut CodexAuthState,
        now: chrono::DateTime<Utc>,
        warnings: &mut Vec<String>,
    ) -> Result<(), CodexError> {
        let refresh_token = auth
            .refresh_token
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or(CodexError::TokenExpired)?;
        let refreshed = self.client.refresh_token(refresh_token)?;
        if let Err(error) = auth.update_and_save_if_current(
            refreshed.access_token,
            refreshed.refresh_token,
            refreshed.id_token,
            now,
        ) {
            if matches!(error, CodexError::AccountChanged) {
                return Err(error);
            }
            crate::app_error!(
                "auth:codex",
                "failed to persist rotated credentials; using them for this session only"
            );
            warnings.push(
                "The refreshed Codex login is active for this session but could not be saved."
                    .into(),
            );
        }
        self.remember_unsaved_renewal(auth);
        Ok(())
    }
}

fn should_refresh_access_token(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED
}

fn resolve_account_usage(
    storage: &Storage,
    account_identity: Option<&str>,
    outcome: AccountUsageOutcome,
    warnings: &mut Vec<String>,
) -> Option<UsageHistory> {
    match outcome {
        AccountUsageOutcome::Available(history) => Some(*history),
        AccountUsageOutcome::Unavailable => {
            crate::app_debug!(
                "http",
                "codex profile contract unavailable; using local history only"
            );
            None
        }
        AccountUsageOutcome::Failed => {
            crate::app_warn!(
                "http",
                "codex account usage history could not be refreshed; using cached history when available"
            );
            warnings.push(
                "Codex account usage history could not be refreshed; cached history is shown when available."
                    .into(),
            );
            let identity = account_identity
                .map(crate::providers::CacheIdentity::Resolved)
                .unwrap_or(crate::providers::CacheIdentity::Unresolved);
            storage
                .load_snapshot_for_identity("codex", identity)
                .ok()
                .flatten()
                .and_then(|snapshot| snapshot.usage_histories.account)
        }
    }
}

fn account_identity_key(identity: &str) -> String {
    sha256_hex(identity.as_bytes())
}

fn validate_account_identity(
    expected: Option<&str>,
    observed: Option<&str>,
) -> Result<(), CodexError> {
    (expected == observed)
        .then_some(())
        .ok_or(CodexError::AccountChanged)
}

fn provider_error(error: CodexError) -> crate::providers::ProviderError {
    use crate::models::ProviderErrorKind as Kind;

    let kind = match error {
        CodexError::NotLoggedIn
        | CodexError::SessionExpired
        | CodexError::TokenConflict
        | CodexError::TokenRevoked
        | CodexError::TokenExpired
        | CodexError::InvalidAuth
        | CodexError::AccountChanged => Kind::Authentication,
        CodexError::ApiKeyOnly => Kind::Permission,
        CodexError::AuthWrite => Kind::CredentialStorage,
        CodexError::RequestFailed(429) => Kind::RateLimited,
        CodexError::RequestFailed(_) | CodexError::ConnectionFailed => Kind::Network,
        CodexError::InvalidResponse => Kind::InvalidResponse,
        CodexError::LocalUsage => Kind::LocalData,
        CodexError::Storage => Kind::Storage,
    };
    crate::providers::ProviderError::from_display(kind, error)
}

impl crate::providers::UsageProvider for CodexProvider {
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
            scan_local_usage(&self.storage, Utc::now(), &self.pricing.current())
                .map_err(provider_error),
        )
    }

    fn has_local_credentials(&self) -> bool {
        CodexAuthState::has_local_credentials()
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
        CodexProvider::refresh(self).map_err(provider_error)
    }

    fn refresh_for_service(
        &self,
    ) -> Result<crate::providers::ProviderRefresh, crate::providers::ProviderError> {
        let (snapshot, identity) = self.refresh_with_identity().map_err(provider_error)?;
        Ok(crate::providers::ProviderRefresh {
            snapshot,
            cache_identity: identity.clone(),
            account: identity.map(|identity| crate::providers::AccountRefresh {
                family: "codex",
                provider_id: "codex",
                identity,
            }),
        })
    }
}

#[cfg(test)]
mod account_tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use super::{
        resolve_account_usage, should_refresh_access_token, validate_account_identity,
        AccountUsageOutcome, CodexClient, CodexError, CodexProvider,
    };
    use crate::{
        models::{ProviderSnapshot, UsageHistories, UsageHistory, UsagePeriod},
        pricing::PricingStore,
        providers::{CacheIdentity, UsageProvider},
        storage::Storage,
    };

    #[test]
    fn pinned_account_rejects_a_different_or_unreadable_login() {
        assert!(validate_account_identity(Some("account-a"), Some("account-a")).is_ok());
        assert!(matches!(
            validate_account_identity(Some("account-a"), Some("account-b")),
            Err(CodexError::AccountChanged)
        ));
        assert!(matches!(
            validate_account_identity(Some("account-a"), None),
            Err(CodexError::AccountChanged)
        ));
        assert!(matches!(
            validate_account_identity(None, Some("account-b")),
            Err(CodexError::AccountChanged)
        ));
        assert!(validate_account_identity(None, None).is_ok());
    }

    #[test]
    fn only_unauthorized_usage_responses_refresh_shared_credentials() {
        assert!(should_refresh_access_token(
            reqwest::StatusCode::UNAUTHORIZED
        ));
        assert!(!should_refresh_access_token(reqwest::StatusCode::FORBIDDEN));
    }

    #[test]
    fn cache_identity_tracks_the_launch_resolved_account() {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let pricing = Arc::new(PricingStore::new(directory.path().join("pricing")).unwrap());
        let provider = CodexProvider {
            account_identity: Some("account-a".into()),
            storage: storage.clone(),
            pricing: pricing.clone(),
            client: CodexClient::new().unwrap(),
            unsaved_renewals: Default::default(),
        };
        let unresolved = CodexProvider {
            account_identity: None,
            storage,
            pricing,
            client: CodexClient::new().unwrap(),
            unsaved_renewals: Default::default(),
        };

        assert_eq!(
            UsageProvider::cache_identity(&provider),
            CacheIdentity::Resolved("account-a")
        );
        assert_eq!(
            UsageProvider::cache_identity(&unresolved),
            CacheIdentity::Unresolved
        );
    }

    #[test]
    fn account_usage_fallback_distinguishes_contract_loss_from_access_failure() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        let cached = UsageHistory {
            last_30_days: Some(UsagePeriod {
                tokens: 123,
                estimated_cost_usd: None,
                cost_estimated: false,
                estimate_complete: true,
                model_breakdown: None,
                unknown_models: Vec::new(),
            }),
            ..UsageHistory::default()
        };
        let snapshot = ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: "codex".into(),
            plan: Some("Plus".into()),
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories {
                local_device_status: None,
                local_device: Some(UsageHistory::default()),
                account: Some(cached.clone()),
            },
            warnings: Vec::new(),
            refreshed_at: chrono::Utc::now(),
        };
        storage
            .save_snapshot_for_identity(&snapshot, Some("account-a"))
            .unwrap();

        let mut warnings = Vec::new();
        assert!(resolve_account_usage(
            &storage,
            Some("account-a"),
            AccountUsageOutcome::Unavailable,
            &mut warnings,
        )
        .is_none());
        assert!(warnings.is_empty());

        assert_eq!(
            resolve_account_usage(
                &storage,
                Some("account-a"),
                AccountUsageOutcome::Failed,
                &mut warnings,
            ),
            Some(cached)
        );
        assert_eq!(warnings.len(), 1);

        warnings.clear();
        assert!(resolve_account_usage(
            &storage,
            Some("account-b"),
            AccountUsageOutcome::Failed,
            &mut warnings,
        )
        .is_none());
        assert_eq!(warnings.len(), 1);
    }
}
