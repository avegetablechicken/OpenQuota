//! The transaction every credential change goes through, after the credential itself is stored.
//!
//! A command supplies its store step, a plan for the settings, and a [`Policy`]. This module owns
//! the rest: the lock order, recording the change, the settings change and its metric template,
//! clearing cached usage, telling the app, and the refresh that follows.

use std::{future::Future, sync::Arc};

use tauri::{AppHandle, Emitter};

use crate::{
    commands::settings::settings_view_state,
    models::AppSettings,
    notifications::finish_refresh,
    pacing::NotificationEvaluator,
    service::ProviderService,
    settings::{MetricTemplateChange, SettingsService},
    tray_presentation,
};

/// What a credential change needs from the app around it.
pub(crate) trait CredentialApp: Send + Sync + 'static {
    fn settings(&self) -> &SettingsService;
    /// Shows changed settings: the tray and the settings view state.
    fn settings_changed(&self, settings: &AppSettings);
    /// Shows the current usage: the tray and the usage view state.
    fn usage_changed(&self);
    fn clear_cached_usage(&self, provider_id: &str) -> Result<(), String>;
    fn refresh(&self, provider_id: &str) -> impl Future<Output = ()> + Send;
    /// Ends a change: the tray, notification pruning and alerts for the current usage.
    fn finish(&self);
}

/// How the settings follow the stored credential.
pub(crate) enum SettingsChange {
    Reconcile {
        detected: bool,
        enable: bool,
        template: Option<TemplateHandoff>,
    },
    Remove,
}

/// A new metric template, and how to record that it was applied.
pub(crate) struct TemplateHandoff {
    pub change: MetricTemplateChange,
    pub mark_applied: Box<dyn FnOnce() -> Result<(), String> + Send>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Timing {
    /// The command returns after the refresh.
    Wait,
    /// The command returns at once and the refresh runs on its own.
    Background,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    /// Names the change in logs, as in "provider state after {action} could not be reconciled".
    pub action: &'static str,
    /// The error when the store step could not run at all.
    pub store_failure: &'static str,
    pub clear_cached_usage: bool,
    pub timing: Timing,
}

pub(crate) struct Committed<T> {
    pub value: T,
    /// False when the store succeeded but the settings or cached usage could not follow it. The
    /// change is not undone: the command reports it as a warning.
    pub completed: bool,
    /// The refresh, when it runs in the background.
    #[cfg_attr(not(test), allow(dead_code))]
    pub background: Option<tauri::async_runtime::JoinHandle<()>>,
}

/// Stores a credential change and brings settings, cached usage and the app in line with it.
///
/// Only a failed store fails the change. Once the credential is stored, a step that cannot follow
/// is logged and reported through [`Committed::completed`], and the rest still runs.
///
/// The store step runs under the credential lock; the settings change runs under the command lock
/// too. Both are released before the refresh, which runs when the provider is enabled after the
/// change. Every change ends with [`CredentialApp::finish`], after the refresh if there is one.
pub(crate) async fn commit<A, T, S, P>(
    app: &Arc<A>,
    provider_id: &str,
    policy: Policy,
    store: S,
    plan: P,
) -> Result<Committed<T>, String>
where
    A: CredentialApp,
    T: Send + 'static,
    S: FnOnce() -> Result<T, String> + Send + 'static,
    P: FnOnce(&T, &AppSettings) -> SettingsChange + Send,
{
    let settings = app.settings();
    let credential_guard = settings.lock_credential_mutation().await;
    settings.record_provider_credential_mutation();
    let value = tauri::async_runtime::spawn_blocking(store)
        .await
        .map_err(|_| policy.store_failure.to_owned())??;

    let command_guard = settings.lock_command_mutation().await;
    let change = plan(&value, &settings.get());
    let mut completed = true;
    if let Err(error) = change_settings(app.as_ref(), provider_id, change).await {
        crate::app_warn!(
            "auth",
            "provider state after {} could not be reconciled for {provider_id}: {error}",
            policy.action
        );
        completed = false;
    }
    if policy.clear_cached_usage {
        match app.clear_cached_usage(provider_id) {
            Ok(()) => app.usage_changed(),
            Err(error) => {
                crate::app_warn!(
                    "auth",
                    "cached usage after {} could not be cleared for {provider_id}: {error}",
                    policy.action
                );
                completed = false;
            }
        }
    }
    let enabled = settings
        .get()
        .providers
        .iter()
        .any(|provider| provider.id == provider_id && provider.enabled);
    drop(command_guard);
    drop(credential_guard);

    let mut background = None;
    match (enabled, policy.timing) {
        (false, _) => app.finish(),
        (true, Timing::Wait) => refresh(app.as_ref(), provider_id).await,
        (true, Timing::Background) => {
            let app = app.clone();
            let provider_id = provider_id.to_owned();
            background = Some(tauri::async_runtime::spawn(async move {
                refresh(app.as_ref(), &provider_id).await;
            }));
        }
    }
    Ok(Committed {
        value,
        completed,
        background,
    })
}

async fn change_settings(
    app: &impl CredentialApp,
    provider_id: &str,
    change: SettingsChange,
) -> Result<(), String> {
    let settings = app.settings();
    match change {
        SettingsChange::Reconcile {
            detected,
            enable,
            template,
        } => {
            let (template, mark_applied) = match template {
                Some(handoff) => (Some(handoff.change), Some(handoff.mark_applied)),
                None => (None, None),
            };
            let updated = settings.reconcile_provider_credential_state_with_metrics(
                provider_id,
                detected,
                enable,
                template,
            )?;
            app.settings_changed(&updated);
            if let Some(mark_applied) = mark_applied {
                let marked = tauri::async_runtime::spawn_blocking(mark_applied)
                    .await
                    .map_err(|_| {
                        "The Sub2API metric template state could not be saved.".to_owned()
                    })?;
                if let Err(error) = marked {
                    crate::app_warn!(
                        "auth",
                        "Sub2API metric template was applied but could not be marked complete: {error}"
                    );
                }
            }
        }
        SettingsChange::Remove => {
            let updated = settings.remove_dynamic_provider(provider_id)?;
            app.settings_changed(&updated);
        }
    }
    Ok(())
}

async fn refresh(app: &impl CredentialApp, provider_id: &str) {
    app.refresh(provider_id).await;
    app.usage_changed();
    app.finish();
}

/// The running app: Tauri events, the tray, the provider service and notifications.
pub(crate) struct TauriApp {
    app: AppHandle,
    service: Arc<ProviderService>,
    settings: Arc<SettingsService>,
    notifications: Arc<NotificationEvaluator>,
}

impl TauriApp {
    pub(crate) fn new(
        app: AppHandle,
        service: &Arc<ProviderService>,
        settings: &Arc<SettingsService>,
        notifications: &Arc<NotificationEvaluator>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            service: service.clone(),
            settings: settings.clone(),
            notifications: notifications.clone(),
        })
    }
}

impl CredentialApp for TauriApp {
    fn settings(&self) -> &SettingsService {
        &self.settings
    }

    fn settings_changed(&self, settings: &AppSettings) {
        tray_presentation::update(
            &self.app,
            &self.service.state(),
            settings,
            self.settings.registry(),
        );
        let _ = self.app.emit(
            "settings-state",
            settings_view_state(&self.app, &self.settings),
        );
    }

    fn usage_changed(&self) {
        let state = self.service.state();
        // A cleared connection's quotas leave the tray now, not when the refresh answers.
        tray_presentation::update(
            &self.app,
            &state,
            &self.settings.get(),
            self.settings.registry(),
        );
        let _ = self.app.emit("usage-state", &state);
    }

    fn clear_cached_usage(&self, provider_id: &str) -> Result<(), String> {
        self.service
            .clear_provider_data(provider_id)
            .map_err(|_| "Cached Sub2API quota could not be cleared.".to_owned())
    }

    fn refresh(&self, provider_id: &str) -> impl Future<Output = ()> + Send {
        let service = self.service.clone();
        let provider_id = provider_id.to_owned();
        async move {
            service.refresh(&provider_id, true).await;
        }
    }

    fn finish(&self) {
        finish_refresh(
            &self.app,
            &self.service.state(),
            &self.settings,
            &self.notifications,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Mutex};

    use tempfile::tempdir;

    use super::{commit, CredentialApp, Policy, SettingsChange};
    use crate::{
        commands::provider::{
            CLEAR_SUB2API, DELETE_API_KEY, DELETE_SUB2API, SAVE_API_KEY, SAVE_SUB2API,
        },
        models::AppSettings,
        providers::{codex, ProviderRegistry},
        settings::SettingsService,
        storage::Storage,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Settings,
        Cleared,
        Usage,
        Refresh,
        Finish,
    }
    use Event::*;

    /// Records what the transaction tells the app.
    struct Recorder {
        settings: SettingsService,
        events: Mutex<Vec<Event>>,
        fail_clear: Mutex<bool>,
    }

    impl Recorder {
        fn push(&self, event: Event) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl CredentialApp for Recorder {
        fn settings(&self) -> &SettingsService {
            &self.settings
        }

        fn settings_changed(&self, _: &AppSettings) {
            self.push(Settings);
        }

        fn usage_changed(&self) {
            self.push(Usage);
        }

        fn clear_cached_usage(&self, _: &str) -> Result<(), String> {
            if *self.fail_clear.lock().unwrap() {
                return Err("The cache is unavailable.".to_owned());
            }
            self.push(Cleared);
            Ok(())
        }

        fn refresh(&self, _: &str) -> impl std::future::Future<Output = ()> + Send {
            self.push(Refresh);
            async {}
        }

        fn finish(&self) {
            self.push(Finish);
        }
    }

    fn recorder(enabled: bool) -> (std::sync::Arc<Recorder>, tempfile::TempDir) {
        let directory = tempdir().unwrap();
        let storage =
            std::sync::Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let registry = std::sync::Arc::new(
            ProviderRegistry::from_definitions(vec![codex::definition()]).unwrap(),
        );
        let detected = HashSet::from(["codex".to_owned()]);
        let settings = SettingsService::new_for_test(storage, registry, &detected).unwrap();
        let mut current = settings.get();
        current.providers[0].enabled = enabled;
        settings.update(current).unwrap();
        let recorder = Recorder {
            settings,
            events: Mutex::new(Vec::new()),
            fail_clear: Mutex::new(false),
        };
        (std::sync::Arc::new(recorder), directory)
    }

    fn reconcile(detected: bool, enable: bool) -> SettingsChange {
        SettingsChange::Reconcile {
            detected,
            enable,
            template: None,
        }
    }

    /// Runs a change whose store step succeeds and returns what the app was told.
    fn run(
        policy: Policy,
        provider_id: &str,
        enabled: bool,
        change: fn() -> SettingsChange,
    ) -> (Result<bool, String>, Vec<Event>, bool) {
        let (app, _directory) = recorder(enabled);
        let result = tauri::async_runtime::block_on(async {
            let committed = commit(&app, provider_id, policy, || Ok(()), |_, _| change()).await?;
            if let Some(background) = committed.background {
                background.await.unwrap();
            }
            Ok(committed.completed)
        });
        let events = app.events.lock().unwrap().clone();
        let enabled_after = app.settings.get().providers[0].enabled;
        (result, events, enabled_after)
    }

    /// A command's policy, whether its provider starts enabled, its settings change, and the events.
    type Case = (
        &'static str,
        Policy,
        bool,
        fn() -> SettingsChange,
        &'static [Event],
    );

    #[test]
    fn each_credential_command_tells_the_app_what_changed() {
        let cases: [Case; 8] = [
            (
                "save key",
                SAVE_API_KEY,
                false,
                || reconcile(true, true),
                &[Settings, Refresh, Usage, Finish],
            ),
            (
                "delete key",
                DELETE_API_KEY,
                true,
                || reconcile(false, false),
                &[Settings, Refresh, Usage, Finish],
            ),
            (
                "delete key, disabled",
                DELETE_API_KEY,
                false,
                || reconcile(false, false),
                &[Settings, Finish],
            ),
            (
                "save Sub2API",
                SAVE_SUB2API,
                true,
                || reconcile(true, false),
                &[Settings, Cleared, Usage, Refresh, Usage, Finish],
            ),
            (
                "save Sub2API, disabled",
                SAVE_SUB2API,
                false,
                || reconcile(true, false),
                &[Settings, Cleared, Usage, Finish],
            ),
            (
                "clear Sub2API",
                CLEAR_SUB2API,
                false,
                || reconcile(false, true),
                &[Settings, Cleared, Usage, Refresh, Usage, Finish],
            ),
            (
                "delete Sub2API",
                DELETE_SUB2API,
                true,
                || SettingsChange::Remove,
                &[Settings, Cleared, Usage, Finish],
            ),
            (
                "delete Sub2API, disabled",
                DELETE_SUB2API,
                false,
                || SettingsChange::Remove,
                &[Settings, Cleared, Usage, Finish],
            ),
        ];
        for (name, policy, enabled, change, expected) in cases {
            let (result, events, _) = run(policy, "codex", enabled, change);
            assert_eq!(result, Ok(true), "{name}");
            assert_eq!(events, expected, "{name}");
        }
    }

    #[test]
    fn a_removed_connection_ends_disabled_and_a_first_setup_ends_enabled() {
        let (_, _, enabled) = run(DELETE_SUB2API, "codex", true, || SettingsChange::Remove);
        assert!(!enabled);
        let (_, _, enabled) = run(SAVE_API_KEY, "codex", false, || reconcile(true, true));
        assert!(enabled);
    }

    #[test]
    fn a_change_whose_settings_cannot_follow_still_finishes_and_reports_it() {
        // No settings exist for an unknown provider, so the settings cannot follow the store.
        // Nothing is enabled, so nothing is refreshed.
        let cases: [Case; 5] = [
            (
                "save key",
                SAVE_API_KEY,
                true,
                || reconcile(true, true),
                &[Finish],
            ),
            (
                "delete key",
                DELETE_API_KEY,
                true,
                || reconcile(false, false),
                &[Finish],
            ),
            (
                "save Sub2API",
                SAVE_SUB2API,
                true,
                || reconcile(true, true),
                &[Cleared, Usage, Finish],
            ),
            (
                "clear Sub2API",
                CLEAR_SUB2API,
                true,
                || reconcile(false, true),
                &[Cleared, Usage, Finish],
            ),
            (
                "delete Sub2API",
                DELETE_SUB2API,
                true,
                || SettingsChange::Remove,
                &[Cleared, Usage, Finish],
            ),
        ];
        for (name, policy, enabled, change, expected) in cases {
            let (result, events, _) = run(policy, "unknown", enabled, change);
            assert_eq!(result, Ok(false), "{name}");
            assert_eq!(events, expected, "{name}");
        }
    }

    #[test]
    fn a_change_whose_cached_usage_cannot_be_cleared_still_refreshes_and_reports_it() {
        let (app, _directory) = recorder(true);
        *app.fail_clear.lock().unwrap() = true;
        let completed = tauri::async_runtime::block_on(async {
            let committed = commit(
                &app,
                "codex",
                CLEAR_SUB2API,
                || Ok(()),
                |_, _| reconcile(false, true),
            )
            .await
            .unwrap();
            committed.completed
        });
        assert!(!completed);
        assert_eq!(
            *app.events.lock().unwrap(),
            [Settings, Refresh, Usage, Finish]
        );
    }

    #[test]
    fn a_failed_store_changes_nothing() {
        let (app, _directory) = recorder(true);
        let before = app.settings.get();
        let result = tauri::async_runtime::block_on(commit(
            &app,
            "codex",
            SAVE_API_KEY,
            || Err::<(), _>("The credential store is locked.".to_owned()),
            |_, _| reconcile(true, true),
        ));
        assert_eq!(
            result.err().as_deref(),
            Some("The credential store is locked.")
        );
        assert!(app.events.lock().unwrap().is_empty());
        assert_eq!(app.settings.get(), before);
    }
}
