use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::{
    hashing::sha256_hex,
    metric_policy::{MetricPolicy, MAX_STARS},
    models::{
        AppSettings, MetricDefinition, MetricLayout, MetricSection, MetricSource,
        ProviderDefinition, ProviderLayout,
    },
    providers::ProviderRegistry,
};

pub(super) fn enabled_provider_set(settings: &AppSettings) -> HashSet<String> {
    settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .map(|provider| provider.id.clone())
        .collect()
}

pub(super) fn detected_provider_set(settings: &AppSettings) -> HashSet<String> {
    settings
        .providers
        .iter()
        .filter(|provider| provider.detected)
        .map(|provider| provider.id.clone())
        .collect()
}

pub fn default_settings(registry: &ProviderRegistry, detected: &HashSet<String>) -> AppSettings {
    let catalog = registry.catalog();
    let mut settings = AppSettings {
        known_provider_ids: catalog
            .providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect(),
        providers: catalog
            .providers
            .iter()
            .map(|provider| {
                reset_provider_layout(registry, provider, detected.contains(&provider.id))
            })
            .collect(),
        ..AppSettings::default()
    };
    if !settings.providers.iter().any(|provider| provider.enabled) {
        for provider in &mut settings.providers {
            provider.enabled = registry
                .definition(&provider.id)
                .is_some_and(|definition| definition.fallback_enabled);
        }
    }
    settings
}

pub fn normalize(
    registry: &ProviderRegistry,
    settings: &mut AppSettings,
    detected: &HashSet<String>,
) {
    let catalog = registry.catalog();
    let migrating_to_multi_provider = settings.schema_version < 3;
    let migrating_reset_metric_order = settings.schema_version < 9;
    settings.schema_version = 9;
    settings.dismissed_update_version = settings
        .dismissed_update_version
        .take()
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty());
    settings.global_shortcut = settings
        .global_shortcut
        .take()
        .map(|shortcut| shortcut.trim().to_owned())
        .filter(|shortcut| !shortcut.is_empty());
    settings.provider_names.retain(|provider_id, name| {
        if !registry.supports_account_names(provider_id) {
            return false;
        }
        *name = name.trim().chars().take(48).collect();
        !name.is_empty()
    });

    if settings.known_provider_ids.is_empty() {
        settings.known_provider_ids = settings
            .providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect();
    }

    let mut normalized = Vec::new();
    for mut provider in settings.providers.clone() {
        let Some(definition) = registry.definition(&provider.id) else {
            continue;
        };
        if normalized
            .iter()
            .any(|known: &ProviderLayout| known.id == provider.id)
        {
            continue;
        }
        let was_known = settings
            .known_provider_ids
            .iter()
            .any(|known| known == &definition.id);
        if !was_known {
            provider.enabled = detected.contains(&definition.id);
            settings.known_provider_ids.push(definition.id.clone());
        }
        provider.detected = detected.contains(&definition.id);
        let provider_id = provider.id.clone();
        let policy = MetricPolicy::for_provider(registry, &provider_id);
        normalize_metrics(
            &mut provider.metrics,
            &definition.metrics,
            &policy,
            migrating_reset_metric_order,
        );
        normalized.push(provider);
    }
    for definition in &catalog.providers {
        if normalized
            .iter()
            .any(|provider| provider.id == definition.id)
        {
            continue;
        }
        let was_known = settings
            .known_provider_ids
            .iter()
            .any(|known| known == &definition.id);
        let is_detected = detected.contains(&definition.id);
        let mut provider = default_provider(definition, is_detected);
        provider.enabled = !was_known && is_detected;
        settings.known_provider_ids.push(definition.id.clone());
        normalized.push(provider);
    }
    if migrating_to_multi_provider {
        normalized.sort_by_key(|provider| {
            catalog
                .providers
                .iter()
                .position(|definition| definition.id == provider.id)
                .unwrap_or(usize::MAX)
        });
    }
    settings.providers = normalized;
    settings.known_provider_ids.sort();
    settings.known_provider_ids.dedup();
}

pub(super) fn account_record(
    records: &[(String, String, String)],
    family: &str,
    identity: &str,
) -> (String, Value) {
    if let Some((_, provider_id, payload)) = records
        .iter()
        .find(|(known_identity, _, _)| known_identity == identity)
    {
        return (
            provider_id.clone(),
            serde_json::from_str(payload).unwrap_or_else(|_| Value::Object(Map::new())),
        );
    }

    let provider_id = if records.is_empty() {
        family.to_owned()
    } else {
        allocate_account_provider_id(records, family, identity)
    };
    (provider_id, Value::Object(Map::new()))
}

fn allocate_account_provider_id(
    records: &[(String, String, String)],
    family: &str,
    identity: &str,
) -> String {
    let occupied = records
        .iter()
        .map(|(_, provider_id, _)| provider_id.as_str())
        .collect::<HashSet<_>>();
    for salt in 0_u32.. {
        let suffix = if salt == 0
            && identity.len() >= 8
            && identity.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            identity[..8].to_ascii_lowercase()
        } else {
            sha256_hex(format!("{identity}:{salt}").as_bytes())[..8].to_owned()
        };
        let candidate = format!("{family}@{suffix}");
        if !occupied.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

pub(super) fn account_custom_name(payload: &Value) -> Option<String> {
    payload
        .get("customName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

pub(super) fn set_account_custom_name(payload: &mut Value, name: Option<&str>) {
    if !payload.is_object() {
        *payload = Value::Object(Map::new());
    }
    let object = payload
        .as_object_mut()
        .expect("account payload was initialized as an object");
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    if let Some(name) = name {
        object.insert("customName".into(), Value::String(name.to_owned()));
    } else {
        object.remove("customName");
    }
}

/// A provider's default layout. A detected connection takes its current track's template, as
/// first setup applies it, rather than the generic layout of its definition.
pub(super) fn reset_provider_layout(
    registry: &ProviderRegistry,
    definition: &ProviderDefinition,
    detected: bool,
) -> ProviderLayout {
    let mut provider = default_provider(definition, detected);
    if let Some(defaults) = detected
        .then(|| track_template_defaults(registry, &definition.id))
        .flatten()
    {
        provider.metrics = defaults;
    }
    provider
}

/// The metric defaults a connection's current track applies to an empty layout, if it has one.
pub(super) fn track_template_defaults(
    registry: &ProviderRegistry,
    provider_id: &str,
) -> Option<Vec<MetricLayout>> {
    let policy = MetricPolicy::for_provider(registry, provider_id);
    let template =
        crate::providers::sub2api::metric_template_for_track(provider_id, policy.track()?)?;
    let mut metrics = Vec::new();
    Some(apply_metric_template(&mut metrics, &template, &[], &policy))
}

fn default_provider(definition: &ProviderDefinition, detected: bool) -> ProviderLayout {
    ProviderLayout {
        id: definition.id.clone(),
        enabled: detected,
        detected,
        expanded: false,
        metrics: definition
            .metrics
            .iter()
            .map(|metric| MetricLayout {
                id: metric.id.clone(),
                enabled: metric.default_enabled,
                section: metric.default_section,
                pinned: metric.default_pinned,
            })
            .collect(),
    }
}

/// A provider's new default metric layout and the template it replaces.
pub struct MetricTemplateChange {
    pub template: Vec<MetricLayout>,
    /// Stands in for the recorded defaults of layouts saved before they were tracked.
    pub previous: Option<Vec<MetricLayout>>,
    /// The new track's upstream id; decides which pins count toward the per-provider cap.
    pub upstream_provider_id: Option<&'static str>,
}

/// Moves an already customized layout onto a new template without losing user choices.
///
/// `applied` holds the default OpenQuota last wrote to each metric. Metrics still matching
/// it take the new default, so a metric one track hides appears on a track that shows it.
/// Anything the user changed keeps its settings, and the existing order is retained.
///
/// Pins the user chose take priority: a new default's pin is dropped when it would push the
/// Stars past [`MAX_STARS`].
///
/// Returns the defaults to record. A customized metric keeps the default it was customized
/// from, even when it happens to match the new one, so later track changes still keep it.
pub(super) fn apply_metric_template(
    metrics: &mut Vec<MetricLayout>,
    template: &[MetricLayout],
    applied: &[MetricLayout],
    policy: &MetricPolicy,
) -> Vec<MetricLayout> {
    let preserve_template_order = metrics.is_empty()
        || (!applied.is_empty()
            && metrics.len() == applied.len()
            && metrics
                .iter()
                .zip(applied.iter())
                .all(|(current, previous)| current.id == previous.id));
    let template_ids = template
        .iter()
        .map(|metric| metric.id.as_str())
        .collect::<Vec<_>>();
    let mut recorded = Vec::with_capacity(template.len());
    let mut adopted = Vec::new();
    for (template_index, default) in template.iter().enumerate() {
        match metrics.iter_mut().find(|current| current.id == default.id) {
            Some(current) if !applied.contains(current) => {
                recorded.extend(
                    applied
                        .iter()
                        .find(|previous| previous.id == default.id)
                        .cloned(),
                );
                continue;
            }
            Some(current) => current.clone_from(default),
            None => insert_metric_in_canonical_order(
                metrics,
                &template_ids,
                template_index,
                default.clone(),
            ),
        }
        adopted.push(default.id.clone());
        recorded.push(default.clone());
    }
    if preserve_template_order {
        metrics.sort_by_key(|metric| {
            template
                .iter()
                .position(|default| default.id == metric.id)
                .unwrap_or(usize::MAX)
        });
    }
    let counts_toward_cap = |metric: &MetricLayout| metric.pinned && policy.shows(&metric.id);
    let mut pins = metrics
        .iter()
        .filter(|metric| counts_toward_cap(metric) && !adopted.contains(&metric.id))
        .count();
    for metric in metrics.iter_mut() {
        if !adopted.contains(&metric.id) || !counts_toward_cap(metric) {
            continue;
        }
        if pins < MAX_STARS {
            pins += 1;
            continue;
        }
        metric.pinned = false;
        if let Some(default) = recorded.iter_mut().find(|default| default.id == metric.id) {
            default.pinned = false;
        }
    }
    recorded
}

pub(super) fn insert_metric_in_canonical_order(
    metrics: &mut Vec<MetricLayout>,
    canonical_ids: &[&str],
    canonical_index: usize,
    metric: MetricLayout,
) {
    if let Some(position) = canonical_ids[canonical_index + 1..]
        .iter()
        .find_map(|next_id| {
            metrics
                .iter()
                .position(|existing| existing.id == *next_id && existing.section == metric.section)
        })
    {
        metrics.insert(position, metric);
        return;
    }
    if let Some(position) = canonical_ids[..canonical_index]
        .iter()
        .rev()
        .find_map(|prev_id| {
            metrics
                .iter()
                .position(|existing| existing.id == *prev_id && existing.section == metric.section)
        })
    {
        metrics.insert(position + 1, metric);
        return;
    }
    if let Some(position) = canonical_ids[canonical_index + 1..]
        .iter()
        .find_map(|next_id| metrics.iter().position(|existing| existing.id == *next_id))
    {
        metrics.insert(position, metric);
        return;
    }
    metrics.push(metric);
}

fn is_migrated_reset_metric(definition: &MetricDefinition) -> bool {
    matches!(
        &definition.source,
        MetricSource::Value { source_id, .. }
            if source_id == "claudeRateLimitResets" || source_id == "sub2apiRateLimitResets"
    )
}

fn normalize_metrics(
    metrics: &mut Vec<MetricLayout>,
    definitions: &[MetricDefinition],
    policy: &MetricPolicy,
    migrating_reset_metric_order: bool,
) {
    let canonical_ids = definitions
        .iter()
        .map(|definition| definition.id.as_str())
        .collect::<Vec<_>>();
    let mut normalized = Vec::with_capacity(definitions.len());
    for metric in metrics.iter() {
        if definitions
            .iter()
            .any(|definition| definition.id == metric.id)
            && !normalized
                .iter()
                .any(|known: &MetricLayout| known.id == metric.id)
        {
            normalized.push(metric.clone());
        }
    }
    for (index, definition) in definitions.iter().enumerate() {
        if !normalized.iter().any(|metric| metric.id == definition.id) {
            insert_metric_in_canonical_order(
                &mut normalized,
                &canonical_ids,
                index,
                MetricLayout {
                    id: definition.id.clone(),
                    enabled: definition.default_enabled,
                    section: definition.default_section,
                    pinned: definition.default_pinned,
                },
            );
        }
    }
    if migrating_reset_metric_order {
        if let Some((canonical_index, definition)) = definitions
            .iter()
            .enumerate()
            .find(|(_, definition)| is_migrated_reset_metric(definition))
        {
            let first_usage_position = definitions[canonical_index + 1..]
                .iter()
                .filter(|later| matches!(later.source, MetricSource::Usage { .. }))
                .filter_map(|later| {
                    normalized.iter().position(|metric| {
                        metric.id == later.id && metric.section == MetricSection::OnDemand
                    })
                })
                .min();
            let reset_position = normalized.iter().position(|metric| {
                metric.id == definition.id
                    && metric.section == definition.default_section
                    && metric.pinned == definition.default_pinned
            });
            if let (Some(first_usage_position), Some(reset_position)) =
                (first_usage_position, reset_position)
            {
                if reset_position > first_usage_position {
                    let reset_metric = normalized.remove(reset_position);
                    insert_metric_in_canonical_order(
                        &mut normalized,
                        &canonical_ids,
                        canonical_index,
                        reset_metric,
                    );
                }
            }
        }
    }
    let mut pin_count = 0;
    for metric in &mut normalized {
        let pinnable = definitions
            .iter()
            .find(|definition| definition.id == metric.id)
            .is_some_and(|definition| definition.pinnable);
        // Until the Track is known, no Star counts toward the limit, so none is dropped.
        if policy.track_known() && policy.shows(&metric.id) {
            metric.pinned &= pinnable && pin_count < MAX_STARS;
            if metric.pinned {
                pin_count += 1;
            }
        } else {
            metric.pinned &= pinnable;
        }
    }
    if !normalized
        .iter()
        .any(|metric| metric.enabled && metric.section == MetricSection::AlwaysVisible)
    {
        if let Some(metric) = normalized.iter_mut().find(|metric| metric.enabled) {
            metric.section = MetricSection::AlwaysVisible;
        }
    }
    *metrics = normalized;
}
