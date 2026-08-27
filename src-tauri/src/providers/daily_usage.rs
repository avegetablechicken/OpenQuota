use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};

use crate::models::{
    DailyUsage, ModelUsageBreakdown, ModelUsageEntry, ModelUsageVariant, OtherUsageHistory,
    OtherUsagePeriod, UsageHistory, UsagePeriod,
};

/// Provider-neutral accumulator for local-log usage. Claude, Codex, and pi keep their own parsing
/// and cost rules, then feed native events into the main series and intentionally excluded events
/// into the separate Others bucket.
#[derive(Default)]
pub struct DailyUsageAccumulator {
    days: BTreeMap<NaiveDate, DayAccumulator>,
    unknown_models_by_day: BTreeMap<NaiveDate, HashSet<String>>,
    other_days: BTreeMap<NaiveDate, OtherDayAccumulator>,
}

#[derive(Default)]
struct DayAccumulator {
    tokens: u64,
    cost: f64,
    has_cost: bool,
    cost_estimated: bool,
    models: HashMap<String, ModelAccumulator>,
}

struct OtherDayAccumulator {
    tokens: u64,
    priced_tokens: u64,
    cost: f64,
    has_cost: bool,
    cost_estimated: bool,
    estimate_complete: bool,
}

impl Default for OtherDayAccumulator {
    fn default() -> Self {
        Self {
            tokens: 0,
            priced_tokens: 0,
            cost: 0.0,
            has_cost: false,
            cost_estimated: false,
            estimate_complete: true,
        }
    }
}

struct ModelAccumulator {
    tokens: u64,
    cost: f64,
    cost_complete: bool,
    spellings: HashMap<String, u64>,
    variants: HashMap<String, VariantAccumulator>,
}

struct VariantAccumulator {
    tokens: u64,
    cost: f64,
    cost_complete: bool,
}

impl Default for ModelAccumulator {
    fn default() -> Self {
        Self {
            tokens: 0,
            cost: 0.0,
            cost_complete: true,
            spellings: HashMap::new(),
            variants: HashMap::new(),
        }
    }
}

impl Default for VariantAccumulator {
    fn default() -> Self {
        Self {
            tokens: 0,
            cost: 0.0,
            cost_complete: true,
        }
    }
}

impl ModelAccumulator {
    fn add(&mut self, spelling: &str, variant: Option<&str>, tokens: u64, cost: f64) {
        self.tokens = self.tokens.saturating_add(tokens);
        self.cost += cost;
        *self.spellings.entry(spelling.to_owned()).or_default() += tokens.max(1);
        if let Some(variant) = variant {
            let variant = self.variants.entry(variant.to_owned()).or_default();
            variant.tokens = variant.tokens.saturating_add(tokens);
            variant.cost += cost;
        }
    }

    fn merge(&mut self, other: &Self) {
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.cost += other.cost;
        self.cost_complete &= other.cost_complete;
        for (spelling, weight) in &other.spellings {
            *self.spellings.entry(spelling.clone()).or_default() += weight;
        }
        for (name, other_variant) in &other.variants {
            let variant = self.variants.entry(name.clone()).or_default();
            variant.tokens = variant.tokens.saturating_add(other_variant.tokens);
            variant.cost += other_variant.cost;
            variant.cost_complete &= other_variant.cost_complete;
        }
    }

    fn add_unpriced(&mut self, spelling: &str, tokens: u64) {
        self.tokens = self.tokens.saturating_add(tokens);
        self.cost_complete = false;
        *self.spellings.entry(spelling.to_owned()).or_default() += tokens.max(1);
    }

    fn display_name(&self) -> String {
        self.spellings
            .iter()
            .min_by(|(left_name, left_weight), (right_name, right_weight)| {
                right_weight
                    .cmp(left_weight)
                    .then_with(|| {
                        let left_lower = *left_name == &left_name.to_lowercase();
                        let right_lower = *right_name == &right_name.to_lowercase();
                        right_lower.cmp(&left_lower)
                    })
                    .then_with(|| left_name.cmp(right_name))
            })
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "Unattributed".to_owned())
    }
}

impl DailyUsageAccumulator {
    /// Adds usage whose dollar cost was derived from a pricing estimate.
    pub fn add(&mut self, date: NaiveDate, tokens: u64, cost: f64, model: &str) {
        self.add_internal(date, tokens, cost, model, None, true);
    }

    /// Adds usage whose dollar cost was recorded exactly by the source.
    #[allow(dead_code)]
    pub fn add_exact(&mut self, date: NaiveDate, tokens: u64, cost: f64, model: &str) {
        self.add_internal(date, tokens, cost, model, None, false);
    }

    /// Adds variant usage whose dollar cost was derived from a pricing estimate.
    pub fn add_variant(
        &mut self,
        date: NaiveDate,
        tokens: u64,
        cost: f64,
        family: &str,
        variant: &str,
    ) {
        let variant = normalized_model_name(variant);
        self.add_internal(date, tokens, cost, family, Some(variant), true);
    }

    /// Adds variant usage whose dollar cost was recorded exactly by the source.
    #[allow(dead_code)]
    pub fn add_exact_variant(
        &mut self,
        date: NaiveDate,
        tokens: u64,
        cost: f64,
        family: &str,
        variant: &str,
    ) {
        let variant = normalized_model_name(variant);
        self.add_internal(date, tokens, cost, family, Some(variant), false);
    }

    fn add_internal(
        &mut self,
        date: NaiveDate,
        tokens: u64,
        cost: f64,
        family: &str,
        variant: Option<&str>,
        cost_estimated: bool,
    ) {
        let family = normalized_model_name(family);
        let day = self.days.entry(date).or_default();
        day.tokens = day.tokens.saturating_add(tokens);
        day.cost += cost;
        day.has_cost = true;
        day.cost_estimated |= cost_estimated;
        day.models
            .entry(family.to_lowercase())
            .or_default()
            .add(family, variant, tokens, cost);
    }

    /// Keeps measured token history visible when the model has no usable pricing entry.
    pub fn add_unpriced(&mut self, date: NaiveDate, tokens: u64, model: &str) {
        if tokens == 0 {
            self.add_unknown_model(date, model);
            return;
        }
        let model = normalized_model_name(model);
        let day = self.days.entry(date).or_default();
        day.tokens = day.tokens.saturating_add(tokens);
        day.models
            .entry(model.to_lowercase())
            .or_default()
            .add_unpriced(model, tokens);
        self.add_unknown_model(date, model);
    }

    pub fn add_unknown_model(&mut self, date: NaiveDate, model: &str) {
        let model = model.trim();
        if !model.is_empty() {
            self.unknown_models_by_day
                .entry(date)
                .or_default()
                .insert(model.to_owned());
        }
    }

    /// Adds usage from a model intentionally excluded from the provider's normal history.
    /// It remains available to the local-device spend chart as one aggregated Others slice.
    pub fn add_other(
        &mut self,
        date: NaiveDate,
        tokens: u64,
        cost: Option<f64>,
        cost_estimated: bool,
    ) {
        if tokens == 0 && cost.is_none_or(|value| value <= 0.0) {
            return;
        }
        let day = self.other_days.entry(date).or_default();
        day.tokens = day.tokens.saturating_add(tokens);
        if let Some(cost) = cost.filter(|value| value.is_finite() && *value >= 0.0) {
            day.priced_tokens = day.priced_tokens.saturating_add(tokens);
            day.cost += cost;
            day.has_cost = true;
            day.cost_estimated |= cost_estimated;
        } else if tokens > 0 {
            day.estimate_complete = false;
        }
    }

    /// Builds the three spend periods. Idle or unknown-marker-only periods stay unbacked (`None`),
    /// while measured unpriced tokens remain available with an absent cost.
    pub fn build(self, now: DateTime<Utc>, source_note: &str) -> UsageHistory {
        let today = now.with_timezone(&Local).date_naive();
        let yesterday = today.checked_sub_days(Days::new(1));
        let other_usage = self.other_usage(now);
        let daily = self
            .days
            .iter()
            .rev()
            .filter(|(_, day)| has_usage(day))
            .map(|(date, day)| DailyUsage {
                date: date.to_string(),
                tokens: day.tokens,
                estimated_cost_usd: day.has_cost.then_some(day.cost),
                estimate_complete: self
                    .unknown_models_by_day
                    .get(date)
                    .is_none_or(HashSet::is_empty),
            })
            .collect();

        let today_period = self.period_for_days(&[today], source_note);
        let yesterday_period =
            yesterday.and_then(|date| self.period_for_days(&[date], source_note));
        let active_days = self
            .days
            .iter()
            .filter(|(_, day)| has_usage(day))
            .map(|(date, _)| *date)
            .collect::<Vec<_>>();
        let mut unknown_models = self
            .unknown_models_by_day
            .values()
            .flat_map(|models| models.iter().cloned())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        unknown_models.sort();
        let last_30_days = self
            .period_for_days(&active_days, source_note)
            .map(|mut period| {
                period.estimate_complete = unknown_models.is_empty();
                period.unknown_models.clone_from(&unknown_models);
                period
            });

        UsageHistory {
            today: today_period,
            yesterday: yesterday_period,
            last_30_days,
            daily,
            unknown_models,
            other_usage,
        }
    }

    fn other_usage(&self, now: DateTime<Utc>) -> Option<OtherUsageHistory> {
        if self.other_days.is_empty() {
            return None;
        }
        let today = now.with_timezone(&Local).date_naive();
        let yesterday = today.checked_sub_days(Days::new(1));
        let since = today.checked_sub_days(Days::new(30)).unwrap_or(today);
        let active_days = self
            .other_days
            .keys()
            .filter(|date| **date >= since && **date <= today)
            .copied()
            .collect::<Vec<_>>();
        Some(OtherUsageHistory {
            today: self.other_period_for_days(&[today]),
            yesterday: yesterday.and_then(|date| self.other_period_for_days(&[date])),
            last_30_days: self.other_period_for_days(&active_days),
        })
    }

    fn other_period_for_days(&self, dates: &[NaiveDate]) -> Option<OtherUsagePeriod> {
        let mut total = OtherDayAccumulator::default();
        for date in dates {
            if let Some(day) = self.other_days.get(date) {
                total.tokens = total.tokens.saturating_add(day.tokens);
                total.priced_tokens = total.priced_tokens.saturating_add(day.priced_tokens);
                total.cost += day.cost;
                total.has_cost |= day.has_cost;
                total.cost_estimated |= day.cost_estimated;
                total.estimate_complete &= day.estimate_complete;
            }
        }
        (total.tokens > 0 || total.has_cost).then(|| OtherUsagePeriod {
            tokens: total.tokens,
            priced_tokens: total.priced_tokens,
            estimated_cost_usd: total.has_cost.then_some(total.cost),
            cost_estimated: total.cost_estimated,
            estimate_complete: total.estimate_complete,
        })
    }

    fn period_for_days(&self, dates: &[NaiveDate], source_note: &str) -> Option<UsagePeriod> {
        let mut total = DayAccumulator::default();
        let mut unknown_models = HashSet::new();
        for date in dates {
            if let Some(day) = self.days.get(date) {
                total.tokens = total.tokens.saturating_add(day.tokens);
                total.cost += day.cost;
                total.has_cost |= day.has_cost;
                total.cost_estimated |= day.cost_estimated;
                for (key, model) in &day.models {
                    total.models.entry(key.clone()).or_default().merge(model);
                }
            }
            if let Some(unknown) = self.unknown_models_by_day.get(date) {
                unknown_models.extend(unknown.iter().cloned());
            }
        }
        if !has_usage(&total) {
            return None;
        }
        let mut unknown_models = unknown_models.into_iter().collect::<Vec<_>>();
        unknown_models.sort();
        Some(UsagePeriod {
            tokens: total.tokens,
            estimated_cost_usd: total.has_cost.then_some(total.cost),
            cost_estimated: total.cost_estimated,
            estimate_complete: unknown_models.is_empty(),
            model_breakdown: model_breakdown(&total, source_note),
            unknown_models,
        })
    }
}

fn normalized_model_name(model: &str) -> &str {
    let model = model.trim();
    if model.is_empty() {
        "Unattributed"
    } else {
        model
    }
}

fn has_usage(day: &DayAccumulator) -> bool {
    day.tokens > 0 || day.has_cost
}

fn model_breakdown(day: &DayAccumulator, source_note: &str) -> Option<ModelUsageBreakdown> {
    let mut entries = day
        .models
        .values()
        .filter(|model| model.tokens > 0 || model.cost > 0.0)
        .map(|model| {
            let display_name = model.display_name();
            let mut variants = model
                .variants
                .iter()
                .map(|(name, variant)| ModelUsageVariant {
                    model: name.clone(),
                    total_tokens: variant.tokens,
                    cost_usd: variant.cost_complete.then(|| round_to_cents(variant.cost)),
                })
                .collect::<Vec<_>>();
            variants.sort_by(variant_sort);
            let variants = if variants.is_empty()
                || (variants.len() == 1 && variants[0].model == display_name)
            {
                None
            } else {
                Some(variants)
            };
            ModelUsageEntry {
                model: display_name,
                total_tokens: model.tokens,
                cost_usd: model.cost_complete.then(|| round_to_cents(model.cost)),
                variants,
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .cost_usd
            .partial_cmp(&left.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.total_tokens.cmp(&left.total_tokens))
            .then_with(|| natural_model_cmp(&left.model, &right.model))
    });
    if entries.is_empty() {
        return None;
    }

    let all_priced = entries.iter().all(|entry| entry.cost_usd.is_some());
    let total_cost = entries
        .iter()
        .filter_map(|entry| entry.cost_usd)
        .sum::<f64>();
    let total_tokens = entries.iter().map(|entry| entry.total_tokens).sum::<u64>();
    let mut visible = Vec::new();
    let mut other_tokens = 0_u64;
    let mut other_cost = 0.0;
    let mut other_cost_complete = true;
    let mut other_variants = Vec::new();
    let mut named_count = 0;
    for entry in entries {
        let share = if all_priced && total_cost > 0.0 {
            entry.cost_usd.unwrap_or_default() / total_cost
        } else if total_tokens > 0 {
            entry.total_tokens as f64 / total_tokens as f64
        } else {
            0.0
        };
        let unattributed = entry.model.to_lowercase() == "unattributed";
        if unattributed || share < 0.05 || named_count >= 5 {
            other_tokens = other_tokens.saturating_add(entry.total_tokens);
            other_cost += entry.cost_usd.unwrap_or_default();
            other_cost_complete &= entry.cost_usd.is_some();
            other_variants.push(ModelUsageVariant {
                model: entry.model,
                total_tokens: entry.total_tokens,
                cost_usd: entry.cost_usd,
            });
        } else {
            visible.push(entry);
            named_count += 1;
        }
    }
    if other_tokens > 0 || other_cost > 0.0 {
        other_variants.sort_by(variant_sort);
        visible.push(ModelUsageEntry {
            model: "Other".to_owned(),
            total_tokens: other_tokens,
            cost_usd: other_cost_complete.then(|| round_to_cents(other_cost)),
            variants: Some(other_variants),
        });
    }
    Some(ModelUsageBreakdown {
        models: visible,
        source_note: source_note.to_owned(),
    })
}

fn variant_sort(left: &ModelUsageVariant, right: &ModelUsageVariant) -> Ordering {
    right
        .cost_usd
        .partial_cmp(&left.cost_usd)
        .unwrap_or(Ordering::Equal)
        .then_with(|| right.total_tokens.cmp(&left.total_tokens))
        .then_with(|| natural_model_cmp(&left.model, &right.model))
}

fn natural_model_cmp(left: &str, right: &str) -> Ordering {
    let left_folded = left.to_lowercase();
    let right_folded = right.to_lowercase();
    let mut left_chars = left_folded.chars().peekable();
    let mut right_chars = right_folded.chars().peekable();

    loop {
        match (left_chars.peek().copied(), right_chars.peek().copied()) {
            (Some(left_char), Some(right_char))
                if left_char.is_ascii_digit() && right_char.is_ascii_digit() =>
            {
                let left_digits = take_ascii_digits(&mut left_chars);
                let right_digits = take_ascii_digits(&mut right_chars);
                let left_significant = left_digits.trim_start_matches('0');
                let right_significant = right_digits.trim_start_matches('0');
                let left_significant = if left_significant.is_empty() {
                    "0"
                } else {
                    left_significant
                };
                let right_significant = if right_significant.is_empty() {
                    "0"
                } else {
                    right_significant
                };
                let ordering = left_significant
                    .len()
                    .cmp(&right_significant.len())
                    .then_with(|| left_significant.cmp(right_significant))
                    .then_with(|| left_digits.len().cmp(&right_digits.len()));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(left_char), Some(right_char)) => {
                left_chars.next();
                right_chars.next();
                let ordering = left_char.cmp(&right_char);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (None, None) => return left.cmp(right),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

fn take_ascii_digits<I>(chars: &mut std::iter::Peekable<I>) -> String
where
    I: Iterator<Item = char>,
{
    let mut digits = String::new();
    while chars.peek().is_some_and(char::is_ascii_digit) {
        digits.push(chars.next().expect("peeked digit must exist"));
    }
    digits
}

fn round_to_cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::DailyUsageAccumulator;

    fn day(year: i32, month: u32, day: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn idle_and_unknown_only_periods_stay_unbacked() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add_unknown_model(day(2026, 6, 26), "mystery");
        let history = accumulator.build(now, "From test logs");
        assert!(history.today.is_none());
        assert!(history.last_30_days.is_none());
        assert!(history.daily.is_empty());
        assert_eq!(history.unknown_models, ["mystery"]);
    }

    #[test]
    fn measured_unpriced_tokens_keep_history_without_inventing_cost() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add_unpriced(day(2026, 6, 26), 420, "future-model");

        let history = accumulator.build(now, "From test logs");
        let today = history.today.unwrap();
        assert_eq!(today.tokens, 420);
        assert_eq!(today.estimated_cost_usd, None);
        assert!(!today.estimate_complete);
        assert_eq!(today.unknown_models, ["future-model"]);
        assert_eq!(today.model_breakdown.unwrap().models[0].cost_usd, None);
        assert_eq!(history.daily[0].tokens, 420);
        assert_eq!(history.daily[0].estimated_cost_usd, None);
    }

    #[test]
    fn periods_include_only_priced_usage_and_period_scoped_unknowns() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add(day(2026, 6, 26), 300, 3.0, "beta");
        accumulator.add(day(2026, 6, 25), 700, 7.0, "alpha");
        accumulator.add_unknown_model(day(2026, 6, 25), "mystery");
        let history = accumulator.build(now, "From test logs");
        assert!(history.today.as_ref().unwrap().estimate_complete);
        assert!(history.today.as_ref().unwrap().cost_estimated);
        assert!(!history.yesterday.as_ref().unwrap().estimate_complete);
        assert_eq!(
            history.yesterday.as_ref().unwrap().unknown_models,
            ["mystery"]
        );
        assert_eq!(history.last_30_days.as_ref().unwrap().tokens, 1_000);
        assert_eq!(history.daily.len(), 2);
    }

    #[test]
    fn exact_only_costs_stay_exact_in_period_aggregates() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add_exact(day(2026, 6, 26), 300, 3.0, "alpha");
        accumulator.add_exact_variant(day(2026, 6, 25), 700, 7.0, "beta", "beta-2");

        let history = accumulator.build(now, "From exact records");

        assert!(!history.today.as_ref().unwrap().cost_estimated);
        assert!(!history.yesterday.as_ref().unwrap().cost_estimated);
        assert!(!history.last_30_days.as_ref().unwrap().cost_estimated);
    }

    #[test]
    fn one_estimated_cost_marks_its_period_and_combined_period_estimated() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add_exact(day(2026, 6, 26), 300, 3.0, "alpha");
        accumulator.add_variant(day(2026, 6, 26), 200, 2.0, "alpha", "alpha-2");
        accumulator.add_exact(day(2026, 6, 25), 700, 7.0, "beta");

        let history = accumulator.build(now, "From mixed records");

        assert!(history.today.as_ref().unwrap().cost_estimated);
        assert!(!history.yesterday.as_ref().unwrap().cost_estimated);
        assert!(history.last_30_days.as_ref().unwrap().cost_estimated);
    }

    #[test]
    fn last_30_days_unions_unknown_only_days_when_the_period_has_usage() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add(day(2026, 6, 26), 300, 3.0, "known");
        accumulator.add_unknown_model(day(2026, 6, 24), "mystery");

        let history = accumulator.build(now, "From test logs");
        assert!(history.today.as_ref().unwrap().estimate_complete);
        assert_eq!(
            history.last_30_days.as_ref().unwrap().unknown_models,
            ["mystery"]
        );
        assert!(!history.last_30_days.unwrap().estimate_complete);
    }

    #[test]
    fn model_breakdown_folds_small_excess_and_unattributed_rows() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let date = day(2026, 6, 26);
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add(date, 900, 90.0, "big");
        accumulator.add(date, 60, 6.0, "mid");
        accumulator.add(date, 30, 3.0, "tiny");
        accumulator.add(date, 400, 4.0, "");
        let history = accumulator.build(now, "From test logs");
        let models = &history.today.unwrap().model_breakdown.unwrap().models;
        assert_eq!(
            models
                .iter()
                .map(|entry| entry.model.as_str())
                .collect::<Vec<_>>(),
            ["big", "mid", "Other"]
        );
        assert_eq!(models.last().unwrap().total_tokens, 430);
        assert_eq!(
            models.last().unwrap().variants.as_ref().unwrap()[0].model,
            "Unattributed"
        );
    }

    #[test]
    fn case_variants_use_the_dominant_spelling() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let date = day(2026, 6, 26);
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add(date, 100, 1.0, "GLM-5.2");
        accumulator.add(date, 300, 3.0, "glm-5.2");
        let history = accumulator.build(now, "From test logs");
        let models = history.today.unwrap().model_breakdown.unwrap().models;
        assert_eq!(models[0].model, "glm-5.2");
        assert_eq!(models[0].total_tokens, 400);
        assert!(models[0].variants.is_none());
    }

    #[test]
    fn unicode_case_variants_collapse_into_one_model() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let date = day(2026, 6, 26);
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add(date, 100, 1.0, "MÖDEL-2");
        accumulator.add(date, 300, 3.0, "mödel-2");
        let models = accumulator
            .build(now, "From test logs")
            .today
            .unwrap()
            .model_breakdown
            .unwrap()
            .models;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "mödel-2");
        assert_eq!(models[0].total_tokens, 400);
    }

    #[test]
    fn tied_model_names_use_natural_numeric_order() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let date = day(2026, 6, 26);
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add(date, 100, 1.0, "model-10");
        accumulator.add(date, 100, 1.0, "model-2");
        let models = accumulator
            .build(now, "From test logs")
            .today
            .unwrap()
            .model_breakdown
            .unwrap()
            .models;
        assert_eq!(models[0].model, "model-2");
        assert_eq!(models[1].model, "model-10");
    }

    #[test]
    fn an_exact_five_percent_model_remains_visible() {
        let now = Utc.with_ymd_and_hms(2026, 6, 26, 12, 0, 0).unwrap();
        let date = day(2026, 6, 26);
        let mut accumulator = DailyUsageAccumulator::default();
        accumulator.add(date, 950, 95.0, "large");
        accumulator.add(date, 50, 5.0, "edge");
        let models = accumulator
            .build(now, "From test logs")
            .today
            .unwrap()
            .model_breakdown
            .unwrap()
            .models;
        assert_eq!(
            models
                .iter()
                .map(|entry| entry.model.as_str())
                .collect::<Vec<_>>(),
            ["large", "edge"]
        );
    }
}
