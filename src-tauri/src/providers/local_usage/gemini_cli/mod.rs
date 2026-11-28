use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use crate::{
    pricing::{ModelPricing, TokenBreakdown},
    providers::local_usage::{
        card_usage::{self, DailyUsageAccumulator, UsageObservation},
        log_usage::{parse_log_timestamp, LocalLogs, LogCacheError, LogFormat},
        session_model::{inherit_in_session, SessionModelCli},
    },
    storage::Storage,
};

const LOG_CACHE_SCHEMA_VERSION: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GeminiCliTokenEvent {
    id: Option<String>,
    session_id: Option<String>,
    timestamp: DateTime<Utc>,
    model: Option<String>,
    #[serde(default)]
    model_inherited: bool,
    input: u64,
    cache_read: u64,
    output: u64,
}

impl GeminiCliTokenEvent {
    fn total_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }

    fn pricing_tokens(&self) -> TokenBreakdown {
        TokenBreakdown {
            input: self.input,
            cache_read: self.cache_read,
            output: self.output,
            ..TokenBreakdown::default()
        }
    }
}

/// Scans Gemini CLI (`~/.gemini/tmp/*/chats/session-*.json` and `.jsonl`) logs into `accumulator`.
///
/// Gemini models (`gemini-*`) and unowned models without a dedicated card route to `"antigravity"`
/// (main series and `other_usage` respectively), while models owned by another first-party card
/// route once to that card. Messages are deduplicated globally by message ID (and fallback
/// `(session_id, timestamp, model, counts)` when an ID is absent) so resumed, copied, or
/// dual-format (`.json` + `.jsonl`) sessions are never double-counted.
pub(crate) fn scan_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, LogCacheError> {
    let tmp_dirs = default_tmp_directories();
    scan_tmp_directories_into(storage, &tmp_dirs, now, pricing, card_id, accumulator)
}

fn default_tmp_directories() -> Vec<PathBuf> {
    let home = home_directory();
    tmp_directories(
        crate::provider_environment::value("GEMINI_CLI_HOME").as_deref(),
        &home,
    )
}

fn tmp_directories(configured_home: Option<&str>, home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |path: PathBuf| {
        let canonical = fs::canonicalize(&path).unwrap_or(path);
        if seen.insert(canonical.clone()) {
            dirs.push(canonical);
        }
    };

    if let Some(configured) = configured_home
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        for raw in configured
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let root = expand_home(raw, home);
            let nested_tmp = root.join(".gemini").join("tmp");
            let direct_tmp = root.join("tmp");
            if nested_tmp.is_dir() {
                push_unique(nested_tmp);
            } else if direct_tmp.is_dir() {
                push_unique(direct_tmp);
            } else {
                push_unique(nested_tmp);
            }
        }
        return dirs;
    }

    push_unique(home.join(".gemini").join("tmp"));
    dirs
}

fn scan_tmp_directories_into(
    storage: &Storage,
    tmp_dirs: &[PathBuf],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, LogCacheError> {
    let events = load_local_events(storage, tmp_dirs, now)?;
    Ok(aggregate_into(&events, now, pricing, card_id, accumulator))
}

struct GeminiCliLogs;

impl LogFormat for GeminiCliLogs {
    type Event = GeminiCliTokenEvent;
    type Context = ();
    const PROVIDER_ID: &'static str = "gemini-cli";
    const SCHEMA_VERSION: u8 = LOG_CACHE_SCHEMA_VERSION;

    fn parse(content: &str) -> Vec<GeminiCliTokenEvent> {
        parse_chat_file(content)
    }

    fn timestamp(event: &GeminiCliTokenEvent) -> DateTime<Utc> {
        event.timestamp
    }

    fn deduplicate(events: Vec<GeminiCliTokenEvent>) -> Vec<GeminiCliTokenEvent> {
        deduplicate(events)
    }
}

static LOGS: LocalLogs<GeminiCliLogs> = LocalLogs::new();

fn load_local_events(
    storage: &Storage,
    tmp_dirs: &[PathBuf],
    now: DateTime<Utc>,
) -> Result<std::sync::Arc<Vec<GeminiCliTokenEvent>>, LogCacheError> {
    let files = discover_chat_files(tmp_dirs)
        .into_iter()
        .map(|path| (path, ()));
    LOGS.load(storage, now, files)
}

fn discover_chat_files(tmp_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    for tmp_dir in tmp_dirs {
        if !tmp_dir.is_dir() {
            continue;
        }
        for project_dir in child_directories(tmp_dir) {
            let chats_dir = project_dir.join("chats");
            if !chats_dir.is_dir() {
                continue;
            }
            for entry in WalkDir::new(&chats_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                let Some(ext) = path.extension().and_then(|value| value.to_str()) else {
                    continue;
                };
                if !matches!(ext, "json" | "jsonl") {
                    continue;
                }
                let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                // Manual checkpoints (`/chat save`) snapshot existing conversation history and
                // must never be counted alongside live session files.
                if file_name.starts_with("checkpoint-") {
                    continue;
                }
                let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                if seen.insert(canonical.clone()) {
                    paths.push(canonical);
                }
            }
        }
    }
    paths.sort();
    paths
}

fn child_directories(path: &Path) -> Vec<PathBuf> {
    let mut directories = fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

fn parse_chat_file(content: &str) -> Vec<GeminiCliTokenEvent> {
    let trimmed = content.trim();
    if trimmed.is_empty() || !trimmed.contains("\"tokens\"") {
        return Vec::new();
    }
    if let Ok(Value::Object(root)) = serde_json::from_str::<Value>(trimmed) {
        if let Some(messages) = root.get("messages").and_then(Value::as_array) {
            let session_id = non_empty_str(root.get("sessionId"));
            let fallback_timestamp = root
                .get("lastUpdated")
                .or_else(|| root.get("startTime"))
                .and_then(Value::as_str)
                .and_then(parse_log_timestamp);
            let mut events = messages
                .iter()
                .filter_map(|item| {
                    parse_message_record(
                        item.as_object()?,
                        session_id.as_deref(),
                        fallback_timestamp,
                    )
                })
                .collect::<Vec<_>>();
            inherit_in_session(&mut events, |event| {
                (&mut event.model, &mut event.model_inherited)
            });
            return events;
        }
        // Single-object message record fallback (including single-line wrapped JSONL logs).
        let record = root
            .get("message")
            .and_then(Value::as_object)
            .unwrap_or(&root);
        let fallback_timestamp = root
            .get("timestamp")
            .or_else(|| root.get("lastUpdated"))
            .or_else(|| root.get("startTime"))
            .and_then(Value::as_str)
            .and_then(parse_log_timestamp);
        return parse_message_record(
            record,
            non_empty_str(root.get("sessionId")).as_deref(),
            fallback_timestamp,
        )
        .into_iter()
        .collect();
    }

    // JSONL session or subagent log format.
    let mut session_id: Option<String> = None;
    let mut events = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.contains("\"tokens\"") && !line.contains("\"sessionId\"") {
            continue;
        }
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = non_empty_str(map.get("sessionId")) {
            session_id = Some(id);
        }
        if !line.contains("\"tokens\"") {
            continue;
        }
        let record = map
            .get("message")
            .and_then(Value::as_object)
            .unwrap_or(&map);
        let fallback_timestamp = map
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_log_timestamp);
        if let Some(event) = parse_message_record(record, session_id.as_deref(), fallback_timestamp)
        {
            events.push(event);
        }
    }
    inherit_in_session(&mut events, |event| {
        (&mut event.model, &mut event.model_inherited)
    });
    events
}

fn parse_message_record(
    record: &Map<String, Value>,
    session_id: Option<&str>,
    fallback_timestamp: Option<DateTime<Utc>>,
) -> Option<GeminiCliTokenEvent> {
    let message_type = record
        .get("type")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase());
    if message_type.as_deref() != Some("gemini") {
        return None;
    }
    let tokens = record.get("tokens").and_then(Value::as_object)?;
    let (input, cache_read, output) = extract_token_buckets(tokens)?;
    let timestamp = record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_log_timestamp)
        .or(fallback_timestamp)?;
    let id = non_empty_str(record.get("id"));
    let session_id =
        non_empty_str(record.get("sessionId")).or_else(|| session_id.map(str::to_owned));
    let model = non_empty_str(record.get("model"));

    Some(GeminiCliTokenEvent {
        id,
        session_id,
        timestamp,
        model,
        model_inherited: false,
        input,
        cache_read,
        output,
    })
}

/// Converts Gemini CLI `TokensSummary` (`input`, `cached`, `output`, `thoughts`, `tool`, `total`)
/// into disjoint `(uncached_input, cache_read, billed_output)` buckets without double-counting
/// `cached` tokens that are already included in Gemini's `promptTokenCount` (`input`).
fn extract_token_buckets(tokens: &Map<String, Value>) -> Option<(u64, u64, u64)> {
    let raw_input = unsigned_u64(tokens.get("input")).unwrap_or_default();
    let cached = unsigned_u64(tokens.get("cached")).unwrap_or_default();
    let output = unsigned_u64(tokens.get("output")).unwrap_or_default();
    let thoughts = unsigned_u64(tokens.get("thoughts")).unwrap_or_default();
    let tool = unsigned_u64(tokens.get("tool")).unwrap_or_default();
    let reported_total = unsigned_u64(tokens.get("total")).unwrap_or_default();

    let input_excludes_cached = cached > 0
        && reported_total > 0
        && raw_input
            .saturating_add(cached)
            .saturating_add(output)
            .saturating_add(thoughts)
            .saturating_add(tool)
            == reported_total;

    let uncached_input = if input_excludes_cached {
        raw_input
    } else {
        raw_input.saturating_sub(cached)
    }
    .saturating_add(tool);
    let billed_output = output.saturating_add(thoughts);
    let total = uncached_input
        .saturating_add(cached)
        .saturating_add(billed_output);
    (total > 0).then_some((uncached_input, cached, billed_output))
}

fn deduplicate(events: Vec<GeminiCliTokenEvent>) -> Vec<GeminiCliTokenEvent> {
    let mut deduplicated: Vec<GeminiCliTokenEvent> = Vec::with_capacity(events.len());
    let mut by_id = HashMap::<String, usize>::new();
    let mut seen_without_id =
        HashSet::<(Option<String>, i64, Option<String>, u64, u64, u64)>::new();

    for event in events {
        if let Some(id) = event.id.as_ref() {
            if let Some(&existing_index) = by_id.get(id) {
                if event.total_tokens() > deduplicated[existing_index].total_tokens() {
                    deduplicated[existing_index] = event;
                }
                continue;
            }
            by_id.insert(id.clone(), deduplicated.len());
            deduplicated.push(event);
        } else {
            let key = (
                event.session_id.clone(),
                event.timestamp.timestamp_millis(),
                event.model.clone(),
                event.input,
                event.cache_read,
                event.output,
            );
            if seen_without_id.insert(key) {
                deduplicated.push(event);
            }
        }
    }
    deduplicated
}

/// Gemini CLI's usage lands on the antigravity Card, which owns Gemini models.
fn aggregate_into(
    events: &[GeminiCliTokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    card_usage::record(
        events.iter().map(|event| UsageObservation {
            session_model_cli: event.model_inherited.then_some(SessionModelCli::GeminiCli),
            ..UsageObservation::new(
                event.timestamp,
                event.model.as_deref(),
                event.pricing_tokens(),
                None,
            )
        }),
        "antigravity",
        card_id,
        now,
        pricing,
        accumulator,
    )
    .own_series
}

fn unsigned_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                Some(value)
            } else {
                let float = number.as_f64()?;
                (float.is_finite() && float >= 0.0 && float <= u64::MAX as f64)
                    .then_some(float.trunc() as u64)
            }
        }
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
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
