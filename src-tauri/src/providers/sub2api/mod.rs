mod client;
mod config;
mod reported;
mod subscription;
mod template;
mod upstream_quota;

#[cfg(test)]
mod tests;

pub use config::{
    claude_provider_base_url, codex_provider_base_url, Sub2ApiConfigInput, Sub2ApiConfigOutcome,
    Sub2ApiConfigSaveOutcome, Sub2ApiConfigState, Sub2ApiMode, Sub2ApiUpstream,
};
pub(crate) use subscription::restore_subscription_windows;
#[cfg(test)]
pub(crate) use template::test_definition;
pub use template::{metric_template, metric_template_for_track, track_provider_id};
pub(crate) use template::{subscription_percentage_metric, track_shows_metric};

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use chrono::Utc;
use reqwest::StatusCode;
use zeroize::Zeroizing;

use crate::models::{ProviderDefinition, ProviderSnapshot, UsageHistories, UsageHistory};
use crate::storage::Storage;

use client::{
    classify_status, clone_session, decode_response, token_expiry, transport_error, CachedSession,
    Sub2ApiAccount, Sub2ApiClient, Sub2ApiError,
};
#[cfg(test)]
use client::{normalize_base_url, AccountPage, Envelope};
#[cfg(test)]
use config::normalized_codex_provider_base_url_text;
use config::{
    connection_target, credential_for_save, default_display_name_for_slot,
    parse_configuration_marker, resolve_claude_provider_base_url, resolve_codex_provider_base_url,
    session_scope, StoredConfig, StoredConnections, ADMIN_CACHE_KEY, METRIC_TEMPLATE_VERSION,
    PROVIDER_ID, SUBSCRIPTION_CACHE_KEY,
};
use reported::AccountStats;
use template::{definition_for, normalize_refreshed_quotas, should_apply_metric_template};

#[cfg(test)]
use super::claude::config::ClaudeConfigError;
use super::{
    claude::map_sub2api_usage,
    codex::{client::UsageResponse, mapper::map_usage},
    credential_store::{delete_owned_password, read_owned_password, write_owned_password},
    CacheIdentity, ProviderError, UsageProvider,
};

const PROVIDER_SLOTS: usize = 8;
const CREDENTIAL_SERVICE: &str = "io.github.deviffyy.openquota.sub2api";

pub struct Sub2ApiProvider {
    provider_id: String,
    display_name: String,
    configured_marker: PathBuf,
    configured: AtomicBool,
    /// The active track id, mirrored in the marker so settings never wait on the credential store.
    track: Mutex<Option<&'static str>>,
    session: Mutex<Option<CachedSession>>,
    model_families: Mutex<Option<subscription::CachedModelFamilies>>,
}

impl Sub2ApiProvider {
    fn new(
        provider_id: String,
        display_name: String,
        configured_marker: PathBuf,
        configured: bool,
    ) -> Self {
        let track = configured
            .then(|| fs::read_to_string(&configured_marker).ok())
            .flatten()
            .and_then(|contents| parse_configuration_marker(&contents).1);
        Self {
            provider_id,
            display_name,
            configured_marker,
            configured: AtomicBool::new(configured),
            track: Mutex::new(track),
            session: Mutex::new(None),
            model_families: Mutex::new(None),
        }
    }

    pub fn config_state(&self) -> Result<Sub2ApiConfigState, ProviderError> {
        Ok(self
            .load_connections()?
            .map(|connections| connections.state())
            .unwrap_or_default())
    }

    fn cache_path(&self) -> PathBuf {
        self.configured_marker.with_extension("usage.sqlite3")
    }

    pub fn save_config(
        &self,
        input: Sub2ApiConfigInput,
    ) -> Result<Sub2ApiConfigSaveOutcome, ProviderError> {
        let previous = self.load_connections()?;
        let previous_track = previous
            .as_ref()
            .map(|saved| (saved.active.mode, saved.active.upstream));
        let apply_metric_template = should_apply_metric_template(
            previous.as_ref().map(|saved| &saved.active),
            input.mode,
            input.upstream,
        );
        let (base_url, codex_provider, custom_base_url) = connection_target(
            &input,
            resolve_codex_provider_base_url,
            resolve_claude_provider_base_url,
        )?;
        let secret = credential_for_save(
            &input,
            &base_url,
            previous
                .as_ref()
                .and_then(|saved| saved.for_mode(input.mode)),
        )?;
        let (email, password, api_key) = match input.mode {
            Sub2ApiMode::Admin => (input.email.trim().to_owned(), secret, String::new()),
            Sub2ApiMode::Subscription => (String::new(), String::new(), secret),
        };
        let config = StoredConfig {
            provider_source: input.provider_source,
            mode: input.mode,
            api_key,
            base_url,
            codex_provider,
            custom_base_url,
            email,
            password,
            upstream: input.upstream,
            metric_template_version: if apply_metric_template {
                0
            } else {
                METRIC_TEMPLATE_VERSION
            },
        };

        if config.mode == Sub2ApiMode::Subscription && config.api_key.is_empty() {
            // Validate locally, then resolve again on refresh to follow key rotations.
            config.subscription_key()?;
        }
        let connections = StoredConnections::with_active(config, previous);
        self.write_connections(&connections)?;
        self.set_configuration_marker(Some(&connections.active))?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = None;
        }
        if let Ok(mut cached) = self.model_families.lock() {
            *cached = None;
        }
        Ok(Sub2ApiConfigSaveOutcome {
            state: connections.state(),
            apply_metric_template,
            previous_track,
        })
    }

    pub fn mark_metric_template_applied(&self) -> Result<(), ProviderError> {
        let mut config = self
            .load_connections()?
            .ok_or(Sub2ApiError::MissingConfig)?;
        config.active.metric_template_version = METRIC_TEMPLATE_VERSION;
        self.write_connections(&config).map_err(ProviderError::from)
    }

    pub fn delete_config(&self) -> Result<Sub2ApiConfigState, ProviderError> {
        match fs::remove_file(self.cache_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Sub2ApiError::HistoryCache.into()),
        }
        delete_owned_password(CREDENTIAL_SERVICE, self.credential_account())
            .map_err(|_| Sub2ApiError::CredentialStorage)?;
        self.set_configuration_marker(None)?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = None;
        }
        if let Ok(mut cached) = self.model_families.lock() {
            *cached = None;
        }
        Ok(Sub2ApiConfigState::default())
    }

    fn connect(&self, config: &StoredConfig) -> Result<CachedSession, Sub2ApiError> {
        let client = Sub2ApiClient::for_provider(&config.base_url, &self.provider_id)?;
        let login = client.login(&config.email, &config.password)?;
        Ok(CachedSession {
            scope: session_scope(config),
            token: login.value,
            expires_at: token_expiry(login.expires_in),
        })
    }

    fn session(&self, config: &StoredConfig) -> Result<CachedSession, Sub2ApiError> {
        let scope = session_scope(config);
        if let Some(session) = self.session.lock().ok().and_then(|cached| {
            cached
                .as_ref()
                .filter(|cached| cached.scope == scope && cached.expires_at > Instant::now())
                .map(clone_session)
        }) {
            return Ok(session);
        }
        let session = self.connect(config)?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = Some(clone_session(&session));
        }
        Ok(session)
    }

    #[cfg(test)]
    fn refresh_snapshot(&self, config: &StoredConfig) -> Result<ProviderSnapshot, Sub2ApiError> {
        self.refresh_snapshot_with_preview(config, &|_| {})
    }

    /// Fetches every account's quota first and hands that to `preview`, then syncs usage
    /// summaries. The quota preview stays visible while server statistics are loading.
    fn refresh_snapshot_with_preview(
        &self,
        config: &StoredConfig,
        preview: &dyn Fn(ProviderSnapshot),
    ) -> Result<ProviderSnapshot, Sub2ApiError> {
        let client = Sub2ApiClient::for_provider(&config.base_url, &self.provider_id)?;
        if config.mode == Sub2ApiMode::Subscription {
            let key = config.subscription_key()?;
            let mut snapshot =
                client.subscription_snapshot(&key, self.empty_snapshot(Vec::new()))?;
            // Codex and Claude double as the configuration an address is read from, so their
            // mark comes from the key's model catalog. Antigravity and Grok are chosen only to
            // name the upstream, whose catalogs can mix model families.
            snapshot.upstream_provider_id = if config.upstream.resolves_provider_names() {
                client
                    .subscription_icon(&key, &self.model_families)
                    .map(str::to_owned)
            } else {
                Some(config.upstream.provider_id().to_owned())
            };
            return Ok(snapshot);
        }
        let mut session = self.session(config)?;
        let accounts = self.with_session(config, &mut session, |token| {
            client.accounts(token, config.upstream)
        })?;
        let now = Utc::now();
        let mut snapshots = Vec::with_capacity(accounts.len());
        for account in &accounts {
            let snapshot = self.with_session(config, &mut session, |token| {
                self.account_quota(config, &client, token, account, now)
            })?;
            snapshots.push(crate::models::AccountSnapshot {
                id: account.id.to_string(),
                name: format!("Sub2API · {} · {}", config.upstream, account.name),
                snapshot,
            });
        }
        preview(self.connection_snapshot(snapshots.clone(), UsageHistory::default()));

        let mut total = AccountStats::default();
        for (account, snapshot) in accounts.iter().zip(&mut snapshots) {
            let stats = self.with_session(config, &mut session, |token| {
                client.stats(token, account.id)
            })?;
            total.history.extend(stats.history.iter().cloned());
            total.models.extend(stats.models.iter().cloned());
            snapshot.snapshot.usage_histories =
                UsageHistories::account(stats.into_standard_history(now));
        }
        Ok(self.connection_snapshot(snapshots, total.into_standard_history(now)))
    }

    /// Runs one administrator request, logging in again once if the cached token expired.
    fn with_session<T>(
        &self,
        config: &StoredConfig,
        session: &mut CachedSession,
        mut request: impl FnMut(&str) -> Result<T, Sub2ApiError>,
    ) -> Result<T, Sub2ApiError> {
        match request(session.token.as_str()) {
            Err(Sub2ApiError::Authentication) => {
                *session = self.connect(config)?;
                if let Ok(mut cached) = self.session.lock() {
                    *cached = Some(clone_session(session));
                }
                request(session.token.as_str())
            }
            result => result,
        }
    }

    /// The connection-level snapshot mirrors the first account and lists all of them.
    fn connection_snapshot(
        &self,
        accounts: Vec<crate::models::AccountSnapshot>,
        total: UsageHistory,
    ) -> ProviderSnapshot {
        let mut snapshot = accounts
            .first()
            .map(|account| account.snapshot.clone())
            .unwrap_or_else(|| self.empty_snapshot(Vec::new()));
        snapshot.usage_histories = UsageHistories::account(total);
        snapshot.accounts = Some(accounts);
        snapshot
    }

    fn empty_snapshot(&self, warnings: Vec<String>) -> ProviderSnapshot {
        ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: self.provider_id.clone(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories::account(UsageHistory::default()),
            warnings,
            refreshed_at: Utc::now(),
        }
    }

    fn account_quota(
        &self,
        config: &StoredConfig,
        client: &Sub2ApiClient,
        token: &str,
        account: &Sub2ApiAccount,
        now: chrono::DateTime<Utc>,
    ) -> Result<ProviderSnapshot, Sub2ApiError> {
        let body = client.usage(token, account.id, config.upstream)?;
        let mut warnings = Vec::new();
        let (plan, quotas, value_metrics) = match config.upstream {
            Sub2ApiUpstream::Codex => {
                let response = UsageResponse {
                    status: StatusCode::OK,
                    headers: HashMap::new(),
                    body,
                };
                let mut mapped =
                    map_usage(&response, None, now).map_err(|_| Sub2ApiError::InvalidResponse)?;
                mapped.value_metrics.retain_mut(|metric| {
                    if metric.id != "rateLimitResets" {
                        return false;
                    }
                    metric.id = "sub2apiRateLimitResets".into();
                    metric.expiries_at.clear();
                    true
                });
                (mapped.plan, mapped.quotas, mapped.value_metrics)
            }
            Sub2ApiUpstream::Claude => {
                let mapped = map_sub2api_usage(&body).map_err(|_| Sub2ApiError::InvalidResponse)?;
                (mapped.plan, mapped.quotas, mapped.value_metrics)
            }
            Sub2ApiUpstream::Antigravity | Sub2ApiUpstream::Grok => {
                let mapped = if config.upstream == Sub2ApiUpstream::Antigravity {
                    upstream_quota::map_antigravity(&body, now)
                } else {
                    upstream_quota::map_grok(&body, now)
                };
                warnings = mapped.warnings;
                (mapped.plan, mapped.quotas, Vec::new())
            }
        };
        let quotas = normalize_refreshed_quotas(config.upstream, quotas, now);
        Ok(ProviderSnapshot {
            upstream_provider_id: Some(config.upstream.provider_id().into()),
            accounts: None,
            provider_id: self.provider_id.clone(),
            plan,
            quotas,
            value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories::account(UsageHistory::default()),
            warnings,
            refreshed_at: now,
        })
    }

    fn credential_account(&self) -> &str {
        if self.provider_id == PROVIDER_ID {
            "connection"
        } else {
            &self.provider_id
        }
    }

    fn set_configuration_marker(&self, config: Option<&StoredConfig>) -> Result<(), Sub2ApiError> {
        let track = config.map(|config| track_provider_id(config.mode, config.upstream));
        // The track lock orders the marker, the cached track, and the configured flag against a
        // refresh catching up in `remember_track`.
        let mut cached = self
            .track
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(config) = config {
            self.write_configuration_marker(config.cache_key(), track)?;
        } else {
            match fs::remove_file(&self.configured_marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(Sub2ApiError::CredentialStorage),
            }
        }
        *cached = track;
        self.configured.store(config.is_some(), Ordering::SeqCst);
        Ok(())
    }

    fn write_configuration_marker(
        &self,
        cache_key: &str,
        track: Option<&str>,
    ) -> Result<(), Sub2ApiError> {
        let parent = self
            .configured_marker
            .parent()
            .ok_or(Sub2ApiError::CredentialStorage)?;
        fs::create_dir_all(parent).map_err(|_| Sub2ApiError::CredentialStorage)?;
        let contents = match track {
            Some(track) => format!("{cache_key}\n{track}"),
            None => cache_key.to_owned(),
        };
        fs::write(&self.configured_marker, contents).map_err(|_| Sub2ApiError::CredentialStorage)
    }

    /// Records the track of a configuration read from the credential store, so markers written
    /// before tracks were recorded catch up on the first read instead of staying unknown.
    ///
    /// Only an unknown track is filled in: a refresh may have read the configuration just before
    /// a save or removal, and must not restore the old track or the removed marker.
    fn remember_track(&self, config: &StoredConfig) {
        let Ok(mut cached) = self.track.lock() else {
            return;
        };
        if cached.is_some() || !self.configured.load(Ordering::SeqCst) {
            return;
        }
        let track = track_provider_id(config.mode, config.upstream);
        if self
            .write_configuration_marker(config.cache_key(), Some(track))
            .is_ok()
        {
            *cached = Some(track);
        }
    }

    fn load_config(&self) -> Result<Option<StoredConfig>, Sub2ApiError> {
        Ok(self.load_connections()?.map(|saved| saved.active))
    }

    fn load_connections(&self) -> Result<Option<StoredConnections>, Sub2ApiError> {
        let Some(bytes) = read_owned_password(CREDENTIAL_SERVICE, self.credential_account())
            .map_err(|_| Sub2ApiError::CredentialStorage)?
        else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        let connections: StoredConnections =
            serde_json::from_slice(&bytes).map_err(|_| Sub2ApiError::CredentialStorage)?;
        self.remember_track(&connections.active);
        Ok(Some(connections))
    }

    fn write_connections(&self, config: &StoredConnections) -> Result<(), Sub2ApiError> {
        let bytes = Zeroizing::new(
            serde_json::to_vec(config).map_err(|_| Sub2ApiError::CredentialStorage)?,
        );
        write_owned_password(
            CREDENTIAL_SERVICE,
            self.credential_account(),
            bytes.as_slice(),
        )
        .map_err(|_| Sub2ApiError::CredentialStorage)
    }
}

pub struct Sub2ApiProviders {
    providers: HashMap<String, Arc<Sub2ApiProvider>>,
}

impl Sub2ApiProviders {
    pub fn new(marker_directory: PathBuf, storage: Arc<Storage>) -> Self {
        let migration_marker = marker_directory.join(".initialized");
        let migrate_snapshots = !migration_marker.is_file();
        let providers = (0..PROVIDER_SLOTS)
            .map(|index| {
                let number = index + 1;
                let provider_id = if index == 0 {
                    PROVIDER_ID.to_owned()
                } else {
                    format!("{PROVIDER_ID}@{number}")
                };
                let display_name = default_display_name_for_slot(&provider_id);
                let configured_marker = marker_directory.join(format!("{provider_id}.configured"));
                let configured = configured_marker.is_file()
                    || (migrate_snapshots
                        && storage
                            .load_snapshot_for_identity(
                                &provider_id,
                                crate::providers::CacheIdentity::Unscoped,
                            )
                            .is_ok_and(|snapshot| snapshot.is_some()));
                if configured && !configured_marker.is_file() {
                    let _ = fs::create_dir_all(&marker_directory);
                    let _ = fs::write(&configured_marker, b"configured");
                }
                let provider = Arc::new(Sub2ApiProvider::new(
                    provider_id.clone(),
                    display_name,
                    configured_marker,
                    configured,
                ));
                (provider_id, provider)
            })
            .collect();
        if migrate_snapshots {
            let _ = fs::create_dir_all(&marker_directory);
            let _ = fs::write(migration_marker, b"initialized");
        }
        Self { providers }
    }

    pub fn runtimes(&self) -> Vec<Arc<dyn UsageProvider>> {
        let mut providers = self.providers.values().cloned().collect::<Vec<_>>();
        providers.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
        providers
            .into_iter()
            .map(|provider| provider as Arc<dyn UsageProvider>)
            .collect()
    }

    pub fn provider(&self, provider_id: &str) -> Option<Arc<Sub2ApiProvider>> {
        self.providers.get(provider_id).cloned()
    }
}

impl UsageProvider for Sub2ApiProvider {
    fn definition(&self) -> ProviderDefinition {
        definition_for(&self.provider_id, &self.display_name)
    }

    fn has_local_credentials(&self) -> bool {
        self.configured.load(Ordering::SeqCst)
    }

    fn supports_account_names(&self) -> bool {
        self.configured.load(Ordering::SeqCst)
    }

    fn account_identity(&self) -> Option<&str> {
        self.configured
            .load(Ordering::SeqCst)
            .then_some(self.provider_id.as_str())
    }

    fn display_name(&self) -> String {
        self.display_name.clone()
    }

    fn cache_identity(&self) -> CacheIdentity<'_> {
        if !self.configured.load(Ordering::SeqCst) {
            return CacheIdentity::Resolved("sub2api-unconfigured");
        }
        let contents = fs::read_to_string(&self.configured_marker).unwrap_or_default();
        let key = match parse_configuration_marker(&contents).0 {
            SUBSCRIPTION_CACHE_KEY => SUBSCRIPTION_CACHE_KEY,
            _ => ADMIN_CACHE_KEY,
        };
        CacheIdentity::Resolved(key)
    }

    fn supports_connection_configuration(&self) -> bool {
        true
    }

    /// Reads the cached track only: settings normalize under their write lock, often on an async
    /// worker, so this must not reach the credential store.
    fn upstream_provider_id(&self) -> Option<&'static str> {
        if !self.configured.load(Ordering::SeqCst) {
            return None;
        }
        self.track.lock().ok().and_then(|cached| *cached)
    }

    fn refresh_for_service(&self) -> Result<super::ProviderRefresh, ProviderError> {
        self.refresh_for_service_with_preview(&|_| {})
    }

    fn refresh_for_service_with_preview(
        &self,
        preview: &dyn Fn(super::ProviderRefresh),
    ) -> Result<super::ProviderRefresh, ProviderError> {
        let config = self.load_config()?.ok_or(Sub2ApiError::MissingConfig)?;
        let cache_identity = config.cache_key();
        let snapshot = self.refresh_snapshot_with_preview(&config, &|snapshot| {
            preview(super::ProviderRefresh {
                snapshot,
                cache_identity: Some(cache_identity.to_owned()),
                account: None,
            })
        })?;
        Ok(super::ProviderRefresh {
            snapshot,
            cache_identity: Some(cache_identity.to_owned()),
            account: None,
        })
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        Ok(self.refresh_for_service()?.snapshot)
    }
}
