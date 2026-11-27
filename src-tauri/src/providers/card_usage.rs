//! How Usage observations read from a Home client's logs land on a Card.
//!
//! Every local-log scanner hands its parsed events to this module as `UsageObservation`s. The
//! module owns the per-observation policy: the Scan window, the zero-token guard, Unattributed
//! and Local runtime usage, the Disposition (the Card's own series, Routed usage on another Card,
//! or Others), which pricing path applies (Recorded cost or estimate), and the label: every Card
//! shows a model by its display family, with the model as logged as the family's variant.
//! Adapters keep discovery, parsing and deduplication. Terms are defined in `GLOSSARY.md`.

use chrono::{DateTime, Days, Local, NaiveDate, Utc};

use crate::pricing::{is_local_runtime_model, ModelPricing, ServiceTier, TokenBreakdown};

use super::model_scope::{
    card_for_model, is_local_runtime_provider, native_log_disposition, LocalModelDisposition,
};

pub(super) mod codex;
mod daily_usage;

pub(super) use daily_usage::DailyUsageAccumulator;

/// One request's usage as read from a Home client's logs, before it is placed on any Card.
#[derive(Debug, Clone, Copy)]
pub(super) struct UsageObservation<'a> {
    pub timestamp: DateTime<Utc>,
    /// Trimmed model name; `None` when the log has no usable name (Unattributed usage). It
    /// decides the Disposition.
    pub model: Option<&'a str>,
    /// Model to price and label the usage by, when it differs from `model` (a model-plus-variant
    /// name that has its own rates).
    pub rate_model: Option<&'a str>,
    /// The home Card is only where this usage's Others go: the Home client did not say which Card
    /// it is for. Usage without a model is then dropped, and only models the home Card owns by
    /// name land on its own series.
    pub home_is_fallback: bool,
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

/// Where an observation lands and what it costs, when its Home client settled both itself
/// because it prices and deduplicates whole messages first. See `record_settled`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Settled {
    pub others: bool,
    pub cost: Option<f64>,
    pub estimated: bool,
    /// Tokens `cost` covers; fewer than the total when only part of the usage is priced.
    pub priced_tokens: u64,
    /// Every part of the usage was priced.
    pub complete: bool,
    /// Unpriced usage leaves only an unknown-model marker instead of counting its tokens.
    pub unpriced_marker_only: bool,
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
            rate_model: None,
            home_is_fallback: false,
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

/// Number of days in the Scan window: the last 30 days including today.
pub(super) const SCAN_WINDOW_DAYS: u64 = 30;

/// First day of the Scan window that ends on `today`.
fn scan_window_start(today: NaiveDate) -> NaiveDate {
    today
        .checked_sub_days(Days::new(SCAN_WINDOW_DAYS - 1))
        .unwrap_or(NaiveDate::MIN)
}

/// Records `observations` from `home_card`'s Home client onto `target_card`.
///
/// When `target_card` is the home Card, observations land on its own series (models it owns and
/// Unattributed usage) or its Others (models no Card owns); models owned by another Card are
/// skipped. For any other `target_card`, only observations whose model that Card owns land, as
/// Routed usage. Own-series and Routed usage are priced under the Card they land on, Others under
/// the home Card. Local runtime usage always lands, unpriced, in the home Card's Others.
///
/// Returns where the observations landed on `target_card`.
pub(super) fn record<'a>(
    observations: impl IntoIterator<Item = UsageObservation<'a>>,
    home_card: &str,
    target_card: &str,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> Landed {
    let mut landed = Landed::default();
    for observation in observations {
        let Some(date) = date_in_window(&observation, now) else {
            continue;
        };
        if target_card == home_card {
            match record_home(observation, date, home_card, pricing, accumulator) {
                Some(Bucket::OwnSeries) => landed.own_series = true,
                Some(Bucket::Others) => landed.others = true,
                None => {}
            }
        } else {
            landed.own_series |=
                record_routed(observation, date, target_card, pricing, accumulator);
        }
    }
    landed
}

/// Records observations whose Card and cost their Home client already settled. Only the Scan
/// window applies; the observations land on the Card the caller is filling.
///
/// Without `apply_scan_window`, as for an account export that keeps its own dates, every
/// observation is recorded and building the history trims the dates.
pub(super) fn record_settled<'a>(
    observations: impl IntoIterator<Item = (UsageObservation<'a>, Settled)>,
    now: DateTime<Utc>,
    apply_scan_window: bool,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> Landed {
    let mut landed = Landed::default();
    for (observation, settled) in observations {
        let date = if apply_scan_window {
            date_in_window(&observation, now)
        } else {
            Some(observation.timestamp.with_timezone(&Local).date_naive())
        };
        let Some(date) = date else {
            continue;
        };
        match add_settled(observation, settled, date, pricing, accumulator) {
            Some(Bucket::OwnSeries) => landed.own_series = true,
            Some(Bucket::Others) => landed.others = true,
            None => {}
        }
    }
    landed
}

/// The observation's local date, when it falls inside the Scan window and not in the future.
fn date_in_window(observation: &UsageObservation<'_>, now: DateTime<Utc>) -> Option<NaiveDate> {
    let since = scan_window_start(now.with_timezone(&Local).date_naive());
    let date = observation.timestamp.with_timezone(&Local).date_naive();
    (date >= since && observation.timestamp <= now).then_some(date)
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

fn add_settled(
    observation: UsageObservation<'_>,
    settled: Settled,
    date: NaiveDate,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> Option<Bucket> {
    let tokens = observation.total_tokens;
    if settled.others {
        accumulator.add_partially_priced_other(
            date,
            tokens,
            settled.priced_tokens,
            settled.cost,
            settled.estimated,
            observation.model.unwrap_or("Unattributed"),
        );
        return Some(Bucket::Others);
    }
    let (family, model) = label(pricing, observation.model);
    match settled.cost {
        // Only some of the usage is priced, so the model's cost must not read as complete.
        Some(cost) if !settled.complete => {
            accumulator.add_partially_priced(date, tokens, cost, &family, model, settled.estimated);
        }
        Some(cost) if settled.estimated => {
            accumulator.add_variant(date, tokens, cost, &family, model)
        }
        Some(cost) => accumulator.add_exact_variant(date, tokens, cost, &family, model),
        None if tokens > 0 && settled.unpriced_marker_only => {
            // A missing model leaves no marker to name.
            accumulator.add_unknown_model(date, observation.model.unwrap_or_default());
        }
        None if tokens > 0 => accumulator.add_unpriced(date, tokens, &family, model),
        None if !settled.complete => accumulator.add_unknown_model(date, model),
        None => return None,
    }
    Some(Bucket::OwnSeries)
}

fn record_home(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    home_card: &str,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> Option<Bucket> {
    let own_series = |accumulator: &mut DailyUsageAccumulator| {
        add_own_series(observation, date, home_card, true, pricing, accumulator)
            .then_some(Bucket::OwnSeries)
    };
    // Unattributed usage under a fallback home may belong to any Card, so it lands nowhere.
    if observation.model.is_none() && observation.home_is_fallback {
        return None;
    }
    if served_by_local_runtime(&observation) {
        add_others(observation, date, None, accumulator);
        return Some(Bucket::Others);
    }
    let Some(model) = observation.model else {
        return own_series(accumulator);
    };
    match native_log_disposition(home_card, model) {
        LocalModelDisposition::Home
            if !observation.home_is_fallback || card_for_model(model) == Some(home_card) =>
        {
            own_series(accumulator)
        }
        LocalModelDisposition::RoutedTo(_) => None,
        LocalModelDisposition::Home | LocalModelDisposition::Other => {
            let cost = observation.recorded_cost.or_else(|| {
                estimate(
                    pricing,
                    home_card,
                    true,
                    priced_model(&observation)?,
                    &observation,
                )
            });
            add_others(observation, date, cost, accumulator);
            Some(Bucket::Others)
        }
    }
}

/// Adds to the home Card's Others, priced at `cost` when there is one.
fn add_others(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    cost: Option<f64>,
    accumulator: &mut DailyUsageAccumulator,
) {
    accumulator.add_other(
        date,
        observation.total_tokens,
        cost,
        observation.recorded_cost.is_none(),
        priced_model(&observation).unwrap_or("Unattributed"),
    );
}

/// How a model's usage is labelled on a Card: by its display family, keeping the model as
/// logged as the family's variant. Usage without a model is labelled Unattributed.
fn label<'a>(pricing: &ModelPricing, model: Option<&'a str>) -> (String, &'a str) {
    match model {
        Some(model) => (pricing.display_family(model), model),
        None => ("Unattributed".to_owned(), "Unattributed"),
    }
}

/// The model usage is priced and labelled by.
fn priced_model<'a>(observation: &UsageObservation<'a>) -> Option<&'a str> {
    observation.rate_model.or(observation.model)
}

/// Local runtime usage: its model slug or its provider hint says a Local runtime served it. It
/// is free whoever owns the model, so it never reaches another Card.
fn served_by_local_runtime(observation: &UsageObservation<'_>) -> bool {
    observation.model.is_some_and(is_local_runtime_model)
        || observation
            .provider_hint
            .is_some_and(is_local_runtime_provider)
}

fn record_routed(
    observation: UsageObservation<'_>,
    date: NaiveDate,
    target_card: &str,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    if served_by_local_runtime(&observation) {
        return false;
    }
    match observation.model {
        Some(model) if card_for_model(model) == Some(target_card) => {
            add_own_series(observation, date, target_card, false, pricing, accumulator)
        }
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
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let tokens = observation.total_tokens;
    let estimated = match observation.recorded_cost {
        Some(_) => None,
        None => priced_model(&observation)
            .and_then(|model| estimate(pricing, card, home, model, &observation)),
    };
    let cost = observation.recorded_cost.or(estimated);
    // Usage with no tokens and nothing to pay is not usage, unless its label must stay visible.
    if tokens == 0 && cost.is_none_or(|cost| cost <= 0.0) && !observation.keep_when_empty {
        return false;
    }
    let (family, model) = label(pricing, priced_model(&observation));
    if let Some(exact) = observation.recorded_cost {
        accumulator.add_exact_variant(date, tokens, exact, &family, model);
    } else if let Some(cost) = estimated {
        accumulator.add_variant(date, tokens, cost, &family, model);
    } else if tokens > 0 || observation.keep_when_empty {
        accumulator.add_unpriced(date, tokens, &family, model);
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
    let tokens = observation.tokens;
    let rates = pricing.rates_at(
        card,
        model,
        observation.timestamp,
        observation.tier,
        tokens.prompt_tokens(),
    )?;
    Some(rates.cost_dollars(tokens, true))
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

    use super::{record, record_settled, Settled, UsageObservation};
    use crate::{
        pricing::{is_local_runtime_model, test_bundled_pricing, ModelPricing, TokenBreakdown},
        providers::card_usage::DailyUsageAccumulator,
    };

    /// Every Card a Home client's usage can land on, and every home Card: pi's and zcode's usage
    /// can have any home Card, chosen per event.
    const CARDS: [&str; 7] = [
        "claude",
        "codex",
        "antigravity",
        "grok",
        "kimi",
        "minimax",
        "zai",
    ];

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
    fn landings(pricing: &ModelPricing, home: &str, observation: UsageObservation) -> usize {
        CARDS
            .iter()
            .filter(|card| {
                let mut accumulator = DailyUsageAccumulator::default();
                record([observation], home, card, now(), pricing, &mut accumulator);
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
        for home in CARDS {
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
                            landings(&pricing, home, observation),
                            1,
                            "{home} home, {model:?}, {days_ago} days ago, cost {recorded_cost:?}"
                        );
                    }
                }
            }
        }
    }

    /// Local runtime usage, recognised by its model slug or its provider hint, lands only in the
    /// home Card's Others, unpriced, whoever owns the model and whatever cost was recorded.
    #[test]
    fn local_runtime_usage_lands_unpriced_in_the_home_cards_others() {
        let pricing = test_bundled_pricing();
        for home in CARDS {
            let foreign = CARDS.iter().copied().find(|card| *card != home).unwrap();
            let cases = [
                ("slug only", Some("ollama/llama3"), None),
                (
                    "slug naming a cloud model",
                    Some("ollama/claude-sonnet-4-5"),
                    None,
                ),
                ("provider only", Some(owned_model(home)), Some("ollama")),
                ("foreign model", Some(owned_model(foreign)), Some("Ollama")),
                ("Unattributed", None, Some("ollama")),
            ];
            for (case, model, provider_hint) in cases {
                for recorded_cost in [None, Some(0.25)] {
                    let observation = UsageObservation {
                        provider_hint,
                        ..UsageObservation::new(now(), model, tokens(1_000, 100), recorded_cost)
                    };
                    for card in CARDS {
                        let mut accumulator = DailyUsageAccumulator::default();
                        record([observation], home, card, now(), &pricing, &mut accumulator);
                        let history = accumulator.build(now(), "test");
                        let context = format!("{case}: {home} home on {card}, {recorded_cost:?}");
                        assert!(history.daily.is_empty(), "{context}");
                        let others = history.other_usage.and_then(|others| others.last_30_days);
                        if card != home {
                            assert!(others.is_none(), "{context}");
                            continue;
                        }
                        let others = others.expect(&context);
                        assert_eq!(others.tokens, 1_100, "{context}");
                        assert_eq!(others.priced_tokens, 0, "{context}");
                        assert_eq!(others.estimated_cost_usd, None, "{context}");
                    }
                }
            }
        }
    }

    #[test]
    fn usage_keeping_its_home_lands_once_unless_it_has_no_home_or_model() {
        let pricing = test_bundled_pricing();
        for home in CARDS {
            for home_is_fallback in [false, true] {
                for provider_hint in [None, Some("ollama"), Some("openrouter")] {
                    for model in MODELS {
                        let observation = UsageObservation {
                            home_is_fallback,
                            provider_hint,
                            ..UsageObservation::new(now(), model, tokens(1_000, 100), None)
                        };
                        let model = observation.model;
                        let expected = usize::from(!(home_is_fallback && model.is_none()));
                        assert_eq!(
                            landings(&pricing, home, observation),
                            expected,
                            "{home} home, fallback {home_is_fallback}, {provider_hint:?}, {model:?}"
                        );
                    }
                }
            }
        }
    }

    /// A model each Card owns.
    fn owned_model(card: &str) -> &'static str {
        match card {
            "claude" => "claude-sonnet-4-5",
            "codex" => "gpt-5.5",
            "antigravity" => "gemini-3-pro",
            "grok" => "grok-4",
            "kimi" => "kimi-k2",
            "minimax" => "minimax-m2",
            _ => "glm-4.6",
        }
    }

    /// The Scan window is the last 30 days including today, for own-series, Routed and settled
    /// usage alike. Usage only on today-30 therefore never names its client in a source note.
    #[test]
    fn the_scan_window_is_the_last_30_days_including_today() {
        let pricing = test_bundled_pricing();
        for (days_ago, inside) in [(0, true), (29, true), (30, false), (31, false)] {
            let at = now() - Duration::days(days_ago);
            for home in CARDS {
                for card in CARDS {
                    let observation = UsageObservation::new(
                        at,
                        Some(owned_model(card)),
                        tokens(1_000, 100),
                        None,
                    );
                    let mut accumulator = DailyUsageAccumulator::default();
                    let landed =
                        record([observation], home, card, now(), &pricing, &mut accumulator);
                    assert_eq!(
                        landed.own_series, inside,
                        "{home} -> {card}, {days_ago} days ago"
                    );
                }
            }
            let observation =
                UsageObservation::new(at, Some("claude-sonnet-4-5"), tokens(1_000, 100), None);
            let settled = Settled {
                others: false,
                cost: Some(0.1),
                estimated: true,
                priced_tokens: 1_100,
                complete: true,
                unpriced_marker_only: false,
            };
            let mut accumulator = DailyUsageAccumulator::default();
            let landed = record_settled(
                [(observation, settled)],
                now(),
                true,
                &pricing,
                &mut accumulator,
            );
            assert_eq!(landed.own_series, inside, "settled, {days_ago} days ago");
        }
    }

    #[test]
    fn observations_outside_the_window_or_in_the_future_land_nowhere() {
        let pricing = test_bundled_pricing();
        for home in CARDS {
            for model in MODELS {
                for timestamp in [now() - Duration::days(30), now() + Duration::hours(1)] {
                    let observation =
                        UsageObservation::new(timestamp, model, tokens(1_000, 100), None);
                    assert_eq!(
                        landings(&pricing, home, observation),
                        0,
                        "{home}, {model:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_token_observations_with_nothing_paid_land_nowhere() {
        let pricing = test_bundled_pricing();
        for home in CARDS {
            for model in MODELS {
                for recorded_cost in [None, Some(0.0)] {
                    let observation =
                        UsageObservation::new(now(), model, tokens(0, 0), recorded_cost);
                    assert_eq!(
                        landings(&pricing, home, observation),
                        0,
                        "{home}, {model:?}, cost {recorded_cost:?}"
                    );
                }
            }
        }
    }

    /// Local runtime usage is free whatever was recorded, so with no tokens it lands nowhere.
    #[test]
    fn zero_token_observations_with_a_recorded_cost_land_exactly_once() {
        let pricing = test_bundled_pricing();
        for home in CARDS {
            for model in MODELS {
                let observation = UsageObservation::new(now(), model, tokens(0, 0), Some(0.25));
                let model = observation.model;
                let local_runtime = model.is_some_and(is_local_runtime_model);
                let lands_nowhere =
                    local_runtime || (observation.home_is_fallback && model.is_none());
                let expected = usize::from(!lands_nowhere);
                assert_eq!(
                    landings(&pricing, home, observation),
                    expected,
                    "{home}, {model:?}"
                );
            }
        }
    }

    #[test]
    fn an_empty_observation_kept_for_its_label_stays_unpriced() {
        let pricing = test_bundled_pricing();
        let observation = UsageObservation {
            keep_when_empty: true,
            ..UsageObservation::new(now(), Some("codex-auto-review"), tokens(0, 0), None)
        };
        let mut accumulator = DailyUsageAccumulator::default();
        let landed = record(
            [observation],
            "codex",
            "codex",
            now(),
            &pricing,
            &mut accumulator,
        );
        assert!(landed.own_series);
        let history = accumulator.build(now(), "test");
        assert_eq!(history.unknown_models, ["codex-auto-review"]);
        assert!(history.daily.is_empty());
    }

    /// The model breakdown of `history`'s last 30 days, as (label, variants) rows.
    fn breakdown(history: &crate::models::UsageHistory) -> Vec<(String, Vec<String>)> {
        history
            .last_30_days
            .as_ref()
            .and_then(|period| period.model_breakdown.as_ref())
            .map(|breakdown| {
                breakdown
                    .models
                    .iter()
                    .map(|entry| {
                        let variants = entry.variants.iter().flatten();
                        let variants = variants.map(|variant| variant.model.clone()).collect();
                        (entry.model.clone(), variants)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every Card labels a model by its display family and keeps the model as logged as its
    /// variant, whichever Home client the usage came from, priced or not.
    #[test]
    fn the_same_model_gets_the_same_family_label_on_every_card() {
        let pricing = test_bundled_pricing();
        for (model, priced) in [
            ("claude-4.5-sonnet-thinking", true),
            ("claude-unreleased-zz-9-fast", false),
        ] {
            let family = pricing.display_family(model);
            assert_ne!(family, model);
            for home in CARDS {
                let mut accumulator = DailyUsageAccumulator::default();
                let observation =
                    UsageObservation::new(now(), Some(model), tokens(1_000, 100), None);
                record(
                    [observation],
                    home,
                    "claude",
                    now(),
                    &pricing,
                    &mut accumulator,
                );
                let history = accumulator.build(now(), "test");
                assert_eq!(
                    breakdown(&history),
                    [(family.clone(), vec![model.to_owned()])],
                    "{model} from {home}"
                );
                let cost = history
                    .last_30_days
                    .and_then(|period| period.estimated_cost_usd);
                assert_eq!(cost.is_some(), priced, "{model} from {home}");
                let unknown = history.unknown_models.iter().any(|name| name == model);
                assert_eq!(unknown, !priced, "{model} from {home}");
            }
        }
    }

    /// Settled usage is labelled the same way as usage the module prices.
    #[test]
    fn settled_usage_gets_the_family_label() {
        let pricing = test_bundled_pricing();
        let model = "claude-4.5-sonnet-thinking";
        for (cost, complete) in [(Some(0.1), true), (Some(0.1), false), (None, true)] {
            let observation = UsageObservation::new(now(), Some(model), tokens(1_000, 100), None);
            let settled = Settled {
                others: false,
                cost,
                estimated: true,
                priced_tokens: 1_100,
                complete,
                unpriced_marker_only: false,
            };
            let mut accumulator = DailyUsageAccumulator::default();
            record_settled(
                [(observation, settled)],
                now(),
                true,
                &pricing,
                &mut accumulator,
            );
            assert_eq!(
                breakdown(&accumulator.build(now(), "test")),
                [(pricing.display_family(model), vec![model.to_owned()])],
                "{cost:?}, complete {complete}"
            );
        }
    }
}
