use std::{
    collections::HashSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    models::UsageHistory,
    pricing::{ModelPricing, ServiceTier, TokenBreakdown},
    storage::Storage,
};

use super::CodexError;
use crate::providers::local_usage::{
    card_usage::DailyUsageAccumulator,
    card_usage::{self, PriceSource, UsageObservation},
    home_clients::{self, Card, FoldContext},
    log_usage::{parse_log_timestamp, LocalLogs, LogCacheError, LogFormat},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenEvent {
    pub timestamp: DateTime<Utc>,
    pub model: String,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub reasoning: u64,
    pub total: u64,
    tier: ServiceTier,
    chatgpt_plan: bool,
    pub cache_write: u64,
    pub cache_write_12h: u64,
}

const LOG_CACHE_SCHEMA_VERSION: u8 = 6;

pub(crate) fn scan_routed_usage_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, CodexError> {
    let home = home_directory();
    let configured_home = crate::provider_environment::value("CODEX_HOME");
    let homes = codex_homes(configured_home.as_deref().map(OsStr::new), &home);
    let events = scan_codex_events(storage, &homes, now)?;
    Ok(aggregate_routed_into(
        &events,
        now,
        pricing,
        card_id,
        accumulator,
    ))
}

pub fn scan_local_usage(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
) -> Result<UsageHistory, CodexError> {
    let home = home_directory();
    let configured_home = crate::provider_environment::value("CODEX_HOME");
    let homes = codex_homes(configured_home.as_deref().map(OsStr::new), &home);
    let events = scan_codex_events(storage, &homes, now)?;

    home_clients::card_history(
        &home_clients::CLIENTS,
        &FoldContext {
            storage,
            now,
            pricing,
        },
        Card {
            id: "codex",
            name: "Codex",
        },
        "Codex logs",
        |accumulator| Ok(aggregate_into(&events, now, pricing, accumulator)),
    )
}

struct CodexLogs;

impl LogFormat for CodexLogs {
    type Event = TokenEvent;
    type Context = ();
    const PROVIDER_ID: &'static str = "codex";
    const SCHEMA_VERSION: u8 = LOG_CACHE_SCHEMA_VERSION;

    fn parse(content: &str) -> Vec<TokenEvent> {
        parse_jsonl(content)
    }

    fn timestamp(event: &TokenEvent) -> DateTime<Utc> {
        event.timestamp
    }
}

static LOGS: LocalLogs<CodexLogs> = LocalLogs::new();

/// Codex events are deduplicated while aggregating, not here.
fn scan_codex_events(
    storage: &Storage,
    homes: &[PathBuf],
    now: DateTime<Utc>,
) -> Result<std::sync::Arc<Vec<TokenEvent>>, CodexError> {
    let files = discover_session_files(homes)
        .into_iter()
        .map(|path| (path, ()));
    LOGS.load(storage, now, files).map_err(|error| match error {
        LogCacheError::Storage(_) => CodexError::Storage,
        LogCacheError::Encode(_) | LogCacheError::Unreadable => CodexError::LocalUsage,
    })
}

fn codex_homes(configured_home: Option<&OsStr>, home: &Path) -> Vec<PathBuf> {
    if let Some(configured_home) = configured_home.filter(|value| !value.is_empty()) {
        let configured_home = configured_home.to_string_lossy();
        if !configured_home.trim().is_empty() {
            return configured_home
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| expand_home(value, home))
                .collect();
        }
    }
    vec![home.join(".codex")]
}

fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    if let Some(relative) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return home.join(relative);
    }
    PathBuf::from(value)
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn discover_session_files(homes: &[PathBuf]) -> Vec<PathBuf> {
    let mut output = Vec::new();
    let mut seen_directories = HashSet::new();
    for home in homes {
        let sources = [home.join("sessions"), home.join("archived_sessions")]
            .into_iter()
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        let sources = if sources.is_empty() {
            vec![home.clone()]
        } else {
            sources
        };
        let mut seen_relative = HashSet::new();
        for source in sources {
            let source = fs::canonicalize(&source).unwrap_or(source);
            if !seen_directories.insert(source.clone()) {
                continue;
            }
            let mut source_files = WalkDir::new(&source)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| entry.into_path())
                .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
                .collect::<Vec<_>>();
            source_files.sort();
            for path in source_files {
                let relative = path.strip_prefix(&source).unwrap_or(&path).to_path_buf();
                if seen_relative.insert(relative) {
                    output.push(path);
                }
            }
        }
    }
    output
}

pub fn parse_jsonl(content: &str) -> Vec<TokenEvent> {
    let mut current_model: Option<String> = None;
    let mut current_tier = ServiceTier::Standard;
    let mut previous_totals: Option<RawUsage> = None;
    let mut saw_session_meta = false;
    let mut first_party = false;
    let mut replay_gate: Option<ChildReplayGate> = None;
    let mut events = Vec::new();

    for line in content.lines() {
        let is_turn_context = line.contains("\"type\":\"turn_context\"");
        let is_session_meta = !saw_session_meta && line.contains("\"type\":\"session_meta\"");
        let is_task_started = replay_gate.is_some() && line.contains("\"type\":\"task_started\"");
        let is_thread_settings = line.contains("\"type\":\"thread_settings_applied\"");
        if !is_turn_context
            && !is_session_meta
            && !is_task_started
            && !is_thread_settings
            && !line.contains("\"type\":\"token_count\"")
        {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let object_type = object.get("type").and_then(Value::as_str);
        let payload = object.get("payload");

        if object_type == Some("turn_context") {
            if let Some(tier) = payload.and_then(service_tier) {
                current_tier = ServiceTier::parse(tier);
            }
            if let Some(model) = model_name(object.get("payload")) {
                current_model = Some(model);
            }
            continue;
        }

        if object_type == Some("session_meta") && !saw_session_meta {
            saw_session_meta = true;
            first_party = payload
                .and_then(|p| p.get("model_provider"))
                .and_then(Value::as_str)
                == Some("openai");
            if payload.is_some_and(is_child_session_meta) {
                replay_gate = Some(
                    object
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .and_then(parse_log_timestamp)
                        .map(|timestamp| ChildReplayGate::UntilStartedAt(timestamp.timestamp()))
                        .unwrap_or(ChildReplayGate::UntilSelfTimedTaskStarted),
                );
            }
            continue;
        }

        let Some(payload) = payload else {
            continue;
        };
        if object_type != Some("event_msg") {
            continue;
        }

        let payload_type = payload.get("type").and_then(Value::as_str);
        if payload_type == Some("thread_settings_applied") {
            if let Some(raw) = payload
                .pointer("/thread_settings/service_tier")
                .or_else(|| payload.get("service_tier"))
            {
                current_tier = raw
                    .as_str()
                    .map(str::trim)
                    .map(ServiceTier::parse)
                    .unwrap_or(ServiceTier::Standard);
            }
            continue;
        }

        if payload_type == Some("task_started") {
            if replay_gate.is_some_and(|gate| gate.is_cleared(payload, object.get("timestamp"))) {
                replay_gate = None;
            }
            continue;
        }

        if payload_type != Some("token_count") {
            continue;
        }
        let Some(timestamp_raw) = object
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::trim)
        else {
            continue;
        };
        let Some(timestamp) = parse_log_timestamp(timestamp_raw) else {
            continue;
        };
        let info = payload.get("info");
        let totals = info
            .and_then(|value| value.get("total_token_usage"))
            .map(RawUsage::from_value);

        if replay_gate.is_some() {
            if let Some(totals) = totals {
                previous_totals = Some(totals);
            }
            continue;
        }

        if totals.is_some_and(|totals| previous_totals == Some(totals)) {
            continue;
        }

        let usage = if let Some(last) = info.and_then(|value| value.get("last_token_usage")) {
            RawUsage::from_value(last)
        } else if let Some(totals) = totals {
            totals.subtracting(previous_totals)
        } else {
            continue;
        };
        if let Some(totals) = totals {
            previous_totals = Some(totals);
        }
        if usage.input == 0 && usage.cached == 0 && usage.output == 0 && usage.reasoning == 0 {
            continue;
        }
        let parsed_model = model_name(Some(payload)).or_else(|| model_name(info));
        let model = resolve_model(parsed_model, &mut current_model);
        events.push(TokenEvent {
            timestamp,
            model,
            input: usage.input,
            cached: usage.cached.min(usage.input),
            output: usage.output,
            reasoning: usage.reasoning,
            total: usage.total,
            chatgpt_plan: first_party
                && matches!(
                    payload
                        .pointer("/rate_limits/plan_type")
                        .and_then(Value::as_str),
                    Some(
                        "free" | "go" | "plus" | "pro" | "team" | "business" | "enterprise" | "edu"
                    )
                ),
            tier: service_tier(payload)
                .or_else(|| info.and_then(service_tier))
                .map(ServiceTier::parse)
                .unwrap_or(current_tier),
            cache_write: usage.write,
            cache_write_12h: usage.write_12h,
        });
    }
    events
}

fn service_tier(payload: &Value) -> Option<&str> {
    [
        payload
            .get("thread_settings")
            .and_then(|settings| settings.get("service_tier")),
        payload.get("service_tier"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

#[derive(Debug, Clone, Copy)]
enum ChildReplayGate {
    UntilStartedAt(i64),
    UntilSelfTimedTaskStarted,
}

impl ChildReplayGate {
    fn is_cleared(self, payload: &Value, line_timestamp: Option<&Value>) -> bool {
        let Some(started_at) = payload.get("started_at").and_then(Value::as_f64) else {
            return false;
        };
        match self {
            Self::UntilStartedAt(gate) => started_at >= gate as f64,
            Self::UntilSelfTimedTaskStarted => line_timestamp
                .and_then(Value::as_str)
                .map(str::trim)
                .and_then(parse_log_timestamp)
                .is_some_and(|timestamp| started_at >= timestamp.timestamp() as f64),
        }
    }
}

fn is_child_session_meta(payload: &Value) -> bool {
    has_non_null_value(payload.get("forked_from_id"))
        || has_non_null_value(payload.get("parent_thread_id"))
        || payload.get("thread_source").and_then(Value::as_str) == Some("subagent")
        || payload
            .get("source")
            .and_then(|source| source.get("subagent"))
            .is_some_and(|value| has_non_null_value(Some(value)))
}

fn has_non_null_value(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(value)) => !value.trim().is_empty(),
        Some(_) => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RawUsage {
    write: u64,
    write_12h: u64,
    input: u64,
    cached: u64,
    output: u64,
    reasoning: u64,
    total: u64,
}

impl RawUsage {
    fn from_value(value: &Value) -> Self {
        let input = integer(value, &["input_tokens", "prompt_tokens", "input"]);
        let details = value.get("input_tokens_details");
        let cached = [
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cached_tokens",
        ]
        .iter()
        .find_map(|key| value.get(*key)?.as_u64())
        .or_else(|| details?.get("cached_tokens")?.as_u64())
        .unwrap_or_default();
        let write = value
            .get("cache_write_input_tokens")
            .and_then(Value::as_u64)
            .or_else(|| details?.get("cache_write_tokens")?.as_u64())
            .unwrap_or_default();
        let write_12h = details
            .and_then(|d| d.pointer("/cache_write_tokens_details/ttl_12h_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or_default()
            .min(write);
        let output = integer(value, &["output_tokens", "completion_tokens", "output"]);
        let reasoning = integer(value, &["reasoning_output_tokens", "reasoning_tokens"]);
        let reported = integer(value, &["total_tokens"]);
        let recomputed = input.saturating_add(output);
        Self {
            input,
            cached,
            write,
            write_12h,
            output,
            reasoning,
            total: if reported > 0 { reported } else { recomputed },
        }
    }

    fn subtracting(self, previous: Option<Self>) -> Self {
        let previous = previous.unwrap_or_default();
        Self {
            input: self.input.saturating_sub(previous.input),
            write: self.write.saturating_sub(previous.write),
            write_12h: self.write_12h.saturating_sub(previous.write_12h),
            cached: self.cached.saturating_sub(previous.cached),
            output: self.output.saturating_sub(previous.output),
            reasoning: self.reasoning.saturating_sub(previous.reasoning),
            total: self.total.saturating_sub(previous.total),
        }
    }
}

fn integer(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
        .unwrap_or_default()
}

fn model_name(value: Option<&Value>) -> Option<String> {
    let value = value?;
    [
        value.get("model"),
        value.get("model_name"),
        value
            .get("metadata")
            .and_then(|metadata| metadata.get("model")),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn resolve_model(parsed: Option<String>, current_model: &mut Option<String>) -> String {
    if let Some(parsed) = parsed.as_ref() {
        *current_model = Some(parsed.clone());
    }
    parsed.or_else(|| current_model.clone()).unwrap_or_default()
}

#[cfg(test)]
fn aggregate(events: &[TokenEvent], now: DateTime<Utc>, pricing: &ModelPricing) -> UsageHistory {
    let mut accumulator = DailyUsageAccumulator::default();
    aggregate_into(events, now, pricing, &mut accumulator);
    accumulator.build(now, "From your Codex logs (estimated)")
}

fn aggregate_into(
    events: &[TokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let landed = card_usage::record(
        unique(events).map(observation),
        "codex",
        "codex",
        now,
        pricing,
        accumulator,
    );
    landed.own_series || landed.others
}

fn aggregate_routed_into(
    events: &[TokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    card_usage::record(
        unique(events).map(observation),
        "codex",
        card_id,
        now,
        pricing,
        accumulator,
    )
    .own_series
}

/// Skips events Codex wrote more than once, keeping the first.
fn unique(events: &[TokenEvent]) -> impl Iterator<Item = &TokenEvent> {
    let mut seen = HashSet::new();
    events.iter().filter(move |event| {
        seen.insert((
            event.timestamp,
            event.model.as_str(),
            event.input,
            event.cached,
            event.output,
            event.reasoning,
            event.total,
            event.chatgpt_plan,
            event.tier,
            event.cache_write,
            event.cache_write_12h,
        ))
    })
}

fn observation(event: &TokenEvent) -> UsageObservation<'_> {
    UsageObservation {
        total_tokens: event.total,
        tier: event.tier,
        keep_when_empty: event.model.trim().eq_ignore_ascii_case("codex-auto-review"),
        price_source: PriceSource::CodexLogs {
            chatgpt_plan: event.chatgpt_plan,
        },
        ..UsageObservation::new(
            event.timestamp,
            Some(&event.model),
            TokenBreakdown {
                is_fast: event.tier == ServiceTier::Fast,
                ..event.pricing_tokens()
            },
            None,
        )
    }
}

#[cfg(test)]
fn estimate_cost(event: &TokenEvent, pricing: &ModelPricing) -> Option<f64> {
    card_usage::estimate_on_home_card(pricing, "codex", &observation(event))
}

impl TokenEvent {
    fn pricing_tokens(&self) -> TokenBreakdown {
        let uncached = self.input.saturating_sub(self.cached);
        let write = self.cache_write.min(uncached);
        let long_write = self.cache_write_12h.min(write);
        TokenBreakdown {
            input: uncached - write,
            cache_read: self.cached,
            cache_write_5m: write - long_write,
            cache_write_12h: long_write,
            output: self.output,
            ..TokenBreakdown::default()
        }
    }
}

#[cfg(test)]
#[path = "local_usage_tests.rs"]
mod tests;
