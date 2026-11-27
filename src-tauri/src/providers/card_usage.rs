//! How Usage observations read from a Home client's logs land on a Card.
//!
//! Every local-log scanner hands its parsed events to this module as `UsageObservation`s. The
//! module owns the per-observation policy: the Scan window, the zero-token guard, Unattributed
//! usage, the Disposition (the Card's own series, Routed usage on another Card, or Others), and
//! which pricing path applies (Recorded cost or estimate). Adapters keep discovery, parsing and
//! deduplication. Terms are defined in `GLOSSARY.md`.

use chrono::{DateTime, Days, Local, NaiveDate, Utc};

use crate::pricing::{ModelPricing, TokenBreakdown};

use super::{
    daily_usage::DailyUsageAccumulator,
    model_scope::{card_for_model, native_log_disposition, LocalModelDisposition},
};

/// One request's usage as read from a Home client's logs, before it is placed on any Card.
#[derive(Debug, Clone, Copy)]
pub(super) struct UsageObservation<'a> {
    pub timestamp: DateTime<Utc>,
    /// Trimmed model name; `None` when the log has no usable name (Unattributed usage).
    pub model: Option<&'a str>,
    pub tokens: TokenBreakdown,
    /// Dollar cost the Home client wrote into its logs. A recorded $0 is still recorded.
    pub recorded_cost: Option<f64>,
}

impl<'a> UsageObservation<'a> {
    /// Trims `model` and treats an empty name as missing.
    pub fn new(
        timestamp: DateTime<Utc>,
        model: Option<&'a str>,
        tokens: TokenBreakdown,
        recorded_cost: Option<f64>,
    ) -> Self {
        Self {
            timestamp,
            model: model.map(str::trim).filter(|name| !name.is_empty()),
            tokens,
            recorded_cost,
        }
    }
}

/// TEMPORARY. Rules that still differ between scanners while they migrate onto this module
/// without changing their output. Each field is removed by the ticket that unifies its rule
/// (`.scratch/card-usage/issues/11`–`14`); the type goes with the last one.
#[derive(Debug, Clone, Copy)]
pub(super) struct TransitionalPolicy {
    /// First counted day, as days before today. The unified Scan window is the last 30 days
    /// including today (29); some loops still start a day earlier (30).
    pub window_start_days_ago: u64,
    /// Drop observations with no tokens and no Recorded cost above zero.
    pub zero_token_guard: bool,
}

/// Records `observations` from `home_card`'s Home client onto `target_card`.
///
/// When `target_card` is the home Card, observations land on its own series (models it owns and
/// Unattributed usage) or its Others (models no Card owns); models owned by another Card are
/// skipped. For any other `target_card`, only observations whose model that Card owns land, as
/// Routed usage. Own-series and Routed usage are priced under the Card they land on, Others under
/// the home Card.
///
/// Returns whether any observation landed on `target_card`'s own series, which is what decides
/// whether a client is named in the Card's source note. Others usage does not count.
pub(super) fn record<'a>(
    observations: impl IntoIterator<Item = UsageObservation<'a>>,
    home_card: &str,
    target_card: &str,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    policy: TransitionalPolicy,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let since = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(policy.window_start_days_ago))
        .unwrap_or(NaiveDate::MIN);
    let mut landed = false;
    for observation in observations {
        let date = observation.timestamp.with_timezone(&Local).date_naive();
        if date < since || observation.timestamp > now {
            continue;
        }
        landed |= if target_card == home_card {
            record_home(observation, date, home_card, pricing, policy, accumulator)
        } else {
            record_routed(observation, date, target_card, pricing, policy, accumulator)
        };
    }
    landed
}

fn record_home(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    home_card: &str,
    pricing: &ModelPricing,
    policy: TransitionalPolicy,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let Some(model) = observation.model else {
        return add_own_series(observation, date, home_card, pricing, policy, accumulator);
    };
    match native_log_disposition(home_card, model) {
        LocalModelDisposition::Home => {
            add_own_series(observation, date, home_card, pricing, policy, accumulator)
        }
        LocalModelDisposition::RoutedTo(_) => false,
        LocalModelDisposition::Other => {
            let cost = observation
                .recorded_cost
                .or_else(|| estimate(pricing, home_card, model, &observation));
            accumulator.add_other(
                date,
                observation.tokens.total_tokens(),
                cost,
                observation.recorded_cost.is_none(),
                model,
            );
            false
        }
    }
}

fn record_routed(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    target_card: &str,
    pricing: &ModelPricing,
    policy: TransitionalPolicy,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    match observation.model {
        Some(model) if card_for_model(model) == Some(target_card) => {
            add_own_series(observation, date, target_card, pricing, policy, accumulator)
        }
        _ => false,
    }
}

/// Adds to the Card's own series, priced under `card`.
fn add_own_series(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    card: &str,
    pricing: &ModelPricing,
    policy: TransitionalPolicy,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let tokens = observation.tokens.total_tokens();
    if policy.zero_token_guard
        && tokens == 0
        && observation.recorded_cost.is_none_or(|cost| cost <= 0.0)
    {
        return false;
    }
    let label = observation.model.unwrap_or("Unattributed");
    if let Some(exact) = observation.recorded_cost {
        accumulator.add_exact(date, tokens, exact, label);
    } else if let Some(cost) = observation
        .model
        .and_then(|model| estimate(pricing, card, model, &observation))
    {
        accumulator.add(date, tokens, cost, label);
    } else if tokens > 0 {
        accumulator.add_unpriced(date, tokens, label);
    } else {
        return false;
    }
    true
}

fn estimate(
    pricing: &ModelPricing,
    card: &str,
    model: &str,
    observation: &UsageObservation<'_>,
) -> Option<f64> {
    pricing.estimated_cost_at(card, model, observation.timestamp, observation.tokens, true)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, TimeZone, Utc};

    use super::{record, TransitionalPolicy, UsageObservation};
    use crate::{
        pricing::{test_bundled_pricing, ModelPricing, TokenBreakdown},
        providers::daily_usage::DailyUsageAccumulator,
    };

    /// Every Card a Home client's usage can land on.
    const CARDS: [&str; 7] = [
        "claude",
        "codex",
        "antigravity",
        "grok",
        "kimi",
        "minimax",
        "zai",
    ];

    /// Home clients migrated onto this module, with the policy their native loop uses.
    const HOMES: [(&str, TransitionalPolicy); 1] = [(
        "claude",
        TransitionalPolicy {
            window_start_days_ago: 30,
            zero_token_guard: false,
        },
    )];

    const ROUTED: TransitionalPolicy = TransitionalPolicy {
        window_start_days_ago: 29,
        zero_token_guard: true,
    };

    const MODELS: [Option<&str>; 13] = [
        Some("claude-sonnet-4-5"),
        Some("gpt-5.5"),
        Some("gemini-3-pro"),
        Some("gpt-oss-120b"),
        Some("grok-4"),
        Some("kimi-k2"),
        Some("minimax-m2"),
        Some("glm-4.6"),
        Some("deepseek-chat"),
        Some("ollama/llama3"),
        Some("totally-unknown-model"),
        Some("  "),
        None,
    ];

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap()
    }

    fn tokens(input: u64, output: u64) -> TokenBreakdown {
        TokenBreakdown {
            input,
            output,
            ..TokenBreakdown::default()
        }
    }

    /// Number of places one observation lands across every Card: a Card's own series or its
    /// Others.
    fn landings(
        pricing: &ModelPricing,
        home: &str,
        home_policy: TransitionalPolicy,
        observation: UsageObservation,
    ) -> usize {
        CARDS
            .iter()
            .filter(|card| {
                let policy = if **card == home { home_policy } else { ROUTED };
                let mut accumulator = DailyUsageAccumulator::default();
                record(
                    [observation],
                    home,
                    card,
                    now(),
                    pricing,
                    policy,
                    &mut accumulator,
                );
                let history = accumulator.build(now(), "test");
                !history.daily.is_empty()
                    || history
                        .other_usage
                        .is_some_and(|others| others.last_30_days.is_some())
            })
            .count()
    }

    #[test]
    fn each_observation_lands_exactly_once_across_cards() {
        let pricing = test_bundled_pricing();
        for (home, policy) in HOMES {
            for model in MODELS {
                for days_ago in [0, 1, 28, 29] {
                    for recorded_cost in [None, Some(0.0), Some(0.25)] {
                        let observation = UsageObservation::new(
                            now() - Duration::days(days_ago),
                            model,
                            tokens(1_000, 100),
                            recorded_cost,
                        );
                        assert_eq!(
                            landings(&pricing, home, policy, observation),
                            1,
                            "{home} home, {model:?}, {days_ago} days ago, cost {recorded_cost:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn observations_outside_the_window_or_in_the_future_land_nowhere() {
        let pricing = test_bundled_pricing();
        for (home, policy) in HOMES {
            for model in MODELS {
                for timestamp in [now() - Duration::days(31), now() + Duration::hours(1)] {
                    let observation =
                        UsageObservation::new(timestamp, model, tokens(1_000, 100), None);
                    assert_eq!(
                        landings(&pricing, home, policy, observation),
                        0,
                        "{home}, {model:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_token_observations_land_at_most_once() {
        let pricing = test_bundled_pricing();
        for (home, policy) in HOMES {
            for model in MODELS {
                let observation = UsageObservation::new(now(), model, tokens(0, 0), None);
                assert!(
                    landings(&pricing, home, policy, observation) <= 1,
                    "{home}, {model:?}"
                );
            }
        }
    }
}
