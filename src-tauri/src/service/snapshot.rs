use std::time::Duration;

use chrono::Utc;

use crate::{
    models::{
        MetricSource, ProviderErrorKind, ProviderSnapshot, ProviderViewState, QuotaWindow,
        SnapshotSource,
    },
    policy::{FAST_REFRESH_INTERVAL, REFRESH_INTERVAL, STALE_AFTER},
    providers::{ProviderError, ProviderRegistry},
};

use super::UsageViewState;

const FAST_REFRESH_UNCHANGED_ROUNDS: u8 = 2;
const RESET_JITTER_TOLERANCE_SECONDS: i64 = 5;
const IDLE_RESET_WINDOW_TOLERANCE_SECONDS: i64 = 90;
const IDLE_RESET_DRIFT_TOLERANCE_SECONDS: i64 = 15;

pub(super) fn normalize_quota_availability(
    definition: &crate::models::ProviderDefinition,
    snapshot: &mut ProviderSnapshot,
) {
    normalize_unstarted_sessions(definition, snapshot);
    for account in snapshot.accounts.iter_mut().flatten() {
        normalize_unstarted_sessions(definition, &mut account.snapshot);
    }
}

/// A session that starts on first use is reported with 0% and no reset until then. Each
/// provider's mapper drops placeholder resets of unused windows; here a reset that has
/// already passed is dropped too, so the session reads as not started rather than as
/// due to reset. Such sessions are kept: a missing quota means the account has no such
/// limit, which differs from a limit that has not started.
fn normalize_unstarted_sessions(
    definition: &crate::models::ProviderDefinition,
    snapshot: &mut ProviderSnapshot,
) {
    let refreshed_at = snapshot.refreshed_at;
    for quota in &mut snapshot.quotas {
        let starts_on_use = definition.metrics.iter().any(|metric| {
            metric.source.session_window() && metric.source.source_id() == Some(quota.id.as_str())
        });
        let has_active_reset = quota.resets_at.is_some_and(|reset| {
            reset
                > refreshed_at + chrono::Duration::seconds(quota.clock_offset_seconds.unwrap_or(0))
        });
        if starts_on_use && quota.used_percent <= 0.0 && !has_active_reset {
            quota.resets_at = None;
        }
    }
}

pub(super) fn validate_snapshot(
    registry: &ProviderRegistry,
    provider_id: &str,
    mut snapshot: ProviderSnapshot,
) -> Result<ProviderSnapshot, ProviderError> {
    let Some(definition) = registry.definition(provider_id) else {
        return Err(snapshot_contract_error());
    };
    if snapshot.provider_id != provider_id {
        return Err(snapshot_contract_error());
    }

    if let Some(accounts) = &mut snapshot.accounts {
        let mut ids = std::collections::HashSet::new();
        for account in accounts {
            if !ids.insert(account.id.clone()) || account.snapshot.accounts.is_some() {
                return Err(snapshot_contract_error());
            }
            account.snapshot = validate_snapshot(registry, provider_id, account.snapshot.clone())?;
        }
    }

    let quota_sources = definition
        .metrics
        .iter()
        .filter_map(|metric| match &metric.source {
            MetricSource::Quota { source_id, .. }
            | MetricSource::QuotaOrValue { source_id, .. } => Some(source_id.as_str()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let value_sources = definition
        .metrics
        .iter()
        .filter_map(|metric| match &metric.source {
            MetricSource::Value { source_id } | MetricSource::QuotaOrValue { source_id, .. } => {
                Some(source_id.as_str())
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let status_sources = definition
        .metrics
        .iter()
        .filter_map(|metric| match &metric.source {
            MetricSource::Status { source_id } => Some(source_id.as_str()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    if snapshot
        .quotas
        .iter()
        .any(|quota| !quota_sources.contains(quota.id.as_str()))
        || snapshot
            .value_metrics
            .iter()
            .any(|metric| !value_sources.contains(metric.id.as_str()))
        || snapshot
            .status_metrics
            .iter()
            .any(|metric| !status_sources.contains(metric.id.as_str()))
        || has_duplicate_ids(snapshot.quotas.iter().map(|metric| metric.id.as_str()))
        || has_duplicate_ids(
            snapshot
                .value_metrics
                .iter()
                .map(|metric| metric.id.as_str()),
        )
        || has_duplicate_ids(
            snapshot
                .status_metrics
                .iter()
                .map(|metric| metric.id.as_str()),
        )
        || snapshot.quotas.iter().any(|quota| {
            (quota.format == crate::models::QuotaFormat::Count
                && quota
                    .unit
                    .as_deref()
                    .is_none_or(|unit| unit.trim().is_empty()))
                || (quota.estimated
                    && quota
                        .source_note
                        .as_deref()
                        .is_none_or(|note| note.trim().is_empty()))
        })
        || snapshot
            .status_metrics
            .iter()
            .any(|metric| metric.text.trim().is_empty() || metric.label.trim().is_empty())
    {
        return Err(snapshot_contract_error());
    }
    Ok(snapshot)
}

/// Replaces a preview's not-yet-loaded histories with the previous snapshot's, per account.
pub(super) fn carry_usage_histories(previous: &ProviderSnapshot, preview: &mut ProviderSnapshot) {
    preview.usage_histories = previous.usage_histories.clone();
    let (Some(previous_accounts), Some(accounts)) = (&previous.accounts, &mut preview.accounts)
    else {
        return;
    };
    for account in accounts {
        if let Some(previous) = previous_accounts.iter().find(|item| item.id == account.id) {
            account.snapshot.usage_histories = previous.snapshot.usage_histories.clone();
        }
    }
}

fn has_duplicate_ids<'a>(mut ids: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = std::collections::HashSet::new();
    ids.any(|id| !seen.insert(id))
}

pub(super) fn usage_data_changed(
    previous: &UsageViewState,
    current: &UsageViewState,
    provider_ids: &[String],
) -> bool {
    provider_ids.iter().any(|provider_id| {
        let current_snapshot = current
            .providers
            .get(provider_id)
            .and_then(|state| state.snapshot.as_ref());
        let previous_snapshot = previous
            .providers
            .get(provider_id)
            .and_then(|state| state.snapshot.as_ref());

        match (previous_snapshot, current_snapshot) {
            (None, Some(snapshot)) => snapshot_has_usage_data(snapshot),
            (Some(previous), Some(current)) => snapshot_usage_changed(previous, current),
            _ => false,
        }
    })
}

fn snapshot_usage_changed(previous: &ProviderSnapshot, current: &ProviderSnapshot) -> bool {
    previous.quotas.len() != current.quotas.len()
        || previous
            .quotas
            .iter()
            .zip(&current.quotas)
            .any(|(before, after)| {
                quota_usage_changed(before, previous.refreshed_at, after, current.refreshed_at)
            })
        || previous.value_metrics != current.value_metrics
        || previous.status_metrics != current.status_metrics
        || previous.usage_histories.local_device != current.usage_histories.local_device
        || previous.usage_histories.account != current.usage_histories.account
        || match (&previous.accounts, &current.accounts) {
            (None, None) => false,
            (Some(previous), Some(current)) => {
                previous.len() != current.len()
                    || previous.iter().zip(current).any(|(previous, current)| {
                        previous.id != current.id
                            || snapshot_usage_changed(&previous.snapshot, &current.snapshot)
                    })
            }
            _ => true,
        }
}

/// Clock estimates, small reset-time corrections, and idle windows whose reset rolls
/// forward alongside each poll (`used == 0`, `reset ≈ observed + period`) are not new usage.
/// Keep the original snapshots intact for countdowns, and compare every other field.
fn quota_usage_changed(
    previous: &QuotaWindow,
    previous_refreshed_at: chrono::DateTime<Utc>,
    current: &QuotaWindow,
    current_refreshed_at: chrono::DateTime<Utc>,
) -> bool {
    let mut comparable = current.clone();
    comparable.clock_offset_seconds = previous.clock_offset_seconds;
    if let (Some(before), Some(after)) = (previous.resets_at, current.resets_at) {
        if (after - before).abs() <= chrono::Duration::seconds(RESET_JITTER_TOLERANCE_SECONDS)
            || is_rolling_idle_reset(
                previous,
                previous_refreshed_at,
                before,
                current,
                current_refreshed_at,
                after,
            )
        {
            comparable.resets_at = previous.resets_at;
        }
    }
    previous != &comparable
}

fn is_rolling_idle_reset(
    previous: &QuotaWindow,
    previous_refreshed_at: chrono::DateTime<Utc>,
    before: chrono::DateTime<Utc>,
    current: &QuotaWindow,
    current_refreshed_at: chrono::DateTime<Utc>,
    after: chrono::DateTime<Utc>,
) -> bool {
    if previous.used_percent != 0.0
        || current.used_percent != 0.0
        || previous.period_seconds == 0
        || previous.period_seconds != current.period_seconds
        || previous_refreshed_at == chrono::DateTime::UNIX_EPOCH
        || current_refreshed_at == chrono::DateTime::UNIX_EPOCH
    {
        return false;
    }
    let Ok(period_seconds) = i64::try_from(previous.period_seconds) else {
        return false;
    };
    let Some(period) = chrono::Duration::try_seconds(period_seconds) else {
        return false;
    };
    let previous_observed = previous_refreshed_at
        + chrono::Duration::seconds(previous.clock_offset_seconds.unwrap_or(0));
    let current_observed =
        current_refreshed_at + chrono::Duration::seconds(current.clock_offset_seconds.unwrap_or(0));
    let observation_interval = current_observed - previous_observed;
    let reset_shift = after - before;
    if observation_interval <= chrono::Duration::zero() || reset_shift <= chrono::Duration::zero() {
        return false;
    }
    let window_tolerance =
        chrono::Duration::seconds((period_seconds / 10).min(IDLE_RESET_WINDOW_TOLERANCE_SECONDS));
    let drift_tolerance =
        chrono::Duration::seconds(IDLE_RESET_DRIFT_TOLERANCE_SECONDS).min(window_tolerance);
    let Some(expected_before) = previous_observed.checked_add_signed(period) else {
        return false;
    };
    let Some(expected_after) = current_observed.checked_add_signed(period) else {
        return false;
    };
    (before - expected_before).abs() <= window_tolerance
        && (after - expected_after).abs() <= window_tolerance
        && (reset_shift - observation_interval).abs() <= drift_tolerance
}

pub(super) fn refresh_interval_for(
    current_interval: Duration,
    data_changed: bool,
    unchanged_refreshes: &mut u8,
) -> Duration {
    if data_changed {
        *unchanged_refreshes = 0;
        FAST_REFRESH_INTERVAL
    } else if current_interval != FAST_REFRESH_INTERVAL {
        *unchanged_refreshes = 0;
        REFRESH_INTERVAL
    } else {
        *unchanged_refreshes = unchanged_refreshes.saturating_add(1);
        if *unchanged_refreshes >= FAST_REFRESH_UNCHANGED_ROUNDS {
            *unchanged_refreshes = 0;
            REFRESH_INTERVAL
        } else {
            FAST_REFRESH_INTERVAL
        }
    }
}

fn snapshot_has_usage_data(snapshot: &ProviderSnapshot) -> bool {
    !snapshot.quotas.is_empty()
        || !snapshot.value_metrics.is_empty()
        || !snapshot.status_metrics.is_empty()
        || snapshot.usage_histories.local_device.is_some()
        || snapshot.usage_histories.account.is_some()
}

fn snapshot_contract_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Internal,
        "Provider data does not match its registered metric contract.",
    )
}

pub(super) fn merge_refresh_result(
    state: &mut ProviderViewState,
    result: Result<ProviderSnapshot, ProviderError>,
) {
    match result {
        Ok(snapshot) => {
            state.snapshot = Some(snapshot);
            state.source = SnapshotSource::Live;
            state.error = None;
            state.error_kind = None;
            state.stale = false;
        }
        Err(error) => {
            state.error_kind = Some(error.kind());
            state.error = Some(error.to_string());
        }
    }
    state.refreshing = false;
}

pub(super) fn update_staleness_from_snapshot_age(state: &mut ProviderViewState) {
    state.stale = state.snapshot.as_ref().is_some_and(|snapshot| {
        let refreshed_at = if snapshot.refreshed_at == chrono::DateTime::UNIX_EPOCH {
            snapshot
                .usage_histories
                .local_device_status
                .as_ref()
                .and_then(|status| status.refreshed_at)
        } else {
            Some(snapshot.refreshed_at)
        };
        refreshed_at
            .filter(|at| *at != chrono::DateTime::UNIX_EPOCH)
            .is_some_and(|at| Utc::now().signed_duration_since(at) >= STALE_AFTER)
    });
}
