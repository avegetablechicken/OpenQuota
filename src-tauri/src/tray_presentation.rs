#[cfg(target_os = "macos")]
use tauri::Manager;
use tauri::{image::Image, AppHandle};

#[cfg(not(target_os = "macos"))]
use crate::tray_icon;
use crate::{
    models::{
        AppSettings, MetricDefinition, MetricSource, MetricValue, MetricValueKind,
        ProviderDefinition, ProviderSnapshot, QuotaFormat, UsageDisplay, UsagePeriod,
        UsagePeriodSelection,
    },
    providers::ProviderRegistry,
    service::UsageViewState,
};

const TRAY_ID: &str = "openquota-tray";

#[derive(Debug, Clone, PartialEq)]
struct TrayMetric {
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    metric_id: String,
    value: String,
    detail: String,
    gauge: Option<TrayGauge>,
}

#[derive(Debug, Clone, PartialEq)]
struct TrayGroup {
    #[cfg(any(target_os = "macos", test))]
    provider_id: String,
    #[cfg(any(target_os = "macos", test))]
    upstream_provider_id: Option<String>,
    metrics: Vec<TrayMetric>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TrayGauge {
    display_fraction: f64,
    #[cfg(any(not(target_os = "macos"), test))]
    remaining_fraction: f64,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq)]
enum MacMenuBarIcon {
    Mark,
    Text(Vec<crate::menu_bar::TextGroup>),
    Bars(Vec<crate::menu_bar::BarGroup>),
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq)]
struct MacMenuBarPresentation {
    icon: MacMenuBarIcon,
}

/// A metric currently drawn in the macOS menu bar, offered in the tray menu so it can be hidden.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MenuBarItem {
    pub(crate) metric_id: String,
    pub(crate) label: String,
}

/// A metric an enabled provider can show in the menu bar, offered in the tray menu's Add submenu.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MenuBarOption {
    pub(crate) metric_id: String,
    pub(crate) label: String,
    pub(crate) pinned: bool,
    /// False once the provider already has its maximum number of pins.
    pub(crate) available: bool,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MenuBarProvider {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) options: Vec<MenuBarOption>,
}

/// The menu bar entries of the tray menu: what is drawn now, and what each enabled provider offers.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MenuBarMenu {
    pub(crate) shown: Vec<MenuBarItem>,
    pub(crate) providers: Vec<MenuBarProvider>,
}

/// The menu bar entries behind the current tray menu, so the menu is rebuilt only when they change.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub(crate) struct MenuBarMenuState(std::sync::Mutex<MenuBarMenu>);

#[cfg(target_os = "macos")]
impl MenuBarMenuState {
    pub(crate) fn get(&self) -> MenuBarMenu {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn replace(&self, menu: MenuBarMenu) -> bool {
        let mut current = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *current == menu {
            return false;
        }
        *current = menu;
        true
    }
}

pub fn update(
    app: &AppHandle,
    state: &UsageViewState,
    settings: &AppSettings,
    registry: &ProviderRegistry,
) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let groups = resolved_groups(state, settings, registry);
    let tooltip = if groups.is_empty() {
        "OpenQuota".to_owned()
    } else {
        format!(
            "OpenQuota\n{}",
            groups
                .iter()
                .flat_map(|group| group.metrics.iter())
                .map(|metric| metric.detail.as_str())
                .collect::<Vec<_>>()
                .join(" · ")
        )
    };
    #[cfg(not(target_os = "linux"))]
    if tray.set_tooltip(Some(tooltip)).is_err() {
        crate::app_warn!("tray", "tray tooltip update failed");
    }
    #[cfg(target_os = "linux")]
    let _ = tooltip;

    #[cfg(not(target_os = "macos"))]
    {
        let icon = primary_gauge(&groups)
            .map(|gauge| tray_icon::render_gauge(gauge.display_fraction, gauge.remaining_fraction))
            .unwrap_or_else(mark_icon);
        if tray.set_icon(Some(icon)).is_err() {
            crate::app_warn!("tray", "tray icon update failed");
        }
    }

    #[cfg(target_os = "macos")]
    {
        apply_mac_menu_bar_presentation(
            &tray,
            mac_menu_bar_presentation(&groups, settings.menu_bar_style),
        );
        let menu = menu_bar_menu(&groups, settings, registry);
        if app
            .try_state::<MenuBarMenuState>()
            .is_some_and(|state| state.replace(menu))
        {
            crate::update_tray_menu(app, settings);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn mac_menu_bar_presentation(
    groups: &[TrayGroup],
    style: crate::models::MenuBarStyle,
) -> MacMenuBarPresentation {
    match style {
        crate::models::MenuBarStyle::Icon => MacMenuBarPresentation {
            icon: MacMenuBarIcon::Mark,
        },
        crate::models::MenuBarStyle::Text => {
            let text_groups = text_groups(groups);
            MacMenuBarPresentation {
                icon: if text_groups.is_empty() {
                    MacMenuBarIcon::Mark
                } else {
                    MacMenuBarIcon::Text(text_groups)
                },
            }
        }
        crate::models::MenuBarStyle::Bars => {
            let bar_groups = bar_groups(groups);
            MacMenuBarPresentation {
                icon: if bar_groups.is_empty() {
                    MacMenuBarIcon::Mark
                } else {
                    MacMenuBarIcon::Bars(bar_groups)
                },
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn apply_mac_menu_bar_presentation(
    tray: &tauri::tray::TrayIcon,
    presentation: MacMenuBarPresentation,
) {
    match presentation.icon {
        MacMenuBarIcon::Mark => {
            // An empty value explicitly clears stale native text before the fallback mark is shown.
            if tray.set_title(Some("")).is_err() {
                crate::app_warn!("tray", "macOS menu bar title clear failed");
            }
            if tray
                .set_icon_with_as_template(Some(mark_icon()), true)
                .is_err()
            {
                crate::app_warn!("tray", "macOS menu bar icon update failed");
            }
        }
        MacMenuBarIcon::Text(groups) => {
            // Text is one template strip image (provider marks + values), matching the single native
            // status-item ownership model while allowing each provider to keep its visual identity.
            if tray.set_title(Some("")).is_err() {
                crate::app_warn!("tray", "macOS menu bar title clear failed");
            }
            let icon = crate::menu_bar::text_icon(&groups).unwrap_or_else(mark_icon);
            if tray.set_icon_with_as_template(Some(icon), true).is_err() {
                crate::app_warn!("tray", "macOS menu bar icon update failed");
            }
        }
        MacMenuBarIcon::Bars(bar_groups) => {
            // Clear Text before installing Bars so no stale value can remain beside the compact glyph.
            if tray.set_title(Some("")).is_err() {
                crate::app_warn!("tray", "macOS menu bar title clear failed");
            }
            let icon = crate::menu_bar::bar_strip_icon(&bar_groups).unwrap_or_else(mark_icon);
            if tray.set_icon_with_as_template(Some(icon), true).is_err() {
                crate::app_warn!("tray", "macOS menu bar icon update failed");
            }
        }
    }
}

/// The metrics each group actually draws in the menu bar for the given style.
#[cfg(any(target_os = "macos", test))]
fn displayed_metrics(
    groups: &[TrayGroup],
    style: crate::models::MenuBarStyle,
) -> Vec<(&TrayGroup, Vec<&TrayMetric>)> {
    match style {
        crate::models::MenuBarStyle::Icon => Vec::new(),
        crate::models::MenuBarStyle::Text => text_metrics(groups),
        crate::models::MenuBarStyle::Bars => bar_metrics(groups),
    }
}

#[cfg(any(target_os = "macos", test))]
fn menu_bar_menu(
    groups: &[TrayGroup],
    settings: &AppSettings,
    registry: &ProviderRegistry,
) -> MenuBarMenu {
    MenuBarMenu {
        shown: menu_bar_items(groups, settings, registry),
        providers: menu_bar_providers(settings, registry),
    }
}

#[cfg(any(target_os = "macos", test))]
fn menu_bar_providers(settings: &AppSettings, registry: &ProviderRegistry) -> Vec<MenuBarProvider> {
    settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .filter_map(|provider| {
            let definition = registry.definition(&provider.id)?;
            let connection_configured = registry
                .catalog()
                .connection_config_provider_ids
                .contains(&provider.id);
            let upstream_provider_id = connection_configured
                .then(|| registry.upstream_provider_id(&provider.id))
                .flatten();
            let pins = provider
                .metrics
                .iter()
                .filter(|metric| metric.pinned)
                .count();
            let options = provider
                .metrics
                .iter()
                .filter_map(|metric| {
                    let metric_definition = registry.metric(&metric.id)?;
                    let supported = !connection_configured
                        || crate::providers::sub2api::metric_supported(
                            &provider.id,
                            &metric.id,
                            upstream_provider_id,
                        );
                    (metric_definition.pinnable && supported).then(|| MenuBarOption {
                        metric_id: metric.id.clone(),
                        label: metric_definition.label.clone(),
                        pinned: metric.pinned,
                        available: metric.pinned || pins < crate::settings::MAX_PINS_PER_PROVIDER,
                    })
                })
                .collect::<Vec<_>>();
            (!options.is_empty()).then(|| MenuBarProvider {
                id: provider.id.clone(),
                name: provider_name(registry, settings, definition),
                options,
            })
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn menu_bar_items(
    groups: &[TrayGroup],
    settings: &AppSettings,
    registry: &ProviderRegistry,
) -> Vec<MenuBarItem> {
    let mut items = Vec::<MenuBarItem>::new();
    for (group, metrics) in displayed_metrics(groups, settings.menu_bar_style) {
        for metric in metrics {
            // Multi-account providers draw one group per account but share a single pin.
            if items.iter().any(|item| item.metric_id == metric.metric_id) {
                continue;
            }
            let (Some(provider), Some(definition)) = (
                registry.definition(&group.provider_id),
                registry.metric(&metric.metric_id),
            ) else {
                continue;
            };
            items.push(MenuBarItem {
                metric_id: metric.metric_id.clone(),
                label: format!(
                    "{} · {}",
                    provider_name(registry, settings, provider),
                    definition.label
                ),
            });
        }
    }
    items
}

#[cfg(any(target_os = "macos", test))]
fn bar_metrics(groups: &[TrayGroup]) -> Vec<(&TrayGroup, Vec<&TrayMetric>)> {
    let mut provider_ids = Vec::new();
    groups
        .iter()
        .filter_map(|group| {
            let metrics = group
                .metrics
                .iter()
                .filter(|metric| metric.gauge.is_some())
                .take(crate::settings::MAX_PINS_PER_PROVIDER)
                .collect::<Vec<_>>();
            if metrics.is_empty() {
                return None;
            }
            if !provider_ids.contains(&group.provider_id.as_str()) {
                if provider_ids.len() == crate::menu_bar::MAX_BARS {
                    return None;
                }
                provider_ids.push(group.provider_id.as_str());
            }
            Some((group, metrics))
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn bar_groups(groups: &[TrayGroup]) -> Vec<crate::menu_bar::BarGroup> {
    bar_metrics(groups)
        .into_iter()
        .map(|(group, metrics)| crate::menu_bar::BarGroup {
            provider_id: group.provider_id.clone(),
            upstream_provider_id: group.upstream_provider_id.clone(),
            fractions: metrics
                .iter()
                .filter_map(|metric| metric.gauge.map(|gauge| gauge.display_fraction))
                .collect(),
        })
        .collect()
}

#[cfg(any(not(target_os = "macos"), test))]
fn primary_gauge(groups: &[TrayGroup]) -> Option<TrayGauge> {
    groups
        .iter()
        .flat_map(|group| group.metrics.iter())
        .find_map(|metric| metric.gauge)
}

#[cfg(any(target_os = "macos", test))]
fn text_metrics(groups: &[TrayGroup]) -> Vec<(&TrayGroup, Vec<&TrayMetric>)> {
    groups
        .iter()
        .map(|group| {
            (
                group,
                group
                    .metrics
                    .iter()
                    .take(crate::settings::MAX_PINS_PER_PROVIDER)
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|(_, metrics)| !metrics.is_empty())
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn text_groups(groups: &[TrayGroup]) -> Vec<crate::menu_bar::TextGroup> {
    text_metrics(groups)
        .into_iter()
        .map(|(group, metrics)| crate::menu_bar::TextGroup {
            provider_id: group.provider_id.clone(),
            upstream_provider_id: group.upstream_provider_id.clone(),
            values: metrics.iter().map(|metric| metric.value.clone()).collect(),
        })
        .collect()
}

fn provider_name(
    registry: &ProviderRegistry,
    settings: &AppSettings,
    definition: &ProviderDefinition,
) -> String {
    registry
        .display_name(&definition.id, settings)
        .unwrap_or_else(|| settings.provider_display_name(definition).to_owned())
}

fn resolved_groups(
    state: &UsageViewState,
    settings: &AppSettings,
    registry: &ProviderRegistry,
) -> Vec<TrayGroup> {
    settings
        .providers
        .iter()
        .filter(|provider| provider.enabled)
        .filter_map(|provider| {
            let definition = registry.definition(&provider.id)?;
            let snapshot = state
                .providers
                .get(&provider.id)
                .and_then(|state| state.snapshot.as_ref())?;
            let provider_name = provider_name(registry, settings, definition);
            let snapshots = match &snapshot.accounts {
                Some(accounts) => accounts
                    .iter()
                    .map(|account| {
                        let account_name = account
                            .name
                            .strip_prefix("Sub2API · Codex · ")
                            .or_else(|| account.name.strip_prefix("Sub2API · Claude · "))
                            .unwrap_or(&account.name);
                        (
                            &account.snapshot,
                            format!("{provider_name} · {account_name}"),
                        )
                    })
                    .collect::<Vec<_>>(),
                None => vec![(snapshot, provider_name)],
            };
            Some(
                snapshots
                    .into_iter()
                    .filter_map(|(snapshot, name)| {
                        let metrics = provider
                            .metrics
                            .iter()
                            .filter(|metric| metric.pinned)
                            .filter_map(|metric| {
                                let metric_definition = registry.metric(&metric.id)?;
                                let mut resolved = tray_metric(
                                    metric_definition,
                                    snapshot,
                                    settings.usage_display,
                                )?;
                                resolved.detail = format!("{} {}", name, resolved.detail);
                                Some(resolved)
                            })
                            .collect::<Vec<_>>();
                        (!metrics.is_empty()).then_some(TrayGroup {
                            #[cfg(any(target_os = "macos", test))]
                            provider_id: definition.id.clone(),
                            #[cfg(any(target_os = "macos", test))]
                            upstream_provider_id: registry
                                .upstream_provider_id(&definition.id)
                                .map(str::to_owned),
                            metrics,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

fn tray_metric(
    definition: &MetricDefinition,
    snapshot: &ProviderSnapshot,
    display: UsageDisplay,
) -> Option<TrayMetric> {
    let tray = definition.tray.as_ref()?;
    let quota = |id: &str| {
        snapshot
            .quotas
            .iter()
            .find(|quota| quota.id == id)
            .map(|quota| {
                if quota.format == QuotaFormat::Count {
                    if let (Some(used), Some(limit)) = (quota.used_value, quota.limit_value) {
                        let used_fraction = (limit > 0.0).then(|| (used / limit).clamp(0.0, 1.0));
                        let value = match display {
                            UsageDisplay::Used => used,
                            UsageDisplay::Left => (limit - used).max(0.0),
                        };
                        let word = match display {
                            UsageDisplay::Used => "used",
                            UsageDisplay::Left => "left",
                        };
                        let unit = quota.unit.as_deref().unwrap_or("requests");
                        return TrayMetric {
                            metric_id: definition.id.clone(),
                            value: format!("{value:.0}"),
                            detail: format!("{} {value:.0} {unit} {word}", quota.label),
                            gauge: used_fraction.map(|used_fraction| TrayGauge {
                                display_fraction: match display {
                                    UsageDisplay::Used => used_fraction,
                                    UsageDisplay::Left => 1.0 - used_fraction,
                                },
                                #[cfg(any(not(target_os = "macos"), test))]
                                remaining_fraction: 1.0 - used_fraction,
                            }),
                        };
                    }
                }
                let used_fraction = (quota.used_percent / 100.0).clamp(0.0, 1.0);
                let display_fraction = match display {
                    UsageDisplay::Used => used_fraction,
                    UsageDisplay::Left => 1.0 - used_fraction,
                };
                let percent = display_fraction * 100.0;
                let word = match display {
                    UsageDisplay::Used => "used",
                    UsageDisplay::Left => "left",
                };
                TrayMetric {
                    metric_id: definition.id.clone(),
                    value: format!("{percent:.0}%"),
                    detail: format!("{} {percent:.0}% {word}", quota.label),
                    gauge: Some(TrayGauge {
                        display_fraction,
                        #[cfg(any(not(target_os = "macos"), test))]
                        remaining_fraction: 1.0 - used_fraction,
                    }),
                }
            })
    };
    match &definition.source {
        MetricSource::Quota { source_id, .. } => quota(source_id),
        MetricSource::QuotaOrValue { source_id, .. } => quota(source_id)
            .or_else(|| value_metric(definition, snapshot, source_id, tray.suffix.as_deref())),
        MetricSource::Value { source_id } => {
            value_metric(definition, snapshot, source_id, tray.suffix.as_deref())
        }
        MetricSource::Status { source_id } => status_metric(definition, snapshot, source_id),
        MetricSource::Usage { period } => usage_metric(definition, usage_period(snapshot, *period)),
        MetricSource::Trend => None,
    }
}

fn usage_period(snapshot: &ProviderSnapshot, period: UsagePeriodSelection) -> Option<&UsagePeriod> {
    let history = snapshot
        .usage_histories
        .local_device
        .as_ref()
        .or(snapshot.usage_histories.account.as_ref())?;
    match period {
        UsagePeriodSelection::Today => history.today.as_ref(),
        UsagePeriodSelection::Yesterday => history.yesterday.as_ref(),
        UsagePeriodSelection::Last30Days => history.last_30_days.as_ref(),
    }
}

fn status_metric(
    definition: &MetricDefinition,
    snapshot: &ProviderSnapshot,
    source_id: &str,
) -> Option<TrayMetric> {
    let metric = snapshot
        .status_metrics
        .iter()
        .find(|metric| metric.id == source_id)?;
    Some(TrayMetric {
        metric_id: definition.id.clone(),
        value: metric.text.clone(),
        detail: format!("{} {}", metric.label, metric.text),
        gauge: None,
    })
}

fn value_metric(
    definition: &MetricDefinition,
    snapshot: &ProviderSnapshot,
    source_id: &str,
    tray_suffix: Option<&str>,
) -> Option<TrayMetric> {
    let metric = snapshot
        .value_metrics
        .iter()
        .find(|metric| metric.id == source_id)?;
    let value = metric
        .values
        .iter()
        .map(format_tray_value)
        .collect::<Vec<_>>()
        .join(" · ");
    let detail = metric
        .values
        .iter()
        .map(format_detail_value)
        .collect::<Vec<_>>()
        .join(" · ");
    let value = tray_suffix
        .map(|suffix| format!("{value} {suffix}"))
        .unwrap_or(value);
    Some(TrayMetric {
        metric_id: definition.id.clone(),
        value,
        detail: format!("{} {detail}", metric.label),
        gauge: None,
    })
}

fn format_tray_value(value: &MetricValue) -> String {
    let number = match value.kind {
        MetricValueKind::Dollars => format!("${:.0}", value.number),
        MetricValueKind::Count => format_tokens(value.number.max(0.0) as u64),
    };
    value
        .label
        .as_deref()
        .map(|label| format!("{number} {label}"))
        .unwrap_or(number)
}

fn format_detail_value(value: &MetricValue) -> String {
    let number = match value.kind {
        MetricValueKind::Dollars => format!("${:.2}", value.number),
        MetricValueKind::Count => format!("{:.0}", value.number),
    };
    value
        .label
        .as_deref()
        .map(|label| format!("{number} {label}"))
        .unwrap_or(number)
}

fn usage_metric(definition: &MetricDefinition, period: Option<&UsagePeriod>) -> Option<TrayMetric> {
    let period = period?;
    let value = period
        .estimated_cost_usd
        .map(|value| format!("${value:.2}"))
        .unwrap_or_else(|| format_tokens(period.tokens));
    let detail = format!("{} {value}", definition.label);
    Some(TrayMetric {
        metric_id: definition.id.clone(),
        value,
        detail,
        gauge: None,
    })
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn mark_icon() -> Image<'static> {
    Image::from_bytes(include_bytes!("../icons/32x32.png"))
        .expect("bundled OpenQuota tray mark must be a valid PNG")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use chrono::Utc;

    use crate::{
        models::{
            MetricDefinition, MetricSection, MetricValue, MetricValueKind, ProviderDefinition,
            ProviderSnapshot, ProviderViewState, QuotaWindow, SnapshotSource, StatusMetric,
            StatusTone, ValueMetric,
        },
        providers::{codex, cursor, ProviderError, ProviderRegistry, UsageProvider},
        settings::default_settings,
    };

    use super::{
        bar_groups, displayed_metrics, format_tokens, mac_menu_bar_presentation, menu_bar_items,
        menu_bar_providers, primary_gauge, resolved_groups, text_groups, MacMenuBarIcon,
        MacMenuBarPresentation, MenuBarItem, TrayGauge, TrayGroup, TrayMetric,
    };
    use crate::service::UsageViewState;

    struct NamedProvider {
        definition: ProviderDefinition,
        name: String,
    }

    impl UsageProvider for NamedProvider {
        fn definition(&self) -> ProviderDefinition {
            self.definition.clone()
        }

        fn has_local_credentials(&self) -> bool {
            true
        }

        fn display_name(&self) -> String {
            self.name.clone()
        }

        fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
            unreachable!()
        }
    }

    #[test]
    fn bundled_tray_mark_decodes_at_the_expected_size() {
        let image = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))
            .expect("bundled tray mark should decode");
        assert_eq!((image.width(), image.height()), (32, 32));
    }

    #[test]
    fn text_groups_keep_provider_identity_and_values_without_choosing_a_primary_metric() {
        let metric = |value: &str| TrayMetric {
            metric_id: String::new(),
            value: value.into(),
            detail: value.into(),
            gauge: None,
        };
        let groups = vec![
            TrayGroup {
                provider_id: "claude".into(),
                upstream_provider_id: None,
                metrics: vec![metric("75%"), metric("40%")],
            },
            TrayGroup {
                provider_id: "codex".into(),
                upstream_provider_id: None,
                metrics: vec![metric("90%")],
            },
            TrayGroup {
                provider_id: "sub2api@2".into(),
                upstream_provider_id: Some("claude".into()),
                metrics: vec![metric("65%")],
            },
        ];

        assert_eq!(
            text_groups(&groups),
            vec![
                crate::menu_bar::TextGroup {
                    provider_id: "claude".into(),
                    upstream_provider_id: None,
                    values: vec!["75%".into(), "40%".into()],
                },
                crate::menu_bar::TextGroup {
                    provider_id: "codex".into(),
                    upstream_provider_id: None,
                    values: vec!["90%".into()],
                },
                crate::menu_bar::TextGroup {
                    provider_id: "sub2api@2".into(),
                    upstream_provider_id: Some("claude".into()),
                    values: vec!["65%".into()],
                },
            ]
        );
        assert!(text_groups(&[]).is_empty());
    }

    #[test]
    fn mac_text_to_bars_transition_explicitly_clears_the_native_title() {
        let groups = vec![TrayGroup {
            provider_id: "codex".into(),
            upstream_provider_id: None,
            metrics: vec![TrayMetric {
                metric_id: String::new(),
                value: "75%".into(),
                detail: String::new(),
                gauge: Some(TrayGauge {
                    display_fraction: 0.75,
                    remaining_fraction: 0.75,
                }),
            }],
        }];

        assert_eq!(
            mac_menu_bar_presentation(&groups, crate::models::MenuBarStyle::Text),
            MacMenuBarPresentation {
                icon: MacMenuBarIcon::Text(vec![crate::menu_bar::TextGroup {
                    provider_id: "codex".into(),
                    upstream_provider_id: None,
                    values: vec!["75%".into()],
                }]),
            }
        );
        assert_eq!(
            mac_menu_bar_presentation(&groups, crate::models::MenuBarStyle::Bars),
            MacMenuBarPresentation {
                icon: MacMenuBarIcon::Bars(vec![crate::menu_bar::BarGroup {
                    provider_id: "codex".into(),
                    upstream_provider_id: None,
                    fractions: vec![0.75],
                }]),
            }
        );
    }

    #[test]
    fn mac_icon_style_always_uses_the_openquota_mark() {
        let groups = vec![TrayGroup {
            provider_id: "codex".into(),
            upstream_provider_id: None,
            metrics: vec![TrayMetric {
                metric_id: String::new(),
                value: "75%".into(),
                detail: String::new(),
                gauge: Some(TrayGauge {
                    display_fraction: 0.75,
                    remaining_fraction: 0.75,
                }),
            }],
        }];
        let mark = MacMenuBarPresentation {
            icon: MacMenuBarIcon::Mark,
        };

        assert_eq!(
            mac_menu_bar_presentation(&groups, crate::models::MenuBarStyle::Icon),
            mark
        );
        assert_eq!(
            mac_menu_bar_presentation(&[], crate::models::MenuBarStyle::Icon),
            mark
        );
    }

    #[test]
    fn mac_empty_and_unbounded_bar_states_fall_back_without_stale_text() {
        let unbounded = vec![TrayGroup {
            provider_id: "codex".into(),
            upstream_provider_id: None,
            metrics: vec![TrayMetric {
                metric_id: String::new(),
                value: "$4".into(),
                detail: String::new(),
                gauge: None,
            }],
        }];
        let fallback = MacMenuBarPresentation {
            icon: MacMenuBarIcon::Mark,
        };

        assert_eq!(
            mac_menu_bar_presentation(&[], crate::models::MenuBarStyle::Text),
            fallback
        );
        assert_eq!(
            mac_menu_bar_presentation(&[], crate::models::MenuBarStyle::Bars),
            fallback
        );
        assert_eq!(
            mac_menu_bar_presentation(&unbounded, crate::models::MenuBarStyle::Bars),
            fallback
        );
    }

    #[test]
    fn bars_keep_pinned_quota_metrics_grouped_by_provider() {
        let metric = |value: f64| TrayMetric {
            metric_id: String::new(),
            value: format!("{value:.0}%"),
            detail: String::new(),
            gauge: Some(TrayGauge {
                display_fraction: value / 100.0,
                remaining_fraction: value / 100.0,
            }),
        };
        let groups = vec![
            TrayGroup {
                provider_id: "claude".into(),
                upstream_provider_id: None,
                metrics: vec![
                    metric(10.0),
                    TrayMetric {
                        metric_id: String::new(),
                        value: "$4".into(),
                        detail: String::new(),
                        gauge: None,
                    },
                    metric(20.0),
                ],
            },
            TrayGroup {
                provider_id: "codex".into(),
                upstream_provider_id: None,
                metrics: vec![metric(30.0), metric(40.0), metric(50.0)],
            },
        ];

        assert_eq!(
            bar_groups(&groups),
            vec![
                crate::menu_bar::BarGroup {
                    provider_id: "claude".into(),
                    upstream_provider_id: None,
                    fractions: vec![0.1, 0.2],
                },
                crate::menu_bar::BarGroup {
                    provider_id: "codex".into(),
                    upstream_provider_id: None,
                    fractions: vec![0.3, 0.4],
                },
            ]
        );
    }

    #[test]
    fn bar_limit_counts_connections_not_upstream_accounts() {
        let groups = [
            "sub2api", "sub2api", "sub2api", "sub2api", "sub2api", "codex", "claude", "cursor",
            "grok",
        ]
        .into_iter()
        .map(|id| TrayGroup {
            provider_id: id.into(),
            upstream_provider_id: None,
            metrics: vec![TrayMetric {
                metric_id: String::new(),
                value: "50%".into(),
                detail: String::new(),
                gauge: Some(TrayGauge {
                    display_fraction: 0.5,
                    remaining_fraction: 0.5,
                }),
            }],
        })
        .collect::<Vec<_>>();
        assert_eq!(
            bar_groups(&groups)
                .iter()
                .map(|group| group.provider_id.as_str())
                .collect::<Vec<_>>(),
            ["sub2api", "sub2api", "sub2api", "sub2api", "sub2api", "codex", "claude", "cursor"]
        );
    }

    #[test]
    fn displayed_metrics_follow_what_each_style_draws() {
        let metric = |id: &str, gauge: bool| TrayMetric {
            metric_id: id.into(),
            value: String::new(),
            detail: String::new(),
            gauge: gauge.then_some(TrayGauge {
                display_fraction: 0.5,
                remaining_fraction: 0.5,
            }),
        };
        let groups = vec![TrayGroup {
            provider_id: "codex".into(),
            upstream_provider_id: None,
            metrics: vec![
                metric("codex.credits", false),
                metric("codex.session", true),
            ],
        }];
        let ids = |style| {
            displayed_metrics(&groups, style)
                .into_iter()
                .flat_map(|(_, metrics)| metrics)
                .map(|metric| metric.metric_id.as_str())
                .collect::<Vec<_>>()
        };

        assert!(ids(crate::models::MenuBarStyle::Icon).is_empty());
        assert_eq!(
            ids(crate::models::MenuBarStyle::Text),
            ["codex.credits", "codex.session"]
        );
        assert_eq!(ids(crate::models::MenuBarStyle::Bars), ["codex.session"]);
    }

    #[test]
    fn menu_bar_items_list_each_displayed_pin_once_and_drop_unpinned_metrics() {
        let quota = |id: &str, label: &str| QuotaWindow {
            id: id.into(),
            label: label.into(),
            used_percent: 25.0,
            resets_at: None,
            period_seconds: 18_000,
            format: crate::models::QuotaFormat::Percent,
            used_value: None,
            limit_value: None,
            unit: None,
            estimated: false,
            source_note: None,
        };
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: "codex".into(),
            plan: None,
            quotas: vec![quota("session", "Session"), quota("weekly", "Weekly")],
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        };
        let mut multi_account = snapshot.clone();
        multi_account.accounts = Some(
            (0..3)
                .map(|index| crate::models::AccountSnapshot {
                    id: index.to_string(),
                    name: format!("Account {index}"),
                    snapshot: snapshot.clone(),
                })
                .collect(),
        );
        let catalog = ProviderRegistry::from_definitions(vec![codex::definition()]).unwrap();
        let mut settings = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
        settings
            .provider_names
            .insert("codex".into(), "Work Codex".into());
        let item = |metric_id: &str, label: &str| MenuBarItem {
            metric_id: metric_id.into(),
            label: label.into(),
        };

        for snapshot in [snapshot, multi_account] {
            let state = UsageViewState {
                providers: [(
                    "codex".into(),
                    ProviderViewState {
                        snapshot: Some(snapshot),
                        source: SnapshotSource::Live,
                        ..ProviderViewState::default()
                    },
                )]
                .into_iter()
                .collect(),
                last_full_refresh_at: None,
                refresh_interval_seconds: crate::policy::REFRESH_INTERVAL.as_secs(),
            };

            settings.menu_bar_style = crate::models::MenuBarStyle::Icon;
            let groups = resolved_groups(&state, &settings, &catalog);
            assert!(menu_bar_items(&groups, &settings, &catalog).is_empty());

            for style in [
                crate::models::MenuBarStyle::Text,
                crate::models::MenuBarStyle::Bars,
            ] {
                settings.menu_bar_style = style;
                assert_eq!(
                    menu_bar_items(&groups, &settings, &catalog),
                    vec![
                        item("codex.session", "Work Codex · Session"),
                        item("codex.weekly", "Work Codex · Weekly"),
                    ]
                );
            }

            let mut hidden = settings.clone();
            hidden.providers[0]
                .metrics
                .iter_mut()
                .find(|metric| metric.id == "codex.session")
                .unwrap()
                .pinned = false;
            let groups = resolved_groups(&state, &hidden, &catalog);
            assert_eq!(
                menu_bar_items(&groups, &hidden, &catalog),
                vec![item("codex.weekly", "Work Codex · Weekly")]
            );
        }
    }

    #[test]
    fn add_menu_offers_pinnable_metrics_of_enabled_providers_within_the_pin_cap() {
        let catalog =
            ProviderRegistry::from_definitions(vec![codex::definition(), cursor::definition()])
                .unwrap();
        let mut settings = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
        settings
            .provider_names
            .insert("codex".into(), "Work Codex".into());
        let pinnable = codex::definition()
            .metrics
            .into_iter()
            .filter(|metric| metric.pinnable)
            .map(|metric| metric.id)
            .collect::<Vec<_>>();

        let providers = menu_bar_providers(&settings, &catalog);
        assert_eq!(providers.len(), 1, "disabled providers are not offered");
        assert_eq!(providers[0].id, "codex");
        assert_eq!(providers[0].name, "Work Codex");
        assert_eq!(
            providers[0]
                .options
                .iter()
                .map(|option| option.metric_id.clone())
                .collect::<Vec<_>>(),
            pinnable
        );
        let session = &providers[0].options[0];
        assert_eq!(session.metric_id, "codex.session");
        assert_eq!(session.label, "Session");
        assert!(session.pinned && session.available);
        assert!(providers[0]
            .options
            .iter()
            .filter(|option| !option.pinned)
            .all(|option| !option.available));

        settings.providers[0]
            .metrics
            .iter_mut()
            .find(|metric| metric.id == "codex.session")
            .unwrap()
            .pinned = false;
        let providers = menu_bar_providers(&settings, &catalog);
        assert!(providers[0].options.iter().all(|option| option.available));
        assert!(!providers[0].options[0].pinned);
    }

    #[test]
    fn pinned_quota_metrics_resolve_in_layout_order() {
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: "codex".into(),
            plan: None,
            quotas: vec![
                QuotaWindow {
                    id: "session".into(),
                    label: "Session".into(),
                    used_percent: 25.0,
                    resets_at: None,
                    period_seconds: 18_000,
                    format: crate::models::QuotaFormat::Percent,
                    used_value: None,
                    limit_value: None,
                    unit: None,
                    estimated: false,
                    source_note: None,
                },
                QuotaWindow {
                    id: "weekly".into(),
                    label: "Weekly".into(),
                    used_percent: 60.0,
                    resets_at: None,
                    period_seconds: 604_800,
                    format: crate::models::QuotaFormat::Percent,
                    used_value: None,
                    limit_value: None,
                    unit: None,
                    estimated: false,
                    source_note: None,
                },
            ],
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        };
        let provider_state = ProviderViewState {
            snapshot: Some(snapshot),
            source: SnapshotSource::Live,
            ..ProviderViewState::default()
        };
        let state = UsageViewState {
            providers: [("codex".into(), provider_state)].into_iter().collect(),
            last_full_refresh_at: None,
            refresh_interval_seconds: crate::policy::REFRESH_INTERVAL.as_secs(),
        };
        let catalog = ProviderRegistry::from_definitions(vec![codex::definition()]).unwrap();
        let groups = resolved_groups(
            &state,
            &default_settings(&catalog, &HashSet::from(["codex".to_owned()])),
            &catalog,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].provider_id, "codex");
        assert_eq!(groups[0].metrics.len(), 2);
        assert_eq!(groups[0].metrics[0].value, "75%");
        assert_eq!(groups[0].metrics[1].value, "40%");
        assert_eq!(
            text_groups(&groups),
            vec![crate::menu_bar::TextGroup {
                provider_id: "codex".into(),
                upstream_provider_id: None,
                values: vec!["75%".into(), "40%".into()],
            }]
        );
        assert_eq!(
            groups[0].metrics[0].gauge,
            Some(TrayGauge {
                display_fraction: 0.75,
                remaining_fraction: 0.75,
            })
        );

        let mut dashboard_hidden = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
        dashboard_hidden.providers[0].metrics[0].enabled = false;
        let hidden_groups = resolved_groups(&state, &dashboard_hidden, &catalog);
        assert_eq!(hidden_groups[0].metrics[0].value, "75%");

        let mut used_settings = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
        used_settings.usage_display = crate::models::UsageDisplay::Used;
        let used_groups = resolved_groups(&state, &used_settings, &catalog);
        assert_eq!(
            used_groups[0].metrics[0].gauge,
            Some(TrayGauge {
                display_fraction: 0.25,
                remaining_fraction: 0.75,
            })
        );
        assert_eq!(used_groups[0].metrics[0].value, "25%");
        assert_eq!(
            bar_groups(&groups),
            vec![crate::menu_bar::BarGroup {
                provider_id: "codex".into(),
                upstream_provider_id: None,
                fractions: vec![0.75, 0.4],
            }]
        );
        assert_eq!(
            bar_groups(&used_groups),
            vec![crate::menu_bar::BarGroup {
                provider_id: "codex".into(),
                upstream_provider_id: None,
                fractions: vec![0.25, 0.6],
            }]
        );

        let template = state.providers["codex"].snapshot.as_ref().unwrap().clone();
        let mut settings = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
        settings
            .provider_names
            .insert("codex".into(), "Configured name".into());
        let mut previous_text_width = 0;
        let mut previous_bar_width = 0;
        for count in [0, 1, 5] {
            let mut state = state.clone();
            state
                .providers
                .get_mut("codex")
                .unwrap()
                .snapshot
                .as_mut()
                .unwrap()
                .accounts = Some(
                (0..count)
                    .map(|index| {
                        let mut snapshot = template.clone();
                        snapshot.quotas[0].used_percent = (index * 10) as f64;
                        crate::models::AccountSnapshot {
                            id: index.to_string(),
                            name: "Sub2API · Codex · Same upstream name".into(),
                            snapshot,
                        }
                    })
                    .collect(),
            );
            let groups = resolved_groups(&state, &settings, &catalog);
            assert_eq!(groups.len(), count);
            assert_eq!(text_groups(&groups).len(), count);
            assert_eq!(bar_groups(&groups).len(), count);
            for (index, group) in groups.iter().enumerate() {
                assert_eq!(group.metrics.len(), 2);
                assert_eq!(group.metrics[0].value, format!("{}%", 100 - index * 10));
                assert!(group.metrics[0]
                    .detail
                    .starts_with("Configured name · Same upstream name Session"));
                assert_eq!(group.metrics[1].value, "40%");
                assert_eq!(
                    bar_groups(std::slice::from_ref(group))[0].fractions[0],
                    1.0 - index as f64 / 10.0
                );
            }
            if count == 0 {
                for style in [
                    crate::models::MenuBarStyle::Text,
                    crate::models::MenuBarStyle::Bars,
                ] {
                    assert_eq!(
                        mac_menu_bar_presentation(&groups, style).icon,
                        MacMenuBarIcon::Mark
                    );
                }
            } else {
                let text = crate::menu_bar::text_icon(&text_groups(&groups)).unwrap();
                let bars = crate::menu_bar::bar_strip_icon(&bar_groups(&groups)).unwrap();
                assert!(text.width() > previous_text_width);
                assert!(bars.width() > previous_bar_width);
                previous_text_width = text.width();
                previous_bar_width = bars.width();
            }
        }
    }

    #[test]
    fn tray_details_use_the_runtime_provider_name() {
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: "codex".into(),
            plan: None,
            quotas: vec![QuotaWindow {
                id: "session".into(),
                label: "Session".into(),
                used_percent: 25.0,
                resets_at: None,
                period_seconds: 18_000,
                format: crate::models::QuotaFormat::Percent,
                used_value: None,
                limit_value: None,
                unit: None,
                estimated: false,
                source_note: None,
            }],
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        };
        let registry = ProviderRegistry::new(vec![Arc::new(NamedProvider {
            definition: codex::definition(),
            name: "Actual Codex".into(),
        })])
        .unwrap();
        let settings = default_settings(&registry, &HashSet::from(["codex".to_owned()]));
        let state = UsageViewState {
            providers: [(
                "codex".into(),
                ProviderViewState {
                    snapshot: Some(snapshot),
                    source: SnapshotSource::Live,
                    ..ProviderViewState::default()
                },
            )]
            .into_iter()
            .collect(),
            last_full_refresh_at: None,
            refresh_interval_seconds: crate::policy::REFRESH_INTERVAL.as_secs(),
        };

        let groups = resolved_groups(&state, &settings, &registry);

        assert_eq!(groups[0].metrics[0].detail, "Actual Codex Session 75% left");
    }

    #[test]
    fn count_quota_display_changes_text_and_fill_but_not_status_fraction() {
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: "cursor".into(),
            plan: None,
            quotas: vec![QuotaWindow {
                id: "requests".into(),
                label: "Requests".into(),
                used_percent: 25.0,
                resets_at: None,
                period_seconds: 2_592_000,
                format: crate::models::QuotaFormat::Count,
                used_value: Some(25.0),
                limit_value: Some(100.0),
                unit: Some("searches".into()),
                estimated: false,
                source_note: None,
            }],
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        };
        let catalog = ProviderRegistry::from_definitions(vec![cursor::definition()]).unwrap();
        let definition = catalog.metric("cursor.requests").unwrap();

        let left =
            super::tray_metric(definition, &snapshot, crate::models::UsageDisplay::Left).unwrap();
        let used =
            super::tray_metric(definition, &snapshot, crate::models::UsageDisplay::Used).unwrap();

        assert_eq!(left.value, "75");
        assert_eq!(left.detail, "Requests 75 searches left");
        assert_eq!(used.value, "25");
        assert_eq!(used.detail, "Requests 25 searches used");
        assert_eq!(
            left.gauge,
            Some(TrayGauge {
                display_fraction: 0.75,
                remaining_fraction: 0.75,
            })
        );
        assert_eq!(
            used.gauge,
            Some(TrayGauge {
                display_fraction: 0.25,
                remaining_fraction: 0.75,
            })
        );
    }

    #[test]
    fn pinned_metrics_without_a_snapshot_do_not_leave_placeholder_content() {
        let catalog = ProviderRegistry::from_definitions(vec![codex::definition()]).unwrap();
        let settings = default_settings(&catalog, &HashSet::from(["codex".to_owned()]));
        assert!(resolved_groups(&UsageViewState::default(), &settings, &catalog).is_empty());
    }

    #[test]
    fn token_fallback_stays_compact() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_340), "12.3K");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn gauge_uses_the_first_pinned_quota_metric() {
        let groups = vec![TrayGroup {
            provider_id: "codex".into(),
            upstream_provider_id: None,
            metrics: vec![
                TrayMetric {
                    metric_id: String::new(),
                    value: "10".into(),
                    detail: "Credits 10".into(),
                    gauge: None,
                },
                TrayMetric {
                    metric_id: String::new(),
                    value: "40%".into(),
                    detail: "Session 40% left".into(),
                    gauge: Some(TrayGauge {
                        display_fraction: 0.4,
                        remaining_fraction: 0.4,
                    }),
                },
                TrayMetric {
                    metric_id: String::new(),
                    value: "80%".into(),
                    detail: "Weekly 80% left".into(),
                    gauge: Some(TrayGauge {
                        display_fraction: 0.8,
                        remaining_fraction: 0.8,
                    }),
                },
            ],
        }];
        assert_eq!(
            primary_gauge(&groups),
            Some(TrayGauge {
                display_fraction: 0.4,
                remaining_fraction: 0.4,
            })
        );
    }

    #[test]
    fn pinned_value_metrics_keep_numeric_values_outside_quota_bars() {
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: "codex".into(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: vec![ValueMetric {
                id: "credits".into(),
                label: "Extra Usage".into(),
                values: vec![
                    MetricValue {
                        number: 32.84,
                        kind: MetricValueKind::Dollars,
                        label: None,
                        estimated: true,
                    },
                    MetricValue {
                        number: 821.0,
                        kind: MetricValueKind::Count,
                        label: Some("credits".into()),
                        estimated: false,
                    },
                ],
                expiries_at: Vec::new(),
            }],
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        };
        let catalog = ProviderRegistry::from_definitions(vec![codex::definition()]).unwrap();
        let metric = super::tray_metric(
            catalog.metric("codex.credits").unwrap(),
            &snapshot,
            crate::models::UsageDisplay::Left,
        )
        .unwrap();
        assert_eq!(metric.value, "$33 · 821 credits");
        assert_eq!(metric.detail, "Extra Usage $32.84 · 821 credits");
        assert_eq!(metric.gauge, None);
    }

    #[test]
    fn pinned_status_metrics_keep_text_and_never_create_a_gauge() {
        let snapshot = ProviderSnapshot {
            accounts: None,
            provider_id: "grok".into(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: vec![StatusMetric {
                id: "payAsYouGo".into(),
                label: "Extra Usage".into(),
                text: "2500 cap".into(),
                tone: StatusTone::Positive,
                subtitle: None,
            }],
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        };
        let definition = MetricDefinition::status(
            "grok.payAsYouGo",
            "Extra Usage",
            "payAsYouGo",
            true,
            MetricSection::AlwaysVisible,
            true,
            "E",
        );

        let metric =
            super::tray_metric(&definition, &snapshot, crate::models::UsageDisplay::Left).unwrap();

        assert_eq!(metric.value, "2500 cap");
        assert_eq!(metric.detail, "Extra Usage 2500 cap");
        assert_eq!(metric.gauge, None);
        assert!(bar_groups(&[TrayGroup {
            provider_id: "grok".into(),
            upstream_provider_id: None,
            metrics: vec![metric],
        }])
        .is_empty());
    }
}
