mod normalize;

pub use normalize::{default_settings, normalize, MetricTemplateChange};

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
};

use serde_json::{Map, Value};

use crate::{
    metric_policy::MetricPolicy,
    models::{AppSettings, MetricLayout, ProviderCatalog, SettingsViewState},
    providers::{CredentialProbeResults, CredentialProbeStatus, ProviderRegistry},
    storage::{ProviderAccountUpdate, Storage, StorageError},
};

use normalize::{
    account_custom_name, account_record, apply_metric_template, detected_provider_set,
    enabled_provider_set, reset_provider_layout, set_account_custom_name, track_template_defaults,
};

#[derive(Debug, Clone)]
pub struct CredentialDetectionPlan {
    provider_ids: Vec<String>,
    auto_enable_provider_ids: HashSet<String>,
    replace_fallback: bool,
    enablement_revision: u64,
    credential_revision: u64,
}

impl CredentialDetectionPlan {
    pub fn provider_ids(&self) -> &[String] {
        &self.provider_ids
    }
}

pub struct CredentialDetectionOutcome {
    pub settings: AppSettings,
    pub newly_enabled_provider_ids: Vec<String>,
}

pub struct SettingsService {
    storage: Arc<Storage>,
    registry: Arc<ProviderRegistry>,
    settings: RwLock<AppSettings>,
    command_mutation: tokio::sync::Mutex<()>,
    credential_mutation: tokio::sync::Mutex<()>,
    enablement_revision: AtomicU64,
    credential_revision: AtomicU64,
    settings_revision: AtomicU64,
    account_revision: AtomicU64,
    active_account_identities: RwLock<HashMap<String, String>>,
}

impl SettingsService {
    #[cfg(test)]
    pub(crate) fn new_for_test(
        storage: Arc<Storage>,
        registry: Arc<ProviderRegistry>,
        detected: &HashSet<String>,
    ) -> Result<Self, StorageError> {
        let mut settings = storage
            .load_settings()?
            .unwrap_or_else(|| default_settings(&registry, detected));
        let migrating_templates = settings.schema_version < 9;
        normalize(&registry, &mut settings, detected);
        let template_updates = sync_connection_metric_templates(
            &registry,
            &storage,
            &mut settings,
            migrating_templates,
        )?;
        storage.save_settings_with_updates(&settings, &[], &template_updates)?;
        let service = Self {
            storage,
            registry,
            settings: RwLock::new(settings),
            command_mutation: tokio::sync::Mutex::new(()),
            credential_mutation: tokio::sync::Mutex::new(()),
            enablement_revision: AtomicU64::new(0),
            credential_revision: AtomicU64::new(0),
            settings_revision: AtomicU64::new(0),
            account_revision: AtomicU64::new(0),
            active_account_identities: RwLock::new(HashMap::new()),
        };
        crate::http_client::set_provider_proxies(&service.get().provider_proxies);
        service.activate_launch_accounts()?;
        Ok(service)
    }

    /// Loads settings immediately and returns a plan for non-blocking credential detection.
    ///
    /// Fresh installs render the registry fallback without waiting for credential stores. Existing
    /// installs keep their choices; only providers never seen before are eligible for automatic
    /// enablement after the probe completes.
    pub fn new_deferred(
        storage: Arc<Storage>,
        registry: Arc<ProviderRegistry>,
    ) -> Result<(Self, CredentialDetectionPlan), StorageError> {
        let saved = storage.load_settings()?;
        let fresh_install = saved.is_none();
        let mut settings = saved.unwrap_or_else(|| default_settings(&registry, &HashSet::new()));
        let detected = settings
            .providers
            .iter()
            .filter(|provider| provider.detected && registry.definition(&provider.id).is_some())
            .map(|provider| provider.id.clone())
            .collect::<HashSet<_>>();
        let previously_known = settings
            .known_provider_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let can_identify_new_providers = !fresh_install && !previously_known.is_empty();
        let auto_enable_provider_ids = registry
            .catalog()
            .providers
            .iter()
            .filter(|provider| {
                can_identify_new_providers && !previously_known.contains(&provider.id)
            })
            .map(|provider| provider.id.clone())
            .collect();

        let migrating_templates = settings.schema_version < 9;
        normalize(&registry, &mut settings, &detected);
        let template_updates = sync_connection_metric_templates(
            &registry,
            &storage,
            &mut settings,
            migrating_templates,
        )?;
        storage.save_settings_with_updates(&settings, &[], &template_updates)?;
        let provider_ids = registry
            .catalog()
            .providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect();
        let service = Self {
            storage,
            registry,
            settings: RwLock::new(settings),
            command_mutation: tokio::sync::Mutex::new(()),
            credential_mutation: tokio::sync::Mutex::new(()),
            enablement_revision: AtomicU64::new(0),
            credential_revision: AtomicU64::new(0),
            settings_revision: AtomicU64::new(0),
            account_revision: AtomicU64::new(0),
            active_account_identities: RwLock::new(HashMap::new()),
        };
        crate::http_client::set_provider_proxies(&service.get().provider_proxies);
        service.activate_launch_accounts()?;
        let plan = CredentialDetectionPlan {
            provider_ids,
            auto_enable_provider_ids,
            replace_fallback: fresh_install,
            enablement_revision: 0,
            credential_revision: 0,
        };
        Ok((service, plan))
    }

    pub fn get(&self) -> AppSettings {
        self.settings
            .read()
            .map(|settings| settings.clone())
            .unwrap_or_default()
    }

    pub(crate) async fn lock_command_mutation(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.command_mutation.lock().await
    }

    pub(crate) async fn lock_credential_mutation(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.credential_mutation.lock().await
    }

    /// Settings with the revisions they were read at, as one consistent snapshot.
    pub(crate) fn get_with_revisions(&self) -> (AppSettings, u64, u64) {
        self.settings
            .read()
            .map(|settings| {
                (
                    settings.clone(),
                    self.settings_revision.load(Ordering::SeqCst),
                    self.account_revision.load(Ordering::SeqCst),
                )
            })
            .unwrap_or_else(|_| {
                (
                    AppSettings::default(),
                    self.settings_revision(),
                    self.account_revision(),
                )
            })
    }

    fn activate_launch_accounts(&self) -> Result<(), StorageError> {
        for (provider_id, identity) in self.registry.launch_accounts() {
            self.activate_account(&provider_id, &provider_id, &identity)?;
        }
        Ok(())
    }

    pub fn activate_account(
        &self,
        family: &str,
        provider_id: &str,
        identity: &str,
    ) -> Result<bool, StorageError> {
        let mut settings = self.settings.write().map_err(|_| StorageError::Poisoned)?;
        let mut active_accounts = self
            .active_account_identities
            .write()
            .map_err(|_| StorageError::Poisoned)?;
        let previous_identity = active_accounts.get(provider_id).cloned();
        if previous_identity.as_deref() == Some(identity) {
            return Ok(false);
        }

        let records = self.storage.load_provider_account_records(family)?;
        let (record_id, mut payload) = account_record(&records, family, identity);
        let current_name = settings.provider_names.get(provider_id).map(String::as_str);
        let mut account_updates = Vec::new();

        // Before account-aware names existed, the visible card held the only saved name. Preserve
        // that name on the original bare-id account before projecting a different active account.
        if previous_identity.is_none() {
            if let Some((legacy_identity, _, legacy_payload)) = records
                .iter()
                .find(|(_, record_provider_id, _)| record_provider_id == provider_id)
            {
                let mut legacy_payload = serde_json::from_str(legacy_payload)
                    .unwrap_or_else(|_| Value::Object(Map::new()));
                if legacy_identity != identity
                    && account_custom_name(&legacy_payload).is_none()
                    && current_name.is_some()
                {
                    set_account_custom_name(&mut legacy_payload, current_name);
                    account_updates.push(ProviderAccountUpdate {
                        provider_family: family.to_owned(),
                        identity_key: legacy_identity.clone(),
                        provider_id: provider_id.to_owned(),
                        payload: serde_json::to_string(&legacy_payload)?,
                    });
                }
            }
        }
        if account_custom_name(&payload).is_none()
            && record_id == provider_id
            && previous_identity.is_none()
        {
            set_account_custom_name(&mut payload, current_name);
        }

        let mut next = settings.clone();
        match account_custom_name(&payload) {
            Some(name) => {
                next.provider_names.insert(provider_id.to_owned(), name);
            }
            None => {
                next.provider_names.remove(provider_id);
            }
        }

        account_updates.push(ProviderAccountUpdate {
            provider_family: family.to_owned(),
            identity_key: identity.to_owned(),
            provider_id: record_id,
            payload: serde_json::to_string(&payload)?,
        });
        self.storage
            .save_settings_with_account_updates(&next, &account_updates)?;
        settings.clone_from(&next);
        active_accounts.insert(provider_id.to_owned(), identity.to_owned());
        self.settings_revision.fetch_add(1, Ordering::SeqCst);
        self.account_revision.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    pub fn settings_revision(&self) -> u64 {
        self.settings_revision.load(Ordering::SeqCst)
    }

    pub fn account_revision(&self) -> u64 {
        self.account_revision.load(Ordering::SeqCst)
    }

    fn active_account_name_updates(
        &self,
        settings: &AppSettings,
    ) -> Result<Vec<ProviderAccountUpdate>, StorageError> {
        let active = self
            .active_account_identities
            .read()
            .map_err(|_| StorageError::Poisoned)?
            .clone();
        let mut updates = Vec::new();
        for (provider_id, identity) in active {
            let family = crate::providers::provider_family(&provider_id);
            let records = self.storage.load_provider_account_records(family)?;
            let (record_id, mut payload) = account_record(&records, family, &identity);
            set_account_custom_name(
                &mut payload,
                settings
                    .provider_names
                    .get(&provider_id)
                    .map(String::as_str),
            );
            updates.push(ProviderAccountUpdate {
                provider_family: family.to_owned(),
                identity_key: identity,
                provider_id: record_id,
                payload: serde_json::to_string(&payload)?,
            });
        }
        Ok(updates)
    }

    #[cfg(test)]
    pub fn update(&self, mut settings: AppSettings) -> Result<AppSettings, String> {
        self.update_internal(&mut settings, None, None, false, &[])
    }

    pub fn update_from_view(
        &self,
        mut settings: AppSettings,
        expected_settings_revision: u64,
        expected_account_revision: u64,
    ) -> Result<AppSettings, String> {
        self.update_internal(
            &mut settings,
            Some(expected_settings_revision),
            Some(expected_account_revision),
            false,
            &[],
        )
    }

    /// Saves layouts reset to [`Self::default_settings`], recording each track template they
    /// applied so later track changes still recognize the untouched metrics.
    pub fn reset_layouts_from_view(
        &self,
        mut settings: AppSettings,
        expected_settings_revision: u64,
        expected_account_revision: u64,
    ) -> Result<AppSettings, String> {
        let template_defaults = self.reset_template_defaults(&settings);
        self.update_internal(
            &mut settings,
            Some(expected_settings_revision),
            Some(expected_account_revision),
            false,
            &template_defaults,
        )
    }

    pub fn reset_all_from_view(
        &self,
        mut settings: AppSettings,
        expected_settings_revision: u64,
        expected_account_revision: u64,
    ) -> Result<AppSettings, String> {
        let template_defaults = self.reset_template_defaults(&settings);
        self.update_internal(
            &mut settings,
            Some(expected_settings_revision),
            Some(expected_account_revision),
            true,
            &template_defaults,
        )
    }

    fn reset_template_defaults(&self, settings: &AppSettings) -> Vec<(String, Vec<MetricLayout>)> {
        settings
            .providers
            .iter()
            .filter(|provider| provider.detected)
            .filter_map(|provider| {
                track_template_defaults(&self.registry, &provider.id)
                    .map(|defaults| (provider.id.clone(), defaults))
            })
            .collect()
    }

    fn update_internal(
        &self,
        settings: &mut AppSettings,
        expected_settings_revision: Option<u64>,
        expected_account_revision: Option<u64>,
        reset_all_account_names: bool,
        template_defaults: &[(String, Vec<MetricLayout>)],
    ) -> Result<AppSettings, String> {
        let mut current = self
            .settings
            .write()
            .map_err(|_| "OpenQuota settings are temporarily unavailable.".to_owned())?;
        if expected_settings_revision
            .is_some_and(|revision| revision != self.settings_revision.load(Ordering::SeqCst))
        {
            return Err(
                "Settings changed before they could be saved. Please try again.".to_owned(),
            );
        }
        settings.provider_proxies.retain(|id, url| {
            *url = url.trim().to_owned();
            self.registry.definition(id).is_some() && !url.is_empty()
        });
        for url in settings.provider_proxies.values() {
            crate::http_client::validate_provider_proxy(url)?;
        }
        let enabled_before = enabled_provider_set(&current);
        let detected = current
            .providers
            .iter()
            .filter(|provider| provider.detected)
            .map(|provider| provider.id.clone())
            .collect::<HashSet<_>>();
        normalize(&self.registry, settings, &detected);
        if expected_account_revision != Some(self.account_revision.load(Ordering::SeqCst)) {
            let active_provider_ids = self
                .active_account_identities
                .read()
                .map_err(|_| "OpenQuota account names are temporarily unavailable.".to_owned())?
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            for provider_id in active_provider_ids {
                if let Some(name) = current.provider_names.get(&provider_id) {
                    settings.provider_names.insert(provider_id, name.clone());
                } else {
                    settings.provider_names.remove(&provider_id);
                }
            }
        }
        let account_updates = if reset_all_account_names {
            self.account_name_reset_updates()
        } else {
            self.active_account_name_updates(settings)
        }
        .map_err(|_| "OpenQuota account names could not be saved.".to_owned())?;
        self.storage
            .save_settings_with_updates(settings, &account_updates, template_defaults)
            .map_err(|_| "OpenQuota settings could not be saved.".to_owned())?;
        let enablement_changed = enabled_provider_set(settings) != enabled_before;
        crate::http_client::set_provider_proxies(&settings.provider_proxies);
        current.clone_from(settings);
        if enablement_changed {
            self.enablement_revision.fetch_add(1, Ordering::SeqCst);
        }
        self.settings_revision.fetch_add(1, Ordering::SeqCst);
        Ok(settings.clone())
    }

    fn account_name_reset_updates(&self) -> Result<Vec<ProviderAccountUpdate>, StorageError> {
        self.storage
            .load_all_provider_account_records()?
            .into_iter()
            .map(|(provider_family, identity_key, provider_id, payload)| {
                let mut payload = serde_json::from_str::<Value>(&payload)?;
                if let Some(object) = payload.as_object_mut() {
                    object.remove("customName");
                }
                Ok(ProviderAccountUpdate {
                    provider_family,
                    identity_key,
                    provider_id,
                    payload: serde_json::to_string(&payload)?,
                })
            })
            .collect()
    }

    pub fn reset_detection_plan(&self) -> CredentialDetectionPlan {
        CredentialDetectionPlan {
            provider_ids: self
                .registry
                .catalog()
                .providers
                .iter()
                .map(|provider| provider.id.clone())
                .collect(),
            auto_enable_provider_ids: HashSet::new(),
            replace_fallback: true,
            enablement_revision: self.enablement_revision.load(Ordering::SeqCst),
            credential_revision: self.credential_revision.load(Ordering::SeqCst),
        }
    }

    /// Applies a completed local credential probe without overriding settings changed while it ran.
    pub fn apply_credential_detection(
        &self,
        plan: &CredentialDetectionPlan,
        probe_results: &CredentialProbeResults,
    ) -> Result<CredentialDetectionOutcome, String> {
        let mut current = self
            .settings
            .write()
            .map_err(|_| "OpenQuota settings are temporarily unavailable.".to_owned())?;
        let enabled_before = enabled_provider_set(&current);
        let detected_before = detected_provider_set(&current);
        let credential_revision_matches =
            self.credential_revision.load(Ordering::SeqCst) == plan.credential_revision;
        let mut next = current.clone();
        let mut detected = detected_before.clone();
        if credential_revision_matches {
            for (provider_id, status) in probe_results {
                match status {
                    CredentialProbeStatus::Detected => {
                        detected.insert(provider_id.clone());
                    }
                    CredentialProbeStatus::Absent => {
                        detected.remove(provider_id);
                    }
                    CredentialProbeStatus::Unknown => {}
                }
            }
        }
        normalize(&self.registry, &mut next, &detected);

        if plan.replace_fallback {
            if credential_revision_matches
                && self.enablement_revision.load(Ordering::SeqCst) == plan.enablement_revision
            {
                let any_detected = !detected.is_empty();
                for provider in &mut next.providers {
                    match probe_results.get(&provider.id) {
                        Some(CredentialProbeStatus::Detected) => provider.enabled = true,
                        Some(CredentialProbeStatus::Absent) => {
                            provider.enabled = !any_detected
                                && self
                                    .registry
                                    .definition(&provider.id)
                                    .is_some_and(|definition| definition.fallback_enabled);
                        }
                        Some(CredentialProbeStatus::Unknown) | None => {}
                    }
                }
            }
        } else if credential_revision_matches {
            for provider in &mut next.providers {
                if plan.auto_enable_provider_ids.contains(&provider.id)
                    && probe_results.get(&provider.id) == Some(&CredentialProbeStatus::Detected)
                {
                    provider.enabled = true;
                }
            }
        }

        if next == *current {
            return Ok(CredentialDetectionOutcome {
                settings: next,
                newly_enabled_provider_ids: Vec::new(),
            });
        }

        self.storage
            .save_settings(&next)
            .map_err(|_| "OpenQuota settings could not be saved.".to_owned())?;
        let newly_enabled_provider_ids = next
            .providers
            .iter()
            .filter(|provider| provider.enabled && !enabled_before.contains(&provider.id))
            .map(|provider| provider.id.clone())
            .collect();
        let enablement_changed = enabled_provider_set(&next) != enabled_before;
        current.clone_from(&next);
        if enablement_changed {
            self.enablement_revision.fetch_add(1, Ordering::SeqCst);
        }
        if detected_provider_set(&next) != detected_before {
            self.credential_revision.fetch_add(1, Ordering::SeqCst);
        }
        self.settings_revision.fetch_add(1, Ordering::SeqCst);
        Ok(CredentialDetectionOutcome {
            settings: next,
            newly_enabled_provider_ids,
        })
    }

    pub fn enabled_provider_ids(&self) -> Vec<String> {
        self.get()
            .providers
            .into_iter()
            .filter(|provider| provider.enabled && self.registry.definition(&provider.id).is_some())
            .map(|provider| provider.id)
            .collect()
    }

    pub fn reset_provider(
        &self,
        provider_id: &str,
        expected_settings_revision: u64,
        expected_account_revision: u64,
    ) -> Result<AppSettings, String> {
        let mut settings = self.get();
        let definition = self
            .registry
            .definition(provider_id)
            .ok_or_else(|| "Unknown provider.".to_owned())?;
        let provider = settings
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| "Provider settings are unavailable.".to_owned())?;
        provider.expanded = false;
        provider.metrics =
            reset_provider_layout(&self.registry, definition, provider.detected).metrics;
        let template_defaults = provider
            .detected
            .then(|| track_template_defaults(&self.registry, provider_id))
            .flatten()
            .map(|defaults| (provider_id.to_owned(), defaults))
            .into_iter()
            .collect::<Vec<_>>();
        self.update_internal(
            &mut settings,
            Some(expected_settings_revision),
            Some(expected_account_revision),
            false,
            &template_defaults,
        )
    }

    pub fn reset_defaults(&self) -> AppSettings {
        self.default_settings(&detected_provider_set(&self.get()))
    }

    pub fn record_provider_credential_mutation(&self) {
        self.credential_revision.fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub fn reconcile_provider_credential_state(
        &self,
        provider_id: &str,
        detected: bool,
        enable: bool,
    ) -> Result<AppSettings, String> {
        self.reconcile_provider_credential_state_with_metrics(provider_id, detected, enable, None)
    }

    pub fn reconcile_provider_credential_state_with_metrics(
        &self,
        provider_id: &str,
        detected: bool,
        enable: bool,
        metric_template: Option<MetricTemplateChange>,
    ) -> Result<AppSettings, String> {
        let mut current = self
            .settings
            .write()
            .map_err(|_| "OpenQuota settings are temporarily unavailable.".to_owned())?;
        let enabled_before = enabled_provider_set(&current);
        let mut next = current.clone();
        let provider = next
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| "Provider settings are unavailable.".to_owned())?;
        let preserve_layout = provider.detected;
        provider.detected = detected;
        if enable {
            provider.enabled = true;
        }
        let saved = if let Some(change) = metric_template {
            let applied = if preserve_layout {
                self.storage
                    .load_metric_template_defaults(provider_id)
                    .map_err(|_| "OpenQuota settings are temporarily unavailable.".to_owned())?
                    .or(change.previous)
                    .unwrap_or_default()
            } else {
                provider.metrics.clear();
                Vec::new()
            };
            let policy =
                MetricPolicy::for_track(&self.registry, provider_id, change.upstream_provider_id);
            let defaults =
                apply_metric_template(&mut provider.metrics, &change.template, &applied, &policy);
            self.storage
                .save_settings_with_metric_template_defaults(&next, provider_id, &defaults)
        } else {
            self.storage.save_settings(&next)
        };
        saved.map_err(|_| "OpenQuota settings could not be saved.".to_owned())?;
        current.clone_from(&next);
        if enabled_provider_set(&next) != enabled_before {
            self.enablement_revision.fetch_add(1, Ordering::SeqCst);
        }
        self.settings_revision.fetch_add(1, Ordering::SeqCst);
        Ok(next)
    }

    pub fn remove_dynamic_provider(&self, provider_id: &str) -> Result<AppSettings, String> {
        let mut current = self
            .settings
            .write()
            .map_err(|_| "OpenQuota settings are temporarily unavailable.".to_owned())?;
        let enabled_before = enabled_provider_set(&current);
        let mut next = current.clone();
        let provider = next
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| "Provider settings are unavailable.".to_owned())?;
        provider.enabled = false;
        provider.detected = false;
        provider.expanded = false;
        next.provider_proxies.remove(provider_id);
        self.storage
            .save_settings(&next)
            .map_err(|_| "OpenQuota settings could not be saved.".to_owned())?;
        crate::http_client::set_provider_proxies(&next.provider_proxies);
        current.clone_from(&next);
        if enabled_provider_set(&next) != enabled_before {
            self.enablement_revision.fetch_add(1, Ordering::SeqCst);
        }
        self.settings_revision.fetch_add(1, Ordering::SeqCst);
        Ok(next)
    }

    pub fn default_settings(&self, detected: &HashSet<String>) -> AppSettings {
        default_settings(&self.registry, detected)
    }

    pub fn catalog(&self) -> &ProviderCatalog {
        self.registry.catalog()
    }

    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    pub fn view_state(
        &self,
        notification_permission: impl Into<String>,
        integration_error: Option<String>,
        tray_available: bool,
        platform_summary: Option<String>,
    ) -> SettingsViewState {
        let (settings, settings_revision, account_revision) = self.get_with_revisions();
        let mut renamable_provider_ids = self.registry.observed_account_provider_ids();
        if let Ok(stored_ids) = self.storage.load_observed_account_provider_ids() {
            for provider_id in stored_ids {
                if self.registry.supports_account_names(&provider_id)
                    && !renamable_provider_ids.contains(&provider_id)
                {
                    renamable_provider_ids.push(provider_id);
                }
            }
        }
        SettingsViewState {
            settings,
            settings_revision,
            account_revision,
            renamable_provider_ids,
            metric_policies: crate::metric_policy::views(&self.registry),
            notification_permission: notification_permission.into(),
            integration_error,
            tray_available,
            platform_summary,
        }
    }
}

/// Applies updated track metric templates to existing detected connections at launch without
/// disturbing user customizations or touching credential stores.
fn sync_connection_metric_templates(
    registry: &ProviderRegistry,
    storage: &Storage,
    settings: &mut AppSettings,
    migrating_legacy_templates: bool,
) -> Result<Vec<(String, Vec<MetricLayout>)>, StorageError> {
    let mut updates = Vec::new();
    for provider in &mut settings.providers {
        if !provider.detected {
            continue;
        }
        let policy = MetricPolicy::for_provider(registry, &provider.id);
        let Some(track) = policy.track() else {
            continue;
        };
        let Some(template) =
            crate::providers::sub2api::metric_template_for_track(&provider.id, track)
        else {
            continue;
        };
        if let Some(applied) = storage.load_metric_template_defaults(&provider.id)? {
            if applied != template {
                let recorded =
                    apply_metric_template(&mut provider.metrics, &template, &applied, &policy);
                updates.push((provider.id.clone(), recorded));
            }
        } else if migrating_legacy_templates {
            let Some(definition) = registry.definition(&provider.id) else {
                continue;
            };
            let Some(reset_definition) = definition.metrics.iter().find(|metric| {
                matches!(
                    &metric.source,
                    crate::models::MetricSource::Value { source_id, .. }
                        if source_id == "sub2apiRateLimitResets"
                )
            }) else {
                continue;
            };
            let template_enables_reset = template
                .iter()
                .any(|metric| metric.id == reset_definition.id && metric.enabled);
            if template_enables_reset {
                if let Some(metric) = provider
                    .metrics
                    .iter_mut()
                    .find(|metric| metric.id == reset_definition.id)
                {
                    if !metric.enabled
                        && metric.section == crate::models::MetricSection::OnDemand
                        && !metric.pinned
                    {
                        metric.enabled = true;
                    }
                }
            }
        }
    }
    Ok(updates)
}

#[cfg(test)]
mod tests;
