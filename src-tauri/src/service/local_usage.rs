//! Device history is refreshed and cached independently of online account data.
use chrono::Utc;

use crate::{
    models::{
        LocalUsageStatus, ProviderSnapshot, ProviderViewState, SnapshotSource, UsageHistories,
    },
    providers::{ProviderError, ScannedUsage},
};

use super::ProviderService;

pub(super) fn carry_local(previous: &ProviderSnapshot, next: &mut ProviderSnapshot) {
    if previous.usage_histories.local_device.is_some()
        || previous.usage_histories.local_device_status.is_some()
    {
        next.usage_histories.local_device = previous.usage_histories.local_device.clone();
        next.usage_histories.local_device_status = previous
            .usage_histories
            .local_device_status
            .clone()
            .or_else(|| {
                Some(LocalUsageStatus {
                    refreshed_at: (previous.refreshed_at != chrono::DateTime::UNIX_EPOCH)
                        .then_some(previous.refreshed_at),
                    ..Default::default()
                })
            });
    }
}

pub(super) fn merge_local(state: &mut ProviderViewState, provider_id: &str, local: UsageHistories) {
    let snapshot = state.snapshot.get_or_insert_with(|| ProviderSnapshot {
        provider_id: provider_id.into(),
        upstream_provider_id: None,
        accounts: None,
        plan: None,
        quotas: Vec::new(),
        value_metrics: Vec::new(),
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage_histories: UsageHistories::default(),
        warnings: Vec::new(),
        // No online request has succeeded. A local scan must not freshen online data.
        refreshed_at: chrono::DateTime::UNIX_EPOCH,
    });
    snapshot.usage_histories.local_device = local.local_device;
    snapshot.usage_histories.local_device_status = local.local_device_status;
}

impl ProviderService {
    pub(super) fn apply_local_usage_result(
        &self,
        provider_id: &str,
        result: Result<ScannedUsage, ProviderError>,
    ) {
        let mut local = self
            .provider_state(provider_id)
            .snapshot
            .map(|snapshot| {
                let mut usage = snapshot.usage_histories;
                if usage.local_device.is_some() && usage.local_device_status.is_none() {
                    usage.local_device_status = Some(LocalUsageStatus {
                        refreshed_at: Some(snapshot.refreshed_at),
                        ..Default::default()
                    });
                }
                usage
            })
            .unwrap_or_default();
        local.account = None;
        match result {
            Ok(scan) => {
                local.local_device = Some(scan.history);
                local.local_device_status = Some(LocalUsageStatus {
                    refreshed_at: Some(Utc::now()),
                    error: None,
                    warnings: scan.warnings,
                });
            }
            Err(error) => {
                let fallback = if local.local_device.is_some() {
                    " Showing the last available history."
                } else {
                    ""
                };
                local
                    .local_device_status
                    .get_or_insert_with(LocalUsageStatus::default)
                    .error = Some(format!(
                    "Local usage history could not be refreshed.{fallback} {error}"
                ));
            }
        }
        if let Err(error) = self.storage.save_local_usage(provider_id, &local) {
            crate::app_warn!(
                "cache",
                "local usage for {provider_id} could not be saved: {error}"
            );
            let status = local
                .local_device_status
                .get_or_insert_with(LocalUsageStatus::default);
            match &mut status.error {
                Some(message) => {
                    message.push_str(" The local usage cache also could not be saved.")
                }
                None => {
                    status.error = Some("Local usage refreshed, but could not be cached.".into())
                }
            }
        }
        self.update_state(provider_id, |state| {
            let refreshed = local
                .local_device_status
                .as_ref()
                .is_some_and(|status| status.error.is_none());
            merge_local(state, provider_id, local);
            if refreshed
                && state
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.refreshed_at == chrono::DateTime::UNIX_EPOCH)
            {
                state.source = SnapshotSource::Live;
            }
        });
        // Publish local history while an online worker may still be waiting on the network.
        self.preview_revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}
