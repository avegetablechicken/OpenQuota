use chrono::Utc;

use crate::models::{
    MetricDefinition, MetricLayout, MetricSection, ProviderDefinition, QuotaWindow,
    UsagePeriodSelection,
};

use super::config::{StoredConfig, Sub2ApiMode, Sub2ApiUpstream, METRIC_TEMPLATE_VERSION};

/// A connection's definition, for tests that need one without its credential store.
#[cfg(test)]
pub(crate) fn test_definition(provider_id: &str) -> ProviderDefinition {
    definition_for(provider_id, "Sub2API")
}

pub(super) fn definition_for(provider_id: &str, display_name: &str) -> ProviderDefinition {
    let metric_id = |suffix: &str| format!("{provider_id}.{suffix}");
    ProviderDefinition {
        id: provider_id.into(),
        display_name: display_name.into(),
        short_name: "S2".into(),
        fallback_enabled: false,
        local_usage_source_note: None,
        links: Vec::new(),
        metrics: vec![
            MetricDefinition::quota(
                &metric_id("session"),
                "Session",
                "session",
                true,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("weekly"),
                "Weekly",
                "weekly",
                true,
                MetricSection::AlwaysVisible,
                true,
                "W",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("daily"),
                "Daily",
                "daily",
                false,
                MetricSection::OnDemand,
                false,
                "D",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("monthly"),
                "Monthly",
                "monthly",
                false,
                MetricSection::OnDemand,
                false,
                "M",
            ),
            MetricDefinition::quota(
                &metric_id("sonnet"),
                "Sonnet",
                "sonnet",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "Sn",
            ),
            MetricDefinition::quota(
                &metric_id("fable"),
                "Fable",
                "fable",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "F",
            ),
            MetricDefinition::quota(
                &metric_id("claude"),
                "Claude",
                "claude",
                true,
                false,
                MetricSection::OnDemand,
                false,
                "C",
            ),
            MetricDefinition::quota(
                &metric_id("claudeWeekly"),
                "Claude Weekly",
                "claudeWeekly",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "CW",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("extra"),
                "Extra Usage",
                "extra",
                false,
                MetricSection::OnDemand,
                false,
                "E",
            ),
            MetricDefinition::trend(&metric_id("trend")),
            MetricDefinition::quota(
                &metric_id("spark"),
                "Spark",
                "spark",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "Sp",
            ),
            MetricDefinition::quota(
                &metric_id("sparkWeekly"),
                "Spark Weekly",
                "sparkWeekly",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "SW",
            ),
            MetricDefinition::value(
                &metric_id("rateLimitResets"),
                "Rate Limit Resets",
                "sub2apiRateLimitResets",
                true,
                MetricSection::OnDemand,
                false,
                "R",
                Some("resets"),
            ),
            MetricDefinition::usage(
                &metric_id("today"),
                "Today",
                UsagePeriodSelection::Today,
                MetricSection::OnDemand,
                "T",
            ),
            MetricDefinition::usage(
                &metric_id("yesterday"),
                "Yesterday",
                UsagePeriodSelection::Yesterday,
                MetricSection::OnDemand,
                "Y",
            ),
            MetricDefinition::usage(
                &metric_id("last30"),
                "Last 30 Days",
                UsagePeriodSelection::Last30Days,
                MetricSection::OnDemand,
                "M",
            ),
        ],
    }
}

/// The upstream id a Sub2API track reports, as passed to [`track_shows_metric`].
pub fn track_provider_id(mode: Sub2ApiMode, upstream: Sub2ApiUpstream) -> &'static str {
    match mode {
        Sub2ApiMode::Subscription => "subscription",
        Sub2ApiMode::Admin => upstream.provider_id(),
    }
}

/// The track a [`track_provider_id`] names. Subscription templates do not depend on the upstream.
pub(super) fn track_for_id(track: &str) -> Option<(Sub2ApiMode, Sub2ApiUpstream)> {
    [
        (Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex),
        (Sub2ApiMode::Admin, Sub2ApiUpstream::Codex),
        (Sub2ApiMode::Admin, Sub2ApiUpstream::Claude),
        (Sub2ApiMode::Admin, Sub2ApiUpstream::Antigravity),
        (Sub2ApiMode::Admin, Sub2ApiUpstream::Grok),
    ]
    .into_iter()
    .find(|(mode, upstream)| track_provider_id(*mode, *upstream) == track)
}

/// The default metric layout of the track a [`track_provider_id`] names.
pub fn metric_template_for_track(provider_id: &str, track: &str) -> Option<Vec<MetricLayout>> {
    track_for_id(track).map(|(mode, upstream)| metric_template(provider_id, upstream, mode))
}

/// Whether a connection's metric can report data on `track`. `None` is a Track not yet read.
///
/// Only [`crate::metric_policy`] asks this; everything else asks the policy.
pub(crate) fn track_shows_metric(
    provider_id: &str,
    metric_id: &str,
    upstream_provider_id: Option<&str>,
) -> bool {
    let Some(suffix) = metric_id
        .strip_prefix(provider_id)
        .and_then(|rest| rest.strip_prefix('.'))
    else {
        return true;
    };
    if upstream_provider_id == Some("subscription") {
        return matches!(
            suffix,
            "daily" | "weekly" | "monthly" | "today" | "yesterday" | "last30" | "trend"
        );
    }
    let upstream_only: &[&str] = match upstream_provider_id {
        Some("codex") => &["session", "spark", "sparkWeekly", "rateLimitResets"],
        Some("claude") => &["session", "sonnet", "fable"],
        Some("antigravity") => &["session", "claude", "claudeWeekly"],
        Some("grok") => &["monthly"],
        _ => {
            return !matches!(
                suffix,
                "daily" | "monthly" | "extra" | "claude" | "claudeWeekly"
            )
        }
    };
    matches!(
        suffix,
        "weekly" | "trend" | "today" | "yesterday" | "last30"
    ) || upstream_only.contains(&suffix)
}

/// Whether a subscription reports this metric as a percentage of a limit: daily, weekly, monthly.
pub(crate) fn subscription_percentage_metric(metric_id: &str) -> bool {
    matches!(
        metric_id.rsplit('.').next(),
        Some("daily" | "weekly" | "monthly")
    )
}

pub fn metric_template(
    provider_id: &str,
    upstream: Sub2ApiUpstream,
    mode: Sub2ApiMode,
) -> Vec<MetricLayout> {
    let metric = |suffix: &str, enabled: bool, section: MetricSection, pinned: bool| MetricLayout {
        id: format!("{provider_id}.{suffix}"),
        enabled,
        section,
        pinned,
    };
    let always = MetricSection::AlwaysVisible;
    let demand = MetricSection::OnDemand;
    if mode == Sub2ApiMode::Subscription {
        let mut template = vec![
            metric("daily", true, always, true),
            metric("weekly", true, always, true),
            metric("monthly", true, always, false),
            metric("trend", true, always, false),
            metric("today", true, demand, false),
            metric("yesterday", true, demand, false),
            metric("last30", true, demand, false),
        ];
        for suffix in [
            "session",
            "sonnet",
            "fable",
            "claude",
            "claudeWeekly",
            "extra",
            "spark",
            "sparkWeekly",
            "rateLimitResets",
        ] {
            template.push(metric(suffix, false, demand, false));
        }
        return template;
    }
    let (shown, hidden): (Vec<MetricLayout>, &[&str]) = match upstream {
        Sub2ApiUpstream::Claude => (
            vec![
                metric("session", true, always, true),
                metric("weekly", true, always, true),
                metric("trend", true, always, false),
                metric("sonnet", false, demand, false),
                metric("fable", false, demand, false),
                metric("extra", false, demand, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "monthly",
                "spark",
                "sparkWeekly",
                "rateLimitResets",
                "claude",
                "claudeWeekly",
            ],
        ),
        Sub2ApiUpstream::Codex => (
            vec![
                metric("session", true, always, true),
                metric("weekly", true, always, true),
                metric("trend", true, always, false),
                metric("spark", true, demand, false),
                metric("sparkWeekly", true, demand, false),
                metric("rateLimitResets", true, demand, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "monthly",
                "sonnet",
                "fable",
                "extra",
                "claude",
                "claudeWeekly",
            ],
        ),
        // Antigravity's Session and Weekly are its Gemini pool, as in the native provider.
        Sub2ApiUpstream::Antigravity => (
            vec![
                metric("session", true, always, true),
                metric("weekly", true, always, true),
                metric("trend", true, always, false),
                metric("claude", true, demand, false),
                metric("claudeWeekly", true, demand, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "monthly",
                "spark",
                "sparkWeekly",
                "rateLimitResets",
                "sonnet",
                "fable",
                "extra",
            ],
        ),
        // Grok's billing period is weekly or monthly; only the reported one has data.
        Sub2ApiUpstream::Grok => (
            vec![
                metric("weekly", true, always, false),
                metric("monthly", true, always, false),
                metric("trend", true, always, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "session",
                "spark",
                "sparkWeekly",
                "rateLimitResets",
                "sonnet",
                "fable",
                "extra",
                "claude",
                "claudeWeekly",
            ],
        ),
    };
    let mut template = shown;
    template.extend(
        hidden
            .iter()
            .map(|suffix| metric(suffix, false, demand, false)),
    );
    template
}

pub(super) fn should_apply_metric_template(
    previous: Option<&StoredConfig>,
    mode: Sub2ApiMode,
    upstream: Sub2ApiUpstream,
) -> bool {
    previous.is_none_or(|previous| {
        previous.metric_template_version < METRIC_TEMPLATE_VERSION
            || previous.mode != mode
            || (mode == Sub2ApiMode::Admin && previous.upstream != upstream)
    })
}

pub(super) fn normalize_refreshed_quotas(
    upstream: Sub2ApiUpstream,
    mut quotas: Vec<QuotaWindow>,
    now: chrono::DateTime<Utc>,
) -> Vec<QuotaWindow> {
    let expected: &[&str] = match upstream {
        Sub2ApiUpstream::Codex => &["session", "weekly", "spark", "sparkWeekly"],
        Sub2ApiUpstream::Claude => &["session", "weekly", "sonnet", "fable"],
        Sub2ApiUpstream::Antigravity => &["session", "weekly", "claude", "claudeWeekly"],
        Sub2ApiUpstream::Grok => &["weekly", "monthly"],
    };
    let mut normalized = Vec::with_capacity(quotas.len().max(expected.len()));

    for &id in expected {
        let Some(index) = quotas.iter().position(|quota| quota.id == id) else {
            continue;
        };
        let mut quota = quotas.remove(index);
        if quota
            .resets_at
            .as_ref()
            .is_some_and(|resets_at| *resets_at <= now)
        {
            quota.used_percent = 0.0;
            quota.resets_at = None;
        }
        normalized.push(quota);
    }

    normalized.extend(quotas);
    normalized
}
