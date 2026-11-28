use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    pricing::{ModelPricing, TokenBreakdown},
    storage::Storage,
};

use super::{
    card_usage::DailyUsageAccumulator,
    card_usage::{self, UsageObservation},
    log_usage::{parse_log_timestamp, LocalLogs, LogCacheError, LogFormat},
    model_scope::{card_for_model, normalize_provider_model},
    session_model::{SessionModelCli, SessionModelTracker},
};

const LOG_CACHE_SCHEMA_VERSION: u8 = 12;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum PiUsageSource {
    Pi,
    OhMyPi,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PiUsageSources {
    pub pi: bool,
    pub oh_my_pi: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PiUsageEvent {
    id: Option<String>,
    timestamp: DateTime<Utc>,
    card_id: String,
    provider: String,
    model: String,
    #[serde(default)]
    model_inherited: bool,
    carried_cost: Option<f64>,
    tokens: PiTokenBreakdown,
    reported_total_tokens: u64,
    source: PiUsageSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
struct PiTokenBreakdown {
    input: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_read: u64,
    output: u64,
}

impl PiTokenBreakdown {
    fn pricing_tokens(self) -> TokenBreakdown {
        TokenBreakdown {
            input: self.input,
            cache_write_5m: self.cache_write_5m,
            cache_write_1h: self.cache_write_1h,
            cache_read: self.cache_read,
            output: self.output,
            ..TokenBreakdown::default()
        }
    }
}

/// Folds usage written by pi-compatible clients into the card of the underlying model provider.
/// Returns whether each client contributed usage or an unknown-model marker to that card.
pub fn scan_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<PiUsageSources, LogCacheError> {
    let home = home_directory();
    let directories = sessions_directories(
        env_text("PI_CODING_AGENT_SESSION_DIR").as_deref(),
        env_text("PI_CODING_AGENT_DIR").as_deref(),
        &home,
    );
    scan_directories_into(storage, &directories, now, pricing, card_id, accumulator)
}

fn scan_directories_into(
    storage: &Storage,
    directories: &[(PiUsageSource, PathBuf)],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<PiUsageSources, LogCacheError> {
    let files = directories.iter().flat_map(|(source, directory)| {
        discover_files(directory)
            .into_iter()
            .map(|path| (path, *source))
    });
    let events = LOGS.load(storage, now, files)?;

    Ok(aggregate_into(&events, card_id, now, pricing, accumulator))
}

struct PiLogs;

impl LogFormat for PiLogs {
    type Event = PiUsageEvent;
    /// The client the file's directory belongs to, which can change while the file does not.
    type Context = PiUsageSource;
    const PROVIDER_ID: &'static str = "pi";
    const SCHEMA_VERSION: u8 = LOG_CACHE_SCHEMA_VERSION;

    /// `attach` sets each event's real client.
    fn parse(content: &str) -> Vec<PiUsageEvent> {
        parse_jsonl(content, PiUsageSource::Pi)
    }

    fn timestamp(event: &PiUsageEvent) -> DateTime<Utc> {
        event.timestamp
    }

    /// The cached events keep the client the file belonged to when it was parsed.
    fn attach(event: &mut PiUsageEvent, source: &PiUsageSource) {
        event.source = *source;
    }

    fn deduplicate(events: Vec<PiUsageEvent>) -> Vec<PiUsageEvent> {
        deduplicate(events)
    }
}

static LOGS: LocalLogs<PiLogs> = LocalLogs::new();

fn sessions_directories(
    session_override: Option<&str>,
    config_override: Option<&str>,
    home: &Path,
) -> Vec<(PiUsageSource, PathBuf)> {
    let pi = if let Some(path) = session_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        expand_home(path, home)
    } else if let Some(path) = config_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        expand_home(path, home).join("sessions")
    } else {
        home.join(".pi").join("agent").join("sessions")
    };
    let oh_my_pi = home.join(".omp").join("agent").join("sessions");
    let pi_canonical = fs::canonicalize(&pi).unwrap_or_else(|_| pi.clone());
    let oh_my_pi_canonical = fs::canonicalize(&oh_my_pi).unwrap_or_else(|_| oh_my_pi.clone());
    if pi_canonical == oh_my_pi_canonical {
        vec![(PiUsageSource::OhMyPi, oh_my_pi)]
    } else {
        vec![(PiUsageSource::Pi, pi), (PiUsageSource::OhMyPi, oh_my_pi)]
    }
}

fn discover_files(directory: &Path) -> Vec<PathBuf> {
    let directory = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    let mut paths = WalkDir::new(directory)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn parse_jsonl(content: &str, source: PiUsageSource) -> Vec<PiUsageEvent> {
    let mut trackers = HashMap::<String, SessionModelTracker>::new();
    content
        .lines()
        .filter(|line| line.contains("\"usage\""))
        .filter_map(|line| parse_line_with_tracker(line, source, &mut trackers))
        .collect()
}

#[cfg(test)]
fn parse_line(line: &str, source: PiUsageSource) -> Option<PiUsageEvent> {
    parse_line_with_tracker(line, source, &mut HashMap::new())
}

fn parse_line_with_tracker(
    line: &str,
    source: PiUsageSource,
    trackers: &mut HashMap<String, SessionModelTracker>,
) -> Option<PiUsageEvent> {
    let object = serde_json::from_str::<Value>(line).ok()?;
    if object.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let timestamp = parse_log_timestamp(object.get("timestamp")?.as_str()?)?;
    let message = object.get("message")?.as_object()?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let provider = message.get("provider")?.as_str()?;
    let raw_model = message
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let explicit_model = normalize_provider_model(provider, raw_model);
    let provider_key = provider.trim().to_ascii_lowercase();
    let resolved = trackers
        .entry(provider_key)
        .or_default()
        .resolve((!explicit_model.is_empty()).then_some(explicit_model));
    let model = resolved.model.unwrap_or_default();
    let card_id = mapped_card(provider, &model)?;
    let usage = message.get("usage")?.as_object()?;
    let cache_write = unsigned_number(usage.get("cacheWrite")).unwrap_or_default();
    let cache_write_1h = unsigned_number(usage.get("cacheWrite1h")).unwrap_or_default();
    let carried_cost = usage
        .get("cost")
        .and_then(Value::as_object)
        .and_then(|cost| finite_number(cost.get("total")));

    Some(PiUsageEvent {
        id: object.get("id").and_then(Value::as_str).map(str::to_owned),
        timestamp,
        card_id: card_id.to_owned(),
        provider: provider.to_owned(),
        model,
        model_inherited: resolved.inherited,
        carried_cost,
        tokens: PiTokenBreakdown {
            input: unsigned_number(usage.get("input")).unwrap_or_default(),
            cache_write_5m: cache_write.saturating_sub(cache_write_1h),
            cache_write_1h,
            cache_read: unsigned_number(usage.get("cacheRead")).unwrap_or_default(),
            output: unsigned_number(usage.get("output")).unwrap_or_default(),
        },
        reported_total_tokens: unsigned_number(usage.get("totalTokens")).unwrap_or_default(),
        source,
    })
}

fn mapped_card(provider: &str, model: &str) -> Option<&'static str> {
    let normalized_model = normalize_provider_model(provider, model);
    if let Some(card) = card_for_model(&normalized_model) {
        return Some(card);
    }
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude-agent-sdk" => Some("claude"),
        "openai" | "openai-codex" => Some("codex"),
        "xai" | "x-ai" | "xai-oauth" | "grok" => Some("grok"),
        "kimi" | "kimi-code" | "kimi-coding" | "moonshot" | "moonshot-cn" | "moonshotai" => {
            Some("kimi")
        }
        "minimax" => Some("minimax"),
        "zai" | "z-ai" | "z_ai" | "zhipu" | "zhipuai" | "bigmodel" => Some("zai"),
        "google-antigravity" | "antigravity" | "antigravity-cli" => Some("antigravity"),
        _ => None,
    }
}

/// pi entry ids are short and unique only within one session file, so a copied or forked entry is
/// recognised by its id together with its timestamp, model and usage; distinct messages that
/// happen to share an id across sessions are both kept.
fn deduplicate(events: Vec<PiUsageEvent>) -> Vec<PiUsageEvent> {
    let mut seen = HashSet::new();
    events
        .into_iter()
        .filter(|event| {
            event.id.as_ref().is_none_or(|id| {
                seen.insert((
                    id.clone(),
                    event.timestamp,
                    event.provider.clone(),
                    event.model.clone(),
                    event.tokens,
                    event.reported_total_tokens,
                    event.carried_cost.map(f64::to_bits),
                ))
            })
        })
        .collect()
}

/// Each event's Card was chosen at parse, from its model and then its provider, so only events
/// already on `card_id` are recorded, as that Card's own usage. A source counts when any of its
/// usage lands on the Card, Others included.
fn aggregate_into(
    events: &[PiUsageEvent],
    card_id: &str,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> PiUsageSources {
    let mut contributed = PiUsageSources::default();
    for event in events.iter().filter(|event| event.card_id == card_id) {
        let landed = card_usage::record(
            [observation(event)],
            card_id,
            card_id,
            now,
            pricing,
            accumulator,
        );
        if landed.own_series || landed.others {
            mark_source(&mut contributed, event.source);
        }
    }
    contributed
}

/// pi counts its reported total, and a carried cost of zero means none was recorded. The provider
/// tells whether a Local runtime served the usage.
fn observation(event: &PiUsageEvent) -> UsageObservation<'_> {
    let session_model_cli = event.model_inherited.then_some(match event.source {
        PiUsageSource::Pi => SessionModelCli::Pi,
        PiUsageSource::OhMyPi => SessionModelCli::OhMyPi,
    });
    UsageObservation {
        total_tokens: event.reported_total_tokens,
        provider_hint: Some(&event.provider),
        session_model_cli,
        ..UsageObservation::new(
            event.timestamp,
            Some(&event.model),
            event.tokens.pricing_tokens(),
            event.carried_cost.filter(|cost| *cost > 0.0),
        )
    }
}

fn mark_source(sources: &mut PiUsageSources, source: PiUsageSource) {
    match source {
        PiUsageSource::Pi => sources.pi = true,
        PiUsageSource::OhMyPi => sources.oh_my_pi = true,
    }
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Number(value) => value.as_f64()?,
        Value::String(value) => value.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

fn unsigned_number(value: Option<&Value>) -> Option<u64> {
    let number = finite_number(value)?;
    if number < 0.0 || number > u64::MAX as f64 {
        return None;
    }
    Some(number.trunc() as u64)
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests;
