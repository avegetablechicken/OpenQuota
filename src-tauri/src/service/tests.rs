use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use tempfile::tempdir;

use super::{
    merge_refresh_result, refresh_interval_for, usage_data_changed, validate_snapshot,
    ProviderService, UsageViewState, PROVIDER_REFRESH_TIMEOUT,
};
use crate::{
    models::{
        AccountSnapshot, MetricDefinition, MetricSection, MetricSource, ProviderDefinition,
        ProviderErrorKind, ProviderSnapshot, ProviderViewState, QuotaFormat, QuotaWindow,
        SnapshotSource, StatusMetric, StatusTone, UsageHistories, UsageHistory,
    },
    policy::{FAILURE_RETRY_BACKOFF, FAST_REFRESH_INTERVAL, REFRESH_INTERVAL, STALE_AFTER},
    providers::{AccountRefresh, ProviderError, ProviderRefresh, ProviderRegistry, UsageProvider},
    settings::SettingsService,
    storage::Storage,
};

const TEST_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn production_refresh_timeout_covers_the_longest_bounded_provider_flow() {
    assert!(PROVIDER_REFRESH_TIMEOUT >= Duration::from_secs(110));
}

#[test]
fn usage_refresh_interval_follows_actual_snapshot_changes() {
    let provider_ids = vec!["codex".to_owned()];
    let previous_snapshot = test_snapshot("codex");
    let previous = UsageViewState {
        providers: [(
            "codex".into(),
            ProviderViewState {
                snapshot: Some(previous_snapshot.clone()),
                ..ProviderViewState::default()
            },
        )]
        .into_iter()
        .collect(),
        last_full_refresh_at: None,
        refresh_interval_seconds: REFRESH_INTERVAL.as_secs(),
    };

    let mut timestamp_only = previous.clone();
    timestamp_only
        .providers
        .get_mut("codex")
        .unwrap()
        .snapshot
        .as_mut()
        .unwrap()
        .refreshed_at = Utc::now();
    assert!(!usage_data_changed(
        &previous,
        &timestamp_only,
        &provider_ids
    ));
    let mut unchanged_refreshes = 0;
    assert_eq!(
        refresh_interval_for(REFRESH_INTERVAL, false, &mut unchanged_refreshes),
        REFRESH_INTERVAL
    );

    let mut changed = timestamp_only.clone();
    changed
        .providers
        .get_mut("codex")
        .unwrap()
        .snapshot
        .as_mut()
        .unwrap()
        .quotas
        .push(QuotaWindow {
            clock_offset_seconds: None,
            id: "session".into(),
            label: "Session".into(),
            used_percent: 25.0,
            resets_at: None,
            period_seconds: 300,
            format: QuotaFormat::Percent,
            used_value: None,
            limit_value: None,
            unit: None,
            estimated: false,
            source_note: None,
        });
    assert!(usage_data_changed(&previous, &changed, &provider_ids));
    assert_eq!(
        refresh_interval_for(REFRESH_INTERVAL, true, &mut unchanged_refreshes),
        FAST_REFRESH_INTERVAL
    );
    assert_eq!(
        refresh_interval_for(FAST_REFRESH_INTERVAL, false, &mut unchanged_refreshes),
        FAST_REFRESH_INTERVAL
    );
    assert_eq!(
        refresh_interval_for(FAST_REFRESH_INTERVAL, false, &mut unchanged_refreshes),
        REFRESH_INTERVAL
    );
}

#[test]
fn clock_jitter_does_not_keep_adaptive_refresh_fast_for_live_or_nested_accounts() {
    let mut baseline = test_snapshot("claude");
    baseline.quotas.push(QuotaWindow {
        clock_offset_seconds: Some(0),
        id: "session".into(),
        label: "Session".into(),
        used_percent: 25.0,
        resets_at: Some(Utc::now() + chrono::Duration::hours(3)),
        period_seconds: 18000,
        format: QuotaFormat::Percent,
        used_value: None,
        limit_value: None,
        unit: None,
        estimated: false,
        source_note: None,
    });
    for nested in [false, true] {
        let view = |snapshot: ProviderSnapshot| {
            let snapshot = if nested {
                let mut parent = test_snapshot("claude");
                parent.accounts = Some(vec![AccountSnapshot {
                    id: "alice".into(),
                    name: "Alice".into(),
                    snapshot,
                }]);
                parent
            } else {
                snapshot
            };
            UsageViewState {
                providers: [(
                    "claude".into(),
                    ProviderViewState {
                        snapshot: Some(snapshot),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            }
        };
        let ids = vec!["claude".into()];
        let mut previous = baseline.clone();
        previous.quotas[0].used_percent = 20.0;
        let changed = usage_data_changed(&view(previous), &view(baseline.clone()), &ids);
        assert!(changed);
        let mut unchanged = 0;
        let mut interval = refresh_interval_for(REFRESH_INTERVAL, changed, &mut unchanged);
        assert_eq!(interval, FAST_REFRESH_INTERVAL);
        previous = baseline.clone();
        for (round, offset, shift) in [(1, Some(-1), -1), (2, None, 0)] {
            let mut current = baseline.clone();
            current.refreshed_at += chrono::Duration::seconds(60 * round);
            current.quotas[0].clock_offset_seconds = offset;
            current.quotas[0].resets_at = baseline.quotas[0]
                .resets_at
                .map(|at| at + chrono::Duration::seconds(shift));
            let changed = usage_data_changed(&view(previous), &view(current.clone()), &ids);
            assert!(!changed, "clock jitter, nested={nested}");
            interval = refresh_interval_for(interval, changed, &mut unchanged);
            assert_eq!(
                interval,
                if round == 1 {
                    FAST_REFRESH_INTERVAL
                } else {
                    REFRESH_INTERVAL
                }
            );
            previous = current;
        }
        // Repeated idle polls stay slow; real usage and window changes still speed up.
        for shift in [-5, 5, 6] {
            let mut current = baseline.clone();
            current.quotas[0].resets_at = baseline.quotas[0]
                .resets_at
                .map(|at| at + chrono::Duration::seconds(shift));
            assert_eq!(
                usage_data_changed(&view(baseline.clone()), &view(current), &ids),
                shift == 6
            );
        }
        let mut ended = baseline.clone();
        ended.quotas[0].resets_at = None;
        assert!(usage_data_changed(
            &view(baseline.clone()),
            &view(ended.clone()),
            &ids
        ));
        assert!(usage_data_changed(
            &view(ended),
            &view(baseline.clone()),
            &ids
        ));
        let mut spent = baseline.clone();
        spent.quotas[0].used_percent += 1.0;
        spent.quotas[0].clock_offset_seconds = Some(-1);
        assert!(usage_data_changed(
            &view(baseline.clone()),
            &view(spent),
            &ids
        ));
        let mut removed = baseline.clone();
        removed.quotas.clear();
        assert!(usage_data_changed(
            &view(baseline.clone()),
            &view(removed),
            &ids
        ));
    }
}

#[test]
fn rolling_idle_resets_do_not_keep_adaptive_refresh_fast() {
    let base_time = chrono::DateTime::parse_from_rfc3339("2026-11-28T17:07:09Z")
        .unwrap()
        .to_utc();
    let weekly_period = 7 * 24 * 60 * 60;
    let make_snapshot = |refreshed_at, used_percent, resets_at, period_seconds| {
        let mut snapshot = test_snapshot("sub2api");
        snapshot.refreshed_at = refreshed_at;
        snapshot.quotas.push(QuotaWindow {
            clock_offset_seconds: None,
            id: "weekly".into(),
            label: "Weekly".into(),
            used_percent,
            resets_at: Some(resets_at),
            period_seconds,
            format: QuotaFormat::Percent,
            used_value: None,
            limit_value: None,
            unit: None,
            estimated: false,
            source_note: None,
        });
        snapshot
    };
    let ids = vec!["sub2api".into()];
    for nested in [false, true] {
        let view = |snapshot: ProviderSnapshot| {
            let snapshot = if nested {
                let mut parent = test_snapshot("sub2api");
                parent.refreshed_at = snapshot.refreshed_at;
                parent.accounts = Some(vec![AccountSnapshot {
                    id: "14".into(),
                    name: "Sub2API · Codex · openai-hmm".into(),
                    snapshot,
                }]);
                parent
            } else {
                snapshot
            };
            UsageViewState {
                providers: [(
                    "sub2api".into(),
                    ProviderViewState {
                        snapshot: Some(snapshot),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            }
        };

        let period = chrono::Duration::seconds(weekly_period as i64);
        // Real observed pattern: +7s then +1s offset around refreshed_at + 7d across a 162s poll,
        // followed by a 60s idle poll. Both polls must be treated as unchanged so adaptive refresh
        // returns to the slow interval without clearing `resets_at`.
        let s0 = make_snapshot(
            base_time,
            0.0,
            base_time + period + chrono::Duration::seconds(7),
            weekly_period,
        );
        let s1 = make_snapshot(
            base_time + chrono::Duration::seconds(162),
            0.0,
            base_time + chrono::Duration::seconds(162) + period + chrono::Duration::seconds(1),
            weekly_period,
        );
        let s2 = make_snapshot(
            base_time + chrono::Duration::seconds(222),
            0.0,
            base_time + chrono::Duration::seconds(222) + period,
            weekly_period,
        );
        let mut unchanged = 0;
        let mut interval = FAST_REFRESH_INTERVAL;
        for (round, (prev, curr)) in [(&s0, &s1), (&s1, &s2)].into_iter().enumerate() {
            let changed = usage_data_changed(&view(prev.clone()), &view(curr.clone()), &ids);
            assert!(
                !changed,
                "rolling idle reset, nested={nested}, round={round}"
            );
            assert!(curr.quotas[0].resets_at.is_some());
            interval = refresh_interval_for(interval, changed, &mut unchanged);
        }
        assert_eq!(interval, REFRESH_INTERVAL);

        // Non-zero usage: rolling reset must still be detected as a change.
        let active_prev = make_snapshot(base_time, 1.0, base_time + period, weekly_period);
        let active_curr = make_snapshot(
            base_time + chrono::Duration::seconds(60),
            1.0,
            base_time + chrono::Duration::seconds(60) + period,
            weekly_period,
        );
        assert!(usage_data_changed(
            &view(active_prev),
            &view(active_curr),
            &ids
        ));

        // Zero period or mismatched period must not ignore reset differences.
        let zero_period_prev = make_snapshot(base_time, 0.0, base_time + period, 0);
        let zero_period_curr = make_snapshot(
            base_time + chrono::Duration::seconds(60),
            0.0,
            base_time + chrono::Duration::seconds(60) + period,
            0,
        );
        assert!(usage_data_changed(
            &view(zero_period_prev),
            &view(zero_period_curr),
            &ids
        ));

        // A reset far from `observed + period` (an already-started window) must still count as changed.
        let mid_prev = make_snapshot(
            base_time,
            0.0,
            base_time + chrono::Duration::days(3),
            weekly_period,
        );
        let mid_curr = make_snapshot(
            base_time + chrono::Duration::seconds(60),
            0.0,
            base_time + chrono::Duration::days(3) + chrono::Duration::seconds(60),
            weekly_period,
        );
        assert!(usage_data_changed(&view(mid_prev), &view(mid_curr), &ids));

        // Reset shift that does not match the observation interval must still count as changed.
        let drift_prev = make_snapshot(base_time, 0.0, base_time + period, weekly_period);
        let drift_curr = make_snapshot(
            base_time + chrono::Duration::seconds(60),
            0.0,
            base_time + period + chrono::Duration::seconds(20),
            weekly_period,
        );
        assert!(usage_data_changed(
            &view(drift_prev.clone()),
            &view(drift_curr),
            &ids
        ));

        // Zero observation interval with a >5s reset jump must still count as changed.
        let same_time_curr = make_snapshot(
            base_time,
            0.0,
            base_time + period + chrono::Duration::seconds(30),
            weekly_period,
        );
        assert!(usage_data_changed(
            &view(drift_prev),
            &view(same_time_curr),
            &ids
        ));
    }
}

struct SlowProvider {
    id: &'static str,
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    delay: Duration,
}

struct GatedProvider {
    id: &'static str,
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

struct SequenceProvider {
    id: &'static str,
    calls: Arc<AtomicUsize>,
    failures_before_success: usize,
}

struct CredentialProvider {
    id: &'static str,
    credential: Arc<Mutex<Option<String>>>,
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    delay: Duration,
}

struct AccountSwitchProvider {
    identity: String,
}

impl UsageProvider for SlowProvider {
    fn definition(&self) -> ProviderDefinition {
        test_definition(self.id)
    }

    fn has_local_credentials(&self) -> bool {
        true
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        thread::sleep(self.delay);
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(test_snapshot(self.id))
    }
}

impl UsageProvider for GatedProvider {
    fn definition(&self) -> ProviderDefinition {
        test_definition(self.id)
    }

    fn has_local_credentials(&self) -> bool {
        true
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);

        let (released, signal) = &*self.gate;
        let released = released.lock().unwrap();
        let (released, _) = signal
            .wait_timeout_while(released, Duration::from_secs(2), |released| !*released)
            .unwrap();
        drop(released);

        self.active.fetch_sub(1, Ordering::SeqCst);
        let mut snapshot = test_snapshot(self.id);
        snapshot.plan = Some(format!("live-{call}"));
        Ok(snapshot)
    }
}

impl UsageProvider for SequenceProvider {
    fn definition(&self) -> ProviderDefinition {
        test_definition(self.id)
    }

    fn has_local_credentials(&self) -> bool {
        true
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call <= self.failures_before_success {
            Err(ProviderError::new(ProviderErrorKind::Network, "offline"))
        } else {
            Ok(test_snapshot(self.id))
        }
    }
}

impl UsageProvider for CredentialProvider {
    fn definition(&self) -> ProviderDefinition {
        test_definition(self.id)
    }

    fn has_local_credentials(&self) -> bool {
        self.credential.lock().unwrap().is_some()
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        let credential = self.credential.lock().unwrap().clone();
        self.calls.fetch_add(1, Ordering::SeqCst);
        thread::sleep(self.delay);
        self.active.fetch_sub(1, Ordering::SeqCst);
        let mut snapshot = test_snapshot(self.id);
        snapshot.plan = credential;
        Ok(snapshot)
    }
}

impl UsageProvider for AccountSwitchProvider {
    fn definition(&self) -> ProviderDefinition {
        test_definition("codex")
    }

    fn has_local_credentials(&self) -> bool {
        true
    }

    fn supports_account_names(&self) -> bool {
        true
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        Ok(test_snapshot("codex"))
    }

    fn refresh_for_service(&self) -> Result<ProviderRefresh, ProviderError> {
        Ok(ProviderRefresh {
            snapshot: test_snapshot("codex"),
            cache_identity: Some(self.identity.clone()),
            account: Some(AccountRefresh {
                family: "codex",
                provider_id: "codex",
                identity: self.identity.clone(),
            }),
        })
    }
}

/// Publishes a quota preview, then holds the final result until the test releases it.
struct PreviewProvider {
    id: &'static str,
    unofficial: bool,
    previews_requested: AtomicUsize,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl PreviewProvider {
    fn snapshot(&self, plan: &str, history_marker: Option<&str>) -> ProviderSnapshot {
        let mut snapshot = test_snapshot(self.id);
        snapshot.plan = Some(plan.into());
        snapshot.usage_histories = UsageHistories::account(UsageHistory {
            unknown_models: history_marker.into_iter().map(str::to_owned).collect(),
            ..UsageHistory::default()
        });
        snapshot.accounts = Some(vec![AccountSnapshot {
            id: "7".into(),
            name: "Upstream".into(),
            snapshot: ProviderSnapshot {
                upstream_provider_id: None,
                accounts: None,
                ..snapshot.clone()
            },
        }]);
        snapshot
    }

    fn wait_for_release(&self) {
        let (released, signal) = &*self.release;
        let released = released.lock().unwrap();
        let _ = signal
            .wait_timeout_while(released, TEST_WAIT_TIMEOUT, |released| !*released)
            .unwrap();
    }
}

impl UsageProvider for PreviewProvider {
    fn definition(&self) -> ProviderDefinition {
        test_definition(self.id)
    }

    fn has_local_credentials(&self) -> bool {
        true
    }

    fn supports_connection_configuration(&self) -> bool {
        self.unofficial
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        self.wait_for_release();
        Ok(self.snapshot("final", Some("fresh")))
    }

    fn refresh_for_service_with_preview(
        &self,
        preview: &dyn Fn(ProviderRefresh),
    ) -> Result<ProviderRefresh, ProviderError> {
        self.previews_requested.fetch_add(1, Ordering::SeqCst);
        preview(ProviderRefresh {
            snapshot: self.snapshot("preview", None),
            cache_identity: Some("server".into()),
            account: None,
        });
        self.wait_for_release();
        Ok(ProviderRefresh {
            snapshot: self.snapshot("final", Some("fresh")),
            cache_identity: Some("server".into()),
            account: None,
        })
    }
}

fn preview_service(
    unofficial: bool,
) -> (
    Arc<ProviderService>,
    Arc<PreviewProvider>,
    tempfile::TempDir,
) {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("cache.db")).unwrap());
    let provider = Arc::new(PreviewProvider {
        id: "relay",
        unofficial,
        previews_requested: AtomicUsize::new(0),
        release: Arc::new((Mutex::new(false), Condvar::new())),
    });
    // The last complete snapshot belongs to the same cache identity as the preview.
    storage
        .save_snapshot_for_identity(&provider.snapshot("cached", Some("cached")), Some("server"))
        .unwrap();
    let registry = Arc::new(ProviderRegistry::new(vec![provider.clone()]).unwrap());
    (
        Arc::new(ProviderService::new(registry, storage)),
        provider,
        directory,
    )
}

fn history_marker(snapshot: &ProviderSnapshot) -> Vec<String> {
    snapshot
        .usage_histories
        .account
        .as_ref()
        .map(|history| history.unknown_models.clone())
        .unwrap_or_default()
}

#[test]
fn unofficial_providers_show_live_quotas_before_their_history_finishes() {
    let (service, provider, _directory) = preview_service(true);
    let mut previews = service.subscribe_previews();
    let refresh = {
        let service = service.clone();
        thread::spawn(move || refresh_with_test_timeout(&service, "relay", true))
    };

    wait_until("quota preview should be shown during the refresh", || {
        service
            .provider_state("relay")
            .snapshot
            .and_then(|s| s.plan)
            == Some("preview".into())
    });
    let state = service.provider_state("relay");
    assert!(state.refreshing);
    assert_eq!(state.source, SnapshotSource::Live);
    let snapshot = state.snapshot.unwrap();
    // History keeps the last complete values instead of blanking while it loads.
    assert_eq!(history_marker(&snapshot), ["cached"]);
    assert_eq!(
        history_marker(&snapshot.accounts.as_ref().unwrap()[0].snapshot),
        ["cached"]
    );
    assert!(previews.has_changed().unwrap());
    previews.mark_unchanged();

    let (released, signal) = &*provider.release;
    *released.lock().unwrap() = true;
    signal.notify_all();
    let finished = refresh.join().unwrap();
    assert!(!finished.refreshing);
    let snapshot = finished.snapshot.unwrap();
    assert_eq!(snapshot.plan.as_deref(), Some("final"));
    assert_eq!(history_marker(&snapshot), ["fresh"]);
    assert!(!previews.has_changed().unwrap());
}

#[test]
fn official_providers_refresh_in_one_step() {
    let (service, provider, _directory) = preview_service(false);
    let (released, signal) = &*provider.release;
    *released.lock().unwrap() = true;
    signal.notify_all();

    let finished = refresh_with_test_timeout(&service, "relay", true);
    assert_eq!(provider.previews_requested.load(Ordering::SeqCst), 0);
    assert_eq!(finished.snapshot.unwrap().plan.as_deref(), Some("final"));
    assert!(!service.subscribe_previews().has_changed().unwrap());
}

fn test_definition(id: &str) -> ProviderDefinition {
    ProviderDefinition {
        id: id.into(),
        display_name: id.into(),
        short_name: "T".into(),
        fallback_enabled: true,
        local_usage_source_note: None,
        links: vec![],
        metrics: vec![MetricDefinition::new(
            format!("{id}.session"),
            "Session",
            MetricSource::Quota {
                source_id: "session".into(),
                session_window: false,
            },
            true,
            true,
            MetricSection::AlwaysVisible,
            true,
            Some("S"),
            None,
        )],
    }
}

fn test_snapshot(provider_id: &str) -> ProviderSnapshot {
    ProviderSnapshot {
        upstream_provider_id: None,
        accounts: None,
        provider_id: provider_id.into(),
        plan: None,
        quotas: Vec::new(),
        value_metrics: Vec::new(),
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage_histories: Default::default(),
        warnings: Vec::new(),
        refreshed_at: Utc::now(),
    }
}

fn refresh_with_test_timeout(
    service: &Arc<ProviderService>,
    provider_id: &str,
    force: bool,
) -> ProviderViewState {
    tauri::async_runtime::block_on(async {
        tokio::time::timeout(TEST_WAIT_TIMEOUT, service.refresh(provider_id, force))
            .await
            .expect("refresh should not deadlock")
    })
}

fn wait_until(message: &str, predicate: impl Fn() -> bool) {
    let started = Instant::now();
    while !predicate() {
        assert!(started.elapsed() < TEST_WAIT_TIMEOUT, "{message}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn refresh_runner_is_idle(service: &ProviderService, provider_id: &str) -> bool {
    service
        .refresh_flights
        .get(provider_id)
        .and_then(|flight| flight.state.lock().ok())
        .is_some_and(|state| !state.runner_active)
}

#[test]
fn failed_refresh_preserves_last_successful_snapshot_without_forcing_stale() {
    let snapshot = ProviderSnapshot {
        upstream_provider_id: None,
        accounts: None,
        provider_id: "codex".into(),
        plan: None,
        quotas: Vec::new(),
        value_metrics: Vec::new(),
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage_histories: Default::default(),
        warnings: Vec::new(),
        refreshed_at: Utc::now(),
    };
    for kind in [
        ProviderErrorKind::Network,
        ProviderErrorKind::InvalidResponse,
    ] {
        let mut state = ProviderViewState {
            snapshot: Some(snapshot.clone()),
            ..ProviderViewState::default()
        };
        merge_refresh_result(&mut state, Err(ProviderError::new(kind, "refresh failed")));
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        assert!(!state.stale);
        assert_eq!(state.error.as_deref(), Some("refresh failed"));
        assert_eq!(state.error_kind, Some(kind));
    }
}

#[test]
fn cached_snapshot_staleness_is_based_on_snapshot_age() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let provider = Arc::new(SlowProvider {
        id: "cached",
        calls: Arc::new(AtomicUsize::new(0)),
        active: Arc::new(AtomicUsize::new(0)),
        maximum: Arc::new(AtomicUsize::new(0)),
        delay: Duration::ZERO,
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());

    storage.save_snapshot(&test_snapshot("cached")).unwrap();
    let fresh_service = ProviderService::new(registry.clone(), storage.clone());
    let fresh = fresh_service.state();
    let fresh = fresh.providers.get("cached").unwrap();
    assert_eq!(fresh.source, SnapshotSource::Cache);
    assert!(!fresh.stale);

    let mut old_snapshot = test_snapshot("cached");
    old_snapshot.refreshed_at = Utc::now() - STALE_AFTER - chrono::Duration::seconds(1);
    storage.save_snapshot(&old_snapshot).unwrap();
    let old_service = ProviderService::new(registry, storage);
    let old = old_service.state();
    assert!(old.providers.get("cached").unwrap().stale);
}

#[test]
fn zero_sessions_with_an_active_reset_stay_started_for_live_and_cached_data() {
    for mut definition in [
        crate::providers::claude::definition(),
        crate::providers::opencode::definition(),
        crate::providers::codex::definition(),
    ] {
        let id = definition.id.clone();
        definition.fallback_enabled = true;
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let registry = Arc::new(ProviderRegistry::from_definitions(vec![definition]).unwrap());
        let mut snapshot = test_snapshot(&id);
        snapshot.quotas.push(QuotaWindow {
            clock_offset_seconds: None,
            id: "session".into(),
            label: "Session".into(),
            used_percent: 0.0,
            resets_at: Some(snapshot.refreshed_at + chrono::Duration::hours(5)),
            period_seconds: 18_000,
            format: QuotaFormat::Percent,
            used_value: None,
            limit_value: None,
            unit: None,
            estimated: false,
            source_note: None,
        });
        let mut weekly = snapshot.quotas[0].clone();
        weekly.id = "weekly".into();
        weekly.label = "Weekly".into();
        snapshot.quotas.push(weekly);
        storage.save_snapshot(&snapshot).unwrap();
        let service = ProviderService::new(registry, storage.clone());
        let cached = service.provider_state(&id).snapshot.unwrap();
        // Mappers drop placeholder resets, so a reset that is still ahead marks a
        // started session for every provider.
        assert_eq!(cached.quotas.len(), 2, "{id}");
        let session = cached
            .quotas
            .iter()
            .find(|quota| quota.id == "session")
            .unwrap();
        assert_eq!(session.used_percent, 0.0);
        assert!(session.resets_at.unwrap() > cached.refreshed_at, "{id}");
        assert!(cached.quotas.iter().any(|quota| quota.id == "weekly"));
        let live = service
            .apply_refresh_result(
                &id,
                Ok(ProviderRefresh {
                    snapshot: snapshot.clone(),
                    cache_identity: None,
                    account: None,
                }),
            )
            .snapshot
            .unwrap();
        assert_eq!(live.quotas, cached.quotas);
        assert_eq!(
            storage.load_snapshot(&id).unwrap().unwrap().quotas,
            live.quotas
        );

        snapshot.quotas[0].used_percent = 1.0;
        let active = service
            .apply_refresh_result(
                &id,
                Ok(ProviderRefresh {
                    snapshot,
                    cache_identity: None,
                    account: None,
                }),
            )
            .snapshot
            .unwrap();
        assert_eq!(active.quotas.len(), 2);
    }
}

#[test]
fn account_sessions_with_a_passed_reset_read_as_not_started() {
    let definition = crate::providers::claude::definition();
    let mut snapshot = test_snapshot("claude");
    let mut account = test_snapshot("claude");
    account.quotas.push(QuotaWindow {
        clock_offset_seconds: None,
        id: "session".into(),
        label: "Session".into(),
        used_percent: 0.0,
        resets_at: Some(account.refreshed_at - chrono::Duration::minutes(1)),
        period_seconds: 18_000,
        format: QuotaFormat::Percent,
        used_value: None,
        limit_value: None,
        unit: None,
        estimated: false,
        source_note: None,
    });
    snapshot.accounts = Some(vec![crate::models::AccountSnapshot {
        id: "1".into(),
        name: "Upstream".into(),
        snapshot: account,
    }]);

    super::normalize_quota_availability(&definition, &mut snapshot);

    let session = &snapshot.accounts.unwrap()[0].snapshot.quotas[0];
    assert_eq!(session.used_percent, 0.0);
    assert_eq!(session.resets_at, None);
    assert!(session.is_unstarted_session(&definition.metrics[0].source));
}

#[test]
fn antigravity_keeps_all_reported_zero_pools_with_active_resets() {
    let definition = crate::providers::antigravity::definition();
    let mut snapshot = test_snapshot(&definition.id);
    for (id, period) in [
        ("geminiPro", 18_000),
        ("geminiWeekly", 604_800),
        ("claude", 18_000),
        ("claudeWeekly", 604_800),
    ] {
        snapshot.quotas.push(QuotaWindow {
            id: id.into(),
            label: id.into(),
            used_percent: 0.0,
            resets_at: Some(snapshot.refreshed_at + chrono::Duration::seconds(period)),
            clock_offset_seconds: None,
            period_seconds: period as u64,
            format: QuotaFormat::Percent,
            used_value: None,
            limit_value: None,
            unit: None,
            estimated: false,
            source_note: None,
        });
    }
    let reported = snapshot.quotas.clone();
    super::normalize_quota_availability(&definition, &mut snapshot);
    assert_eq!(snapshot.quotas, reported);
}

#[test]
fn zero_sessions_without_an_active_reset_are_not_started() {
    for (mut definition, reset_offset) in [
        crate::providers::claude::definition(),
        crate::providers::codex::definition(),
        crate::providers::opencode::definition(),
    ]
    .into_iter()
    .flat_map(|definition| {
        [None, Some(-1), Some(0), Some(18_000)].map(|offset| (definition.clone(), offset))
    }) {
        definition.fallback_enabled = true;
        let id = definition.id.clone();
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let registry =
            Arc::new(ProviderRegistry::from_definitions(vec![definition.clone()]).unwrap());
        let mut snapshot = test_snapshot(&id);
        assert_eq!(definition.metrics[0].id, format!("{id}.session"));
        snapshot.quotas.push(QuotaWindow {
            clock_offset_seconds: None,
            id: "session".into(),
            label: "Session".into(),
            used_percent: 0.0,
            resets_at: reset_offset
                .map(|seconds| snapshot.refreshed_at + chrono::Duration::seconds(seconds)),
            period_seconds: 18_000,
            format: QuotaFormat::Percent,
            used_value: None,
            limit_value: None,
            unit: None,
            estimated: false,
            source_note: None,
        });
        storage.save_snapshot(&snapshot).unwrap();
        let service = ProviderService::new(registry, storage);
        let cached = service.provider_state(&id).snapshot.unwrap();
        let session = cached
            .quotas
            .iter()
            .find(|quota| quota.id == "session")
            .unwrap();
        let active = reset_offset.is_some_and(|seconds| seconds > 0);
        assert_eq!(session.resets_at.is_some(), active, "{id} {reset_offset:?}");
        assert_eq!(
            session.is_unstarted_session(&definition.metrics[0].source),
            !active,
            "{id} {reset_offset:?}"
        );
        let live = service
            .apply_refresh_result(
                &id,
                Ok(ProviderRefresh {
                    snapshot,
                    cache_identity: None,
                    account: None,
                }),
            )
            .snapshot
            .unwrap();
        assert_eq!(live.quotas, cached.quotas);
    }
}

#[test]
fn clearing_provider_data_is_scoped_to_one_provider() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    storage.save_snapshot(&test_snapshot("sub2api")).unwrap();
    storage.save_snapshot(&test_snapshot("sub2api@2")).unwrap();
    let providers = ["sub2api", "sub2api@2"]
        .into_iter()
        .map(|id| {
            Arc::new(SlowProvider {
                id,
                calls: Arc::new(AtomicUsize::new(0)),
                active: Arc::new(AtomicUsize::new(0)),
                maximum: Arc::new(AtomicUsize::new(0)),
                delay: Duration::ZERO,
            }) as Arc<dyn UsageProvider>
        })
        .collect();
    let registry = Arc::new(ProviderRegistry::new(providers).unwrap());
    let service = ProviderService::new(registry, storage.clone());

    service.clear_provider_data("sub2api@2").unwrap();

    let state = service.state();
    assert!(state.providers["sub2api"].snapshot.is_some());
    assert!(state.providers["sub2api@2"].snapshot.is_none());
    assert_eq!(state.providers["sub2api@2"].source, SnapshotSource::None);
    assert!(storage.load_snapshot("sub2api").unwrap().is_some());
    assert!(storage.load_snapshot("sub2api@2").unwrap().is_none());
}

#[tokio::test]
async fn a_refresh_using_previous_credentials_cannot_repopulate_a_cleared_connection() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("cache.db")).unwrap());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(GatedProvider {
        id: "sub2api",
        gate: gate.clone(),
        calls: calls.clone(),
        active: Default::default(),
        maximum: Default::default(),
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let service = Arc::new(ProviderService::new(registry, storage.clone()));
    let worker = service.clone();
    let running = tokio::spawn(async move { worker.refresh("sub2api", true).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    service.clear_provider_data("sub2api").unwrap();
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(running.await.unwrap().snapshot.is_none());
    assert!(storage.load_snapshot("sub2api").unwrap().is_none());
    assert!(service.refresh("sub2api", true).await.snapshot.is_some());
}

#[test]
fn successful_refresh_switches_account_name_and_cache_without_a_restart() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let provider = Arc::new(AccountSwitchProvider {
        identity: "bbbbbbbb22222222".into(),
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let (settings, _) = SettingsService::new_deferred(storage.clone(), registry.clone()).unwrap();
    let settings = Arc::new(settings);
    settings
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap();
    let mut renamed = settings.get();
    renamed.provider_names.insert("codex".into(), "GPT".into());
    settings
        .update_from_view(
            renamed,
            settings.settings_revision(),
            settings.account_revision(),
        )
        .unwrap();

    let service = Arc::new(ProviderService::new_with_settings(
        registry,
        storage.clone(),
        settings.clone(),
    ));
    let state = refresh_with_test_timeout(&service, "codex", true);

    assert!(state.error.is_none());
    assert!(!settings.get().provider_names.contains_key("codex"));
    assert!(storage
        .load_snapshot_for_identity(
            "codex",
            crate::providers::CacheIdentity::Resolved("bbbbbbbb22222222"),
        )
        .unwrap()
        .is_some());
    assert!(storage
        .load_snapshot_for_identity(
            "codex",
            crate::providers::CacheIdentity::Resolved("aaaaaaaa11111111"),
        )
        .unwrap()
        .is_none());
}

#[test]
fn snapshot_contract_rejects_wrong_provider_and_unknown_sources() {
    let provider = SlowProvider {
        id: "contract",
        calls: Arc::new(AtomicUsize::new(0)),
        active: Arc::new(AtomicUsize::new(0)),
        maximum: Arc::new(AtomicUsize::new(0)),
        delay: Duration::ZERO,
    };
    let registry = ProviderRegistry::from_definitions(vec![provider.definition()]).unwrap();
    let snapshot = provider.refresh().unwrap();

    let mut wrong_provider = snapshot.clone();
    wrong_provider.provider_id = "other".into();
    assert!(validate_snapshot(&registry, "contract", wrong_provider).is_err());

    let mut unknown_source = snapshot;
    unknown_source.quotas.push(crate::models::QuotaWindow {
        clock_offset_seconds: None,
        id: "unknown".into(),
        label: "Unknown".into(),
        used_percent: 0.0,
        resets_at: None,
        period_seconds: 1,
        format: crate::models::QuotaFormat::Percent,
        used_value: None,
        limit_value: None,
        unit: None,
        estimated: false,
        source_note: None,
    });
    assert!(validate_snapshot(&registry, "contract", unknown_source).is_err());
}

#[test]
fn snapshot_contract_validates_dynamic_metric_metadata_and_unique_ids() {
    let definition = ProviderDefinition {
        id: "dynamic".into(),
        display_name: "Dynamic".into(),
        short_name: "D".into(),
        fallback_enabled: true,
        local_usage_source_note: None,
        links: Vec::new(),
        metrics: vec![
            MetricDefinition::quota(
                "dynamic.searches",
                "Web Searches",
                "searches",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::status(
                "dynamic.extra",
                "Extra Usage",
                "extra",
                true,
                MetricSection::OnDemand,
                false,
                "E",
            ),
        ],
    };
    let registry = ProviderRegistry::from_definitions(vec![definition]).unwrap();
    let mut snapshot = test_snapshot("dynamic");
    snapshot.quotas.push(QuotaWindow {
        clock_offset_seconds: None,
        id: "searches".into(),
        label: "Web Searches".into(),
        used_percent: 25.0,
        resets_at: None,
        period_seconds: 86_400,
        format: QuotaFormat::Count,
        used_value: Some(25.0),
        limit_value: Some(100.0),
        unit: Some("searches".into()),
        estimated: false,
        source_note: None,
    });
    snapshot.status_metrics.push(StatusMetric {
        id: "extra".into(),
        label: "Extra Usage".into(),
        text: "2500 cap".into(),
        tone: StatusTone::Positive,
        subtitle: None,
    });

    assert!(validate_snapshot(&registry, "dynamic", snapshot.clone()).is_ok());

    let mut missing_unit = snapshot.clone();
    missing_unit.quotas[0].unit = Some(" ".into());
    assert!(validate_snapshot(&registry, "dynamic", missing_unit).is_err());

    let mut missing_estimate_source = snapshot.clone();
    missing_estimate_source.quotas[0].estimated = true;
    assert!(validate_snapshot(&registry, "dynamic", missing_estimate_source).is_err());

    let mut duplicate_quota = snapshot.clone();
    duplicate_quota
        .quotas
        .push(duplicate_quota.quotas[0].clone());
    assert!(validate_snapshot(&registry, "dynamic", duplicate_quota).is_err());

    let mut unknown_status = snapshot;
    unknown_status.status_metrics[0].id = "unknown".into();
    assert!(validate_snapshot(&registry, "dynamic", unknown_status).is_err());
}

#[test]
fn enabled_providers_refresh_in_parallel() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let providers = ["claude", "antigravity"]
        .into_iter()
        .map(|id| {
            Arc::new(SlowProvider {
                id,
                calls: calls.clone(),
                active: active.clone(),
                maximum: maximum.clone(),
                delay: Duration::from_millis(75),
            }) as Arc<dyn UsageProvider>
        })
        .collect();
    let registry = Arc::new(ProviderRegistry::new(providers).unwrap());
    let service = Arc::new(ProviderService::new(registry, storage));

    tauri::async_runtime::block_on(
        service.refresh_enabled(&["claude".into(), "antigravity".into()], true),
    );

    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert!(service.state().last_full_refresh_at.is_none());

    let completed = tauri::async_runtime::block_on(
        service.refresh_all(&["claude".into(), "antigravity".into()], true),
    );
    assert!(completed.last_full_refresh_at.is_some());
}

#[test]
fn timed_out_refresh_quarantines_the_late_worker_and_releases_waiters() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let mut cached = test_snapshot("slow");
    cached.plan = Some("cached".into());
    storage.save_snapshot(&cached).unwrap();

    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let provider = Arc::new(GatedProvider {
        id: "slow",
        calls: calls.clone(),
        active: active.clone(),
        maximum: maximum.clone(),
        gate: gate.clone(),
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let service = Arc::new(ProviderService::with_refresh_timeout(
        registry,
        storage.clone(),
        Duration::from_millis(250),
    ));

    let (timed_out, queued) = tauri::async_runtime::block_on(async {
        let first_service = service.clone();
        let first =
            tauri::async_runtime::spawn(async move { first_service.refresh("slow", true).await });
        tokio::time::timeout(TEST_WAIT_TIMEOUT, async {
            while active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first refresh should start");
        let queued_service = service.clone();
        let queued =
            tauri::async_runtime::spawn(async move { queued_service.refresh("slow", true).await });
        tokio::time::timeout(TEST_WAIT_TIMEOUT, async {
            loop {
                let queued = service
                    .refresh_flights
                    .get("slow")
                    .and_then(|flight| flight.state.lock().ok())
                    .is_some_and(|state| state.requested_generation >= 2);
                if queued && active.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("forced follow-up should be queued before timeout");

        tokio::time::timeout(Duration::from_secs(1), async {
            (first.await.unwrap(), queued.await.unwrap())
        })
        .await
        .expect("timeout should release active and queued waiters")
    });

    for state in [&timed_out, &queued] {
        assert_eq!(state.error.as_deref(), Some("Provider refresh timed out."));
        assert_eq!(state.error_kind, Some(ProviderErrorKind::Network));
        assert!(!state.stale);
        assert_eq!(
            state
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.plan.as_deref()),
            Some("cached")
        );
    }
    {
        let flight_state = service.refresh_flights["slow"].state.lock().unwrap();
        assert!(flight_state.runner_active);
        assert_eq!(flight_state.attempt_generation, None);
        assert_eq!(flight_state.completed_generation, 2);
        assert_eq!(flight_state.requested_generation, 2);
    }

    let quarantined = tauri::async_runtime::block_on(async {
        let tasks = (0..32)
            .map(|_| {
                let service = service.clone();
                tauri::async_runtime::spawn(async move { service.refresh("slow", true).await })
            })
            .collect::<Vec<_>>();
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut states = Vec::with_capacity(tasks.len());
            for task in tasks {
                states.push(task.await.unwrap());
            }
            states
        })
        .await
        .expect("quarantined refresh calls should return immediately")
    });
    assert!(quarantined
        .iter()
        .all(|state| state.error == timed_out.error));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    assert_eq!(active.load(Ordering::SeqCst), 1);
    assert!(!refresh_runner_is_idle(&service, "slow"));

    let (released, signal) = &*gate;
    *released.lock().unwrap() = true;
    signal.notify_all();

    wait_until("timed-out refresh worker should drain", || {
        active.load(Ordering::SeqCst) == 0 && refresh_runner_is_idle(&service, "slow")
    });
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert!(refresh_runner_is_idle(&service, "slow"));
    assert_eq!(
        service
            .state()
            .providers
            .get("slow")
            .and_then(|state| state.snapshot.as_ref())
            .and_then(|snapshot| snapshot.plan.as_deref()),
        Some("cached")
    );
    assert_eq!(
        storage
            .load_snapshot("slow")
            .unwrap()
            .and_then(|snapshot| snapshot.plan),
        Some("cached".into())
    );

    let refreshed = refresh_with_test_timeout(&service, "slow", true);
    assert!(refreshed.error.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    assert_eq!(
        refreshed
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.plan.as_deref()),
        Some("live-2")
    );
}

#[test]
fn cancelled_refresh_keeps_single_flight_until_blocking_worker_finishes() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SlowProvider {
        id: "cancelled",
        calls: calls.clone(),
        active: active.clone(),
        maximum: maximum.clone(),
        delay: Duration::from_millis(180),
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let service = Arc::new(ProviderService::with_refresh_timeout(
        registry,
        storage,
        Duration::from_secs(1),
    ));

    let first_service = service.clone();
    let first =
        tauri::async_runtime::spawn(async move { first_service.refresh("cancelled", true).await });
    wait_until("cancelled refresh worker should start", || {
        active.load(Ordering::SeqCst) != 0
    });
    assert_eq!(active.load(Ordering::SeqCst), 1);

    first.abort();
    let _ = tauri::async_runtime::block_on(first);
    assert!(service
        .state()
        .providers
        .get("cancelled")
        .is_some_and(|state| state.refreshing));

    let retry = refresh_with_test_timeout(&service, "cancelled", true);
    assert!(!retry.refreshing);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert!(retry.error.is_none());
}

#[test]
fn concurrent_forced_waiters_coalesce_one_follow_up() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SlowProvider {
        id: "lifecycle",
        calls: calls.clone(),
        active: active.clone(),
        maximum: maximum.clone(),
        delay: Duration::from_millis(80),
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let service = Arc::new(ProviderService::new(registry, storage));

    tauri::async_runtime::block_on(async {
        tokio::time::timeout(TEST_WAIT_TIMEOUT, async {
            let first_service = service.clone();
            let first = tauri::async_runtime::spawn(async move {
                first_service.refresh("lifecycle", true).await
            });
            tokio::time::timeout(TEST_WAIT_TIMEOUT, async {
                while active.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("first refresh should start");
            let second_service = service.clone();
            let second = tauri::async_runtime::spawn(async move {
                second_service.refresh("lifecycle", true).await
            });
            let third_service = service.clone();
            let third = tauri::async_runtime::spawn(async move {
                third_service.refresh("lifecycle", true).await
            });
            assert!(first.await.unwrap().error.is_none());
            assert!(second.await.unwrap().error.is_none());
            assert!(third.await.unwrap().error.is_none());
        })
        .await
        .expect("coalesced refreshes should not deadlock");
    });

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
}

#[test]
fn forced_refresh_after_credential_change_returns_authoritative_snapshot() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let credential = Arc::new(Mutex::new(Some("old".to_owned())));
    let calls = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CredentialProvider {
        id: "credential",
        credential: credential.clone(),
        calls: calls.clone(),
        active: active.clone(),
        maximum: maximum.clone(),
        delay: Duration::from_millis(60),
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let service = Arc::new(ProviderService::new(registry, storage));

    tauri::async_runtime::block_on(async {
        tokio::time::timeout(TEST_WAIT_TIMEOUT, async {
            let old_service = service.clone();
            let old_refresh = tauri::async_runtime::spawn(async move {
                old_service.refresh("credential", true).await
            });
            tokio::time::timeout(TEST_WAIT_TIMEOUT, async {
                while calls.load(Ordering::SeqCst) < 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("old-credential refresh should start");
            *credential.lock().unwrap() = Some("saved".to_owned());
            let saved = service.refresh("credential", true).await;
            assert_eq!(
                saved.snapshot.and_then(|snapshot| snapshot.plan),
                Some("saved".to_owned())
            );
            assert!(old_refresh.await.unwrap().error.is_none());

            let saved_service = service.clone();
            let saved_refresh = tauri::async_runtime::spawn(async move {
                saved_service.refresh("credential", true).await
            });
            tokio::time::timeout(TEST_WAIT_TIMEOUT, async {
                while calls.load(Ordering::SeqCst) < 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("saved-credential refresh should start");
            *credential.lock().unwrap() = None;
            let deleted = service.refresh("credential", true).await;
            assert_eq!(deleted.snapshot.and_then(|snapshot| snapshot.plan), None);
            assert!(saved_refresh.await.unwrap().error.is_none());
        })
        .await
        .expect("credential refreshes should not deadlock");
    });

    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
}

#[test]
fn progress_reports_every_provider_completion() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let providers = [
        ("slow", Duration::from_millis(160)),
        ("fast", Duration::ZERO),
    ]
    .into_iter()
    .map(|(id, delay)| {
        Arc::new(SlowProvider {
            id,
            calls: calls.clone(),
            active: active.clone(),
            maximum: maximum.clone(),
            delay,
        }) as Arc<dyn UsageProvider>
    })
    .collect();
    let registry = Arc::new(ProviderRegistry::new(providers).unwrap());
    let service = Arc::new(ProviderService::with_refresh_timeout(
        registry,
        storage.clone(),
        Duration::from_millis(40),
    ));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let observed = observations.clone();

    let final_state = tauri::async_runtime::block_on(service.refresh_enabled_with_progress(
        &["slow".into(), "fast".into()],
        true,
        move |state| {
            observed.lock().unwrap().push(state.clone());
        },
    ));

    let observations = observations.lock().unwrap();
    assert_eq!(observations.len(), 2);
    let completed = observations.last().unwrap();
    assert!(completed
        .providers
        .get("fast")
        .and_then(|state| state.snapshot.as_ref())
        .is_some());
    assert_eq!(
        completed
            .providers
            .get("slow")
            .and_then(|state| state.error.as_deref()),
        Some("Provider refresh timed out.")
    );
    assert!(storage.load_snapshot("fast").unwrap().is_some());
    assert_eq!(
        final_state
            .providers
            .get("slow")
            .and_then(|state| state.error.as_deref()),
        Some("Provider refresh timed out.")
    );
    drop(observations);

    wait_until("timed-out progress worker should drain", || {
        active.load(Ordering::SeqCst) == 0 && refresh_runner_is_idle(&service, "slow")
    });
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[test]
fn failed_provider_is_backed_off_but_force_and_expiry_retry() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SequenceProvider {
        id: "failing",
        calls: calls.clone(),
        failures_before_success: usize::MAX,
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let service = Arc::new(ProviderService::new(registry, storage));

    tauri::async_runtime::block_on(service.refresh("failing", false));
    tauri::async_runtime::block_on(service.refresh("failing", false));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    tauri::async_runtime::block_on(service.refresh("failing", true));
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    service.last_failed_refresh.lock().unwrap().insert(
        "failing".into(),
        std::time::Instant::now()
            .checked_sub(FAILURE_RETRY_BACKOFF)
            .unwrap(),
    );
    tauri::async_runtime::block_on(service.refresh("failing", false));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn successful_retry_clears_failure_backoff() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(SequenceProvider {
        id: "recovering",
        calls,
        failures_before_success: 1,
    }) as Arc<dyn UsageProvider>;
    let registry = Arc::new(ProviderRegistry::new(vec![provider]).unwrap());
    let service = Arc::new(ProviderService::new(registry, storage));

    tauri::async_runtime::block_on(service.refresh("recovering", false));
    assert!(service
        .last_failed_refresh
        .lock()
        .unwrap()
        .contains_key("recovering"));

    let recovered = tauri::async_runtime::block_on(service.refresh("recovering", true));
    assert!(recovered.error.is_none());
    assert!(!service
        .last_failed_refresh
        .lock()
        .unwrap()
        .contains_key("recovering"));
}

/// Exercises the service contract shared by all local-history providers, without credentials,
/// real home directories, or network calls.
struct IndependentUsageProvider {
    id: &'static str,
    online_failure: AtomicUsize,
    local_failure: AtomicUsize,
    tokens: AtomicUsize,
    online_gate: Mutex<bool>,
    local_gate: Mutex<bool>,
    gate_changed: Condvar,
}

impl IndependentUsageProvider {
    fn new(id: &'static str) -> Self {
        Self {
            id,
            online_failure: AtomicUsize::new(0),
            local_failure: AtomicUsize::new(0),
            tokens: AtomicUsize::new(42),
            online_gate: Mutex::new(false),
            local_gate: Mutex::new(false),
            gate_changed: Condvar::new(),
        }
    }
    fn release(&self) {
        *self.online_gate.lock().unwrap() = false;
        self.gate_changed.notify_all();
    }
}

impl UsageProvider for IndependentUsageProvider {
    fn supports_local_usage(&self) -> bool {
        true
    }
    fn definition(&self) -> ProviderDefinition {
        let mut definition = test_definition(self.id);
        definition.local_usage_source_note = Some("Test device logs".into());
        definition
    }
    fn has_local_credentials(&self) -> bool {
        false
    }
    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        let gate = self.online_gate.lock().unwrap();
        let _ = self
            .gate_changed
            .wait_timeout_while(gate, TEST_WAIT_TIMEOUT, |held| *held)
            .unwrap();
        match self.online_failure.load(Ordering::SeqCst) {
            0 => Ok(test_snapshot(self.id)),
            1 => Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "Login expired",
            )),
            _ => Err(ProviderError::new(ProviderErrorKind::Network, "Offline")),
        }
    }
    fn refresh_local_usage(&self) -> Option<Result<crate::providers::ScannedUsage, ProviderError>> {
        let gate = self.local_gate.lock().unwrap();
        let _ = self
            .gate_changed
            .wait_timeout_while(gate, TEST_WAIT_TIMEOUT, |held| *held)
            .unwrap();
        Some(if self.local_failure.load(Ordering::SeqCst) != 0 {
            Err(ProviderError::new(
                ProviderErrorKind::LocalData,
                "Logs unreadable",
            ))
        } else {
            Ok(crate::providers::ScannedUsage {
                history: UsageHistory {
                    daily: vec![crate::models::DailyUsage {
                        date: Utc::now().date_naive().to_string(),
                        tokens: self.tokens.load(Ordering::SeqCst) as u64,
                        estimated_cost_usd: Some(0.5),
                        estimate_complete: true,
                    }],
                    ..Default::default()
                },
                warnings: vec!["Local attribution warning".into()],
            })
        })
    }
}

fn independent_service(
    provider: Arc<IndependentUsageProvider>,
    storage: Arc<Storage>,
) -> Arc<ProviderService> {
    Arc::new(ProviderService::new(
        Arc::new(ProviderRegistry::new(vec![provider]).unwrap()),
        storage,
    ))
}

#[test]
fn local_history_refreshes_without_login_for_every_local_card_and_survives_restart() {
    for id in [
        "antigravity",
        "codex",
        "claude",
        "grok",
        "kimi",
        "minimax",
        "zai",
        "opencode",
    ] {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
        let provider = Arc::new(IndependentUsageProvider::new(id));
        provider.online_failure.store(1, Ordering::SeqCst);
        let service = independent_service(provider.clone(), storage.clone());
        let state = refresh_with_test_timeout(&service, id, true);
        assert_eq!(state.error_kind, Some(ProviderErrorKind::Authentication));
        let snapshot = state.snapshot.unwrap();
        assert_eq!(
            snapshot
                .usage_histories
                .local_device
                .as_ref()
                .unwrap()
                .daily[0]
                .tokens,
            42
        );
        assert!(snapshot
            .usage_histories
            .local_device_status
            .as_ref()
            .unwrap()
            .refreshed_at
            .is_some());
        assert_eq!(snapshot.refreshed_at, chrono::DateTime::UNIX_EPOCH);
        assert!(snapshot.quotas.is_empty());
        assert!(snapshot.usage_histories.account.is_none());
        drop(service);
        let restarted = independent_service(provider, storage);
        assert_eq!(
            restarted.state().providers[id]
                .snapshot
                .as_ref()
                .unwrap()
                .usage_histories,
            snapshot.usage_histories
        );
    }
}

#[test]
fn each_refresh_part_keeps_its_own_cache_and_timestamp_and_recovers() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
    let provider = Arc::new(IndependentUsageProvider::new("codex"));
    let service = independent_service(provider.clone(), storage.clone());
    let first = refresh_with_test_timeout(&service, "codex", true)
        .snapshot
        .unwrap();
    provider.online_failure.store(2, Ordering::SeqCst);
    provider.tokens.store(84, Ordering::SeqCst);
    let second = refresh_with_test_timeout(&service, "codex", true);
    assert_eq!(second.error_kind, Some(ProviderErrorKind::Network));
    let second = second.snapshot.unwrap();
    assert_eq!(second.refreshed_at, first.refreshed_at);
    assert_eq!(
        second.usage_histories.local_device.as_ref().unwrap().daily[0].tokens,
        84
    );
    let last_local = second
        .usage_histories
        .local_device_status
        .as_ref()
        .unwrap()
        .refreshed_at;

    provider.online_failure.store(0, Ordering::SeqCst);
    provider.local_failure.store(1, Ordering::SeqCst);
    let third = refresh_with_test_timeout(&service, "codex", true);
    assert!(third.error.is_none());
    let third = third.snapshot.unwrap();
    assert!(third.refreshed_at >= second.refreshed_at);
    assert_eq!(
        third.usage_histories.local_device,
        second.usage_histories.local_device
    );
    let status = third.usage_histories.local_device_status.as_ref().unwrap();
    assert_eq!(status.refreshed_at, last_local);
    assert!(status.error.as_ref().unwrap().contains("Logs unreadable"));
    assert_eq!(status.warnings, ["Local attribution warning"]);
    assert!(storage
        .load_local_usage("codex")
        .unwrap()
        .unwrap()
        .account
        .is_none());

    provider.local_failure.store(0, Ordering::SeqCst);
    provider.tokens.store(0, Ordering::SeqCst);
    let recovered = refresh_with_test_timeout(&service, "codex", true)
        .snapshot
        .unwrap();
    assert!(recovered
        .usage_histories
        .local_device_status
        .unwrap()
        .error
        .is_none());
    assert_eq!(
        recovered.usage_histories.local_device.unwrap().daily[0].tokens,
        0
    );
}

#[test]
fn local_history_is_published_before_online_finishes_and_late_results_cannot_undo_invalidation() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
    let provider = Arc::new(IndependentUsageProvider::new("codex"));
    *provider.online_gate.lock().unwrap() = true;
    let service = independent_service(provider.clone(), storage.clone());
    let runner = {
        let service = service.clone();
        thread::spawn(move || refresh_with_test_timeout(&service, "codex", true))
    };
    wait_until(
        "local history should publish before online completes",
        || {
            service.state().providers["codex"]
                .snapshot
                .as_ref()
                .is_some_and(|s| s.usage_histories.local_device.is_some())
        },
    );
    assert!(service.state().providers["codex"].refreshing);
    service.clear_provider_data("codex").unwrap();
    provider.release();
    runner.join().unwrap();
    assert!(service.state().providers["codex"].snapshot.is_none());
    assert!(storage.load_local_usage("codex").unwrap().is_none());
    assert!(storage
        .load_snapshot_for_identity("codex", crate::providers::CacheIdentity::Unscoped)
        .unwrap()
        .is_none());
}

#[test]
fn online_timeout_keeps_new_local_history_without_freshening_cached_quotas() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
    let provider = Arc::new(IndependentUsageProvider::new("codex"));
    *provider.online_gate.lock().unwrap() = true;
    let old = test_snapshot("codex");
    storage.save_snapshot(&old).unwrap();
    let service = Arc::new(ProviderService::with_refresh_timeout(
        Arc::new(ProviderRegistry::new(vec![provider.clone()]).unwrap()),
        storage,
        Duration::from_millis(100),
    ));
    let state = refresh_with_test_timeout(&service, "codex", true);
    provider.release();
    assert_eq!(state.error_kind, Some(ProviderErrorKind::Network));
    let snapshot = state.snapshot.unwrap();
    assert_eq!(snapshot.refreshed_at, old.refreshed_at);
    assert_eq!(
        snapshot.usage_histories.local_device.unwrap().daily[0].tokens,
        42
    );
}

#[test]
fn local_timeout_does_not_block_online_result_or_publish_late_local_data() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
    let provider = Arc::new(IndependentUsageProvider::new("codex"));
    *provider.local_gate.lock().unwrap() = true;
    let service = Arc::new(ProviderService::with_refresh_timeout(
        Arc::new(ProviderRegistry::new(vec![provider.clone()]).unwrap()),
        storage.clone(),
        Duration::from_millis(300),
    ));
    let runner = {
        let service = service.clone();
        thread::spawn(move || refresh_with_test_timeout(&service, "codex", true))
    };
    let state = runner.join().unwrap();
    assert!(state.error.is_none());
    let snapshot = state.snapshot.unwrap();
    assert!(snapshot.usage_histories.local_device.is_none());
    assert!(snapshot
        .usage_histories
        .local_device_status
        .unwrap()
        .error
        .unwrap()
        .contains("timed out"));
    *provider.local_gate.lock().unwrap() = false;
    provider.gate_changed.notify_all();
    wait_until("local worker drains", || {
        !service.refresh_flights["codex"]
            .state
            .lock()
            .unwrap()
            .runner_active
    });
    assert!(service.state().providers["codex"]
        .snapshot
        .as_ref()
        .unwrap()
        .usage_histories
        .local_device
        .is_none());
    assert!(storage
        .load_local_usage("codex")
        .unwrap()
        .unwrap()
        .local_device
        .is_none());
}

#[test]
fn local_refresh_timestamps_do_not_speed_up_adaptive_polling() {
    let mut before = UsageViewState::default();
    let mut snapshot = test_snapshot("codex");
    snapshot.usage_histories.local_device = Some(UsageHistory::default());
    snapshot.usage_histories.local_device_status = Some(crate::models::LocalUsageStatus {
        refreshed_at: Some(Utc::now() - chrono::Duration::minutes(1)),
        ..Default::default()
    });
    before
        .providers
        .insert("codex".into(), ProviderViewState::from_cache(snapshot));
    let mut after = before.clone();
    after
        .providers
        .get_mut("codex")
        .unwrap()
        .snapshot
        .as_mut()
        .unwrap()
        .usage_histories
        .local_device_status
        .as_mut()
        .unwrap()
        .refreshed_at = Some(Utc::now());
    assert!(!usage_data_changed(&before, &after, &["codex".into()]));
}

struct ReviewRefreshProvider {
    inner: IndependentUsageProvider,
    local_capability: bool,
    calls: AtomicUsize,
    account: bool,
    online_done: AtomicUsize,
}
impl UsageProvider for ReviewRefreshProvider {
    fn cache_identity(&self) -> crate::providers::CacheIdentity<'_> {
        crate::providers::CacheIdentity::Unresolved
    }
    fn definition(&self) -> ProviderDefinition {
        self.inner.definition()
    }
    fn has_local_credentials(&self) -> bool {
        true
    }
    fn supports_local_usage(&self) -> bool {
        self.local_capability
    }
    fn refresh_local_usage(&self) -> Option<Result<crate::providers::ScannedUsage, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.local_capability,
            "unsupported local worker must never run"
        );
        self.inner.refresh_local_usage()
    }
    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        self.inner.refresh()
    }
    fn refresh_for_service(&self) -> Result<ProviderRefresh, ProviderError> {
        let mut snapshot = self.refresh()?;
        snapshot.plan = Some("new-account-plan".into());
        snapshot.usage_histories.account = Some(UsageHistory {
            unknown_models: vec!["new account history".into()],
            ..Default::default()
        });
        self.online_done.store(1, Ordering::SeqCst);
        Ok(ProviderRefresh {
            snapshot,
            cache_identity: Some("new-identity".into()),
            account: self.account.then(|| AccountRefresh {
                family: "codex",
                provider_id: "codex",
                identity: "new-identity".into(),
            }),
        })
    }
}
fn review_provider(local_capability: bool, account: bool) -> Arc<ReviewRefreshProvider> {
    Arc::new(ReviewRefreshProvider {
        inner: IndependentUsageProvider::new("codex"),
        local_capability,
        account,
        calls: AtomicUsize::new(0),
        online_done: AtomicUsize::new(0),
    })
}

#[test]
fn completed_online_result_waits_for_account_activation_and_keeps_new_account_history() {
    for fail_activation in [false, true] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.db");
        let storage = Arc::new(Storage::open(&path).unwrap());
        let provider = review_provider(true, true);
        *provider.inner.local_gate.lock().unwrap() = true;
        let registry = Arc::new(ProviderRegistry::new(vec![provider.clone()]).unwrap());
        let (settings, _) =
            SettingsService::new_deferred(storage.clone(), registry.clone()).unwrap();
        let settings = Arc::new(settings);
        settings
            .activate_account("codex", "codex", "old-identity")
            .unwrap();
        let mut old = test_snapshot("codex");
        old.plan = Some("old-account-plan".into());
        old.usage_histories.account = Some(UsageHistory {
            unknown_models: vec!["old account history".into()],
            ..Default::default()
        });
        storage
            .save_snapshot_for_identity(&old, Some("old-identity"))
            .unwrap();
        if fail_activation {
            rusqlite::Connection::open(&path).unwrap().execute_batch(
                "CREATE TRIGGER fail_account BEFORE INSERT ON provider_account_records BEGIN SELECT RAISE(FAIL, 'activation blocked'); END;"
            ).unwrap();
        }
        let service = Arc::new(ProviderService::new_with_settings(
            registry,
            storage.clone(),
            settings,
        ));
        let runner = {
            let service = service.clone();
            thread::spawn(move || refresh_with_test_timeout(&service, "codex", true))
        };
        wait_until("online worker should finish", || {
            provider.online_done.load(Ordering::SeqCst) == 1
        });
        assert_eq!(
            service.state().providers["codex"]
                .snapshot
                .as_ref()
                .unwrap()
                .plan,
            old.plan
        );
        assert!(storage
            .load_snapshot_for_identity(
                "codex",
                crate::providers::CacheIdentity::Resolved("new-identity")
            )
            .unwrap()
            .is_none());
        *provider.inner.local_gate.lock().unwrap() = false;
        provider.inner.gate_changed.notify_all();
        let state = runner.join().unwrap();
        let snapshot = state.snapshot.unwrap();
        if fail_activation {
            assert_eq!(state.error_kind, Some(ProviderErrorKind::Storage));
            assert_eq!(snapshot.plan, old.plan);
            assert_eq!(
                snapshot.usage_histories.account,
                old.usage_histories.account
            );
            assert!(storage
                .load_snapshot_for_identity(
                    "codex",
                    crate::providers::CacheIdentity::Resolved("new-identity")
                )
                .unwrap()
                .is_none());
        } else {
            assert!(state.error.is_none());
            assert_eq!(
                snapshot.usage_histories.account.unwrap().unknown_models,
                ["new account history"]
            );
            let saved = storage
                .load_snapshot_for_identity(
                    "codex",
                    crate::providers::CacheIdentity::Resolved("new-identity"),
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                saved.usage_histories.account.unwrap().unknown_models,
                ["new account history"]
            );
        }
    }
}

#[test]
fn source_note_without_capability_never_launches_or_caches_a_local_worker() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
    let provider = review_provider(false, false);
    *provider.inner.online_gate.lock().unwrap() = true;
    let registry = Arc::new(ProviderRegistry::new(vec![provider.clone()]).unwrap());
    let service = Arc::new(ProviderService::with_refresh_timeout(
        registry.clone(),
        storage.clone(),
        Duration::from_millis(50),
    ));
    let state = refresh_with_test_timeout(&service, "codex", true);
    provider.inner.release();
    assert_eq!(state.error_kind, Some(ProviderErrorKind::Network));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(storage.load_local_usage("codex").unwrap().is_none());
    // Also ignore bad records left by the earlier unconditional-worker implementation.
    let bad = UsageHistories {
        local_device_status: Some(crate::models::LocalUsageStatus {
            error: Some("stale error".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    storage.save_local_usage("codex", &bad).unwrap();
    let restarted = ProviderService::new(registry, storage);
    assert!(restarted.state().providers["codex"].snapshot.is_none());
}

#[test]
fn first_failure_has_no_successful_timestamp_and_is_not_outdated() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
    let provider = Arc::new(IndependentUsageProvider::new("codex"));
    provider.online_failure.store(1, Ordering::SeqCst);
    provider.local_failure.store(1, Ordering::SeqCst);
    let service = independent_service(provider, storage);
    let state = refresh_with_test_timeout(&service, "codex", true);
    assert!(state.error.is_some());
    assert!(!state.stale);
    let history = state.snapshot.unwrap().usage_histories;
    assert!(history.local_device.is_none());
    assert!(history.local_device_status.unwrap().refreshed_at.is_none());
}

#[test]
fn legacy_local_history_survives_online_commit_and_failed_scan() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("state.db")).unwrap());
    let provider = Arc::new(IndependentUsageProvider::new("codex"));
    provider.local_failure.store(1, Ordering::SeqCst);
    let mut old = test_snapshot("codex");
    old.usage_histories.local_device = Some(UsageHistory {
        unknown_models: vec!["legacy history".into()],
        ..Default::default()
    });
    storage.save_snapshot(&old).unwrap();
    let service = independent_service(provider, storage);
    // Commit online data first to exercise migration even when scan order changes.
    service.apply_refresh_result(
        "codex",
        Ok(ProviderRefresh {
            snapshot: test_snapshot("codex"),
            cache_identity: None,
            account: None,
        }),
    );
    let state = refresh_with_test_timeout(&service, "codex", true);
    let history = state.snapshot.unwrap().usage_histories;
    assert_eq!(history.local_device, old.usage_histories.local_device);
    assert_eq!(
        history.local_device_status.unwrap().refreshed_at,
        Some(old.refreshed_at)
    );
}

#[test]
fn scan_and_cache_write_failures_preserve_the_scan_error() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.db");
    let storage = Arc::new(Storage::open(&path).unwrap());
    let provider = Arc::new(IndependentUsageProvider::new("codex"));
    provider.local_failure.store(1, Ordering::SeqCst);
    rusqlite::Connection::open(&path).unwrap().execute_batch(
        "CREATE TRIGGER fail_cache BEFORE INSERT ON local_usage_snapshots BEGIN SELECT RAISE(FAIL, 'cache blocked'); END;"
    ).unwrap();
    let service = independent_service(provider, storage);
    let state = refresh_with_test_timeout(&service, "codex", true);
    let message = state
        .snapshot
        .unwrap()
        .usage_histories
        .local_device_status
        .unwrap()
        .error
        .unwrap();
    assert!(message.contains("Logs unreadable"));
    assert!(message.contains("cache also could not be saved"));
    assert!(!message.contains("Local usage refreshed"));
}

#[test]
fn malformed_local_cache_does_not_hide_a_valid_online_snapshot() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.db");
    let storage = Arc::new(Storage::open(&path).unwrap());
    let old = test_snapshot("codex");
    storage.save_snapshot(&old).unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO local_usage_snapshots VALUES ('codex', 'not json')",
            [],
        )
        .unwrap();
    let service = independent_service(Arc::new(IndependentUsageProvider::new("codex")), storage);
    assert_eq!(
        service.state().providers["codex"].snapshot.as_ref(),
        Some(&old)
    );
}
