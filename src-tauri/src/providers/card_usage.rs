//! How Usage observations read from a Home client's logs land on a Card.
//!
//! Every local-log scanner hands its parsed events to this module as `UsageObservation`s. The
//! module owns the per-observation policy: the Scan window, the zero-token guard, Unattributed
//! usage, the Disposition (the Card's own series, Routed usage on another Card, or Others), and
//! which pricing path applies (Recorded cost or estimate). Adapters keep discovery, parsing and
//! deduplication. Terms are defined in `GLOSSARY.md`.

use chrono::{DateTime, Days, Local, NaiveDate, Utc};

use crate::pricing::{ModelPricing, ServiceTier, TokenBreakdown};

use super::{
    daily_usage::DailyUsageAccumulator,
    model_scope::{
        card_for_model, is_local_runtime_provider, native_log_disposition, LocalModelDisposition,
    },
};

pub(super) mod codex;

/// One request's usage as read from a Home client's logs, before it is placed on any Card.
#[derive(Debug, Clone, Copy)]
pub(super) struct UsageObservation<'a> {
    pub timestamp: DateTime<Utc>,
    /// Trimmed model name; `None` when the log has no usable name (Unattributed usage).
    pub model: Option<&'a str>,
    /// Token breakdown used for pricing.
    pub tokens: TokenBreakdown,
    /// Provider the Home client says served the request, when its logs name one.
    pub provider_hint: Option<&'a str>,
    /// Tokens counted on the Card. Usually the breakdown's total; some clients report their own.
    pub total_tokens: u64,
    pub tier: ServiceTier,
    /// Dollar cost the Home client wrote into its logs. A recorded $0 is still recorded.
    pub recorded_cost: Option<f64>,
    /// Keep the observation even with no tokens and no cost, so its model's label stays visible.
    pub keep_when_empty: bool,
    /// How usage priced under its home Card is estimated.
    pub price_source: PriceSource,
}

/// How an observation is estimated when it is priced under its home Card. Routed usage is
/// always estimated from the receiving Card's rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PriceSource {
    /// The Card's rates for the observation's tier.
    CardRates,
    /// Codex's own logs: ChatGPT-plan auto-review rates and Codex's priority, long-context and
    /// cache rules.
    CodexLogs { chatgpt_plan: bool },
}

impl<'a> UsageObservation<'a> {
    /// Trims `model` and treats an empty name as missing. Counts the breakdown's total, takes the
    /// tier from `tokens.is_fast`, and prices from the Card's rates.
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
            provider_hint: None,
            total_tokens: tokens.total_tokens(),
            tier: if tokens.is_fast {
                ServiceTier::Fast
            } else {
                ServiceTier::Standard
            },
            recorded_cost,
            keep_when_empty: false,
            price_source: PriceSource::CardRates,
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
    /// Drop observations with no tokens and no cost above zero.
    pub zero_token_guard: bool,
    /// How priced usage on the Card's own series is labelled.
    pub label: LabelMode,
    /// How Local runtime usage is recognised.
    pub local_runtime: LocalRuntimeMode,
}

/// TEMPORARY, see `TransitionalPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalRuntimeMode {
    /// Only by the model slug (`ollama/...`), which no Card owns, so it lands in Others.
    ModelSlug,
    /// Also by the provider hint: such usage lands, unpriced, in the home Card's Others whatever
    /// the model, and never on another Card.
    ModelSlugOrProvider,
}

/// TEMPORARY, see `TransitionalPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LabelMode {
    /// The model name as logged.
    RawModel,
    /// The model's display family, without a variant.
    Family,
    /// The model's display family, keeping the logged name as a variant when it differs.
    FamilyWithVariant,
}

/// Records `observations` from `home_card`'s Home client onto `target_card`.
///
/// When `target_card` is the home Card, observations land on its own series (models it owns and
/// Unattributed usage) or its Others (models no Card owns); models owned by another Card are
/// skipped. For any other `target_card`, only observations whose model that Card owns land, as
/// Routed usage. Own-series and Routed usage are priced under the Card they land on, Others under
/// the home Card.
///
/// Returns where the observations landed on `target_card`.
pub(super) fn record<'a>(
    observations: impl IntoIterator<Item = UsageObservation<'a>>,
    home_card: &str,
    target_card: &str,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    policy: TransitionalPolicy,
    accumulator: &mut DailyUsageAccumulator,
) -> Landed {
    let since = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(policy.window_start_days_ago))
        .unwrap_or(NaiveDate::MIN);
    let mut landed = Landed::default();
    for observation in observations {
        let date = observation.timestamp.with_timezone(&Local).date_naive();
        if date < since || observation.timestamp > now {
            continue;
        }
        if policy.local_runtime == LocalRuntimeMode::ModelSlugOrProvider
            && observation
                .provider_hint
                .is_some_and(is_local_runtime_provider)
        {
            if target_card == home_card {
                accumulator.add_other(
                    date,
                    observation.total_tokens,
                    None,
                    true,
                    observation.model.unwrap_or("Unattributed"),
                );
                landed.others = true;
            }
            continue;
        }
        if target_card == home_card {
            match record_home(observation, date, home_card, pricing, policy, accumulator) {
                Some(Bucket::OwnSeries) => landed.own_series = true,
                Some(Bucket::Others) => landed.others = true,
                None => {}
            }
        } else {
            landed.own_series |=
                record_routed(observation, date, target_card, pricing, policy, accumulator);
        }
    }
    landed
}

/// Where a batch of observations landed on the target Card.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Landed {
    /// Some observation was added to the Card's own series. This decides whether a client is
    /// named in the Card's source note.
    pub own_series: bool,
    /// Some observation was given to the Card's Others. Others drops usage with no tokens and no
    /// cost, so this can be true while Others stays empty.
    pub others: bool,
}

enum Bucket {
    OwnSeries,
    Others,
}

fn record_home(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    home_card: &str,
    pricing: &ModelPricing,
    policy: TransitionalPolicy,
    accumulator: &mut DailyUsageAccumulator,
) -> Option<Bucket> {
    let Some(model) = observation.model else {
        return add_own_series(
            observation,
            date,
            home_card,
            true,
            pricing,
            policy,
            accumulator,
        )
        .then_some(Bucket::OwnSeries);
    };
    match native_log_disposition(home_card, model) {
        LocalModelDisposition::Home => add_own_series(
            observation,
            date,
            home_card,
            true,
            pricing,
            policy,
            accumulator,
        )
        .then_some(Bucket::OwnSeries),
        LocalModelDisposition::RoutedTo(_) => None,
        LocalModelDisposition::Other => {
            let cost = observation
                .recorded_cost
                .or_else(|| estimate(pricing, home_card, true, model, &observation));
            accumulator.add_other(
                date,
                observation.total_tokens,
                cost,
                observation.recorded_cost.is_none(),
                model,
            );
            Some(Bucket::Others)
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
        Some(model) if card_for_model(model) == Some(target_card) => add_own_series(
            observation,
            date,
            target_card,
            false,
            pricing,
            policy,
            accumulator,
        ),
        _ => false,
    }
}

/// Adds to the Card's own series, priced under `card`. `home` says whether `card` is the
/// observation's home Card.
fn add_own_series(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    card: &str,
    home: bool,
    pricing: &ModelPricing,
    policy: TransitionalPolicy,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let tokens = observation.total_tokens;
    let estimated = match observation.recorded_cost {
        Some(_) => None,
        None => observation
            .model
            .and_then(|model| estimate(pricing, card, home, model, &observation)),
    };
    let cost = observation.recorded_cost.or(estimated);
    if policy.zero_token_guard && tokens == 0 && cost.is_none_or(|cost| cost <= 0.0) {
        return false;
    }
    let label = observation.model.unwrap_or("Unattributed");
    let family = match (policy.label, observation.model) {
        (LabelMode::Family | LabelMode::FamilyWithVariant, Some(model)) => {
            Some(pricing.display_family(model))
        }
        _ => None,
    };
    let variant = family
        .as_deref()
        .filter(|family| policy.label == LabelMode::FamilyWithVariant && *family != label);
    if let Some(exact) = observation.recorded_cost {
        match (family.as_deref(), variant) {
            (Some(family), Some(_)) => {
                accumulator.add_exact_variant(date, tokens, exact, family, label)
            }
            (Some(family), None) => accumulator.add_exact(date, tokens, exact, family),
            _ => accumulator.add_exact(date, tokens, exact, label),
        }
    } else if let Some(cost) = estimated {
        match (family.as_deref(), variant) {
            (Some(family), Some(_)) => accumulator.add_variant(date, tokens, cost, family, label),
            (Some(family), None) => accumulator.add(date, tokens, cost, family),
            _ => accumulator.add(date, tokens, cost, label),
        }
    } else if tokens > 0 || observation.keep_when_empty {
        accumulator.add_unpriced(date, tokens, label);
    } else {
        return false;
    }
    true
}

/// Estimates `observation`'s cost under `card`. `home` says whether `card` is its home Card.
fn estimate(
    pricing: &ModelPricing,
    card: &str,
    home: bool,
    model: &str,
    observation: &UsageObservation<'_>,
) -> Option<f64> {
    match observation.price_source {
        PriceSource::CodexLogs { chatgpt_plan } if home => {
            codex::estimate(pricing, model, chatgpt_plan, observation)
        }
        _ => estimate_from_card_rates(pricing, card, model, observation),
    }
}

/// Estimates `observation` as if priced under its home Card `home_card`, for tests of a
/// scanner's pricing.
#[cfg(test)]
pub(super) fn estimate_on_home_card(
    pricing: &ModelPricing,
    home_card: &str,
    observation: &UsageObservation<'_>,
) -> Option<f64> {
    estimate(pricing, home_card, true, observation.model?, observation)
}

/// Prices from `card`'s historical rates for the tier, falling back to current catalog rates
/// for the Standard and Fast tiers. An Unknown tier is never priced.
fn estimate_from_card_rates(
    pricing: &ModelPricing,
    card: &str,
    model: &str,
    observation: &UsageObservation<'_>,
) -> Option<f64> {
    let tier = observation.tier;
    if tier == ServiceTier::Unknown {
        return None;
    }
    let tokens = observation.tokens;
    match pricing.historical_rates(
        card,
        model,
        observation.timestamp,
        tier,
        tokens.prompt_tokens(),
    ) {
        Some(rates) => rates.map(|rates| rates.cost_dollars(tokens, true)),
        None if matches!(tier, ServiceTier::Standard | ServiceTier::Fast) => {
            pricing.estimated_cost_dollars(model, tokens, true)
        }
        None => None,
    }
}

/// Helpers for the characterization tests each scanner keeps while it migrates.
#[cfg(test)]
pub(super) mod test_support {
    use chrono::{DateTime, Days, Local, Utc};

    use crate::models::UsageHistory;

    /// Serializes `history` with its dates rewritten as `today-N`, so pinned output does not
    /// depend on the machine's timezone.
    pub fn relative_json(history: &UsageHistory, now: DateTime<Utc>) -> String {
        let mut json = serde_json::to_string_pretty(history).unwrap();
        let today = now.with_timezone(&Local).date_naive();
        for offset in 0..=40 {
            let date = today.checked_sub_days(Days::new(offset)).unwrap();
            json = json.replace(&format!("\"{date}\""), &format!("\"today-{offset}\""));
        }
        json
    }

    /// Panics with the actual output, so a new baseline can be read off a failing run.
    pub fn assert_pinned(name: &str, actual: &str, expected: &str) {
        if actual.trim() != expected.trim() {
            panic!("{name} changed. Actual output:\n{actual}");
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, TimeZone, Utc};

    use super::{record, LabelMode, LocalRuntimeMode, TransitionalPolicy, UsageObservation};
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

    /// Home Cards of the Home clients migrated onto this module, with the policy their native
    /// loop uses. Gemini CLI's home Card is antigravity. pi's events can have any home Card,
    /// chosen at parse, so pi appears once per Card.
    const HOMES: [(&str, TransitionalPolicy); 13] = [
        (
            "claude",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "codex",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "grok",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: true,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "antigravity",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: true,
                label: LabelMode::FamilyWithVariant,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "kimi",
            TransitionalPolicy {
                window_start_days_ago: 29,
                zero_token_guard: true,
                label: LabelMode::Family,
                local_runtime: LocalRuntimeMode::ModelSlugOrProvider,
            },
        ),
        (
            "antigravity",
            TransitionalPolicy {
                window_start_days_ago: 29,
                zero_token_guard: true,
                label: LabelMode::FamilyWithVariant,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "claude",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "codex",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "antigravity",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "grok",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "kimi",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "minimax",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
        (
            "zai",
            TransitionalPolicy {
                window_start_days_ago: 30,
                zero_token_guard: false,
                label: LabelMode::RawModel,
                local_runtime: LocalRuntimeMode::ModelSlug,
            },
        ),
    ];

    /// The policy a home's scanner passes for its Routed usage: its native policy, with the
    /// unified window and the zero-token guard, which every routed loop already has.
    fn routed(native: TransitionalPolicy) -> TransitionalPolicy {
        TransitionalPolicy {
            window_start_days_ago: 29,
            zero_token_guard: true,
            ..native
        }
    }

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
                let policy = if **card == home {
                    home_policy
                } else {
                    routed(home_policy)
                };
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
    fn local_runtime_providers_land_exactly_once_across_cards() {
        let pricing = test_bundled_pricing();
        for (home, policy) in HOMES {
            if policy.local_runtime != LocalRuntimeMode::ModelSlugOrProvider {
                continue;
            }
            for model in MODELS {
                let observation = UsageObservation {
                    provider_hint: Some("ollama"),
                    ..UsageObservation::new(now(), model, tokens(1_000, 100), None)
                };
                assert_eq!(
                    landings(&pricing, home, policy, observation),
                    1,
                    "{home} home, {model:?}"
                );
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
