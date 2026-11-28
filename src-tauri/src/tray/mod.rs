//! The tray icon and its menu: building it, rebuilding it when settings change, and what each
//! entry does.

mod menu;
mod platform;

use menu::{TrayAction, TrayEntry};

use platform::after_tray_menu_closes;
#[cfg(target_os = "macos")]
use platform::remember_tray_position;
pub(crate) use platform::show_standalone_window_fallback;
#[cfg(target_os = "linux")]
pub(crate) use platform::spawn_status_notifier_monitor;

use std::sync::Arc;

#[cfg(not(target_os = "linux"))]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
#[cfg(target_os = "macos")]
use tauri::Emitter;
use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    App, AppHandle, Manager,
};

#[cfg(not(target_os = "linux"))]
use crate::window::toggle_main_window;
use crate::{
    commands::settings::{apply_tray_setting, TraySetting},
    models::AppSettings,
    popup::PopupDismissGuard,
    settings::SettingsService,
    window::{self, open_screen, show_main_window, MAIN_WINDOW},
};
#[cfg(target_os = "macos")]
use crate::{
    desktop_integration::DesktopIntegration, status_item, tray_presentation,
    window::hide_main_window,
};

pub(crate) const TRAY_ID: &str = "openquota-tray";

/// Turns menu entries into Tauri menu items, ids from [`TrayAction::id`].
fn build_items(
    app: &AppHandle,
    entries: Vec<TrayEntry>,
) -> tauri::Result<Vec<Box<dyn IsMenuItem<tauri::Wry>>>> {
    entries
        .into_iter()
        .map(|entry| -> tauri::Result<Box<dyn IsMenuItem<tauri::Wry>>> {
            Ok(match entry {
                TrayEntry::Item {
                    action,
                    label,
                    checked: Some(checked),
                    enabled,
                    accelerator,
                } => Box::new(CheckMenuItem::with_id(
                    app,
                    action.id(),
                    label,
                    enabled,
                    checked,
                    accelerator,
                )?),
                TrayEntry::Item {
                    action,
                    label,
                    checked: None,
                    enabled,
                    accelerator,
                } => Box::new(MenuItem::with_id(
                    app,
                    action.id(),
                    label,
                    enabled,
                    accelerator,
                )?),
                TrayEntry::Submenu {
                    id,
                    label,
                    enabled,
                    entries,
                } => {
                    let items = build_items(app, entries)?;
                    Box::new(Submenu::with_id_and_items(
                        app,
                        id,
                        label,
                        enabled,
                        &item_refs(&items),
                    )?)
                }
                TrayEntry::Separator => Box::new(PredefinedMenuItem::separator(app)?),
            })
        })
        .collect()
}

fn item_refs(items: &[Box<dyn IsMenuItem<tauri::Wry>>]) -> Vec<&dyn IsMenuItem<tauri::Wry>> {
    items.iter().map(|item| item.as_ref()).collect()
}

fn build_tray_menu(app: &AppHandle, settings: &AppSettings) -> tauri::Result<Menu<tauri::Wry>> {
    #[cfg(target_os = "macos")]
    let entries = menu::mac_menu(
        settings,
        app.try_state::<tray_presentation::MenuBarMenuState>()
            .map(|state| state.get())
            .unwrap_or_default(),
    );
    #[cfg(not(target_os = "macos"))]
    let entries = menu::standard_menu(settings);
    let items = build_items(app, entries)?;
    Menu::with_items(app, &item_refs(&items))
}

/// Does what a clicked tray menu entry asks.
fn perform(app: &AppHandle, action: TrayAction) {
    match action {
        // The window mode change shows the panel, so it waits for the menu to close.
        TrayAction::Setting(setting @ TraySetting::WindowMode(_)) => {
            after_tray_menu_closes(app, move |app| apply_tray_setting(app, setting))
        }
        TrayAction::Setting(setting) => apply_tray_setting(app, setting),
        #[cfg(any(target_os = "macos", test))]
        TrayAction::HideMenuBarMetric(metric_id) => apply_tray_setting(
            app,
            TraySetting::MenuBarMetricPinned {
                metric_id,
                pinned: false,
            },
        ),
        TrayAction::Open => after_tray_menu_closes(app, |app| {
            app.state::<PopupDismissGuard>().cancel_pending();
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                show_main_window(&window);
            }
        }),
        TrayAction::Customize => after_tray_menu_closes(app, |app| open_screen(app, "customize")),
        TrayAction::Settings => after_tray_menu_closes(app, |app| open_screen(app, "settings")),
        TrayAction::Quit => {
            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                window::finish_native_panel_resize(&window);
            }
            app.exit(0);
        }
    }
}

pub(crate) fn update_tray_menu(app: &AppHandle, settings: &AppSettings) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match build_tray_menu(app, settings) {
        Ok(menu) => {
            if tray.set_menu(Some(menu)).is_err() {
                crate::app_warn!("tray", "tray menu update failed");
            }
            #[cfg(target_os = "macos")]
            status_item::detach(app);
        }
        Err(error) => crate::app_warn!("tray", "tray menu could not be rebuilt: {error}"),
    }
}

pub(crate) fn install_tray(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    let menu = build_tray_menu(app.handle(), &app.state::<Arc<SettingsService>>().get())?;

    let icon = app
        .default_window_icon()
        .ok_or_else(|| std::io::Error::other("OpenQuota application icon is unavailable"))?
        .clone();
    let tray = TrayIconBuilder::with_id(TRAY_ID).icon(icon).menu(&menu);
    #[cfg(not(target_os = "linux"))]
    let tray = tray.tooltip("OpenQuota").show_menu_on_left_click(false);
    let tray = tray.on_menu_event(|app, event| {
        if let Some(action) = TrayAction::from_id(event.id.as_ref()) {
            perform(app, action);
        }
    });
    #[cfg(not(target_os = "linux"))]
    let tray = tray.on_tray_icon_event(|tray, event| {
        tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);

        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            toggle_main_window(tray.app_handle());
        }
        #[cfg(all(target_os = "macos", tray_icon_attached_menu))]
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Down,
                ..
            }
        ) {
            status_item::show_menu();
        }
    });
    let _tray = tray.build(app)?;
    #[cfg(target_os = "macos")]
    {
        let (press, begin, end) = (
            app.handle().clone(),
            app.handle().clone(),
            app.handle().clone(),
        );
        status_item::install(
            &_tray,
            status_item::Handlers {
                press: Box::new(move || {
                    remember_tray_position(&press);
                    toggle_main_window(&press);
                }),
                begin: Box::new(move |clicked| {
                    let floating = begin.state::<DesktopIntegration>().is_floating();
                    // A click toggles the floating window, which outlives the session.
                    if floating && clicked {
                        toggle_main_window(&begin);
                        status_item::end_session(&begin);
                        return;
                    }
                    remember_tray_position(&begin);
                    begin.state::<PopupDismissGuard>().cancel_pending();
                    if let Some(window) = begin.get_webview_window(MAIN_WINDOW) {
                        show_main_window(&window);
                        // Begun by keyboard navigation, it passes unused ←/→ back to the menu bar.
                        let _ = window.emit(
                            "menu-bar-session",
                            MenuBarSession {
                                active: true,
                                keyboard: !clicked,
                            },
                        );
                    }
                }),
                end: Box::new(move || {
                    if let Some(window) = end.get_webview_window(MAIN_WINDOW) {
                        let _ = window.emit(
                            "menu-bar-session",
                            MenuBarSession {
                                active: false,
                                keyboard: false,
                            },
                        );
                    }
                    if end.state::<DesktopIntegration>().is_floating() {
                        return;
                    }
                    if let Some(window) = end.get_webview_window(MAIN_WINDOW) {
                        hide_main_window(&window);
                    }
                }),
            },
        );
    }
    Ok(())
}

/// The status item's expanded interface session (macOS 27), which holds the
/// window as the item's menu while it lasts.
#[cfg(target_os = "macos")]
#[derive(Clone, serde::Serialize)]
struct MenuBarSession {
    active: bool,
    /// Keyboard navigation of the menu bar began it, not a click.
    keyboard: bool,
}
