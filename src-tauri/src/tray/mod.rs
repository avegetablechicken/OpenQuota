//! The tray icon and its menu: building it, rebuilding it when settings change, and what each
//! entry does.

mod platform;

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
use tauri::{menu::IsMenuItem, Emitter};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    App, AppHandle, Manager,
};

#[cfg(not(target_os = "linux"))]
use crate::window::toggle_main_window;
use crate::{
    commands,
    models::{AppSettings, MenuBarStyle, UpdateFrequency, UsageDisplay, WindowMode},
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
#[cfg(target_os = "macos")]
const MENU_BAR_HIDE_PREFIX: &str = "menu-bar-hide:";
#[cfg(target_os = "macos")]
const MENU_BAR_PIN_PREFIX: &str = "menu-bar-pin:";
#[cfg(target_os = "macos")]
const MENU_BAR_UNPIN_PREFIX: &str = "menu-bar-unpin:";

/// Maps a tray menu event to the metric whose menu bar pin it sets, and the pin's new state.
#[cfg(target_os = "macos")]
fn menu_bar_pin_change(id: &str) -> Option<(&str, bool)> {
    id.strip_prefix(MENU_BAR_HIDE_PREFIX)
        .or_else(|| id.strip_prefix(MENU_BAR_UNPIN_PREFIX))
        .map(|metric_id| (metric_id, false))
        .or_else(|| {
            id.strip_prefix(MENU_BAR_PIN_PREFIX)
                .map(|metric_id| (metric_id, true))
        })
}

#[cfg(target_os = "macos")]
fn build_menu_bar_submenu(
    app: &AppHandle,
    menu: tray_presentation::MenuBarMenu,
) -> Result<Option<Submenu<tauri::Wry>>, Box<dyn std::error::Error>> {
    // Only shown while the menu bar draws usage; hiding a metric unpins it, so it leaves this list
    // until it is starred again from Add or the Customize page.
    if menu.shown.is_empty() {
        return Ok(None);
    }
    let shown = menu
        .shown
        .into_iter()
        .map(|item| {
            CheckMenuItem::with_id(
                app,
                format!("{MENU_BAR_HIDE_PREFIX}{}", item.metric_id),
                item.label,
                true,
                true,
                None::<&str>,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let providers = menu
        .providers
        .into_iter()
        .map(|provider| {
            let options = provider
                .options
                .into_iter()
                .map(|option| {
                    let prefix = if option.pinned {
                        MENU_BAR_UNPIN_PREFIX
                    } else {
                        MENU_BAR_PIN_PREFIX
                    };
                    CheckMenuItem::with_id(
                        app,
                        format!("{prefix}{}", option.metric_id),
                        option.label,
                        option.available,
                        option.pinned,
                        None::<&str>,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            Submenu::with_id_and_items(
                app,
                format!("menu-bar-add:{}", provider.id),
                provider.name,
                true,
                &options
                    .iter()
                    .map(|option| option as &dyn IsMenuItem<tauri::Wry>)
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let separator = PredefinedMenuItem::separator(app)?;
    let add = Submenu::with_id_and_items(
        app,
        "menu-bar-add",
        "Add",
        !providers.is_empty(),
        &providers
            .iter()
            .map(|provider| provider as &dyn IsMenuItem<tauri::Wry>)
            .collect::<Vec<_>>(),
    )?;
    let mut items = shown
        .iter()
        .map(|item| item as &dyn IsMenuItem<tauri::Wry>)
        .collect::<Vec<_>>();
    items.extend([&separator as &dyn IsMenuItem<tauri::Wry>, &add]);
    Ok(Some(Submenu::with_id_and_items(
        app,
        "menu-bar-items",
        "Show in Menu Bar",
        true,
        &items,
    )?))
}

fn build_tray_menu(
    app: &AppHandle,
    settings: &AppSettings,
) -> Result<Menu<tauri::Wry>, Box<dyn std::error::Error>> {
    let tray_popup = CheckMenuItem::with_id(
        app,
        "tray-mode-popup",
        "Tray Popup",
        true,
        settings.window_mode == WindowMode::Popup,
        None::<&str>,
    )?;
    let floating_window = CheckMenuItem::with_id(
        app,
        "tray-mode-floating",
        "Floating Window",
        true,
        settings.window_mode == WindowMode::Floating,
        None::<&str>,
    )?;
    let tray_mode = Submenu::with_id_and_items(
        app,
        "tray-mode",
        "Tray Mode",
        true,
        &[&tray_popup, &floating_window],
    )?;

    let show_left = CheckMenuItem::with_id(
        app,
        "usage-display-left",
        "Left",
        true,
        settings.usage_display == UsageDisplay::Left,
        None::<&str>,
    )?;
    let show_used = CheckMenuItem::with_id(
        app,
        "usage-display-used",
        "Used",
        true,
        settings.usage_display == UsageDisplay::Used,
        None::<&str>,
    )?;
    let show_usage_as = Submenu::with_id_and_items(
        app,
        "usage-display",
        "Show Usage as",
        true,
        &[&show_left, &show_used],
    )?;
    let one_minute = CheckMenuItem::with_id(
        app,
        "update-frequency-one-minute",
        "1 minute",
        true,
        settings.update_frequency == UpdateFrequency::OneMinute,
        None::<&str>,
    )?;
    let five_minutes = CheckMenuItem::with_id(
        app,
        "update-frequency-five-minutes",
        "5 minutes",
        true,
        settings.update_frequency == UpdateFrequency::FiveMinutes,
        None::<&str>,
    )?;
    let adaptive = CheckMenuItem::with_id(
        app,
        "update-frequency-adaptive",
        "Adaptive",
        true,
        settings.update_frequency == UpdateFrequency::Adaptive,
        None::<&str>,
    )?;
    let update_frequency = Submenu::with_id_and_items(
        app,
        "update-frequency",
        "Update Frequency",
        true,
        &[&one_minute, &five_minutes, &adaptive],
    )?;
    let launch_at_login = CheckMenuItem::with_id(
        app,
        "launch-at-login",
        "Launch at Login",
        true,
        settings.launch_at_login,
        None::<&str>,
    )?;

    #[cfg(target_os = "macos")]
    {
        let openquota_icon = CheckMenuItem::with_id(
            app,
            "icon-style-openquota",
            "OpenQuota",
            true,
            settings.menu_bar_style == MenuBarStyle::Icon,
            None::<&str>,
        )?;
        let text_icon = CheckMenuItem::with_id(
            app,
            "icon-style-text",
            "Text",
            true,
            settings.menu_bar_style == MenuBarStyle::Text,
            None::<&str>,
        )?;
        let bars_icon = CheckMenuItem::with_id(
            app,
            "icon-style-bars",
            "Bars",
            true,
            settings.menu_bar_style == MenuBarStyle::Bars,
            None::<&str>,
        )?;
        let icon_style = Submenu::with_id_and_items(
            app,
            "icon-style",
            "Icon Style",
            true,
            &[&openquota_icon, &text_icon, &bars_icon],
        )?;
        let menu_bar_items = build_menu_bar_submenu(
            app,
            app.try_state::<tray_presentation::MenuBarMenuState>()
                .map(|state| state.get())
                .unwrap_or_default(),
        )?;
        let settings_item =
            MenuItem::with_id(app, "settings", "More Settings…", true, Some("CmdOrCtrl+,"))?;
        let second_separator = PredefinedMenuItem::separator(app)?;
        let quit = MenuItem::with_id(app, "quit", "Quit OpenQuota", true, Some("CmdOrCtrl+Q"))?;
        let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&icon_style];
        if let Some(menu_bar_items) = &menu_bar_items {
            items.push(menu_bar_items);
        }
        items.extend([
            &tray_mode as &dyn IsMenuItem<tauri::Wry>,
            &show_usage_as,
            &update_frequency,
            &settings_item,
            &second_separator,
            &launch_at_login,
            &quit,
        ]);
        Ok(Menu::with_items(app, &items)?)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let open = MenuItem::with_id(app, "open", "Open OpenQuota", true, None::<&str>)?;
        let customize = MenuItem::with_id(app, "customize", "Customize…", true, None::<&str>)?;
        let settings_item =
            MenuItem::with_id(app, "settings", "More Settings…", true, None::<&str>)?;
        let second_separator = PredefinedMenuItem::separator(app)?;
        let quit = MenuItem::with_id(app, "quit", "Quit OpenQuota", true, None::<&str>)?;
        Ok(Menu::with_items(
            app,
            &[
                &open,
                &customize,
                &tray_mode,
                &show_usage_as,
                &update_frequency,
                &second_separator,
                &settings_item,
                &launch_at_login,
                &quit,
            ],
        )?)
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
        #[cfg(target_os = "macos")]
        if let Some((metric_id, pinned)) = menu_bar_pin_change(event.id.as_ref()) {
            return commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::MenuBarMetricPinned {
                    metric_id: metric_id.to_owned(),
                    pinned,
                },
            );
        }
        match event.id.as_ref() {
            "icon-style-openquota" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::MenuBarStyle(MenuBarStyle::Icon),
            ),
            "icon-style-text" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::MenuBarStyle(MenuBarStyle::Text),
            ),
            "icon-style-bars" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::MenuBarStyle(MenuBarStyle::Bars),
            ),
            "tray-mode-popup" => after_tray_menu_closes(app, |app| {
                commands::settings::apply_tray_setting(
                    app,
                    commands::settings::TraySetting::WindowMode(WindowMode::Popup),
                )
            }),
            "tray-mode-floating" => after_tray_menu_closes(app, |app| {
                commands::settings::apply_tray_setting(
                    app,
                    commands::settings::TraySetting::WindowMode(WindowMode::Floating),
                )
            }),
            "usage-display-left" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::UsageDisplay(UsageDisplay::Left),
            ),
            "usage-display-used" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::UsageDisplay(UsageDisplay::Used),
            ),
            "update-frequency-one-minute" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::UpdateFrequency(UpdateFrequency::OneMinute),
            ),
            "update-frequency-five-minutes" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::UpdateFrequency(UpdateFrequency::FiveMinutes),
            ),
            "update-frequency-adaptive" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::UpdateFrequency(UpdateFrequency::Adaptive),
            ),
            "launch-at-login" => commands::settings::apply_tray_setting(
                app,
                commands::settings::TraySetting::LaunchAtLoginToggle,
            ),
            "open" => after_tray_menu_closes(app, |app| {
                app.state::<PopupDismissGuard>().cancel_pending();
                if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                    show_main_window(&window);
                }
            }),
            "customize" => after_tray_menu_closes(app, |app| open_screen(app, "customize")),
            "settings" => after_tray_menu_closes(app, |app| open_screen(app, "settings")),
            "quit" => {
                if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                    window::finish_native_panel_resize(&window);
                }
                app.exit(0);
            }
            _ => {}
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
