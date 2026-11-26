use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use walkdir::WalkDir;

use crate::{
    pricing::{ModelPricing, TokenBreakdown},
    providers::{
        daily_usage::DailyUsageAccumulator,
        log_usage::{load_or_parse_log, parse_log_timestamp, LogCacheError},
        model_scope::{card_for_model, native_log_disposition, LocalModelDisposition},
    },
    storage::Storage,
};

const LOG_CACHE_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GeminiCliTokenEvent {
    id: Option<String>,
    session_id: Option<String>,
    timestamp: DateTime<Utc>,
    model: Option<String>,
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
    let since_date = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);
    let events = load_local_events(storage, tmp_dirs, since_date)?;
    Ok(aggregate_into(events, now, pricing, card_id, accumulator))
}

fn load_local_events(
    storage: &Storage,
    tmp_dirs: &[PathBuf],
    since_date: NaiveDate,
) -> Result<Vec<GeminiCliTokenEvent>, LogCacheError> {
    let paths = discover_chat_files(tmp_dirs);
    let mut seen_paths = HashSet::with_capacity(paths.len());
    let mut events = Vec::new();

    for path in paths {
        seen_paths.insert(path.clone());
        let Some(parsed) = load_or_parse_log(
            storage,
            "gemini-cli",
            &path,
            LOG_CACHE_SCHEMA_VERSION,
            parse_chat_file,
        )?
        else {
            continue;
        };
        events.extend(
            parsed
                .into_iter()
                .filter(|event| event.timestamp.with_timezone(&Local).date_naive() >= since_date),
        );
    }
    storage.prune_log_events("gemini-cli", &seen_paths)?;
    Ok(deduplicate(events))
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
            return messages
                .iter()
                .filter_map(|item| {
                    parse_message_record(
                        item.as_object()?,
                        session_id.as_deref(),
                        fallback_timestamp,
                    )
                })
                .collect();
        }
        // Single-object message record fallback.
        return parse_message_record(&root, non_empty_str(root.get("sessionId")).as_deref(), None)
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

fn aggregate_into(
    events: Vec<GeminiCliTokenEvent>,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let since = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);
    let mut contributed = false;

    for event in events {
        if event.timestamp > now {
            continue;
        }
        let date = event.timestamp.with_timezone(&Local).date_naive();
        if date < since {
            continue;
        }
        let total = event.total_tokens();
        if total == 0 {
            continue;
        }
        let tokens = event.pricing_tokens();

        if card_id == "antigravity" {
            let Some(model) = event.model.as_deref().filter(|value| !value.is_empty()) else {
                accumulator.add_unpriced(date, total, "Unattributed");
                contributed = true;
                continue;
            };
            match native_log_disposition("antigravity", model) {
                LocalModelDisposition::Home => {
                    add_priced_event(accumulator, date, &event, model, "antigravity", pricing);
                    contributed = true;
                }
                LocalModelDisposition::RoutedTo(_) => {}
                LocalModelDisposition::Other => {
                    let cost = pricing.estimated_cost_at(
                        "antigravity",
                        model,
                        event.timestamp,
                        tokens,
                        true,
                    );
                    accumulator.add_other(date, total, cost, true, model);
                }
            }
        } else {
            let Some(model) = event.model.as_deref().filter(|value| !value.is_empty()) else {
                continue;
            };
            if card_for_model(model) != Some(card_id) {
                continue;
            }
            add_priced_event(accumulator, date, &event, model, card_id, pricing);
            contributed = true;
        }
    }

    contributed
}

fn add_priced_event(
    accumulator: &mut DailyUsageAccumulator,
    date: NaiveDate,
    event: &GeminiCliTokenEvent,
    model: &str,
    provider: &str,
    pricing: &ModelPricing,
) {
    let total = event.total_tokens();
    let tokens = event.pricing_tokens();
    if let Some(cost) = pricing.estimated_cost_at(provider, model, event.timestamp, tokens, true) {
        let family = pricing.display_family(model);
        if family != model {
            accumulator.add_variant(date, total, cost, &family, model);
        } else {
            accumulator.add(date, total, cost, &family);
        }
    } else {
        accumulator.add_unpriced(date, total, model);
    }
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
mod tests {
    use std::{fs, path::Path};

    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    use super::{parse_chat_file, scan_tmp_directories_into, tmp_directories};
    use crate::{
        pricing::test_bundled_pricing, providers::daily_usage::DailyUsageAccumulator,
        storage::Storage,
    };

    fn session_json(session_id: &str, messages: Value) -> String {
        serde_json::json!({
            "sessionId": session_id,
            "projectHash": "abc123",
            "startTime": "2026-11-20T10:00:00.000Z",
            "lastUpdated": "2026-11-20T10:05:00.000Z",
            "messages": messages,
        })
        .to_string()
    }

    use serde_json::Value;

    #[test]
    fn tmp_directories_respect_gemini_cli_home_and_deduplicate() {
        let dir = tempdir().unwrap();
        let nested_home = dir.path().join("custom-home");
        let nested_tmp = nested_home.join(".gemini").join("tmp");
        fs::create_dir_all(&nested_tmp).unwrap();
        assert_eq!(
            tmp_directories(Some(nested_home.to_str().unwrap()), dir.path()),
            [fs::canonicalize(&nested_tmp).unwrap()]
        );

        // Pointing directly at a `.gemini` directory also resolves `<dir>/tmp`.
        let direct_gemini = nested_home.join(".gemini");
        assert_eq!(
            tmp_directories(Some(direct_gemini.to_str().unwrap()), dir.path()),
            [fs::canonicalize(&nested_tmp).unwrap()]
        );

        assert_eq!(
            tmp_directories(None, Path::new("/home/tester")),
            [Path::new("/home/tester/.gemini/tmp")]
        );
    }

    #[test]
    fn token_buckets_avoid_double_counting_cached_tokens_and_include_thoughts_and_tool() {
        let content = session_json(
            "sess-1",
            serde_json::json!([
                {
                    "id": "u1",
                    "timestamp": "2026-11-20T10:00:00.000Z",
                    "type": "user",
                    "content": "hi"
                },
                {
                    "id": "m1",
                    "timestamp": "2026-11-20T10:01:00.000Z",
                    "type": "gemini",
                    "model": "gemini-4-neon",
                    "tokens": {
                        "input": 1_000_000,
                        "cached": 400_000,
                        "output": 80_000,
                        "thoughts": 20_000,
                        "tool": 50_000,
                        "total": 1_150_000
                    }
                }
            ]),
        );
        let events = parse_chat_file(&content);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].input, 650_000);
        assert_eq!(events[0].cache_read, 400_000);
        assert_eq!(events[0].output, 100_000);
        assert_eq!(events[0].total_tokens(), 1_150_000);
    }

    #[test]
    fn scanner_deduplicates_resumed_and_dual_format_sessions_and_skips_checkpoints() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
        let pricing = test_bundled_pricing();
        let now = Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap();

        let tmp_dir = dir.path().join(".gemini").join("tmp");
        let project = tmp_dir.join("proj_hash");
        let chats = project.join("chats");
        fs::create_dir_all(&chats).unwrap();

        let msg_a = serde_json::json!({
            "id": "msg-1111",
            "timestamp": "2026-11-20T10:00:00.000Z",
            "type": "gemini",
            "model": "gemini-4-neon",
            "tokens": {
                "input": 1_000_000,
                "cached": 0,
                "output": 100_000,
                "thoughts": 0,
                "tool": 0,
                "total": 1_100_000
            }
        });
        let msg_b = serde_json::json!({
            "id": "msg-2222",
            "timestamp": "2026-11-20T10:05:00.000Z",
            "type": "gemini",
            "model": "gemini-4-neon",
            "tokens": {
                "input": 500_000,
                "cached": 0,
                "output": 50_000,
                "thoughts": 0,
                "tool": 0,
                "total": 550_000
            }
        });

        // Original session file.
        fs::write(
            chats.join("session-2026-11-20T10-00-aaaa.json"),
            session_json("sess-a", serde_json::json!([msg_a.clone()])),
        )
        .unwrap();

        // Resumed/copied session repeating msg-1111 and adding msg-2222.
        fs::write(
            chats.join("session-2026-11-20T10-05-bbbb.json"),
            session_json("sess-b", serde_json::json!([msg_a.clone(), msg_b])),
        )
        .unwrap();

        // JSONL copy of msg-1111 + a subagent message with a distinct ID.
        let subagent_dir = chats.join("sess-a");
        fs::create_dir_all(&subagent_dir).unwrap();
        let subagent_jsonl = [
            msg_a.to_string(),
            serde_json::json!({
                "id": "msg-sub-3333",
                "timestamp": "2026-11-20T10:06:00.000Z",
                "type": "gemini",
                "model": "gemini-4-neon",
                "tokens": {
                    "input": 200_000,
                    "cached": 0,
                    "output": 0,
                    "thoughts": 0,
                    "tool": 0,
                    "total": 200_000
                }
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(subagent_dir.join("subagent-1.jsonl"), subagent_jsonl).unwrap();

        // Checkpoints outside and inside chats/ must be ignored.
        let checkpoint_msg = serde_json::json!({
            "id": "msg-checkpoint-9999",
            "timestamp": "2026-11-20T10:07:00.000Z",
            "type": "gemini",
            "model": "gemini-4-neon",
            "tokens": { "input": 9_000_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 9_000_000 }
        });
        fs::write(
            project.join("checkpoint-saved.json"),
            session_json("chk-1", serde_json::json!([checkpoint_msg.clone()])),
        )
        .unwrap();
        fs::write(
            chats.join("checkpoint-inline.json"),
            session_json("chk-2", serde_json::json!([checkpoint_msg])),
        )
        .unwrap();

        let mut antigravity_acc = DailyUsageAccumulator::default();
        assert!(scan_tmp_directories_into(
            &storage,
            &[tmp_dir],
            now,
            &pricing,
            "antigravity",
            &mut antigravity_acc,
        )
        .unwrap());
        let history = antigravity_acc.build(now, "From your Gemini CLI history (estimated)");
        let today = history.today.unwrap();
        // msg-1111 (1.1M) + msg-2222 (0.55M) + msg-sub-3333 (0.2M) = 1.85M tokens.
        assert_eq!(today.tokens, 1_850_000);
        // gemini-4-neon: $3/M input, $15/M output -> 1.7M*$3 + 0.15M*$15 = $5.10 + $2.25 = $7.35.
        assert!((today.estimated_cost_usd.unwrap() - 7.35).abs() < 1e-6);
        assert!(today.unknown_models.is_empty());
    }

    #[test]
    fn scanner_routes_non_gemini_models_once_without_double_counting() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
        let pricing = test_bundled_pricing();
        let now = Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap();

        let tmp_dir = dir.path().join(".gemini").join("tmp");
        let chats = tmp_dir.join("proj_hash").join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(
            chats.join("session-mixed.json"),
            session_json(
                "sess-mixed",
                serde_json::json!([
                    {
                        "id": "m-gemini",
                        "timestamp": "2026-11-20T10:00:00.000Z",
                        "type": "gemini",
                        "model": "gemini-4-neon",
                        "tokens": { "input": 100_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 100_000 }
                    },
                    {
                        "id": "m-claude",
                        "timestamp": "2026-11-20T10:01:00.000Z",
                        "type": "gemini",
                        "model": "claude-sonnet-4-6",
                        "tokens": { "input": 200_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 200_000 }
                    },
                    {
                        "id": "m-deepseek",
                        "timestamp": "2026-11-20T10:02:00.000Z",
                        "type": "gemini",
                        "model": "deepseek-flash",
                        "tokens": { "input": 300_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 300_000 }
                    }
                ]),
            ),
        )
        .unwrap();

        let roots = [tmp_dir];
        let mut antigravity_acc = DailyUsageAccumulator::default();
        assert!(scan_tmp_directories_into(
            &storage,
            &roots,
            now,
            &pricing,
            "antigravity",
            &mut antigravity_acc
        )
        .unwrap());
        let antigravity = antigravity_acc.build(now, "test");
        assert_eq!(antigravity.today.unwrap().tokens, 100_000);
        assert_eq!(
            antigravity.other_usage.unwrap().today.unwrap().tokens,
            300_000
        );

        let mut claude_acc = DailyUsageAccumulator::default();
        assert!(scan_tmp_directories_into(
            &storage,
            &roots,
            now,
            &pricing,
            "claude",
            &mut claude_acc
        )
        .unwrap());
        let claude = claude_acc.build(now, "test");
        assert_eq!(claude.today.unwrap().tokens, 200_000);
        assert!(claude.other_usage.is_none());
    }
}
