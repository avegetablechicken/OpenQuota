mod local_usage;
mod snapshot;

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use chrono::Utc;

use crate::{
    models::{ProviderErrorKind, ProviderViewState, SnapshotSource, UpdateFrequency},
    policy::{FAILURE_RETRY_BACKOFF, FAST_REFRESH_INTERVAL, REFRESH_INTERVAL},
    providers::{CacheIdentity, ProviderError, ProviderRefresh, ProviderRegistry},
    settings::SettingsService,
    storage::Storage,
};

use snapshot::{
    carry_usage_histories, merge_refresh_result, normalize_quota_availability,
    refresh_interval_for, update_staleness_from_snapshot_age, usage_data_changed,
    validate_snapshot,
};

// Cursor can make several bounded requests in sequence; allow its full healthy network budget
// before quarantining the synchronous provider worker.
const PROVIDER_REFRESH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageViewState {
    pub providers: BTreeMap<String, ProviderViewState>,
    pub last_full_refresh_at: Option<chrono::DateTime<Utc>>,
    pub refresh_interval_seconds: u64,
}

impl Default for UsageViewState {
    fn default() -> Self {
        Self {
            providers: BTreeMap::new(),
            last_full_refresh_at: None,
            refresh_interval_seconds: REFRESH_INTERVAL.as_secs(),
        }
    }
}

pub struct ProviderService {
    registry: Arc<ProviderRegistry>,
    storage: Arc<Storage>,
    states: RwLock<BTreeMap<String, ProviderViewState>>,
    refresh_flights: HashMap<String, Arc<RefreshFlight>>,
    last_live_refresh: Mutex<HashMap<String, Instant>>,
    last_failed_refresh: Mutex<HashMap<String, Instant>>,
    last_full_refresh_at: RwLock<Option<chrono::DateTime<Utc>>>,
    refresh_interval: RwLock<Duration>,
    unchanged_refreshes: Mutex<u8>,
    refresh_frequency_wakeup: tokio::sync::Notify,
    refresh_timeout: Duration,
    settings: Option<Arc<SettingsService>>,
    preview_revision: tokio::sync::watch::Sender<u64>,
}

impl ProviderService {
    #[cfg(test)]
    pub fn new(registry: Arc<ProviderRegistry>, storage: Arc<Storage>) -> Self {
        Self::with_refresh_timeout_and_settings(registry, storage, PROVIDER_REFRESH_TIMEOUT, None)
    }

    pub fn new_with_settings(
        registry: Arc<ProviderRegistry>,
        storage: Arc<Storage>,
        settings: Arc<SettingsService>,
    ) -> Self {
        Self::with_refresh_timeout_and_settings(
            registry,
            storage,
            PROVIDER_REFRESH_TIMEOUT,
            Some(settings),
        )
    }

    #[cfg(test)]
    fn with_refresh_timeout(
        registry: Arc<ProviderRegistry>,
        storage: Arc<Storage>,
        refresh_timeout: Duration,
    ) -> Self {
        Self::with_refresh_timeout_and_settings(registry, storage, refresh_timeout, None)
    }

    fn with_refresh_timeout_and_settings(
        registry: Arc<ProviderRegistry>,
        storage: Arc<Storage>,
        refresh_timeout: Duration,
        settings: Option<Arc<SettingsService>>,
    ) -> Self {
        let mut states = BTreeMap::new();
        let mut refresh_flights = HashMap::new();
        for definition in &registry.catalog().providers {
            let id = definition.id.clone();
            let mut state =
                match storage.load_snapshot_for_identity(&id, registry.cache_identity(&id)) {
                    Ok(Some(mut snapshot)) => {
                        crate::app_debug!("cache", "loaded cached snapshot for {id}");
                        crate::providers::sub2api::restore_subscription_windows(&mut snapshot);
                        normalize_quota_availability(definition, &mut snapshot);
                        ProviderViewState::from_cache(snapshot)
                    }
                    Ok(None) => ProviderViewState::default(),
                    Err(error) => {
                        crate::app_warn!(
                            "cache",
                            "cached snapshot for {id} could not be loaded: {error}"
                        );
                        ProviderViewState::default()
                    }
                };
            if registry
                .runtime(&id)
                .is_some_and(|provider| provider.supports_local_usage())
            {
                match storage.load_local_usage(&id) {
                    Ok(Some(local)) => {
                        local_usage::merge_local(&mut state, &id, local);
                        state.source = SnapshotSource::Cache;
                    }
                    Ok(None) => {}
                    Err(error) => crate::app_warn!(
                        "cache",
                        "cached local usage for {id} could not be loaded: {error}"
                    ),
                }
            }
            states.insert(id.clone(), state);
            refresh_flights.insert(id, Arc::new(RefreshFlight::new()));
        }
        Self {
            registry,
            storage,
            states: RwLock::new(states),
            refresh_flights,
            last_live_refresh: Mutex::new(HashMap::new()),
            last_failed_refresh: Mutex::new(HashMap::new()),
            last_full_refresh_at: RwLock::new(None),
            refresh_interval: RwLock::new(REFRESH_INTERVAL),
            unchanged_refreshes: Mutex::new(0),
            refresh_frequency_wakeup: tokio::sync::Notify::new(),
            refresh_timeout,
            settings,
            preview_revision: tokio::sync::watch::channel(0).0,
        }
    }

    /// Changes whenever local history or quota data is published before the whole refresh ends.
    pub fn subscribe_previews(&self) -> tokio::sync::watch::Receiver<u64> {
        self.preview_revision.subscribe()
    }

    pub fn state(&self) -> UsageViewState {
        let mut providers = self
            .states
            .read()
            .map(|value| value.clone())
            .unwrap_or_default();
        for state in providers.values_mut() {
            update_staleness_from_snapshot_age(state);
        }
        let last_full_refresh_at = self
            .last_full_refresh_at
            .read()
            .ok()
            .and_then(|value| value.to_owned());
        let refresh_interval_seconds = self.refresh_interval().as_secs();
        UsageViewState {
            providers,
            last_full_refresh_at,
            refresh_interval_seconds,
        }
    }

    pub fn clear_provider_data(
        &self,
        provider_id: &str,
    ) -> Result<(), crate::storage::StorageError> {
        // A request started with the previous credentials may still be in flight.
        // Serialize invalidation with publication to both memory and disk.
        let mut flight = self.refresh_flights.get(provider_id).map(|flight| {
            flight
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        });
        self.storage.delete_snapshot(provider_id)?;
        if let Some(state) = flight.as_mut() {
            state.data_revision += 1;
        }
        self.update_state(provider_id, |state| {
            *state = ProviderViewState::default();
        });
        if let Ok(mut last) = self.last_live_refresh.lock() {
            last.remove(provider_id);
        }
        if let Ok(mut failures) = self.last_failed_refresh.lock() {
            failures.remove(provider_id);
        }
        Ok(())
    }

    pub async fn refresh(self: &Arc<Self>, provider_id: &str, force: bool) -> ProviderViewState {
        if self.registry.runtime(provider_id).is_none() {
            crate::app_error!(
                "refresh",
                "refresh requested for unknown provider {provider_id}"
            );
            return ProviderViewState {
                error: Some("Unknown provider.".into()),
                error_kind: Some(ProviderErrorKind::Internal),
                ..ProviderViewState::default()
            };
        };
        if !force && self.is_fresh_this_session(provider_id) {
            crate::app_debug!("refresh", "cache hit {provider_id}");
            return self.provider_state(provider_id);
        }
        if !force && self.is_in_failure_backoff(provider_id) {
            crate::app_debug!(
                "refresh",
                "backoff skip {provider_id} (failed <{}s ago)",
                FAILURE_RETRY_BACKOFF.as_secs()
            );
            return self.provider_state(provider_id);
        }
        let Some(flight) = self.refresh_flights.get(provider_id).cloned() else {
            return self.provider_state(provider_id);
        };
        let mut completed = flight.completed_tx.subscribe();
        let (target_generation, start_runner) = {
            let Ok(mut flight_state) = flight.state.lock() else {
                crate::app_error!(
                    "refresh",
                    "refresh coordination unavailable for {provider_id}"
                );
                return ProviderViewState {
                    error: Some("Provider refresh is temporarily unavailable.".into()),
                    error_kind: Some(ProviderErrorKind::Internal),
                    ..self.provider_state(provider_id)
                };
            };
            if !flight_state.runner_active {
                let generation = flight_state.completed_generation.saturating_add(1);
                flight_state.runner_active = true;
                flight_state.attempt_generation = Some(generation);
                flight_state.requested_generation = generation;
                (generation, true)
            } else if flight_state.attempt_generation.is_none() {
                crate::app_debug!(
                    "refresh",
                    "timed-out refresh worker is still draining for {provider_id}"
                );
                drop(flight_state);
                return self.provider_state(provider_id);
            } else if force {
                let generation = flight_state
                    .attempt_generation
                    .expect("active refresh generation must exist")
                    .saturating_add(1);
                flight_state.requested_generation =
                    flight_state.requested_generation.max(generation);
                crate::app_debug!("refresh", "queued forced follow-up for {provider_id}");
                (flight_state.requested_generation, false)
            } else {
                (
                    flight_state
                        .attempt_generation
                        .expect("active refresh generation must exist"),
                    false,
                )
            }
        };

        if start_runner {
            let service = self.clone();
            let provider_id = provider_id.to_owned();
            let runner_flight = flight.clone();
            tauri::async_runtime::spawn(async move {
                service
                    .run_refresh_flight(provider_id, runner_flight, force)
                    .await;
            });
        }

        while *completed.borrow_and_update() < target_generation {
            if completed.changed().await.is_err() {
                break;
            }
        }
        self.provider_state(provider_id)
    }

    async fn run_refresh_flight(
        self: Arc<Self>,
        provider_id: String,
        flight: Arc<RefreshFlight>,
        initial_force: bool,
    ) {
        let Some(provider) = self.registry.runtime(&provider_id) else {
            return;
        };
        let mut force = initial_force;
        loop {
            let started = Instant::now();
            let tag = format!("plugin:{provider_id}");
            crate::app_info!(&tag, "refresh start (force={force})");
            let (generation, data_revision) = {
                let flight_state = flight
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                self.update_state(&provider_id, |state| {
                    state.refreshing = true;
                    state.error = None;
                    state.error_kind = None;
                    state.last_attempt_at = Some(Utc::now());
                });
                (
                    flight_state.attempt_generation.unwrap_or_default(),
                    flight_state.data_revision,
                )
            };
            let mut local_worker = provider.supports_local_usage().then(|| {
                let local_provider = provider.clone();
                tauri::async_runtime::spawn_blocking(move || local_provider.refresh_local_usage())
            });
            let mut local_finished = local_worker.is_none();
            let worker_provider = provider.clone();
            // Unofficial providers relay through servers whose latency is unknown, so their
            // fast quota data is shown as soon as it arrives instead of waiting for history.
            let (preview_tx, mut preview_rx) = tokio::sync::mpsc::unbounded_channel();
            let previews = provider.supports_connection_configuration();
            let mut worker = tauri::async_runtime::spawn_blocking(move || {
                if previews {
                    worker_provider.refresh_for_service_with_preview(&|preview| {
                        let _ = preview_tx.send(preview);
                    })
                } else {
                    drop(preview_tx);
                    worker_provider.refresh_for_service()
                }
            });
            let deadline = tokio::time::sleep(self.refresh_timeout);
            tokio::pin!(deadline);
            let mut timed_out = false;
            let mut online_result = None;
            let mut online_finished = false;
            let refresh_result = loop {
                if local_finished && online_result.is_some() {
                    break online_result.take().unwrap();
                }
                tokio::select! {
                    result = &mut worker, if online_result.is_none() => {
                        online_finished = true;
                        online_result = Some(match result {
                            Ok(result) => result,
                            Err(_) => Err(ProviderError::new(ProviderErrorKind::Internal, "Provider refresh stopped unexpectedly.")),
                        });
                    },
                    result = async { local_worker.as_mut().expect("local capability checked").await }, if !local_finished => {
                        local_finished = true;
                        let result = result.unwrap_or_else(|_| Some(Err(ProviderError::new(
                            ProviderErrorKind::LocalData, "Local usage refresh stopped unexpectedly.",
                        ))));
                        let flight_state = flight.state.lock().unwrap_or_else(|error| error.into_inner());
                        if flight_state.data_revision == data_revision {
                            if let Some(result) = result {
                                self.apply_local_usage_result(&provider_id, result);
                            }
                        }
                    },
                    Some(preview) = preview_rx.recv() => {
                        let flight_state = flight.state.lock().unwrap_or_else(|error| error.into_inner());
                        if flight_state.data_revision == data_revision {
                            self.apply_preview(&provider_id, preview, started);
                        }
                    }
                    _ = &mut deadline => {
                        crate::app_warn!(
                            &tag,
                            "refresh timed out after {}ms; late result will be discarded",
                            self.refresh_timeout.as_millis()
                        );
                        timed_out = true;
                        if !local_finished {
                            let flight_state = flight.state.lock().unwrap_or_else(|error| error.into_inner());
                            if flight_state.data_revision == data_revision {
                                self.apply_local_usage_result(&provider_id, Err(ProviderError::new(
                                    ProviderErrorKind::LocalData, "Local usage refresh timed out.",
                                )));
                            }
                        }
                        break online_result.take().unwrap_or_else(|| Err(ProviderError::new(
                            ProviderErrorKind::Network, "Provider refresh timed out.",
                        )));
                    }
                }
            };
            // Dropping the receiver discards previews from a worker that outlived its deadline.
            drop(preview_rx);
            let late_worker = (timed_out && !online_finished).then_some(worker);
            let late_local_worker = if timed_out && !local_finished {
                local_worker
            } else {
                None
            };
            let refresh_result = refresh_result.and_then(|refresh| {
                validate_snapshot(&self.registry, &provider_id, refresh.snapshot.clone())?;
                Ok(refresh)
            });
            match &refresh_result {
                Ok(_) => {
                    crate::app_info!(&tag, "refresh end ({}ms)", started.elapsed().as_millis())
                }
                Err(error) => crate::app_warn!(
                    &tag,
                    "refresh failed ({}ms, kind={:?}): {error}",
                    started.elapsed().as_millis(),
                    error.kind()
                ),
            }
            {
                let flight_state = flight
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if flight_state.data_revision == data_revision {
                    let state = self.apply_refresh_result(&provider_id, refresh_result);
                    if state.error.is_none() {
                        if let Ok(mut last) = self.last_live_refresh.lock() {
                            last.insert(provider_id.clone(), Instant::now());
                        }
                        if let Ok(mut failures) = self.last_failed_refresh.lock() {
                            failures.remove(&provider_id);
                        }
                    } else if let Ok(mut failures) = self.last_failed_refresh.lock() {
                        failures.insert(provider_id.clone(), Instant::now());
                    }
                }
            }

            let run_follow_up = if late_worker.is_some() || late_local_worker.is_some() {
                let completed_generation = if let Ok(mut flight_state) = flight.state.lock() {
                    let completed = flight_state.requested_generation.max(generation);
                    flight_state.completed_generation = completed;
                    flight_state.attempt_generation = None;
                    completed
                } else {
                    generation.saturating_add(1)
                };
                flight.completed_tx.send_replace(completed_generation);

                if let Some(worker) = late_worker {
                    let _ = worker.await;
                }
                if let Some(worker) = late_local_worker {
                    let _ = worker.await;
                }
                crate::app_debug!(
                    &tag,
                    "timed-out refresh workers drained; discarded late results"
                );

                if let Ok(mut flight_state) = flight.state.lock() {
                    flight_state.runner_active = false;
                    flight_state.requested_generation = flight_state.completed_generation;
                }
                false
            } else {
                let run_follow_up = if let Ok(mut flight_state) = flight.state.lock() {
                    settle_completed_generation(&mut flight_state, generation)
                } else {
                    false
                };
                flight.completed_tx.send_replace(generation);
                run_follow_up
            };
            if !run_follow_up {
                return;
            }
            force = true;
        }
    }

    #[cfg(test)]
    async fn refresh_enabled(
        self: &Arc<Self>,
        provider_ids: &[String],
        force: bool,
    ) -> UsageViewState {
        self.refresh_enabled_with_progress(provider_ids, force, |_| {})
            .await
    }

    pub async fn refresh_enabled_with_progress<F>(
        self: &Arc<Self>,
        provider_ids: &[String],
        force: bool,
        mut on_progress: F,
    ) -> UsageViewState
    where
        F: FnMut(&UsageViewState) + Send,
    {
        let started = Instant::now();
        crate::app_info!(
            "refresh",
            "batch start ({} providers, force={force})",
            provider_ids.len()
        );
        let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
        for provider_id in provider_ids {
            let service = self.clone();
            let provider_id = provider_id.clone();
            let completed_tx = completed_tx.clone();
            tauri::async_runtime::spawn(async move {
                let state = service.refresh(&provider_id, force).await;
                let _ = completed_tx.send(state);
            });
        }
        drop(completed_tx);

        let mut succeeded = 0;
        let mut failed = 0;
        let mut completed = 0;
        while let Some(state) = completed_rx.recv().await {
            completed += 1;
            if state.error.is_none() {
                succeeded += 1;
            } else {
                failed += 1;
            }
            let current = self.state();
            on_progress(&current);
        }
        failed += provider_ids.len().saturating_sub(completed);
        crate::app_info!(
            "refresh",
            "batch end ({}ms, {succeeded} ok / {failed} failed)",
            started.elapsed().as_millis()
        );
        self.state()
    }

    #[cfg(test)]
    async fn refresh_all(self: &Arc<Self>, provider_ids: &[String], force: bool) -> UsageViewState {
        self.refresh_all_with_progress(provider_ids, force, |_| {})
            .await
    }

    pub async fn refresh_all_with_progress<F>(
        self: &Arc<Self>,
        provider_ids: &[String],
        force: bool,
        on_progress: F,
    ) -> UsageViewState
    where
        F: FnMut(&UsageViewState) + Send,
    {
        let previous = self.state();
        self.refresh_enabled_with_progress(provider_ids, force, on_progress)
            .await;
        if let Ok(mut completed_at) = self.last_full_refresh_at.write() {
            *completed_at = Some(Utc::now());
        }
        let current = self.state();
        self.update_refresh_interval(usage_data_changed(&previous, &current, provider_ids));
        self.state()
    }

    fn provider_state(&self, provider_id: &str) -> ProviderViewState {
        let mut state = self
            .states
            .read()
            .ok()
            .and_then(|states| states.get(provider_id).cloned())
            .unwrap_or_default();
        update_staleness_from_snapshot_age(&mut state);
        state
    }

    fn apply_refresh_result(
        &self,
        provider_id: &str,
        result: Result<ProviderRefresh, ProviderError>,
    ) -> ProviderViewState {
        let result = result.and_then(|refresh| {
            if refresh.snapshot.refreshed_at == chrono::DateTime::UNIX_EPOCH {
                if let Some(error) = self
                    .provider_state(provider_id)
                    .snapshot
                    .and_then(|snapshot| snapshot.usage_histories.local_device_status)
                    .and_then(|status| status.error)
                {
                    return Err(ProviderError::new(ProviderErrorKind::LocalData, error));
                }
            }
            Ok(refresh)
        });
        let result = result.map(|mut refresh| {
            if let Some(previous) = self.provider_state(provider_id).snapshot {
                for quota in &refresh.snapshot.quotas {
                    if let Some(old) = previous.quotas.iter().find(|old| old.id == quota.id) {
                        if old.used_percent != quota.used_percent
                            || old.resets_at != quota.resets_at
                        {
                            crate::app_debug!(
                                "refresh",
                                "{provider_id} quota {}: used {} -> {}, reset {:?} -> {:?}",
                                quota.id,
                                old.used_percent,
                                quota.used_percent,
                                old.resets_at,
                                quota.resets_at
                            );
                        }
                    }
                }
                local_usage::carry_local(&previous, &mut refresh.snapshot);
            }
            if let Some(definition) = self.registry.definition(provider_id) {
                normalize_quota_availability(definition, &mut refresh.snapshot);
            }
            refresh
        });
        let account_error = result
            .as_ref()
            .ok()
            .and_then(|refresh| refresh.account.as_ref())
            .is_some_and(|account| {
                self.settings.as_ref().is_some_and(|settings| {
                    settings
                        .activate_account(account.family, account.provider_id, &account.identity)
                        .is_err()
                })
            });
        let cache_error = !account_error
            && result.as_ref().ok().is_some_and(|refresh| {
                self.storage
                    .save_snapshot_for_identity(
                        &refresh.snapshot,
                        refresh.cache_identity.as_deref(),
                    )
                    .is_err()
            });
        if account_error {
            crate::app_warn!(
                "config",
                "account settings for {provider_id} could not be updated"
            );
        } else if cache_error {
            crate::app_warn!("cache", "snapshot for {provider_id} could not be persisted");
        } else if result.is_ok() {
            crate::app_debug!("cache", "snapshot for {provider_id} persisted");
        }
        let result = if account_error {
            Err(ProviderError::new(
                ProviderErrorKind::Storage,
                "The refreshed account state could not be saved.",
            ))
        } else {
            result.map(|refresh| refresh.snapshot)
        };
        self.update_state(provider_id, |state| {
            merge_refresh_result(state, result);
            if cache_error {
                state.error = Some(
                    "Usage refreshed, but the last successful snapshot could not be cached.".into(),
                );
                state.error_kind = Some(ProviderErrorKind::Storage);
            }
        });
        self.provider_state(provider_id)
    }

    /// Shows a refresh's live quotas while it keeps loading usage history. The preview
    /// carries the last persisted histories of the same cache identity, so history never
    /// blanks out or mixes accounts, and it is persisted so a later failure still keeps it.
    fn apply_preview(&self, provider_id: &str, preview: ProviderRefresh, started: Instant) {
        let tag = format!("plugin:{provider_id}");
        let mut snapshot = match validate_snapshot(&self.registry, provider_id, preview.snapshot) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                crate::app_warn!(&tag, "quota preview rejected: {error}");
                return;
            }
        };
        let identity = match preview.cache_identity.as_deref() {
            Some(identity) => CacheIdentity::Resolved(identity),
            None => CacheIdentity::Unscoped,
        };
        if let Ok(Some(previous)) = self
            .storage
            .load_snapshot_for_identity(provider_id, identity)
        {
            carry_usage_histories(&previous, &mut snapshot);
        }
        if let Some(previous) = self.provider_state(provider_id).snapshot {
            local_usage::carry_local(&previous, &mut snapshot);
        }
        if let Some(definition) = self.registry.definition(provider_id) {
            normalize_quota_availability(definition, &mut snapshot);
        }
        if self
            .storage
            .save_snapshot_for_identity(&snapshot, preview.cache_identity.as_deref())
            .is_err()
        {
            crate::app_warn!(
                "cache",
                "quota preview for {provider_id} could not be persisted"
            );
        }
        self.update_state(provider_id, |state| {
            state.snapshot = Some(snapshot);
            state.source = SnapshotSource::Live;
            state.stale = false;
        });
        crate::app_info!(&tag, "quota preview ({}ms)", started.elapsed().as_millis());
        self.preview_revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    fn is_fresh_this_session(&self, provider_id: &str) -> bool {
        self.last_live_refresh
            .lock()
            .ok()
            .and_then(|value| value.get(provider_id).copied())
            .is_some_and(|instant| instant.elapsed() < self.refresh_interval())
    }

    fn refresh_interval(&self) -> Duration {
        match self.update_frequency() {
            UpdateFrequency::OneMinute => FAST_REFRESH_INTERVAL,
            UpdateFrequency::FiveMinutes => REFRESH_INTERVAL,
            UpdateFrequency::Adaptive => self.dynamic_refresh_interval(),
        }
    }

    fn dynamic_refresh_interval(&self) -> Duration {
        self.refresh_interval
            .read()
            .map(|value| *value)
            .unwrap_or(REFRESH_INTERVAL)
    }

    fn update_frequency(&self) -> UpdateFrequency {
        self.settings
            .as_ref()
            .map(|settings| settings.get().update_frequency)
            .unwrap_or_default()
    }

    fn set_refresh_interval(&self, interval: Duration) {
        if let Ok(mut current) = self.refresh_interval.write() {
            *current = interval;
        }
    }

    fn update_refresh_interval(&self, data_changed: bool) -> Duration {
        let interval = match self.update_frequency() {
            UpdateFrequency::OneMinute => {
                self.reset_unchanged_refreshes();
                FAST_REFRESH_INTERVAL
            }
            UpdateFrequency::FiveMinutes => {
                self.reset_unchanged_refreshes();
                REFRESH_INTERVAL
            }
            UpdateFrequency::Adaptive => {
                let current = self.dynamic_refresh_interval();
                self.unchanged_refreshes
                    .lock()
                    .map(|mut unchanged| {
                        refresh_interval_for(current, data_changed, &mut unchanged)
                    })
                    .unwrap_or_else(|_| {
                        if data_changed {
                            FAST_REFRESH_INTERVAL
                        } else {
                            REFRESH_INTERVAL
                        }
                    })
            }
        };
        self.set_refresh_interval(interval);
        interval
    }

    fn reset_unchanged_refreshes(&self) {
        if let Ok(mut unchanged) = self.unchanged_refreshes.lock() {
            *unchanged = 0;
        }
    }

    pub fn refresh_frequency_changed(&self) {
        self.reset_unchanged_refreshes();
        self.set_refresh_interval(REFRESH_INTERVAL);
        self.refresh_frequency_wakeup.notify_one();
    }

    pub async fn wait_for_refresh_frequency_change(&self) {
        self.refresh_frequency_wakeup.notified().await;
    }

    fn is_in_failure_backoff(&self, provider_id: &str) -> bool {
        self.last_failed_refresh
            .lock()
            .ok()
            .and_then(|value| value.get(provider_id).copied())
            .is_some_and(|instant| instant.elapsed() < FAILURE_RETRY_BACKOFF)
    }

    fn update_state(&self, provider_id: &str, update: impl FnOnce(&mut ProviderViewState)) {
        if let Ok(mut states) = self.states.write() {
            update(states.entry(provider_id.to_owned()).or_default());
        }
    }
}

struct RefreshFlight {
    state: Mutex<RefreshFlightState>,
    completed_tx: tokio::sync::watch::Sender<u64>,
}

#[derive(Default)]
struct RefreshFlightState {
    data_revision: u64,
    runner_active: bool,
    attempt_generation: Option<u64>,
    requested_generation: u64,
    completed_generation: u64,
}

fn settle_completed_generation(state: &mut RefreshFlightState, generation: u64) -> bool {
    state.completed_generation = generation;
    state.attempt_generation = None;
    if state.requested_generation > state.completed_generation {
        state.attempt_generation = Some(state.completed_generation.saturating_add(1));
        true
    } else {
        state.runner_active = false;
        false
    }
}

impl RefreshFlight {
    fn new() -> Self {
        let (completed_tx, _) = tokio::sync::watch::channel(0);
        Self {
            state: Mutex::new(RefreshFlightState::default()),
            completed_tx,
        }
    }
}

#[cfg(test)]
mod tests;
