#[cfg(target_os = "linux")]
mod appearance;
#[cfg(target_os = "linux")]
mod backdrop;
mod child_process;
mod codex_environment;
mod commands;
mod desktop_integration;
mod hashing;
mod http_client;
mod logging;
#[cfg(target_os = "macos")]
mod login_item;
#[cfg(any(target_os = "macos", test))]
mod menu_bar;
mod metric_policy;
mod models;
mod notifications;
mod pacing;
mod policy;
mod popup;
mod pricing;
mod provider_environment;
mod providers;
mod refresh_loop;
mod service;
mod settings;
#[cfg(target_os = "macos")]
mod status_item;
mod storage;
mod tray;
#[cfg(any(not(target_os = "macos"), test))]
mod tray_icon;
mod tray_presentation;
mod updates;
mod webview_memory;
mod window;
#[cfg(any(target_os = "linux", test))]
mod xdg_autostart;

use std::sync::Arc;

use popup::PopupDismissGuard;
use service::ProviderService;
use settings::{CredentialDetectionPlan, SettingsService};
use tauri::{AppHandle, Emitter, Manager};
#[cfg(not(target_os = "linux"))]
use tauri_plugin_autostart::ManagerExt as AutostartExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use crate::{
    desktop_integration::DesktopIntegration,
    pacing::NotificationEvaluator,
    pricing::PricingStore,
    providers::{
        antigravity::AntigravityProvider, claude::ClaudeProvider,
        codex::reset_claim::CodexResetClaimService, codex::CodexProvider, copilot::CopilotProvider,
        cursor::CursorProvider, detect_local_credentials, devin::DevinProvider, grok::GrokProvider,
        kimi::KimiProvider, minimax::MiniMaxProvider, opencode::OpenCodeProvider,
        openrouter::OpenRouterProvider, sub2api::Sub2ApiProviders, zai::ZaiProvider,
        ProviderRegistry, UsageProvider,
    },
    storage::Storage,
    window::{handle_window_event, toggle_main_window, PanelResizeSession, MAIN_WINDOW},
};

fn spawn_startup_credential_detection(
    app: AppHandle,
    registry: Arc<ProviderRegistry>,
    service: Arc<ProviderService>,
    settings: Arc<SettingsService>,
    notifications: Arc<NotificationEvaluator>,
    plan: CredentialDetectionPlan,
) {
    tauri::async_runtime::spawn(async move {
        app_info!("config", "startup credential detection began");
        let detected = detect_local_credentials(registry, plan.provider_ids()).await;
        let command_guard = settings.lock_command_mutation().await;
        let Ok(outcome) = settings.apply_credential_detection(&plan, &detected) else {
            app_error!(
                "config",
                "startup credential detection could not be applied"
            );
            return;
        };
        app_info!(
            "config",
            "startup credential detection completed ({} detected, {} newly enabled)",
            detected
                .values()
                .filter(|status| { **status == providers::CredentialProbeStatus::Detected })
                .count(),
            outcome.newly_enabled_provider_ids.len()
        );

        tray_presentation::update(
            &app,
            &service.state(),
            &outcome.settings,
            settings.registry(),
        );
        let _ = app.emit(
            "settings-state",
            commands::settings::settings_view_state(&app, &settings),
        );
        drop(command_guard);
        if outcome.newly_enabled_provider_ids.is_empty() {
            return;
        }
        let watch = commands::settings::SettingsWatch::new(&settings);
        service
            .refresh_enabled_with_progress(&outcome.newly_enabled_provider_ids, true, |state| {
                watch.emit_if_changed(&app, &settings);
                commands::usage::publish_refresh_progress(&app, &settings, state);
            })
            .await;
        watch.emit_if_changed(&app, &settings);
        let state = service.state();
        let _ = app.emit("usage-state", &state);
        notifications::finish_refresh(&app, &state, &settings, &notifications);
    });
}

fn register_shortcut(app: &AppHandle, shortcut: &str) -> Result<(), String> {
    app.global_shortcut()
        .on_shortcut(shortcut, |app, _, event| {
            if event.state == ShortcutState::Released {
                toggle_main_window(app);
            }
        })
        .map_err(|_| {
            crate::app_warn!("config", "global shortcut registration failed");
            "That global shortcut is invalid or already in use.".to_owned()
        })
}

pub(crate) fn apply_shortcut_change(
    app: &AppHandle,
    previous: Option<&str>,
    next: Option<&str>,
) -> Result<(), String> {
    if previous == next {
        return Ok(());
    }
    if let Some(previous) = previous {
        let _ = app.global_shortcut().unregister(previous);
    }
    if let Some(next) = next.filter(|shortcut| !shortcut.trim().is_empty()) {
        if let Err(error) = register_shortcut(app, next) {
            if let Some(previous) = previous {
                let _ = register_shortcut(app, previous);
            }
            return Err(error);
        }
    }
    crate::app_debug!("config", "global shortcut configuration updated");
    Ok(())
}

pub(crate) fn set_autostart(app: &AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        xdg_autostart::set_enabled(enabled)
            .map_err(|_| "Launch at login could not be updated.".to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let manager = app.autolaunch();
        #[cfg(target_os = "macos")]
        if login_item::available() {
            login_item::set_enabled(enabled).map_err(|error| {
                crate::app_warn!("config", "login item could not be updated: {error}");
                "Launch at login could not be updated.".to_owned()
            })?;
            // Replaced by the login item either way.
            if manager.is_enabled().unwrap_or(false) {
                let _ = manager.disable();
            }
            return Ok(());
        }
        let result = if enabled {
            manager.enable()
        } else {
            manager.disable()
        };
        result.map_err(|_| "Launch at login could not be updated.".to_owned())
    }
}

pub(crate) fn autostart_is_enabled(app: &AppHandle) -> Result<bool, ()> {
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        xdg_autostart::is_enabled().map_err(|_| ())
    }
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "macos")]
        if login_item::is_enabled() {
            return Ok(true);
        }
        app.autolaunch().is_enabled().map_err(|_| ())
    }
}

/// Whether System Settings blocks the registered login item.
pub(crate) fn autostart_needs_approval() -> bool {
    #[cfg(target_os = "macos")]
    return login_item::needs_approval();
    #[cfg(not(target_os = "macos"))]
    false
}

/// Moves a launch agent written by the autostart plugin to the login item.
#[cfg(target_os = "macos")]
fn migrate_autostart(app: &AppHandle) {
    if app.autolaunch().is_enabled().unwrap_or(false) && login_item::available() {
        let _ = set_autostart(app, true);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let process_start = std::time::Instant::now();
    #[cfg(target_os = "linux")]
    appearance::enable_compositing();
    #[cfg(target_os = "linux")]
    desktop_integration::initialize_x11_threads();

    let builder = tauri::Builder::default();
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        window::activate_existing_instance(app);
    }));

    let app = builder
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(PopupDismissGuard::default())
        .manage(updates::UpdateCoordinator::default())
        .setup(move |app| {
            #[cfg(target_os = "linux")]
            appearance::configure(app)?;
            logging::init(logging::default_log_path(), models::LogLevel::Info);

            app_info!(
                "lifecycle",
                "native setup entered ({} ms since run)",
                process_start.elapsed().as_millis()
            );
            #[cfg(target_os = "macos")]
            migrate_autostart(app.handle());

            app.handle().plugin(tauri_plugin_positioner::init())?;
            let desktop_integration = DesktopIntegration::detect();
            app_info!(
                "lifecycle",
                "desktop integration detected (tray={})",
                desktop_integration.tray_available()
            );
            app.manage(desktop_integration.clone());
            #[cfg(target_os = "macos")]
            app.manage(tray_presentation::MenuBarMenuState::default());

            let app_data_dir = app.path().app_data_dir()?;
            let database_path = app_data_dir.join("openquota.db");
            let storage = Arc::new(Storage::open(&database_path)?);
            provider_environment::initialize(storage.load_provider_environment()?);
            provider_environment::refresh_for_next_launch(storage.clone());
            app.manage(Arc::new(PanelResizeSession::new(storage.clone())));
            app_debug!("cache", "application database opened");
            let pricing = Arc::new(PricingStore::new(app_data_dir.join("pricing"))?);
            let sub2api = Arc::new(Sub2ApiProviders::new(
                app_data_dir.join("sub2api-connections"),
                storage.clone(),
            ));
            let mut providers = vec![
                Arc::new(ClaudeProvider::new(storage.clone(), pricing.clone())?)
                    as Arc<dyn UsageProvider>,
                Arc::new(CodexProvider::new(storage.clone(), pricing.clone())?)
                    as Arc<dyn UsageProvider>,
                Arc::new(CursorProvider::new(pricing.clone())?) as Arc<dyn UsageProvider>,
                Arc::new(AntigravityProvider::new(
                    app_data_dir.join("antigravity").join("auth.json"),
                    storage.clone(),
                    pricing.clone(),
                )?) as Arc<dyn UsageProvider>,
                Arc::new(CopilotProvider::new()?) as Arc<dyn UsageProvider>,
                Arc::new(DevinProvider::new()?) as Arc<dyn UsageProvider>,
                Arc::new(GrokProvider::new(storage.clone(), pricing.clone())?)
                    as Arc<dyn UsageProvider>,
                Arc::new(OpenCodeProvider::new(pricing.clone())) as Arc<dyn UsageProvider>,
                Arc::new(OpenRouterProvider::new()?) as Arc<dyn UsageProvider>,
                Arc::new(ZaiProvider::new(storage.clone(), pricing.clone())?)
                    as Arc<dyn UsageProvider>,
                Arc::new(KimiProvider::new(storage.clone(), pricing.clone())?)
                    as Arc<dyn UsageProvider>,
                Arc::new(MiniMaxProvider::new(storage.clone(), pricing.clone())?)
                    as Arc<dyn UsageProvider>,
            ];
            providers.extend(sub2api.runtimes());
            let registry = Arc::new(ProviderRegistry::new(providers)?);
            let (settings_service, credential_detection_plan) =
                SettingsService::new_deferred(storage.clone(), registry.clone())?;
            let settings = Arc::new(settings_service);
            let floating_window = desktop_integration.apply_window_mode(settings.get().window_mode);
            let service = Arc::new(ProviderService::new_with_settings(
                registry.clone(),
                storage.clone(),
                settings.clone(),
            ));
            logging::set_level(settings.get().log_level);
            app_info!(
                "config",
                "OpenQuota v{} starting (level={}, log=OpenQuota.log)",
                app.package_info().version,
                logging::current_level().log_label()
            );
            let notifications = Arc::new(NotificationEvaluator::default());
            app.manage(registry.clone());
            app.manage(service.clone());
            app.manage(settings.clone());
            app.manage(notifications.clone());
            app.manage(sub2api);
            app.manage(Arc::new(CodexResetClaimService::new()?));

            if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                if window::apply_panel_surface(&window, settings.get().theme).is_err() {
                    app_warn!("window", "initial panel surface theme could not be applied");
                }
                if !floating_window {
                    webview_memory::set_inactive(&window, true);
                }
            }

            if let Some(shortcut) = settings.get().global_shortcut {
                let _ = register_shortcut(app.handle(), &shortcut);
            }

            let tray_installed = if desktop_integration.tray_available() {
                match tray::install_tray(app) {
                    Ok(()) => {
                        app_info!("lifecycle", "system tray integration ready");
                        true
                    }
                    Err(error) => {
                        app_warn!(
                            "lifecycle",
                            "system tray integration failed; using standalone window: {error}"
                        );
                        desktop_integration.disable_tray();
                        let _ = app.remove_tray_by_id(tray::TRAY_ID);
                        false
                    }
                }
            } else {
                false
            };

            if desktop_integration.is_floating() {
                if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
                    if let Err(error) =
                        window::apply_window_mode(&window, settings.get().window_mode, true)
                    {
                        app_warn!(
                            "window",
                            "standalone startup mode could not be applied: {error}"
                        );
                        tray::show_standalone_window_fallback(&window);
                    }
                }
            }

            #[cfg(target_os = "linux")]
            if tray_installed {
                tray::spawn_status_notifier_monitor(app.handle().clone());
            }
            #[cfg(not(target_os = "linux"))]
            let _ = tray_installed;

            tray_presentation::update(
                app.handle(),
                &service.state(),
                &settings.get(),
                settings.registry(),
            );
            spawn_startup_credential_detection(
                app.handle().clone(),
                registry,
                service.clone(),
                settings.clone(),
                notifications.clone(),
                credential_detection_plan,
            );
            refresh_loop::spawn(
                app.handle().clone(),
                service,
                settings,
                notifications,
                pricing,
            );
            app_info!("lifecycle", "OpenQuota startup completed");

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap::get_bootstrap_state,
            commands::bootstrap::report_first_panel_frame,
            commands::provider::open_provider_link,
            commands::provider::probe_provider_proxy,
            commands::provider::get_provider_api_key_state,
            commands::provider::save_provider_api_key,
            commands::provider::delete_provider_api_key,
            commands::provider::get_sub2api_config_state,
            commands::provider::resolve_sub2api_codex_provider,
            commands::provider::resolve_sub2api_claude_provider,
            commands::provider::save_sub2api_config,
            commands::provider::clear_sub2api_config,
            commands::provider::delete_sub2api_config,
            commands::usage::refresh_usage,
            commands::usage::refresh_provider_usage,
            commands::usage::claim_codex_reset_credit,
            commands::settings::get_app_settings,
            commands::settings::save_app_settings,
            commands::settings::reset_customization,
            commands::settings::reset_all_settings,
            commands::settings::reset_provider_customization,
            commands::settings::request_notification_permission,
            commands::settings::open_notification_settings,
            commands::settings::get_log_path,
            commands::settings::open_log_folder,
            commands::window::dismiss_main_window,
            commands::window::navigate_menu_bar,
            commands::window::get_panel_resize_edge,
            commands::window::get_panel_height_mode,
            commands::window::replay_panel_backdrop,
            commands::window::fit_panel_to_content,
            commands::window::set_panel_height_automatic,
            commands::window::set_panel_height_manual,
            commands::window::begin_panel_resize,
            commands::window::lock_panel_resize_axis,
            commands::window::quit_app,
            updates::check_for_updates,
            updates::install_update,
            updates::open_update_page
        ])
        .on_window_event(handle_window_event)
        .build(tauri::generate_context!())
        .expect("error while running OpenQuota");
    // A menu bar app: no Dock icon and no app menu. tao applies this policy at
    // launch, before it creates the window, which can join a full-screen app's
    // space only if created while the app is an accessory.
    #[cfg(target_os = "macos")]
    let app = {
        let mut app = app;
        app.set_activation_policy(tauri::ActivationPolicy::Accessory);
        app
    };
    app.run(|_, _| {});
}
