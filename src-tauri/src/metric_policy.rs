//! Which metrics a Card shows and can Star, and which Stars count toward its limit.
//!
//! Plain providers show every metric. A Sub2API connection's Track decides what its Card shows:
//! each Track reports only some of the connection's metrics.

use std::collections::BTreeMap;

#[cfg(any(target_os = "macos", test))]
use crate::models::MetricLayout;
use crate::models::MetricPolicyView;
use crate::providers::{sub2api, ProviderRegistry};

/// The most Stars a Card can have. Only metrics the Card shows count toward it.
pub const MAX_STARS: usize = 2;

/// Every provider's policy, for the settings view state.
pub fn views(registry: &ProviderRegistry) -> BTreeMap<String, MetricPolicyView> {
    registry
        .catalog()
        .providers
        .iter()
        .map(|definition| {
            let policy = MetricPolicy::for_provider(registry, &definition.id);
            let ids = |keep: &dyn Fn(&str) -> bool| {
                definition
                    .metrics
                    .iter()
                    .filter(|metric| keep(&metric.id))
                    .map(|metric| metric.id.clone())
                    .collect()
            };
            let view = MetricPolicyView {
                track: policy.track().map(str::to_owned),
                visible_metric_ids: ids(&|id| policy.shows(id)),
                starrable_metric_ids: ids(&|id| policy.starrable(id)),
                max_stars: MAX_STARS,
            };
            (definition.id.clone(), view)
        })
        .collect()
}

pub struct MetricPolicy<'a> {
    registry: &'a ProviderRegistry,
    provider_id: &'a str,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    Plain,
    /// A connection, with its Track when known.
    Connection(Option<&'static str>),
}

impl<'a> MetricPolicy<'a> {
    /// The policy for a provider's current Track.
    pub fn for_provider(registry: &'a ProviderRegistry, provider_id: &'a str) -> Self {
        Self::for_track(
            registry,
            provider_id,
            registry.upstream_provider_id(provider_id),
        )
    }

    /// The policy a connection would have on `track`, as when its template changes before the
    /// registry reports the new Track. Plain providers ignore `track`.
    pub fn for_track(
        registry: &'a ProviderRegistry,
        provider_id: &'a str,
        track: Option<&'static str>,
    ) -> Self {
        let connection = registry
            .catalog()
            .connection_config_provider_ids
            .iter()
            .any(|id| id == provider_id);
        Self {
            registry,
            provider_id,
            kind: if connection {
                Kind::Connection(track)
            } else {
                Kind::Plain
            },
        }
    }

    /// The connection's Track, if this is a connection and its Track is known.
    pub fn track(&self) -> Option<&'static str> {
        match self.kind {
            Kind::Plain => None,
            Kind::Connection(track) => track,
        }
    }

    /// False only for a connection whose Track has not been read yet.
    pub fn track_known(&self) -> bool {
        !matches!(self.kind, Kind::Connection(None))
    }

    /// Whether the Card can report data for this metric.
    pub fn shows(&self, metric_id: &str) -> bool {
        match self.kind {
            Kind::Plain => true,
            Kind::Connection(track) => {
                sub2api::track_shows_metric(self.provider_id, metric_id, track)
            }
        }
    }

    /// Whether this metric can be given a Star. Subscriptions Star only their percentage metrics.
    pub fn starrable(&self, metric_id: &str) -> bool {
        self.pinnable(metric_id) && self.shows(metric_id) && self.star_allowed(metric_id)
    }

    /// Whether the Star control applies to this metric: a Star already given can be removed
    /// even where a new one would not be allowed.
    #[cfg(any(target_os = "macos", test))]
    pub fn can_toggle_star(&self, metric: &MetricLayout) -> bool {
        self.pinnable(&metric.id)
            && self.shows(&metric.id)
            && (metric.pinned || self.star_allowed(&metric.id))
    }

    /// The Stars that count toward [`MAX_STARS`].
    #[cfg(any(target_os = "macos", test))]
    pub fn stars(&self, metrics: &[MetricLayout]) -> usize {
        metrics
            .iter()
            .filter(|metric| metric.pinned && self.shows(&metric.id))
            .count()
    }

    /// Whether another Star fits under [`MAX_STARS`].
    #[cfg(any(target_os = "macos", test))]
    pub fn can_add_star(&self, metrics: &[MetricLayout]) -> bool {
        self.stars(metrics) < MAX_STARS
    }

    fn pinnable(&self, metric_id: &str) -> bool {
        self.registry
            .metric(metric_id)
            .is_some_and(|definition| definition.pinnable)
    }

    fn star_allowed(&self, metric_id: &str) -> bool {
        self.track() != Some("subscription") || sub2api::subscription_percentage_metric(metric_id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{views, MetricPolicy, MAX_STARS};
    use crate::models::{MetricLayout, MetricSection, ProviderDefinition, ProviderSnapshot};
    use crate::providers::{codex, sub2api, ProviderError, ProviderRegistry, UsageProvider};

    const CONNECTION: &str = "sub2api@2";

    /// A connection whose Track the test sets.
    struct Connection(ProviderDefinition, Mutex<Option<&'static str>>);

    impl UsageProvider for Connection {
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

    fn registry_on(track: Option<&'static str>) -> ProviderRegistry {
        let connection = Connection(sub2api::test_definition(CONNECTION), Mutex::new(track));
        let plain = ProviderRegistry::from_definitions(vec![codex::definition()]).unwrap();
        let plain = plain.runtime("codex").unwrap();
        ProviderRegistry::new(vec![Arc::new(connection), plain]).unwrap()
    }

    fn metric(suffix: &str, pinned: bool) -> MetricLayout {
        MetricLayout {
            id: format!("{CONNECTION}.{suffix}"),
            enabled: true,
            section: MetricSection::AlwaysVisible,
            pinned,
        }
    }

    #[test]
    fn each_track_shows_only_the_metrics_it_reports() {
        let cases: &[(Option<&str>, &[&str], &[&str])] = &[
            (
                Some("subscription"),
                &["daily", "weekly", "monthly", "today", "last30", "trend"],
                &["session", "spark", "extra", "claude"],
            ),
            (
                Some("codex"),
                &[
                    "session",
                    "weekly",
                    "spark",
                    "sparkWeekly",
                    "rateLimitResets",
                ],
                &["daily", "monthly", "sonnet", "claude", "extra"],
            ),
            (
                Some("claude"),
                &["session", "weekly", "sonnet", "fable", "rateLimitResets"],
                &["spark", "claude", "monthly", "extra"],
            ),
            (
                Some("antigravity"),
                &["session", "weekly", "claude", "claudeWeekly"],
                &["sonnet", "spark", "monthly", "extra"],
            ),
            (
                Some("grok"),
                &["weekly", "monthly", "trend"],
                &["session", "claude", "daily", "extra"],
            ),
            // A Track not yet read hides only what no administrator Track reports.
            (
                None,
                &["session", "weekly", "spark", "sonnet"],
                &["daily", "monthly", "extra", "claude", "claudeWeekly"],
            ),
        ];
        for (track, shown, hidden) in cases {
            let registry = registry_on(*track);
            let policy = MetricPolicy::for_provider(&registry, CONNECTION);
            assert_eq!(policy.track(), *track);
            assert_eq!(policy.track_known(), track.is_some());
            for suffix in *shown {
                assert!(
                    policy.shows(&metric(suffix, false).id),
                    "{track:?} {suffix}"
                );
            }
            for suffix in *hidden {
                assert!(
                    !policy.shows(&metric(suffix, false).id),
                    "{track:?} {suffix}"
                );
            }
        }
    }

    #[test]
    fn a_template_change_uses_the_new_track_before_the_registry_reports_it() {
        let registry = registry_on(Some("claude"));
        let policy = MetricPolicy::for_track(&registry, CONNECTION, Some("codex"));
        assert_eq!(policy.track(), Some("codex"));
        assert!(policy.shows(&metric("spark", false).id));
    }

    #[test]
    fn plain_providers_show_every_metric_and_star_the_pinnable_ones() {
        let registry = registry_on(Some("subscription"));
        let policy = MetricPolicy::for_track(&registry, "codex", Some("subscription"));
        assert_eq!(policy.track(), None);
        assert!(policy.track_known());
        for definition in codex::definition().metrics {
            assert!(policy.shows(&definition.id), "{}", definition.id);
            assert_eq!(
                policy.starrable(&definition.id),
                definition.pinnable,
                "{}",
                definition.id
            );
        }
    }

    #[test]
    fn subscriptions_star_only_their_percentage_metrics_but_keep_existing_stars() {
        let registry = registry_on(Some("subscription"));
        let policy = MetricPolicy::for_provider(&registry, CONNECTION);
        for suffix in ["daily", "weekly", "monthly"] {
            assert!(policy.starrable(&metric(suffix, false).id), "{suffix}");
        }
        // Today is shown and pinnable, but a subscription's menu bar shows only percentages.
        assert!(policy.shows(&metric("today", false).id));
        assert!(!policy.starrable(&metric("today", false).id));
        assert!(!policy.can_toggle_star(&metric("today", false)));
        assert!(policy.can_toggle_star(&metric("today", true)));
        // A hidden metric offers no Star control, even with a Star.
        assert!(!policy.can_toggle_star(&metric("session", true)));
        // Trend is never pinnable.
        assert!(!policy.can_toggle_star(&metric("trend", true)));

        let codex = registry_on(Some("codex"));
        let policy = MetricPolicy::for_provider(&codex, CONNECTION);
        assert!(policy.starrable(&metric("today", false).id));
    }

    #[test]
    fn only_stars_on_shown_metrics_count_toward_the_limit() {
        let registry = registry_on(Some("subscription"));
        let policy = MetricPolicy::for_provider(&registry, CONNECTION);
        let mut metrics = vec![
            metric("session", true),
            metric("spark", true),
            metric("weekly", true),
            metric("daily", false),
        ];
        assert_eq!(policy.stars(&metrics), 1);
        assert!(policy.can_add_star(&metrics));
        metrics[3].pinned = true;
        assert_eq!(policy.stars(&metrics), MAX_STARS);
        assert!(!policy.can_add_star(&metrics));
    }

    #[test]
    fn the_view_lists_each_providers_visible_and_starrable_metrics() {
        let registry = registry_on(Some("subscription"));
        let views = views(&registry);
        let connection = &views[CONNECTION];
        assert_eq!(connection.track.as_deref(), Some("subscription"));
        assert_eq!(connection.max_stars, MAX_STARS);
        let id = |suffix: &str| metric(suffix, false).id;
        assert!(connection.visible_metric_ids.contains(&id("today")));
        assert!(!connection.visible_metric_ids.contains(&id("session")));
        assert!(connection.starrable_metric_ids.contains(&id("weekly")));
        assert!(!connection.starrable_metric_ids.contains(&id("today")));

        let plain = &views["codex"];
        assert_eq!(plain.track, None);
        let definition = codex::definition();
        assert_eq!(plain.visible_metric_ids.len(), definition.metrics.len());
        assert_eq!(
            plain.starrable_metric_ids.len(),
            definition
                .metrics
                .iter()
                .filter(|metric| metric.pinnable)
                .count()
        );
    }
}
