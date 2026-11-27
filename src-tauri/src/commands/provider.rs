use std::sync::Arc;

use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use zeroize::Zeroizing;

use crate::{
    commands::credential_change::{
        self, OnSettingsError, Policy, SettingsChange, TauriApp, TemplateHandoff, Timing,
    },
    models::{ApiKeyMutationOutcome, ApiKeyStatus, ProviderApiKeyState, ProviderLink},
    pacing::NotificationEvaluator,
    providers::{
        sub2api::{
            claude_provider_base_url, codex_provider_base_url, metric_template, track_provider_id,
            Sub2ApiConfigInput, Sub2ApiConfigSaveOutcome, Sub2ApiConfigState, Sub2ApiProviders,
        },
        ProviderRegistry, UsageProvider,
    },
    service::ProviderService,
    settings::{MetricTemplateChange, SettingsService},
};

#[tauri::command]
pub async fn probe_provider_proxy(
    registry: State<'_, Arc<ProviderRegistry>>,
    provider_id: String,
    proxy: String,
) -> Result<crate::http_client::ProxyExitLocation, String> {
    if registry.definition(&provider_id).is_none() {
        return Err("Unknown provider.".to_owned());
    }
    tauri::async_runtime::spawn_blocking(move || crate::http_client::proxy_exit_location(&proxy))
        .await
        .map_err(|_| "The proxy exit location could not be detected.".to_owned())?
}

fn resolve_provider_link<'a>(
    registry: &'a ProviderRegistry,
    provider_id: &str,
    link_index: usize,
) -> Result<&'a ProviderLink, String> {
    registry
        .definition(provider_id)
        .and_then(|provider| provider.links.get(link_index))
        .ok_or_else(|| "That provider link is unavailable.".to_owned())
}

#[tauri::command]
pub fn open_provider_link(
    app: AppHandle,
    registry: State<'_, Arc<ProviderRegistry>>,
    provider_id: String,
    link_index: usize,
) -> Result<(), String> {
    let link = resolve_provider_link(&registry, &provider_id, link_index)?;
    crate::app_debug!(
        "http",
        "opening {provider_id} provider link {}",
        crate::logging::redact_url(&link.url)
    );
    app.opener()
        .open_url(&link.url, None::<&str>)
        .map_err(|_| "That provider link could not be opened.".to_owned())
}

async fn api_key_state(
    registry: Arc<ProviderRegistry>,
    provider_id: String,
) -> Result<Option<ProviderApiKeyState>, String> {
    let runtime = registry
        .runtime(&provider_id)
        .ok_or_else(|| "Unknown provider.".to_owned())?;
    tauri::async_runtime::spawn_blocking(move || {
        let Some(status) = runtime.api_key_status() else {
            return Ok(None);
        };
        let status = status.map_err(|error| error.to_string())?;
        Ok(Some(ProviderApiKeyState {
            provider_id,
            status,
        }))
    })
    .await
    .map_err(|_| "The API key status could not be read.".to_owned())?
}

enum ApiKeyMutation<'a> {
    Save(&'a str),
    Delete,
}

struct AppliedApiKeyMutation {
    state: ProviderApiKeyState,
    status_uncertain: bool,
}

fn mutate_api_key(
    runtime: &dyn UsageProvider,
    provider_id: String,
    mutation: ApiKeyMutation<'_>,
) -> Result<AppliedApiKeyMutation, String> {
    let initial_status = runtime
        .api_key_status()
        .ok_or_else(|| "That provider does not accept an API key.".to_owned())?
        .ok();
    let fallback_status = match &mutation {
        ApiKeyMutation::Save(_) => {
            if matches!(
                initial_status,
                Some(
                    ApiKeyStatus::FromEnvironment
                        | ApiKeyStatus::FromConfig
                        | ApiKeyStatus::OverrideActive
                )
            ) {
                ApiKeyStatus::OverrideActive
            } else {
                ApiKeyStatus::Saved
            }
        }
        ApiKeyMutation::Delete => ApiKeyStatus::NotSet,
    };

    match mutation {
        ApiKeyMutation::Save(value) => runtime.save_api_key(value),
        ApiKeyMutation::Delete => runtime.delete_api_key(),
    }
    .map_err(|error| error.to_string())?;

    let (status, status_uncertain) = match runtime.api_key_status() {
        Some(Ok(status)) => (status, false),
        Some(Err(_)) | None => (fallback_status, true),
    };
    Ok(AppliedApiKeyMutation {
        state: ProviderApiKeyState {
            provider_id,
            status,
        },
        status_uncertain,
    })
}

fn incomplete_mutation_warning(action: &str) -> String {
    format!(
        "The API key was {action}, but OpenQuota could not finish updating provider status. Restart OpenQuota or try again."
    )
}

#[tauri::command]
pub async fn get_provider_api_key_state(
    registry: State<'_, Arc<ProviderRegistry>>,
    provider_id: String,
) -> Result<Option<ProviderApiKeyState>, String> {
    api_key_state(registry.inner().clone(), provider_id).await
}

pub(crate) const SAVE_API_KEY: Policy = Policy {
    action: "API key save",
    store_failure: "The API key could not be saved.",
    on_settings_error: OnSettingsError::Warn,
    clear_cached_usage: false,
    timing: Timing::Wait,
};

pub(crate) const DELETE_API_KEY: Policy = Policy {
    action: "API key removal",
    store_failure: "The API key could not be removed.",
    on_settings_error: OnSettingsError::Warn,
    clear_cached_usage: false,
    timing: Timing::Wait,
};

pub(crate) const SAVE_SUB2API: Policy = Policy {
    action: "Sub2API save",
    store_failure: "The Sub2API connection could not be saved.",
    on_settings_error: OnSettingsError::Fail,
    clear_cached_usage: true,
    timing: Timing::Background,
};

pub(crate) const CLEAR_SUB2API: Policy = Policy {
    action: "Sub2API clear",
    store_failure: "The Sub2API connection could not be cleared.",
    on_settings_error: OnSettingsError::Fail,
    clear_cached_usage: true,
    timing: Timing::Wait,
};

pub(crate) const DELETE_SUB2API: Policy = Policy {
    action: "Sub2API removal",
    store_failure: "The Sub2API connection could not be removed.",
    on_settings_error: OnSettingsError::Fail,
    clear_cached_usage: true,
    timing: Timing::Wait,
};

#[tauri::command]
pub async fn save_provider_api_key(
    app: AppHandle,
    registry: State<'_, Arc<ProviderRegistry>>,
    service: State<'_, Arc<ProviderService>>,
    settings: State<'_, Arc<SettingsService>>,
    notifications: State<'_, Arc<NotificationEvaluator>>,
    provider_id: String,
    api_key: String,
) -> Result<ApiKeyMutationOutcome, String> {
    let api_key = Zeroizing::new(api_key);
    let runtime = registry
        .runtime(&provider_id)
        .ok_or_else(|| "Unknown provider.".to_owned())?;
    let app = TauriApp::new(app, &service, &settings, &notifications);
    let provider_for_save = provider_id.clone();
    let committed = credential_change::commit(
        &app,
        &provider_id,
        SAVE_API_KEY,
        move || {
            mutate_api_key(
                runtime.as_ref(),
                provider_for_save,
                ApiKeyMutation::Save(api_key.as_str()),
            )
        },
        |_, _| SettingsChange::Reconcile {
            detected: true,
            enable: true,
            template: None,
        },
    )
    .await?;
    let applied = committed.value;
    if applied.status_uncertain {
        crate::app_warn!(
            "auth",
            "API key status could not be confirmed after saving for {provider_id}"
        );
    }
    crate::app_info!("auth", "API key saved for {provider_id}");
    Ok(ApiKeyMutationOutcome {
        state: applied.state,
        warning: (applied.status_uncertain || !committed.settings_saved)
            .then(|| incomplete_mutation_warning("saved securely")),
    })
}

#[tauri::command]
pub async fn delete_provider_api_key(
    app: AppHandle,
    registry: State<'_, Arc<ProviderRegistry>>,
    service: State<'_, Arc<ProviderService>>,
    settings: State<'_, Arc<SettingsService>>,
    notifications: State<'_, Arc<NotificationEvaluator>>,
    provider_id: String,
) -> Result<ApiKeyMutationOutcome, String> {
    let runtime = registry
        .runtime(&provider_id)
        .ok_or_else(|| "Unknown provider.".to_owned())?;
    let app = TauriApp::new(app, &service, &settings, &notifications);
    let provider_for_delete = provider_id.clone();
    let committed = credential_change::commit(
        &app,
        &provider_id,
        DELETE_API_KEY,
        move || {
            mutate_api_key(
                runtime.as_ref(),
                provider_for_delete,
                ApiKeyMutation::Delete,
            )
        },
        |applied: &AppliedApiKeyMutation, _| SettingsChange::Reconcile {
            detected: applied.state.status != ApiKeyStatus::NotSet,
            enable: false,
            template: None,
        },
    )
    .await?;
    let applied = committed.value;
    if applied.status_uncertain {
        crate::app_warn!(
            "auth",
            "API key status could not be confirmed after removal for {provider_id}"
        );
    }
    crate::app_info!("auth", "saved API key removed for {provider_id}");
    Ok(ApiKeyMutationOutcome {
        state: applied.state,
        warning: (applied.status_uncertain || !committed.settings_saved)
            .then(|| incomplete_mutation_warning("removed")),
    })
}

#[tauri::command]
pub async fn get_sub2api_config_state(
    providers: State<'_, Arc<Sub2ApiProviders>>,
    provider_id: String,
) -> Result<Sub2ApiConfigState, String> {
    let provider = providers
        .provider(&provider_id)
        .ok_or_else(|| "Unknown Sub2API provider.".to_owned())?;
    tauri::async_runtime::spawn_blocking(move || provider.config_state())
        .await
        .map_err(|_| "The Sub2API connection could not be read.".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn resolve_sub2api_codex_provider(provider: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || codex_provider_base_url(&provider))
        .await
        .map_err(|_| "The Codex provider could not be resolved.".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn resolve_sub2api_claude_provider(provider: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || claude_provider_base_url(&provider))
        .await
        .map_err(|_| "The Claude provider could not be resolved.".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn save_sub2api_config(
    app: AppHandle,
    providers: State<'_, Arc<Sub2ApiProviders>>,
    service: State<'_, Arc<ProviderService>>,
    settings: State<'_, Arc<SettingsService>>,
    notifications: State<'_, Arc<NotificationEvaluator>>,
    provider_id: String,
    config: Sub2ApiConfigInput,
) -> Result<Sub2ApiConfigState, String> {
    let provider = providers
        .provider(&provider_id)
        .ok_or_else(|| "Unknown Sub2API provider.".to_owned())?;
    let app = TauriApp::new(app, &service, &settings, &notifications);
    let provider_for_save = provider.clone();
    let committed = credential_change::commit(
        &app,
        &provider_id,
        SAVE_SUB2API,
        move || {
            provider_for_save
                .save_config(config)
                .map_err(|error| error.to_string())
        },
        |outcome: &Sub2ApiConfigSaveOutcome, current| {
            let template = outcome.apply_metric_template.then(|| TemplateHandoff {
                change: MetricTemplateChange {
                    template: metric_template(
                        &provider_id,
                        outcome.state.upstream,
                        outcome.state.mode,
                    ),
                    previous: outcome
                        .previous_track
                        .map(|(mode, upstream)| metric_template(&provider_id, upstream, mode)),
                    upstream_provider_id: Some(track_provider_id(
                        outcome.state.mode,
                        outcome.state.upstream,
                    )),
                },
                mark_applied: Box::new(move || {
                    provider
                        .mark_metric_template_applied()
                        .map_err(|error| error.to_string())
                }),
            });
            SettingsChange::Reconcile {
                detected: true,
                // First setup enables the provider; editing an existing connection keeps its toggle.
                enable: current
                    .providers
                    .iter()
                    .find(|layout| layout.id == provider_id)
                    .is_some_and(|layout| !layout.detected),
                template,
            }
        },
    )
    .await?;
    crate::app_info!("auth", "Sub2API connection saved");
    Ok(committed.value.state)
}

#[tauri::command]
pub async fn clear_sub2api_config(
    app: AppHandle,
    providers: State<'_, Arc<Sub2ApiProviders>>,
    service: State<'_, Arc<ProviderService>>,
    settings: State<'_, Arc<SettingsService>>,
    notifications: State<'_, Arc<NotificationEvaluator>>,
    provider_id: String,
) -> Result<Sub2ApiConfigState, String> {
    let provider = providers
        .provider(&provider_id)
        .ok_or_else(|| "Unknown Sub2API provider.".to_owned())?;
    let app = TauriApp::new(app, &service, &settings, &notifications);
    let committed = credential_change::commit(
        &app,
        &provider_id,
        CLEAR_SUB2API,
        move || provider.delete_config().map_err(|error| error.to_string()),
        |_, _| SettingsChange::Reconcile {
            detected: false,
            enable: true,
            template: None,
        },
    )
    .await?;
    crate::app_info!("auth", "Sub2API connection cleared");
    Ok(committed.value)
}

#[tauri::command]
pub async fn delete_sub2api_config(
    app: AppHandle,
    providers: State<'_, Arc<Sub2ApiProviders>>,
    service: State<'_, Arc<ProviderService>>,
    settings: State<'_, Arc<SettingsService>>,
    notifications: State<'_, Arc<NotificationEvaluator>>,
    provider_id: String,
) -> Result<Sub2ApiConfigState, String> {
    let provider = providers
        .provider(&provider_id)
        .ok_or_else(|| "Unknown Sub2API provider.".to_owned())?;
    let app = TauriApp::new(app, &service, &settings, &notifications);
    let committed = credential_change::commit(
        &app,
        &provider_id,
        DELETE_SUB2API,
        move || provider.delete_config().map_err(|error| error.to_string()),
        |_, _| SettingsChange::Remove,
    )
    .await?;
    crate::app_info!("auth", "Sub2API connection removed");
    Ok(committed.value)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
    };

    use crate::{
        models::{
            ApiKeyStatus, MetricDefinition, MetricSection, MetricSource, ProviderDefinition,
            ProviderErrorKind, ProviderLink, ProviderSnapshot,
        },
        providers::{ProviderError, ProviderRegistry, UsageProvider},
    };

    use super::{mutate_api_key, resolve_provider_link, ApiKeyMutation};

    struct MutatingProvider {
        statuses: Mutex<VecDeque<Result<ApiKeyStatus, ProviderError>>>,
        saved_value: Mutex<Option<String>>,
        deleted: AtomicBool,
    }

    impl MutatingProvider {
        fn new(statuses: Vec<Result<ApiKeyStatus, ProviderError>>) -> Self {
            Self {
                statuses: Mutex::new(statuses.into()),
                saved_value: Mutex::new(None),
                deleted: AtomicBool::new(false),
            }
        }
    }

    impl UsageProvider for MutatingProvider {
        fn definition(&self) -> ProviderDefinition {
            registry().definition("provider").unwrap().clone()
        }

        fn has_local_credentials(&self) -> bool {
            false
        }

        fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
            unreachable!()
        }

        fn api_key_status(&self) -> Option<Result<ApiKeyStatus, ProviderError>> {
            Some(
                self.statuses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok(ApiKeyStatus::NotSet)),
            )
        }

        fn save_api_key(&self, value: &str) -> Result<(), ProviderError> {
            *self.saved_value.lock().unwrap() = Some(value.to_owned());
            Ok(())
        }

        fn delete_api_key(&self) -> Result<(), ProviderError> {
            self.deleted.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn registry() -> ProviderRegistry {
        ProviderRegistry::from_definitions(vec![ProviderDefinition {
            id: "provider".into(),
            display_name: "Provider".into(),
            short_name: "P".into(),
            fallback_enabled: true,
            local_usage_source_note: None,
            links: vec![ProviderLink::new("Status", "https://status.example.com/")],
            metrics: vec![MetricDefinition::new(
                "provider.session",
                "Session",
                MetricSource::Quota {
                    source_id: "session".into(),
                    session_window: true,
                },
                true,
                true,
                MetricSection::AlwaysVisible,
                true,
                Some("S"),
                None,
            )],
        }])
        .unwrap()
    }

    #[test]
    fn resolves_only_links_declared_by_the_provider_registry() {
        let registry = registry();

        assert_eq!(
            resolve_provider_link(&registry, "provider", 0).unwrap().url,
            "https://status.example.com/"
        );
        assert!(resolve_provider_link(&registry, "provider", 1).is_err());
        assert!(resolve_provider_link(&registry, "unknown", 0).is_err());
    }

    #[test]
    fn applied_api_key_save_is_not_reported_as_failed_when_status_refresh_fails() {
        let provider = MutatingProvider::new(vec![
            Ok(ApiKeyStatus::FromEnvironment),
            Err(ProviderError::new(
                ProviderErrorKind::CredentialStorage,
                "status unavailable",
            )),
        ]);

        let applied =
            mutate_api_key(&provider, "provider".into(), ApiKeyMutation::Save("secret")).unwrap();

        assert_eq!(applied.state.status, ApiKeyStatus::OverrideActive);
        assert!(applied.status_uncertain);
        assert_eq!(
            provider.saved_value.lock().unwrap().as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn applied_api_key_delete_is_not_reported_as_failed_when_status_refresh_fails() {
        let provider = MutatingProvider::new(vec![
            Ok(ApiKeyStatus::Saved),
            Err(ProviderError::new(
                ProviderErrorKind::CredentialStorage,
                "status unavailable",
            )),
        ]);

        let applied = mutate_api_key(&provider, "provider".into(), ApiKeyMutation::Delete).unwrap();

        assert_eq!(applied.state.status, ApiKeyStatus::NotSet);
        assert!(applied.status_uncertain);
        assert!(provider.deleted.load(Ordering::SeqCst));
    }
}
