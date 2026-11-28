//! Platform fixes around the tray: where macOS's icon is, waiting for GNOME's menu to fade, and
//! the standalone window when a Linux tray disappears.

#[cfg(target_os = "linux")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
use tauri::tray::TrayIconEvent;
#[cfg(target_os = "linux")]
use tauri::Emitter;
use tauri::{AppHandle, Manager};

#[cfg(target_os = "macos")]
use super::TRAY_ID;
#[cfg(target_os = "linux")]
use crate::{
    commands, desktop_integration,
    popup::PopupDismissGuard,
    settings::SettingsService,
    window::{self, MAIN_WINDOW},
};
use crate::{desktop_integration::DesktopIntegration, window::show_main_window};

/// Tells the positioner where the icon is, which it otherwise learns only from
/// pointer events over the icon.
#[cfg(target_os = "macos")]
pub(super) fn remember_tray_position(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Ok(Some(rect)) = tray.rect() {
        let event = TrayIconEvent::Move {
            id: tray.id().clone(),
            position: rect.position.to_physical(1.0),
            rect,
        };
        tauri_plugin_positioner::on_tray_event(app, &event);
    }
}

/// GNOME fades the tray menu out (150 ms) after activation; waiting keeps it
/// out of the panel's captured backdrop.
#[cfg(target_os = "linux")]
const TRAY_MENU_FADE_OUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Runs a tray menu action that shows the panel once the menu has closed.
pub(super) fn after_tray_menu_closes(
    app: &AppHandle,
    action: impl FnOnce(&AppHandle) + Send + 'static,
) {
    #[cfg(target_os = "linux")]
    {
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(TRAY_MENU_FADE_OUT).await;
            let app = handle.clone();
            let _ = handle.run_on_main_thread(move || action(&app));
        });
    }
    #[cfg(not(target_os = "linux"))]
    action(app);
}

pub(crate) fn show_standalone_window_fallback(window: &tauri::WebviewWindow) {
    window
        .app_handle()
        .state::<DesktopIntegration>()
        .set_floating(true);
    let _ = window.set_resizable(false);
    let _ = window.set_skip_taskbar(false);
    let _ = window.set_always_on_top(false);
    #[cfg(target_os = "linux")]
    {
        window::finish_native_panel_resize(window);
        let _ = window::show_linux_panel(window, window::LinuxPanelPlacement::Center);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window.center();
        show_main_window(window);
    }
}

#[cfg(target_os = "linux")]
fn apply_linux_tray_fallback(app: &AppHandle) {
    let integration = app.state::<DesktopIntegration>();
    if !integration.disable_tray() {
        return;
    }
    crate::app_warn!(
        "lifecycle",
        "system tray became unavailable; using standalone window"
    );
    let _ = app.remove_tray_by_id(super::TRAY_ID);
    app.state::<PopupDismissGuard>().cancel_pending();

    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let mode = app.state::<Arc<SettingsService>>().get().window_mode;
        match window::apply_window_mode(&window, mode, true) {
            Ok(()) => show_main_window(&window),
            Err(error) => {
                crate::app_warn!(
                    "window",
                    "standalone fallback could not apply window mode: {error}"
                );
                show_standalone_window_fallback(&window);
            }
        }
    }

    let settings = app.state::<Arc<SettingsService>>();
    let state = commands::settings::settings_view_state(app, settings.inner().as_ref());
    let _ = app.emit("settings-state", state);
}

#[cfg(target_os = "linux")]
pub(crate) fn spawn_status_notifier_monitor(app: AppHandle) {
    if desktop_integration::status_notifier_monitor_forced_off() {
        return;
    }
    let monitor_app = app.clone();
    if std::thread::Builder::new()
        .name("openquota-tray-monitor".to_owned())
        .spawn(move || {
            if let Err(error) = desktop_integration::wait_for_status_notifier_loss() {
                crate::app_warn!("lifecycle", "system tray monitor stopped: {error}");
            }
            let fallback_app = monitor_app.clone();
            if monitor_app
                .run_on_main_thread(move || apply_linux_tray_fallback(&fallback_app))
                .is_err()
            {
                crate::app_warn!(
                    "lifecycle",
                    "standalone tray fallback could not be scheduled"
                );
            }
        })
        .is_err()
    {
        crate::app_warn!("lifecycle", "system tray monitor could not be started");
        apply_linux_tray_fallback(&app);
    }
}
