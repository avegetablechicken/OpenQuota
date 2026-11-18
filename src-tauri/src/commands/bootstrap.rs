use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::{
    desktop_integration::DesktopIntegration,
    models::{ProviderCatalog, SettingsViewState},
    service::{ProviderService, UsageViewState},
    settings::SettingsService,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapState {
    pub usage: UsageViewState,
    pub settings: SettingsViewState,
    pub catalog: ProviderCatalog,
}

#[tauri::command]
pub fn get_bootstrap_state(
    desktop: State<'_, DesktopIntegration>,
    service: State<'_, Arc<ProviderService>>,
    settings: State<'_, Arc<SettingsService>>,
) -> BootstrapState {
    let started = std::time::Instant::now();
    let state = BootstrapState {
        usage: service.state(),
        // The dashboard only needs local settings. Live OS integration status is
        // fetched separately so a slow permission query cannot hold up rendering.
        settings: settings.view_state(
            "unavailable",
            None,
            desktop.tray_available(),
            desktop.platform_summary(),
        ),
        catalog: settings.catalog().clone(),
    };
    crate::app_debug!(
        "lifecycle",
        "bootstrap snapshot assembled ({} ms)",
        started.elapsed().as_millis()
    );
    state
}

#[tauri::command]
pub fn report_first_panel_frame(bootstrap_ms: u32, document_ms: u32) {
    crate::app_info!(
        "lifecycle",
        "first visible panel frame observed (bootstrap round trip {} ms, document age {} ms)",
        bootstrap_ms,
        document_ms
    );
}
