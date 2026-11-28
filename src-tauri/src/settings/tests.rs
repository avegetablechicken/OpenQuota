use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
};

use serde_json::Value;
use tempfile::tempdir;

use crate::{
    models::{
        AppSettings, MetricLayout, MetricSection, ProviderDefinition, ProviderSnapshot,
        ThemePreference,
    },
    providers::{
        antigravity, claude, codex, cursor, openrouter,
        sub2api::{
            metric_template, track_provider_id, Sub2ApiMode, Sub2ApiProviders, Sub2ApiUpstream,
        },
        CredentialProbeResults, CredentialProbeStatus, ProviderError, ProviderRegistry,
        UsageProvider,
    },
    storage::Storage,
};

use super::{default_settings, normalize, MetricTemplateChange, SettingsService};

struct CatalogProvider(ProviderDefinition);

impl UsageProvider for CatalogProvider {
    fn definition(&self) -> ProviderDefinition {
        self.0.clone()
    }

    fn supports_account_names(&self) -> bool {
        matches!(
            crate::providers::provider_family(&self.0.id),
            "claude" | "codex"
        )
    }

    fn has_local_credentials(&self) -> bool {
        false
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        unreachable!()
    }
}

fn catalog() -> Arc<ProviderRegistry> {
    let providers = [
        claude::definition(),
        codex::definition(),
        cursor::definition(),
        antigravity::definition(),
        openrouter::definition(),
    ]
    .into_iter()
    .map(|definition| Arc::new(CatalogProvider(definition)) as Arc<dyn UsageProvider>)
    .collect();
    Arc::new(ProviderRegistry::new(providers).unwrap())
}

fn claude_account_definition() -> ProviderDefinition {
    let mut definition = claude::definition();
    definition.id = "claude@1234abcd".into();
    definition.display_name = "Claude — Work".into();
    definition.fallback_enabled = false;
    for metric in &mut definition.metrics {
        metric.id = metric.id.replacen("claude.", "claude@1234abcd.", 1);
    }
    definition
}

fn catalog_with_claude_account() -> Arc<ProviderRegistry> {
    let providers = [
        claude::definition(),
        claude_account_definition(),
        codex::definition(),
        cursor::definition(),
        antigravity::definition(),
        openrouter::definition(),
    ]
    .into_iter()
    .map(|definition| Arc::new(CatalogProvider(definition)) as Arc<dyn UsageProvider>)
    .collect();
    Arc::new(ProviderRegistry::new(providers).unwrap())
}

fn enabled_ids(settings: &crate::models::AppSettings) -> Vec<&str> {
    settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| provider.id.as_str())
        .collect()
}

fn probe_results(detected: &[&str]) -> CredentialProbeResults {
    ["claude", "codex", "cursor", "antigravity", "openrouter"]
        .into_iter()
        .map(|provider_id| {
            let status = if detected.contains(&provider_id) {
                CredentialProbeStatus::Detected
            } else {
                CredentialProbeStatus::Absent
            };
            (provider_id.to_owned(), status)
        })
        .collect()
}

#[test]
fn empty_detection_uses_the_established_fallback_set() {
    let registry = catalog();
    let settings = default_settings(&registry, &HashSet::new());

    assert_eq!(enabled_ids(&settings), ["claude", "codex", "cursor"]);
}

#[test]
fn deferred_first_run_replaces_fallback_with_detected_providers() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let (service, plan) = SettingsService::new_deferred(storage, catalog()).unwrap();
    assert_eq!(enabled_ids(&service.get()), ["claude", "codex", "cursor"]);

    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&["antigravity"]))
        .unwrap();

    assert_eq!(enabled_ids(&outcome.settings), ["antigravity"]);
    assert_eq!(outcome.newly_enabled_provider_ids, ["antigravity"]);
    assert!(
        outcome
            .settings
            .providers
            .iter()
            .find(|provider| provider.id == "antigravity")
            .unwrap()
            .detected
    );
}

#[test]
fn deferred_first_run_keeps_fallback_when_nothing_is_detected() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let (service, plan) = SettingsService::new_deferred(storage, catalog()).unwrap();

    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&[]))
        .unwrap();

    assert_eq!(
        enabled_ids(&outcome.settings),
        ["claude", "codex", "cursor"]
    );
    assert!(outcome.newly_enabled_provider_ids.is_empty());
}

#[test]
fn unknown_first_run_probes_preserve_the_fallback_enablement() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let (service, plan) = SettingsService::new_deferred(storage, catalog()).unwrap();
    let unknown = ["claude", "codex", "cursor", "antigravity", "openrouter"]
        .into_iter()
        .map(|provider_id| (provider_id.to_owned(), CredentialProbeStatus::Unknown))
        .collect();

    let outcome = service.apply_credential_detection(&plan, &unknown).unwrap();

    assert_eq!(
        enabled_ids(&outcome.settings),
        ["claude", "codex", "cursor"]
    );
    assert!(outcome
        .settings
        .providers
        .iter()
        .all(|provider| !provider.detected));
}

#[test]
fn unknown_reset_probe_preserves_existing_detection_and_enablement() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service =
        SettingsService::new_for_test(storage, catalog(), &HashSet::from(["codex".to_owned()]))
            .unwrap();
    let plan = service.reset_detection_plan();
    let mut results = probe_results(&["antigravity"]);
    results.insert("codex".to_owned(), CredentialProbeStatus::Unknown);

    let outcome = service.apply_credential_detection(&plan, &results).unwrap();
    let codex = outcome
        .settings
        .providers
        .iter()
        .find(|provider| provider.id == "codex")
        .unwrap();
    let antigravity = outcome
        .settings
        .providers
        .iter()
        .find(|provider| provider.id == "antigravity")
        .unwrap();

    assert!(codex.detected);
    assert!(codex.enabled);
    assert!(antigravity.detected);
    assert!(antigravity.enabled);
}

#[test]
fn definitive_reset_absence_restores_the_fallback_set() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(
        storage,
        catalog(),
        &HashSet::from(["antigravity".to_owned()]),
    )
    .unwrap();
    let plan = service.reset_detection_plan();

    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&[]))
        .unwrap();

    assert_eq!(
        enabled_ids(&outcome.settings),
        ["claude", "codex", "cursor"]
    );
    assert!(outcome
        .settings
        .providers
        .iter()
        .all(|provider| !provider.detected));
}

#[test]
fn unknown_existing_detection_does_not_enable_the_fallback_set() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service =
        SettingsService::new_for_test(storage, catalog(), &HashSet::from(["codex".to_owned()]))
            .unwrap();
    let plan = service.reset_detection_plan();
    let mut results = probe_results(&[]);
    results.insert("codex".to_owned(), CredentialProbeStatus::Unknown);

    let outcome = service.apply_credential_detection(&plan, &results).unwrap();

    assert_eq!(enabled_ids(&outcome.settings), ["codex"]);
    assert!(outcome
        .settings
        .providers
        .iter()
        .find(|provider| provider.id == "codex")
        .is_some_and(|provider| provider.detected));
}

#[test]
fn user_enablement_change_wins_over_a_running_detection_pass() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let (service, plan) = SettingsService::new_deferred(storage, catalog()).unwrap();
    let mut changed = service.get();
    changed
        .providers
        .iter_mut()
        .find(|provider| provider.id == "claude")
        .unwrap()
        .enabled = false;
    service.update(changed).unwrap();

    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&["antigravity"]))
        .unwrap();

    assert_eq!(enabled_ids(&outcome.settings), ["codex", "cursor"]);
    assert!(
        outcome
            .settings
            .providers
            .iter()
            .find(|provider| provider.id == "antigravity")
            .unwrap()
            .detected
    );
}

#[test]
fn deferred_new_provider_is_auto_enabled_only_once() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let registry = catalog();
    let mut saved = default_settings(&registry, &HashSet::from(["codex".to_owned()]));
    saved.known_provider_ids.retain(|id| id != "antigravity");
    saved
        .providers
        .retain(|provider| provider.id != "antigravity");
    storage.save_settings(&saved).unwrap();

    let (first, plan) = SettingsService::new_deferred(storage.clone(), registry.clone()).unwrap();
    let detected = probe_results(&["codex", "antigravity"]);
    let outcome = first.apply_credential_detection(&plan, &detected).unwrap();
    assert!(
        outcome
            .settings
            .providers
            .iter()
            .find(|provider| provider.id == "antigravity")
            .unwrap()
            .enabled
    );
    let mut disabled = outcome.settings;
    disabled
        .providers
        .iter_mut()
        .find(|provider| provider.id == "antigravity")
        .unwrap()
        .enabled = false;
    first.update(disabled).unwrap();
    drop(first);

    let (second, second_plan) = SettingsService::new_deferred(storage, registry).unwrap();
    let outcome = second
        .apply_credential_detection(&second_plan, &detected)
        .unwrap();
    assert!(
        !outcome
            .settings
            .providers
            .iter()
            .find(|provider| provider.id == "antigravity")
            .unwrap()
            .enabled
    );
}

#[test]
fn account_name_and_rename_availability_survive_a_restart() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    storage
        .save_provider_account_record("claude", "identity-a", "claude", "{}")
        .unwrap();
    let registry = catalog();
    let (first, _) = SettingsService::new_deferred(storage.clone(), registry.clone()).unwrap();
    let mut settings = first.get();
    settings
        .provider_names
        .insert("claude".into(), "Personal".into());
    first
        .update_from_view(
            settings,
            first.settings_revision(),
            first.account_revision(),
        )
        .unwrap();
    drop(first);

    let (second, _) = SettingsService::new_deferred(storage, registry).unwrap();
    let state = second.view_state("prompt", None, false, None);

    assert_eq!(
        state
            .settings
            .provider_names
            .get("claude")
            .map(String::as_str),
        Some("Personal")
    );
    assert!(state.renamable_provider_ids.contains(&"claude".to_owned()));
}

#[test]
fn codex_names_follow_accounts_when_the_active_login_changes() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(
        storage.clone(),
        catalog(),
        &HashSet::from(["codex".to_owned()]),
    )
    .unwrap();

    service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap();
    let mut settings = service.get();
    settings.provider_names.insert("codex".into(), "GPT".into());
    service
        .update_from_view(
            settings,
            service.settings_revision(),
            service.account_revision(),
        )
        .unwrap();

    service
        .activate_account("codex", "codex", "bbbbbbbb22222222")
        .unwrap();
    assert!(!service.get().provider_names.contains_key("codex"));

    let mut settings = service.get();
    settings
        .provider_names
        .insert("codex".into(), "Work".into());
    service
        .update_from_view(
            settings,
            service.settings_revision(),
            service.account_revision(),
        )
        .unwrap();

    service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap();
    assert_eq!(
        service
            .get()
            .provider_names
            .get("codex")
            .map(String::as_str),
        Some("GPT")
    );
    service
        .activate_account("codex", "codex", "bbbbbbbb22222222")
        .unwrap();
    assert_eq!(
        service
            .get()
            .provider_names
            .get("codex")
            .map(String::as_str),
        Some("Work")
    );

    let records = storage.load_provider_account_records("codex").unwrap();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .any(|(identity, provider_id, _)| identity == "aaaaaaaa11111111"
                && provider_id == "codex")
    );
    assert!(records
        .iter()
        .any(|(identity, provider_id, _)| identity == "bbbbbbbb22222222"
            && provider_id == "codex@bbbbbbbb"));
}

#[test]
fn account_revision_only_changes_when_the_active_identity_changes() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service =
        SettingsService::new_for_test(storage, catalog(), &HashSet::from(["codex".to_owned()]))
            .unwrap();
    let initial_revision = service.account_revision();

    assert!(service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap());
    assert_eq!(service.account_revision(), initial_revision + 1);
    assert!(!service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap());
    assert_eq!(service.account_revision(), initial_revision + 1);
    assert!(service
        .activate_account("codex", "codex", "bbbbbbbb22222222")
        .unwrap());
    assert_eq!(service.account_revision(), initial_revision + 2);
}

#[test]
fn settings_revision_rejects_a_stale_full_snapshot() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(storage, catalog(), &HashSet::new()).unwrap();
    let initial_revision = service.settings_revision();
    let stale = service.get();
    let mut newer = stale.clone();
    newer.density = crate::models::DensityPreference::Compact;

    service
        .update_from_view(newer, initial_revision, service.account_revision())
        .unwrap();
    let revision_after_save = service.settings_revision();
    assert_eq!(revision_after_save, initial_revision + 1);

    let error = service
        .update_from_view(stale, initial_revision, service.account_revision())
        .unwrap_err();

    assert!(error.contains("Settings changed"));
    assert_eq!(service.settings_revision(), revision_after_save);
    assert_eq!(
        service.get().density,
        crate::models::DensityPreference::Compact
    );
}

#[test]
fn settings_revision_tracks_each_persisted_mutation_path() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(storage, catalog(), &HashSet::new()).unwrap();
    let mut revision = service.settings_revision();

    service.update(service.get()).unwrap();
    revision += 1;
    assert_eq!(service.settings_revision(), revision);

    service.record_provider_credential_mutation();
    service
        .reconcile_provider_credential_state("openrouter", true, true)
        .unwrap();
    revision += 1;
    assert_eq!(service.settings_revision(), revision);

    let plan = service.reset_detection_plan();
    service
        .apply_credential_detection(&plan, &probe_results(&[]))
        .unwrap();
    revision += 1;
    assert_eq!(service.settings_revision(), revision);

    assert!(service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap());
    revision += 1;
    assert_eq!(service.settings_revision(), revision);

    service
        .reset_provider(
            "codex",
            service.settings_revision(),
            service.account_revision(),
        )
        .unwrap();
    revision += 1;
    assert_eq!(service.settings_revision(), revision);
    assert_eq!(
        service
            .view_state("prompt", None, true, None)
            .settings_revision,
        revision
    );
}

#[test]
fn unchanged_credential_probe_does_not_create_a_settings_revision() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service =
        SettingsService::new_for_test(storage, catalog(), &HashSet::from(["codex".to_owned()]))
            .unwrap();
    let revision = service.settings_revision();
    let plan = service.reset_detection_plan();

    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&["codex"]))
        .unwrap();

    assert!(outcome.newly_enabled_provider_ids.is_empty());
    assert_eq!(service.settings_revision(), revision);
}

#[test]
fn stale_settings_save_cannot_move_a_name_to_the_new_active_account() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service =
        SettingsService::new_for_test(storage, catalog(), &HashSet::from(["codex".to_owned()]))
            .unwrap();
    service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap();
    let mut named = service.get();
    named.provider_names.insert("codex".into(), "GPT".into());
    service
        .update_from_view(
            named,
            service.settings_revision(),
            service.account_revision(),
        )
        .unwrap();

    let stale_account_revision = service.account_revision();
    let mut stale_settings = service.get();
    service
        .activate_account("codex", "codex", "bbbbbbbb22222222")
        .unwrap();
    stale_settings.theme = ThemePreference::Dark;
    stale_settings
        .provider_names
        .insert("claude".into(), "Personal".into());
    let updated = service
        .update_from_view(
            stale_settings,
            service.settings_revision(),
            stale_account_revision,
        )
        .unwrap();

    assert_eq!(updated.theme, ThemePreference::Dark);
    assert!(!updated.provider_names.contains_key("codex"));
    assert_eq!(
        updated.provider_names.get("claude").map(String::as_str),
        Some("Personal")
    );
    service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap();
    assert_eq!(
        service
            .get()
            .provider_names
            .get("codex")
            .map(String::as_str),
        Some("GPT")
    );
}

#[test]
fn codex_name_migration_keeps_the_original_accounts_name_after_an_offline_swap() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    storage
        .save_provider_account_record("codex", "aaaaaaaa11111111", "codex", "{}")
        .unwrap();
    let mut saved = default_settings(&catalog(), &HashSet::from(["codex".to_owned()]));
    saved.provider_names.insert("codex".into(), "GPT".into());
    storage.save_settings(&saved).unwrap();
    let service = SettingsService::new_for_test(
        storage.clone(),
        catalog(),
        &HashSet::from(["codex".to_owned()]),
    )
    .unwrap();

    service
        .activate_account("codex", "codex", "bbbbbbbb22222222")
        .unwrap();
    assert!(!service.get().provider_names.contains_key("codex"));
    service
        .activate_account("codex", "codex", "aaaaaaaa11111111")
        .unwrap();
    assert_eq!(
        service
            .get()
            .provider_names
            .get("codex")
            .map(String::as_str),
        Some("GPT")
    );
}

#[test]
fn unrelated_toggle_does_not_cancel_new_provider_detection() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let registry = catalog();
    let mut saved = default_settings(&registry, &HashSet::from(["claude".to_owned()]));
    saved.known_provider_ids.retain(|id| id != "antigravity");
    saved
        .providers
        .retain(|provider| provider.id != "antigravity");
    storage.save_settings(&saved).unwrap();
    let (service, plan) = SettingsService::new_deferred(storage, registry).unwrap();
    let mut changed = service.get();
    changed
        .providers
        .iter_mut()
        .find(|provider| provider.id == "claude")
        .unwrap()
        .enabled = false;
    service.update(changed).unwrap();

    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&["antigravity"]))
        .unwrap();

    assert!(
        outcome
            .settings
            .providers
            .iter()
            .find(|provider| provider.id == "antigravity")
            .unwrap()
            .enabled
    );
    assert!(
        !outcome
            .settings
            .providers
            .iter()
            .find(|provider| provider.id == "claude")
            .unwrap()
            .enabled
    );
}

#[test]
fn normalization_preserves_order_and_enforces_pin_cap_per_provider() {
    let detected = HashSet::from(["codex".to_owned(), "claude".to_owned()]);
    let catalog = catalog();
    let mut settings = default_settings(&catalog, &detected);
    let metrics = &mut settings.providers[0].metrics;
    metrics.rotate_left(2);
    for metric in metrics.iter_mut() {
        metric.enabled = true;
        metric.pinned = true;
    }
    normalize(&catalog, &mut settings, &detected);
    let metrics = &settings.providers[0].metrics;
    assert_eq!(
        metrics.iter().filter(|metric| metric.pinned).count(),
        crate::metric_policy::MAX_STARS
    );
    assert!(metrics
        .iter()
        .find(|metric| metric.id.ends_with(".trend"))
        .is_none_or(|metric| !metric.pinned));
}

#[test]
fn normalization_preserves_valid_pins_for_dashboard_hidden_metrics() {
    let detected = HashSet::from(["codex".to_owned()]);
    let catalog = catalog();
    let mut settings = default_settings(&catalog, &detected);
    let codex = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "codex")
        .unwrap();
    let pinned = codex
        .metrics
        .iter_mut()
        .find(|metric| metric.pinned)
        .unwrap();
    pinned.enabled = false;
    let pinned_id = pinned.id.clone();

    normalize(&catalog, &mut settings, &detected);

    let pinned = settings
        .providers
        .iter()
        .find(|provider| provider.id == "codex")
        .unwrap()
        .metrics
        .iter()
        .find(|metric| metric.id == pinned_id)
        .unwrap();
    assert!(!pinned.enabled);
    assert!(pinned.pinned);
}

#[test]
fn normalization_keeps_one_always_visible_metric() {
    let detected = HashSet::from(["codex".to_owned()]);
    let catalog = catalog();
    let mut settings = default_settings(&catalog, &detected);
    for metric in &mut settings.providers[1].metrics {
        metric.section = MetricSection::OnDemand;
    }
    normalize(&catalog, &mut settings, &detected);
    assert!(settings.providers[1]
        .metrics
        .iter()
        .any(|metric| metric.enabled && metric.section == MetricSection::AlwaysVisible));
}

#[test]
fn normalization_adds_new_codex_metrics_without_disturbing_existing_order() {
    let detected = HashSet::from(["codex".to_owned()]);
    let catalog = catalog();
    let mut settings = default_settings(&catalog, &detected);
    let codex = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "codex")
        .unwrap();
    codex.metrics.retain(|metric| {
        !matches!(
            metric.id.as_str(),
            "codex.spark" | "codex.sparkWeekly" | "codex.credits" | "codex.rateLimitResets"
        )
    });

    normalize(&catalog, &mut settings, &detected);

    let codex = settings
        .providers
        .iter()
        .find(|provider| provider.id == "codex")
        .unwrap();
    assert_eq!(
        &codex.metrics[..2]
            .iter()
            .map(|metric| metric.id.as_str())
            .collect::<Vec<_>>(),
        &["codex.session", "codex.weekly"]
    );
    for id in [
        "codex.spark",
        "codex.sparkWeekly",
        "codex.credits",
        "codex.rateLimitResets",
    ] {
        let metric = codex.metrics.iter().find(|metric| metric.id == id).unwrap();
        assert!(metric.enabled);
        assert_eq!(metric.section, MetricSection::OnDemand);
        assert!(!metric.pinned);
    }
    let position = |id: &str| {
        codex
            .metrics
            .iter()
            .position(|metric| metric.id == id)
            .unwrap()
    };
    assert!(position("codex.spark") < position("codex.sparkWeekly"));
    assert!(position("codex.sparkWeekly") < position("codex.credits"));
    assert!(position("codex.credits") < position("codex.rateLimitResets"));
    assert!(position("codex.rateLimitResets") < position("codex.today"));
}

#[test]
fn normalization_inserts_claude_reset_credits_before_usage_periods_and_migrates_trailing_position()
{
    let detected = HashSet::from(["claude".to_owned()]);
    let catalog = catalog();
    let mut settings = default_settings(&catalog, &detected);
    let claude = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "claude")
        .unwrap();
    claude
        .metrics
        .retain(|metric| metric.id != "claude.rateLimitResets");

    normalize(&catalog, &mut settings, &detected);

    let claude = settings
        .providers
        .iter()
        .find(|provider| provider.id == "claude")
        .unwrap();
    let position = |id: &str| {
        claude
            .metrics
            .iter()
            .position(|metric| metric.id == id)
            .unwrap()
    };
    assert!(position("claude.rateLimitResets") < position("claude.today"));

    // Simulate an older layout where both claude.fable and claude.rateLimitResets were appended
    // after claude.last30 (which previously blocked canonical-order migration).
    let mut legacy = settings.clone();
    legacy.schema_version = 8;
    let legacy_claude = legacy
        .providers
        .iter_mut()
        .find(|provider| provider.id == "claude")
        .unwrap();
    for trailing_id in ["claude.fable", "claude.rateLimitResets"] {
        let index = legacy_claude
            .metrics
            .iter()
            .position(|metric| metric.id == trailing_id)
            .unwrap();
        let metric = legacy_claude.metrics.remove(index);
        legacy_claude.metrics.push(metric);
    }

    normalize(&catalog, &mut legacy, &detected);

    let migrated_claude = legacy
        .providers
        .iter()
        .find(|provider| provider.id == "claude")
        .unwrap();
    let migrated_pos = |id: &str| {
        migrated_claude
            .metrics
            .iter()
            .position(|metric| metric.id == id)
            .unwrap()
    };
    assert!(migrated_pos("claude.rateLimitResets") < migrated_pos("claude.today"));
}

#[test]
fn startup_upgrades_untouched_sub2api_claude_v1_template_to_enable_and_position_resets() {
    let claude_admin = (Sub2ApiMode::Admin, Sub2ApiUpstream::Claude);
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let providers = Sub2ApiProviders::new(directory.path().join("sub2api"), storage.clone());
    let mut definition = providers.provider("sub2api").unwrap().definition();
    definition.fallback_enabled = true;
    let connection = Arc::new(TrackedConnection(
        definition,
        std::sync::Mutex::new(Some(track_provider_id(claude_admin.0, claude_admin.1))),
    ));
    let registry = Arc::new(ProviderRegistry::new(vec![connection]).unwrap());

    // Build a v1 Claude Sub2API template where rateLimitResets was hidden at the end after last30.
    let v1_order = [
        ("session", true, MetricSection::AlwaysVisible, true),
        ("weekly", true, MetricSection::AlwaysVisible, true),
        ("trend", true, MetricSection::AlwaysVisible, false),
        ("sonnet", false, MetricSection::OnDemand, false),
        ("fable", false, MetricSection::OnDemand, false),
        ("extra", false, MetricSection::OnDemand, false),
        ("today", true, MetricSection::OnDemand, false),
        ("yesterday", true, MetricSection::OnDemand, false),
        ("last30", true, MetricSection::OnDemand, false),
        ("daily", false, MetricSection::OnDemand, false),
        ("monthly", false, MetricSection::OnDemand, false),
        ("spark", false, MetricSection::OnDemand, false),
        ("sparkWeekly", false, MetricSection::OnDemand, false),
        ("rateLimitResets", false, MetricSection::OnDemand, false),
        ("claude", false, MetricSection::OnDemand, false),
        ("claudeWeekly", false, MetricSection::OnDemand, false),
    ];
    let v1_metrics: Vec<MetricLayout> = v1_order
        .iter()
        .map(|(suffix, enabled, section, pinned)| MetricLayout {
            id: format!("sub2api.{suffix}"),
            enabled: *enabled,
            section: *section,
            pinned: *pinned,
        })
        .collect();
    let mut saved = default_settings(&registry, &HashSet::from(["sub2api".to_owned()]));
    saved.schema_version = 7;
    saved.providers[0].metrics = v1_metrics.clone();
    storage
        .save_settings_with_metric_template_defaults(&saved, "sub2api", &v1_metrics)
        .unwrap();

    let (service, _) = SettingsService::new_deferred(storage, registry).unwrap();
    let upgraded = service.get();
    let metrics = &upgraded.providers[0].metrics;
    let reset = metrics
        .iter()
        .find(|metric| metric.id == "sub2api.rateLimitResets")
        .unwrap();
    assert!(reset.enabled);
    assert_eq!(reset.section, MetricSection::OnDemand);
    let pos = |id: &str| metrics.iter().position(|metric| metric.id == id).unwrap();
    assert!(pos("sub2api.rateLimitResets") < pos("sub2api.today"));
}

#[test]
fn layout_and_preferences_survive_a_service_restart() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let detected = HashSet::from(["codex".to_owned(), "antigravity".to_owned()]);
    let catalog = catalog();
    let first = SettingsService::new_for_test(storage.clone(), catalog.clone(), &detected).unwrap();
    let mut settings = first.get();
    settings.density = crate::models::DensityPreference::Compact;
    settings.window_mode = crate::models::WindowMode::Floating;
    settings.dismissed_update_version = Some("0.2.0".to_owned());
    settings.last_update_check_at = Some(chrono::Utc::now());
    settings.providers.rotate_left(1);
    settings.providers[1].metrics.rotate_right(1);
    let expected = first.update(settings).unwrap();
    let second = SettingsService::new_for_test(storage, catalog, &detected).unwrap();
    assert_eq!(second.get(), expected);
}

#[test]
fn new_detected_provider_is_enabled_once_without_overriding_later_choice() {
    let catalog = catalog();
    let mut settings = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
    settings.known_provider_ids.retain(|id| id != "antigravity");
    settings
        .providers
        .retain(|provider| provider.id != "antigravity");
    let detected = HashSet::from(["codex".to_owned(), "antigravity".to_owned()]);
    normalize(&catalog, &mut settings, &detected);
    let antigravity = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "antigravity")
        .unwrap();
    assert!(antigravity.enabled);
    antigravity.enabled = false;
    normalize(&catalog, &mut settings, &detected);
    assert!(
        !settings
            .providers
            .iter()
            .find(|provider| provider.id == "antigravity")
            .unwrap()
            .enabled
    );
}

#[test]
fn legacy_claude_account_cards_fold_back_into_the_claude_card() {
    let legacy = catalog_with_claude_account();
    let detected = HashSet::from(["claude".to_owned(), "claude@1234abcd".to_owned()]);
    let mut settings = default_settings(&legacy, &detected);
    let claude = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "claude")
        .unwrap();
    claude.expanded = true;
    claude.metrics.reverse();
    let expected_metrics = claude.metrics.clone();
    settings
        .provider_names
        .insert("claude".into(), "  Personal  ".into());
    settings
        .provider_names
        .insert("claude@1234abcd".into(), "Work".into());
    settings
        .provider_names
        .insert("codex".into(), "  Work Codex  ".into());

    normalize(&catalog(), &mut settings, &detected);

    assert!(!settings
        .providers
        .iter()
        .any(|provider| provider.id.starts_with("claude@")));
    let claude = settings
        .providers
        .iter()
        .find(|provider| provider.id == "claude")
        .unwrap();
    assert!(claude.expanded);
    assert_eq!(claude.metrics, expected_metrics);
    assert_eq!(settings.provider_names.len(), 2);
    assert_eq!(
        settings.provider_names.get("claude").map(String::as_str),
        Some("Personal")
    );
    assert_eq!(
        settings.provider_names.get("codex").map(String::as_str),
        Some("Work Codex")
    );
}

#[test]
fn schema_two_migration_uses_the_multi_provider_default_order() {
    let catalog = catalog();
    let mut settings = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
    settings.schema_version = 2;
    settings.known_provider_ids.clear();
    settings.providers.retain(|provider| provider.id == "codex");
    normalize(
        &catalog,
        &mut settings,
        &HashSet::from(["codex".to_owned(), "antigravity".to_owned()]),
    );
    assert_eq!(settings.schema_version, 9);
    assert_eq!(
        settings
            .providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>(),
        ["claude", "codex", "cursor", "antigravity", "openrouter"]
    );
}

#[test]
fn invalid_saved_settings_are_not_overwritten_with_defaults() {
    let directory = tempdir().unwrap();
    let database_path = directory.path().join("openquota.db");
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE app_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                payload TEXT NOT NULL
            );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO app_settings(id, payload) VALUES (1, ?1)",
            ["{not-valid-json"],
        )
        .unwrap();
    drop(connection);
    let storage = Arc::new(Storage::open(&database_path).unwrap());

    assert!(SettingsService::new_deferred(storage.clone(), catalog()).is_err());
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let payload: String = connection
        .query_row("SELECT payload FROM app_settings WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(payload, "{not-valid-json");
}

#[test]
fn provider_reset_uses_the_backend_catalog_and_preserves_provider_state() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let detected = HashSet::from(["codex".to_owned()]);
    let catalog = catalog();
    let service = SettingsService::new_for_test(storage, catalog.clone(), &detected).unwrap();
    let mut settings = service.get();
    let codex = settings
        .providers
        .iter_mut()
        .find(|provider| provider.id == "codex")
        .unwrap();
    codex.enabled = false;
    codex.expanded = true;
    codex.metrics.reverse();
    codex.metrics[0].pinned = true;
    service.update(settings).unwrap();

    let reset = service
        .reset_provider(
            "codex",
            service.settings_revision(),
            service.account_revision(),
        )
        .unwrap();
    let codex = reset
        .providers
        .iter()
        .find(|provider| provider.id == "codex")
        .unwrap();
    let defaults = default_settings(&catalog, &detected);
    let default_codex = defaults
        .providers
        .iter()
        .find(|provider| provider.id == "codex")
        .unwrap();

    assert!(!codex.enabled);
    assert!(codex.detected);
    assert!(!codex.expanded);
    assert_eq!(codex.metrics, default_codex.metrics);
}

#[test]
fn full_reset_restores_defaults_without_deleting_usage_or_accounts() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    storage
        .save_provider_account_record(
            "codex",
            "identity-a",
            "codex",
            r#"{"customName":"Work","plan":"plus"}"#,
        )
        .unwrap();
    storage
        .save_provider_account_record(
            "codex",
            "identity-b",
            "codex@bbbbbbbb",
            r#"{"customName":"Personal","region":"eu"}"#,
        )
        .unwrap();
    let snapshot = ProviderSnapshot {
        upstream_provider_id: None,
        accounts: None,
        provider_id: "codex".into(),
        plan: Some("Plus".into()),
        quotas: Vec::new(),
        value_metrics: Vec::new(),
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage_histories: Default::default(),
        warnings: Vec::new(),
        refreshed_at: chrono::Utc::now(),
    };
    storage.save_snapshot(&snapshot).unwrap();
    let service = SettingsService::new_for_test(
        storage.clone(),
        catalog(),
        &HashSet::from(["codex".to_owned()]),
    )
    .unwrap();
    service
        .activate_account("codex", "codex", "identity-a")
        .unwrap();
    let mut customized = service.get();
    customized.theme = ThemePreference::Dark;
    customized.density = crate::models::DensityPreference::Compact;
    customized.reduce_animations = true;
    customized.launch_at_login = true;
    customized.global_shortcut = Some("Ctrl+Shift+Q".into());
    customized.notifications.almost_out = true;
    customized
        .provider_names
        .insert("codex".into(), "Work".into());
    customized.providers.reverse();
    service.update(customized).unwrap();

    let defaults = service.reset_defaults();
    let reset = service
        .reset_all_from_view(
            defaults,
            service.settings_revision(),
            service.account_revision(),
        )
        .unwrap();

    let expected = service.reset_defaults();
    assert_eq!(reset.providers, expected.providers);
    assert_eq!(reset.provider_names, expected.provider_names);
    assert_eq!(reset.theme, expected.theme);
    assert_eq!(reset.density, expected.density);
    assert_eq!(reset.reduce_animations, expected.reduce_animations);
    assert_eq!(reset.launch_at_login, expected.launch_at_login);
    assert_eq!(reset.global_shortcut, expected.global_shortcut);
    assert_eq!(reset.notifications, expected.notifications);
    assert_eq!(storage.load_snapshot("codex").unwrap(), Some(snapshot));
    let records = storage.load_provider_account_records("codex").unwrap();
    assert_eq!(records.len(), 2);
    let payloads = records
        .into_iter()
        .map(|(identity, provider_id, payload)| {
            (
                identity,
                provider_id,
                serde_json::from_str::<Value>(&payload).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads[0].0, "identity-a");
    assert_eq!(payloads[0].1, "codex");
    assert_eq!(payloads[0].2["plan"], "plus");
    assert!(payloads[0].2.get("customName").is_none());
    assert_eq!(payloads[1].0, "identity-b");
    assert_eq!(payloads[1].1, "codex@bbbbbbbb");
    assert_eq!(payloads[1].2["region"], "eu");
    assert!(payloads[1].2.get("customName").is_none());
}

#[test]
fn api_key_save_marks_and_enables_provider_while_delete_only_clears_detection() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(storage, catalog(), &HashSet::new()).unwrap();

    service.record_provider_credential_mutation();
    let saved = service
        .reconcile_provider_credential_state("openrouter", true, true)
        .unwrap();
    let openrouter = saved
        .providers
        .iter()
        .find(|provider| provider.id == "openrouter")
        .unwrap();
    assert!(openrouter.detected);
    assert!(openrouter.enabled);

    service.record_provider_credential_mutation();
    let deleted = service
        .reconcile_provider_credential_state("openrouter", false, false)
        .unwrap();
    let openrouter = deleted
        .providers
        .iter()
        .find(|provider| provider.id == "openrouter")
        .unwrap();
    assert!(!openrouter.detected);
    assert!(openrouter.enabled);
}

#[test]
fn credential_reconciliation_persists_a_metric_template_with_provider_state() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service =
        SettingsService::new_for_test(storage.clone(), catalog(), &HashSet::new()).unwrap();
    let mut template = service
        .get()
        .providers
        .into_iter()
        .find(|provider| provider.id == "openrouter")
        .unwrap()
        .metrics;
    template.reverse();
    template[0].enabled = false;

    let saved = service
        .reconcile_provider_credential_state_with_metrics(
            "openrouter",
            true,
            true,
            Some(MetricTemplateChange {
                template: template.clone(),
                previous: None,
                upstream_provider_id: None,
            }),
        )
        .unwrap();
    let provider = saved
        .providers
        .iter()
        .find(|provider| provider.id == "openrouter")
        .unwrap();
    assert!(provider.detected);
    assert!(provider.enabled);
    assert_eq!(provider.metrics, template);
    let persisted = storage.load_settings().unwrap().unwrap();
    assert_eq!(
        persisted
            .providers
            .iter()
            .find(|provider| provider.id == "openrouter")
            .unwrap()
            .metrics,
        template
    );
}

#[test]
fn sub2api_track_changes_preserve_all_preferences() {
    use crate::providers::sub2api::{
        metric_template, track_provider_id, Sub2ApiMode, Sub2ApiProviders, Sub2ApiUpstream,
    };

    for pinned in [
        vec![],
        vec!["daily", "monthly"],
        vec!["weekly"],
        vec!["session", "spark"],
    ] {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let providers = Sub2ApiProviders::new(directory.path().join("sub2api"), storage.clone());
        let mut definition = providers.provider("sub2api").unwrap().definition();
        definition.fallback_enabled = true;
        let registry =
            Arc::new(ProviderRegistry::new(vec![Arc::new(CatalogProvider(definition))]).unwrap());
        let service =
            SettingsService::new_for_test(storage.clone(), registry, &HashSet::new()).unwrap();
        let template =
            metric_template("sub2api", Sub2ApiUpstream::Codex, Sub2ApiMode::Subscription);
        let mut settings = service
            .reconcile_provider_credential_state_with_metrics(
                "sub2api",
                true,
                true,
                Some(MetricTemplateChange {
                    template: template.clone(),
                    previous: None,
                    upstream_provider_id: None,
                }),
            )
            .unwrap();
        // Initial setup still gets the mode's default layout.
        assert_eq!(settings.providers[0].metrics, template);
        settings.providers[0].enabled = !pinned.is_empty();
        settings.providers[0].expanded = true;
        settings.providers[0].metrics.reverse();
        for metric in &mut settings.providers[0].metrics {
            metric.enabled = !metric.enabled;
            metric.section = match metric.section {
                MetricSection::AlwaysVisible => MetricSection::OnDemand,
                MetricSection::OnDemand => MetricSection::AlwaysVisible,
            };
            metric.pinned = pinned
                .iter()
                .any(|suffix| metric.id == format!("sub2api.{suffix}"));
        }
        settings
            .provider_names
            .insert("sub2api".into(), "My account".into());
        settings
            .provider_proxies
            .insert("sub2api".into(), "direct".into());
        settings.theme = ThemePreference::Dark;
        let expected = service.update(settings).unwrap();

        let mut previous = (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex);
        for (mode, upstream) in [
            (Sub2ApiMode::Admin, Sub2ApiUpstream::Codex),
            (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex),
            (Sub2ApiMode::Admin, Sub2ApiUpstream::Claude),
            (Sub2ApiMode::Admin, Sub2ApiUpstream::Grok),
            (Sub2ApiMode::Subscription, Sub2ApiUpstream::Grok),
        ] {
            let saved = service
                .reconcile_provider_credential_state_with_metrics(
                    "sub2api",
                    true,
                    false,
                    Some(MetricTemplateChange {
                        template: metric_template("sub2api", upstream, mode),
                        previous: Some(metric_template("sub2api", previous.1, previous.0)),
                        upstream_provider_id: Some(track_provider_id(mode, upstream)),
                    }),
                )
                .unwrap();
            previous = (mode, upstream);
            assert_eq!(saved, expected);
            // Include a settings save to catch normalization changing retained preferences.
            let saved = service.update(saved).unwrap();
            assert_eq!(saved, expected);
            assert_eq!(storage.load_settings().unwrap().unwrap(), expected);
        }
    }
}

fn sub2api_settings_service(directory: &std::path::Path) -> SettingsService {
    let storage = Arc::new(Storage::open(&directory.join("openquota.db")).unwrap());
    let providers = Sub2ApiProviders::new(directory.join("sub2api"), storage.clone());
    let mut definition = providers.provider("sub2api").unwrap().definition();
    definition.fallback_enabled = true;
    let registry =
        Arc::new(ProviderRegistry::new(vec![Arc::new(CatalogProvider(definition))]).unwrap());
    SettingsService::new_for_test(storage, registry, &HashSet::new()).unwrap()
}

fn reconcile_sub2api_track(
    service: &SettingsService,
    (mode, upstream): (Sub2ApiMode, Sub2ApiUpstream),
    previous: Option<(Sub2ApiMode, Sub2ApiUpstream)>,
) -> AppSettings {
    service
        .reconcile_provider_credential_state_with_metrics(
            "sub2api",
            true,
            true,
            Some(MetricTemplateChange {
                template: metric_template("sub2api", upstream, mode),
                previous: previous
                    .map(|(mode, upstream)| metric_template("sub2api", upstream, mode)),
                upstream_provider_id: Some(track_provider_id(mode, upstream)),
            }),
        )
        .unwrap()
}

fn sub2api_metric(settings: &AppSettings, suffix: &str) -> MetricLayout {
    settings.providers[0]
        .metrics
        .iter()
        .find(|metric| metric.id == format!("sub2api.{suffix}"))
        .cloned()
        .unwrap()
}

#[test]
fn sub2api_track_changes_move_untouched_defaults_to_the_new_template() {
    let codex_admin = (Sub2ApiMode::Admin, Sub2ApiUpstream::Codex);

    // An untouched Codex admin layout shows Daily and Monthly once it moves to a subscription.
    let directory = tempdir().unwrap();
    let service = sub2api_settings_service(directory.path());
    reconcile_sub2api_track(&service, codex_admin, None);
    let saved = reconcile_sub2api_track(
        &service,
        (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex),
        Some(codex_admin),
    );
    let daily = sub2api_metric(&saved, "daily");
    assert!(daily.enabled && daily.pinned);
    assert_eq!(daily.section, MetricSection::AlwaysVisible);
    assert!(sub2api_metric(&saved, "monthly").enabled);
    assert!(sub2api_metric(&saved, "weekly").pinned);
    assert!(!sub2api_metric(&saved, "session").enabled);
    assert!(!sub2api_metric(&saved, "spark").enabled);

    // Grok shows its monthly billing window after a switch from Codex.
    let directory = tempdir().unwrap();
    let service = sub2api_settings_service(directory.path());
    reconcile_sub2api_track(&service, codex_admin, None);
    let saved = reconcile_sub2api_track(
        &service,
        (Sub2ApiMode::Admin, Sub2ApiUpstream::Grok),
        Some(codex_admin),
    );
    assert!(sub2api_metric(&saved, "monthly").enabled);
    assert!(sub2api_metric(&saved, "weekly").enabled);
    assert!(!sub2api_metric(&saved, "weekly").pinned);
    assert!(!sub2api_metric(&saved, "session").enabled);

    // Layouts saved before defaults were recorded fall back to the previous track's template.
    let directory = tempdir().unwrap();
    let service = sub2api_settings_service(directory.path());
    let mut settings = service
        .reconcile_provider_credential_state("sub2api", true, true)
        .unwrap();
    settings.providers[0].metrics =
        metric_template("sub2api", Sub2ApiUpstream::Codex, Sub2ApiMode::Admin);
    service.update(settings).unwrap();
    let saved = reconcile_sub2api_track(
        &service,
        (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex),
        Some(codex_admin),
    );
    assert!(sub2api_metric(&saved, "daily").enabled);
    assert!(sub2api_metric(&saved, "monthly").enabled);
}

/// A connection runtime whose current track the test sets, as a saved connection would.
struct TrackedConnection(ProviderDefinition, std::sync::Mutex<Option<&'static str>>);

impl UsageProvider for TrackedConnection {
    fn definition(&self) -> ProviderDefinition {
        self.0.clone()
    }

    fn has_local_credentials(&self) -> bool {
        false
    }

    fn supports_connection_configuration(&self) -> bool {
        true
    }

    fn upstream_provider_id(&self) -> Option<&'static str> {
        *self.1.lock().unwrap()
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        unreachable!()
    }
}

#[test]
fn an_unknown_track_keeps_every_pin_until_the_track_is_read() {
    let subscription = (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex);
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let providers = Sub2ApiProviders::new(directory.path().join("sub2api"), storage.clone());
    let mut definition = providers.provider("sub2api").unwrap().definition();
    definition.fallback_enabled = true;
    let connection = Arc::new(TrackedConnection(definition, Default::default()));
    let registry = Arc::new(ProviderRegistry::new(vec![connection.clone()]).unwrap());
    let service = SettingsService::new_for_test(storage, registry, &HashSet::new()).unwrap();
    *connection.1.lock().unwrap() = Some(track_provider_id(subscription.0, subscription.1));
    let mut settings = reconcile_sub2api_track(&service, subscription, None);
    // Session and Spark report nothing on a subscription, so they do not use up the cap.
    for metric in &mut settings.providers[0].metrics {
        if matches!(
            metric.id.as_str(),
            "sub2api.session" | "sub2api.spark" | "sub2api.daily" | "sub2api.weekly"
        ) {
            metric.pinned = true;
        }
    }
    let expected = service.update(settings).unwrap();
    assert!(sub2api_metric(&expected, "weekly").pinned);

    // A marker from before tracks were recorded leaves the track unknown at launch.
    *connection.1.lock().unwrap() = None;
    let saved = service.update(expected.clone()).unwrap();
    assert_eq!(saved.providers[0].metrics, expected.providers[0].metrics);
}

#[test]
fn resets_apply_the_current_track_and_later_track_changes_still_move_it() {
    let subscription = (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex);
    let grok_admin = (Sub2ApiMode::Admin, Sub2ApiUpstream::Grok);
    let set_track = |connection: &TrackedConnection, (mode, upstream)| {
        *connection.1.lock().unwrap() = Some(track_provider_id(mode, upstream));
    };
    for reset in ["provider", "customization", "all"] {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let providers = Sub2ApiProviders::new(directory.path().join("sub2api"), storage.clone());
        let mut definition = providers.provider("sub2api").unwrap().definition();
        definition.fallback_enabled = true;
        let connection = Arc::new(TrackedConnection(definition, Default::default()));
        let registry = Arc::new(ProviderRegistry::new(vec![connection.clone()]).unwrap());
        assert!(registry
            .catalog()
            .connection_config_provider_ids
            .contains(&"sub2api".to_owned()));
        let service = SettingsService::new_for_test(storage, registry, &HashSet::new()).unwrap();

        set_track(&connection, subscription);
        let mut settings = reconcile_sub2api_track(&service, subscription, None);
        for metric in &mut settings.providers[0].metrics {
            metric.enabled = !metric.enabled;
        }
        service.update(settings).unwrap();

        let settings_revision = service.settings_revision();
        let account_revision = service.account_revision();
        let reset_settings = match reset {
            "provider" => service
                .reset_provider("sub2api", settings_revision, account_revision)
                .unwrap(),
            "customization" => {
                let mut next = service.get();
                next.providers = service
                    .default_settings(&HashSet::from(["sub2api".to_owned()]))
                    .providers;
                service
                    .reset_layouts_from_view(next, settings_revision, account_revision)
                    .unwrap()
            }
            _ => service
                .reset_all_from_view(
                    service.reset_defaults(),
                    settings_revision,
                    account_revision,
                )
                .unwrap(),
        };
        // A reset restores the connection's own track layout, not the generic one.
        for default in metric_template("sub2api", Sub2ApiUpstream::Codex, Sub2ApiMode::Subscription)
        {
            assert_eq!(
                sub2api_metric(&reset_settings, default.id.trim_start_matches("sub2api.")),
                default,
                "{reset}"
            );
        }

        set_track(&connection, grok_admin);
        let saved = reconcile_sub2api_track(&service, grok_admin, Some(subscription));
        for default in metric_template("sub2api", Sub2ApiUpstream::Grok, Sub2ApiMode::Admin) {
            assert_eq!(
                sub2api_metric(&saved, default.id.trim_start_matches("sub2api.")),
                default,
                "{reset}"
            );
        }
    }
}

#[test]
fn sub2api_track_changes_keep_user_pins_within_the_pin_cap() {
    let grok_admin = (Sub2ApiMode::Admin, Sub2ApiUpstream::Grok);
    let directory = tempdir().unwrap();
    let service = sub2api_settings_service(directory.path());
    let mut settings = reconcile_sub2api_track(&service, grok_admin, None);
    for metric in &mut settings.providers[0].metrics {
        if matches!(metric.id.as_str(), "sub2api.weekly" | "sub2api.monthly") {
            metric.enabled = true;
            metric.pinned = true;
        }
    }
    service.update(settings).unwrap();
    assert!(!sub2api_metric(&service.get(), "daily").pinned);

    // Subscription pins Daily by default, but the user's two pins already fill the cap.
    let saved = reconcile_sub2api_track(
        &service,
        (Sub2ApiMode::Subscription, Sub2ApiUpstream::Grok),
        Some(grok_admin),
    );
    assert!(sub2api_metric(&saved, "weekly").pinned);
    assert!(sub2api_metric(&saved, "monthly").pinned);
    let daily = sub2api_metric(&saved, "daily");
    assert!(daily.enabled && !daily.pinned);
    let pins = saved.providers[0]
        .metrics
        .iter()
        .filter(|metric| metric.pinned)
        .count();
    assert!(pins <= crate::metric_policy::MAX_STARS);
}

#[test]
fn sub2api_track_changes_keep_customized_metrics_and_order() {
    let codex_admin = (Sub2ApiMode::Admin, Sub2ApiUpstream::Codex);
    let directory = tempdir().unwrap();
    let service = sub2api_settings_service(directory.path());
    let mut settings = reconcile_sub2api_track(&service, codex_admin, None);
    settings.providers[0].metrics.reverse();
    let monthly = settings.providers[0]
        .metrics
        .iter_mut()
        .find(|metric| metric.id == "sub2api.monthly")
        .unwrap();
    // Still hidden, but no longer the template's default.
    monthly.section = MetricSection::AlwaysVisible;
    let order: Vec<String> = settings.providers[0]
        .metrics
        .iter()
        .map(|metric| metric.id.clone())
        .collect();
    service.update(settings).unwrap();

    let saved = reconcile_sub2api_track(
        &service,
        (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex),
        Some(codex_admin),
    );
    let ids: Vec<&str> = saved.providers[0]
        .metrics
        .iter()
        .map(|metric| metric.id.as_str())
        .collect();
    assert_eq!(ids, order);
    let monthly = sub2api_metric(&saved, "monthly");
    assert!(!monthly.enabled);
    assert_eq!(monthly.section, MetricSection::AlwaysVisible);
    assert!(sub2api_metric(&saved, "daily").enabled);
}

#[test]
fn dynamic_provider_removal_disables_and_clears_detection() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(storage, catalog(), &HashSet::new()).unwrap();
    service
        .reconcile_provider_credential_state("openrouter", true, true)
        .unwrap();

    let removed = service.remove_dynamic_provider("openrouter").unwrap();
    let provider = removed
        .providers
        .iter()
        .find(|provider| provider.id == "openrouter")
        .unwrap();
    assert!(!provider.enabled);
    assert!(!provider.detected);
    assert!(!provider.expanded);
}

#[test]
fn stale_absent_probe_cannot_undo_an_api_key_save_for_a_detected_provider() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(
        storage,
        catalog(),
        &HashSet::from(["openrouter".to_owned()]),
    )
    .unwrap();
    let plan = service.reset_detection_plan();

    service.record_provider_credential_mutation();
    service
        .reconcile_provider_credential_state("openrouter", true, true)
        .unwrap();
    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&[]))
        .unwrap();
    let openrouter = outcome
        .settings
        .providers
        .iter()
        .find(|provider| provider.id == "openrouter")
        .unwrap();

    assert!(openrouter.detected);
    assert!(openrouter.enabled);
    assert_eq!(service.credential_revision.load(Ordering::SeqCst), 1);
}

#[test]
fn stale_detected_probe_cannot_undo_an_api_key_delete() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let service = SettingsService::new_for_test(
        storage,
        catalog(),
        &HashSet::from(["openrouter".to_owned()]),
    )
    .unwrap();
    let plan = service.reset_detection_plan();

    service.record_provider_credential_mutation();
    service
        .reconcile_provider_credential_state("openrouter", false, false)
        .unwrap();
    let outcome = service
        .apply_credential_detection(&plan, &probe_results(&["openrouter"]))
        .unwrap();
    let openrouter = outcome
        .settings
        .providers
        .iter()
        .find(|provider| provider.id == "openrouter")
        .unwrap();

    assert!(!openrouter.detected);
    assert!(openrouter.enabled);
    assert_eq!(service.credential_revision.load(Ordering::SeqCst), 1);
}
