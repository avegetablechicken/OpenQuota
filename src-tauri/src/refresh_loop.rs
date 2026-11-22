use std::{
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use tauri::{AppHandle, Emitter};

use crate::{
    commands::{settings::emit_settings_if_account_changed, usage::publish_refresh_progress},
    notifications::finish_refresh,
    pacing::NotificationEvaluator,
    policy::REFRESH_INTERVAL,
    pricing::PricingStore,
    service::ProviderService,
    settings::SettingsService,
};

pub fn spawn(
    app: AppHandle,
    service: Arc<ProviderService>,
    settings: Arc<SettingsService>,
    notifications: Arc<NotificationEvaluator>,
    pricing: Arc<PricingStore>,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            pricing.refresh_if_due();
            let provider_ids = settings.enabled_provider_ids();
            let mut interval = REFRESH_INTERVAL;
            if !provider_ids.is_empty() {
                let observed_account_revision = AtomicU64::new(settings.account_revision());
                let state = service
                    .refresh_all_with_progress(&provider_ids, false, |state| {
                        emit_settings_if_account_changed(
                            &app,
                            &settings,
                            &observed_account_revision,
                        );
                        publish_refresh_progress(&app, &settings, state);
                    })
                    .await;
                interval = Duration::from_secs(state.refresh_interval_seconds);
                emit_settings_if_account_changed(&app, &settings, &observed_account_revision);
                let _ = app.emit("usage-state", &state);
                finish_refresh(&app, &state, &settings, &notifications);
            }
            let refresh_frequency_changed = service.wait_for_refresh_frequency_change();
            tokio::pin!(refresh_frequency_changed);
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = &mut refresh_frequency_changed => {}
            }
        }
    });
}
