use std::{sync::Arc, time::Duration};

use tauri::{AppHandle, Emitter};

use crate::{
    commands::{settings::SettingsWatch, usage::publish_refresh_progress},
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
    spawn_preview_publisher(app.clone(), service.clone(), settings.clone());
    tauri::async_runtime::spawn(async move {
        loop {
            pricing.refresh_if_due();
            let provider_ids = settings.enabled_provider_ids();
            let mut interval = REFRESH_INTERVAL;
            if !provider_ids.is_empty() {
                let watch = SettingsWatch::new(&settings);
                let state = service
                    .refresh_all_with_progress(&provider_ids, false, |state| {
                        watch.emit_if_changed(&app, &settings);
                        publish_refresh_progress(&app, &settings, state);
                    })
                    .await;
                interval = Duration::from_secs(state.refresh_interval_seconds);
                watch.emit_if_changed(&app, &settings);
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

/// Publishes quota previews that unofficial providers report while their refresh continues.
fn spawn_preview_publisher(
    app: AppHandle,
    service: Arc<ProviderService>,
    settings: Arc<SettingsService>,
) {
    let mut previews = service.subscribe_previews();
    tauri::async_runtime::spawn(async move {
        while previews.changed().await.is_ok() {
            publish_refresh_progress(&app, &settings, &service.state());
        }
    });
}
