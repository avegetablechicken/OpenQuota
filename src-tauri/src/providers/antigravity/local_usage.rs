use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    models::UsageHistory,
    pricing::{ModelPricing, TokenBreakdown},
    storage::Storage,
};

use super::AntigravityError;
use crate::providers::{
    card_usage::DailyUsageAccumulator,
    card_usage::{self, UsageObservation},
    home_clients::{self, Card, FoldContext, HomeClient},
    log_usage::{parse_log_timestamp, LocalLogs, LogFormat},
};

pub(super) const USAGE_SOURCE_NOTE: &str = "From your Antigravity transcripts (estimated)";
const LOG_CACHE_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AntigravityTokenEvent {
    pub timestamp: DateTime<Utc>,
    pub step_index: Option<u64>,
    pub model: Option<String>,
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl AntigravityTokenEvent {
    fn total_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }

    fn pricing_tokens(&self) -> TokenBreakdown {
        TokenBreakdown {
            input: self.input,
            cache_write_5m: self.cache_write,
            cache_read: self.cache_read,
            output: self.output,
            ..TokenBreakdown::default()
        }
    }
}

fn default_roots() -> Vec<PathBuf> {
    let home = home_directory();
    antigravity_roots(
        env_text("ANTIGRAVITY_HOME").as_deref(),
        env_text("ANTIGRAVITY_DATA_DIR").as_deref(),
        &home,
    )
}

struct AntigravityLogs;

impl LogFormat for AntigravityLogs {
    type Event = AntigravityTokenEvent;
    type Context = ();
    const PROVIDER_ID: &'static str = "antigravity";
    const SCHEMA_VERSION: u8 = LOG_CACHE_SCHEMA_VERSION;

    fn parse(content: &str) -> Vec<AntigravityTokenEvent> {
        parse_jsonl(content)
    }

    fn timestamp(event: &AntigravityTokenEvent) -> DateTime<Utc> {
        event.timestamp
    }

    fn deduplicate(events: Vec<AntigravityTokenEvent>) -> Vec<AntigravityTokenEvent> {
        deduplicate(events)
    }
}

static LOGS: LocalLogs<AntigravityLogs> = LocalLogs::new();

fn load_local_events(
    storage: &Storage,
    roots: &[PathBuf],
    now: DateTime<Utc>,
) -> Result<std::sync::Arc<Vec<AntigravityTokenEvent>>, AntigravityError> {
    let files = discover_transcript_files(roots)
        .into_iter()
        .map(|path| (path, ()));
    LOGS.load(storage, now, files)
        .map_err(|_| AntigravityError::LocalUsage)
}

fn deduplicate(events: Vec<AntigravityTokenEvent>) -> Vec<AntigravityTokenEvent> {
    let mut seen = HashSet::with_capacity(events.len());
    events
        .into_iter()
        .filter(|event| {
            seen.insert((
                event.timestamp,
                event.step_index,
                event.model.clone(),
                event.input,
                event.cache_write,
                event.cache_read,
                event.output,
            ))
        })
        .collect()
}

pub(crate) fn scan_routed_usage_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, AntigravityError> {
    let roots = default_roots();
    scan_routed_roots_into(storage, &roots, now, pricing, card_id, accumulator)
}

fn scan_routed_roots_into(
    storage: &Storage,
    roots: &[PathBuf],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, AntigravityError> {
    let events = load_local_events(storage, roots, now)?;
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
) -> Result<UsageHistory, AntigravityError> {
    let roots = default_roots();
    scan_roots(storage, &roots, now, pricing, &home_clients::CLIENTS)
}

fn scan_roots(
    storage: &Storage,
    roots: &[PathBuf],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    clients: &[&dyn HomeClient],
) -> Result<UsageHistory, AntigravityError> {
    let events = load_local_events(storage, roots, now)?;

    home_clients::card_history(
        clients,
        &FoldContext {
            storage,
            now,
            pricing,
        },
        Card {
            id: "antigravity",
            name: "Antigravity",
        },
        "Antigravity transcripts",
        |accumulator| Ok(aggregate_into(&events, now, pricing, accumulator)),
    )
}

fn antigravity_roots(
    configured_home: Option<&str>,
    configured_data_dir: Option<&str>,
    home: &Path,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    let mut add_if_valid = |mut root: PathBuf| {
        if root.file_name().and_then(|name| name.to_str()) == Some("brain") {
            root.pop();
        }
        if !root.join("brain").is_dir() {
            return;
        }
        let canonical = fs::canonicalize(&root).unwrap_or(root);
        if seen.insert(canonical.clone()) {
            roots.push(canonical);
        }
    };

    let configured = configured_home
        .or(configured_data_dir)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(configured) = configured {
        for value in configured
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            add_if_valid(expand_home(value, home));
        }
        roots.sort();
        return roots;
    }

    let gemini_dir = home.join(".gemini");
    for candidate in child_directories(&gemini_dir) {
        let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "antigravity" || name.starts_with("antigravity-") {
            add_if_valid(candidate);
        }
    }
    roots.sort();
    roots
}

fn discover_transcript_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut by_conversation = HashMap::<String, (usize, u64, Option<std::time::SystemTime>)>::new();
    for root in roots {
        let brain = root.join("brain");
        for conversation in child_directories(&brain) {
            let logs = conversation.join(".system_generated").join("logs");
            let compact = logs.join("transcript.jsonl");
            let full = logs.join("transcript_full.jsonl");
            let candidate = if compact.is_file() {
                compact
            } else if full.is_file() {
                full
            } else {
                continue;
            };
            let canonical = fs::canonicalize(&candidate).unwrap_or(candidate);
            if !seen_paths.insert(canonical.clone()) {
                continue;
            }
            let metadata = fs::metadata(&canonical).ok();
            let size = metadata.as_ref().map(|meta| meta.len()).unwrap_or_default();
            let modified = metadata.and_then(|meta| meta.modified().ok());
            if let Some(conv_key) = conversation
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_ascii_lowercase)
            {
                if let Some((index, best_size, best_modified)) = by_conversation.get_mut(&conv_key)
                {
                    if (size, modified) > (*best_size, *best_modified) {
                        paths[*index] = canonical;
                        *best_size = size;
                        *best_modified = modified;
                    }
                    continue;
                }
                by_conversation.insert(conv_key, (paths.len(), size, modified));
            }
            paths.push(canonical);
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

fn env_text(name: &str) -> Option<String> {
    crate::provider_environment::value(name)
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

pub fn parse_jsonl(content: &str) -> Vec<AntigravityTokenEvent> {
    let mut events: Vec<AntigravityTokenEvent> = Vec::new();
    let mut by_step = HashMap::<u64, usize>::new();

    for line in content.lines() {
        if !line.contains("_tokens") {
            continue;
        }
        let Some(event) = parse_line(line) else {
            continue;
        };
        if let Some(step_index) = event.step_index {
            if let Some(&existing_index) = by_step.get(&step_index) {
                if event.total_tokens() >= events[existing_index].total_tokens() {
                    events[existing_index] = event;
                }
                continue;
            }
            by_step.insert(step_index, events.len());
        }
        events.push(event);
    }
    events
}

fn parse_line(line: &str) -> Option<AntigravityTokenEvent> {
    let object: Value = serde_json::from_str(line).ok()?;
    let timestamp = object
        .get("created_at")
        .or_else(|| object.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(parse_log_timestamp)?;
    let input = integer(object.get("input_tokens")).unwrap_or_default();
    let cache_read = integer(
        object
            .get("cache_read_tokens")
            .or_else(|| object.get("cache_read_input_tokens")),
    )
    .unwrap_or_default();
    let cache_write = integer(
        object
            .get("cache_write_tokens")
            .or_else(|| object.get("cache_creation_tokens"))
            .or_else(|| object.get("cache_creation_input_tokens")),
    )
    .unwrap_or_default();
    let output = integer(object.get("output_tokens")).unwrap_or_default();
    let total = input
        .saturating_add(cache_write)
        .saturating_add(cache_read)
        .saturating_add(output);
    if total == 0 {
        return None;
    }
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let step_index = integer(object.get("step_index"));

    Some(AntigravityTokenEvent {
        timestamp,
        step_index,
        model,
        input,
        cache_write,
        cache_read,
        output,
    })
}

fn integer(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(integer) = value.as_u64() {
        return Some(integer);
    }
    let float = value.as_f64()?;
    if float.is_finite() && float >= 0.0 && float.fract() == 0.0 && float <= u64::MAX as f64 {
        Some(float as u64)
    } else {
        None
    }
}

fn aggregate_into(
    events: &[AntigravityTokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let landed = card_usage::record(
        events.iter().map(observation),
        "antigravity",
        "antigravity",
        now,
        pricing,
        accumulator,
    );
    landed.own_series || landed.others
}

fn aggregate_routed_into(
    events: &[AntigravityTokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    card_usage::record(
        events.iter().map(observation),
        "antigravity",
        card_id,
        now,
        pricing,
        accumulator,
    )
    .own_series
}

fn observation(event: &AntigravityTokenEvent) -> UsageObservation<'_> {
    UsageObservation::new(
        event.timestamp,
        event.model.as_deref(),
        event.pricing_tokens(),
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{DateTime, Local, TimeZone, Utc};
    use tempfile::tempdir;

    use super::{
        antigravity_roots, discover_transcript_files, parse_jsonl, scan_roots,
        scan_routed_roots_into, USAGE_SOURCE_NOTE,
    };
    use crate::{
        pricing::test_bundled_pricing, providers::card_usage::DailyUsageAccumulator,
        storage::Storage,
    };

    fn local_noon(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .single()
            .expect("local noon should be valid")
            .to_utc()
    }

    fn write_transcript(root: &std::path::Path, conversation: &str, name: &str, body: &str) {
        let logs = root
            .join("brain")
            .join(conversation)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join(name), body).unwrap();
    }

    #[test]
    fn discovers_antigravity_and_cli_roots_without_double_counting_full_transcripts() {
        let home = tempdir().unwrap();
        let ide_root = home.path().join(".gemini").join("antigravity");
        let cli_root = home.path().join(".gemini").join("antigravity-cli");
        let unrelated = home.path().join(".gemini").join("skills");
        fs::create_dir_all(&unrelated).unwrap();

        write_transcript(&ide_root, "conv-ide", "transcript.jsonl", "");
        write_transcript(&ide_root, "conv-ide", "transcript_full.jsonl", "");
        write_transcript(&cli_root, "conv-cli", "transcript.jsonl", "");
        write_transcript(&cli_root, "conv-full-only", "transcript_full.jsonl", "");

        let roots = antigravity_roots(None, None, home.path());
        assert_eq!(roots.len(), 2);

        let files = discover_transcript_files(&roots);
        assert_eq!(files.len(), 3);
        assert!(files
            .iter()
            .any(|path| path.ends_with("conv-ide/.system_generated/logs/transcript.jsonl")));
        assert!(files
            .iter()
            .any(|path| path.ends_with("conv-cli/.system_generated/logs/transcript.jsonl")));
        assert!(files.iter().any(|path| {
            path.ends_with("conv-full-only/.system_generated/logs/transcript_full.jsonl")
        }));
        assert!(!files
            .iter()
            .any(|path| path.ends_with("conv-ide/.system_generated/logs/transcript_full.jsonl")));
    }

    #[test]
    fn parses_and_deduplicates_steps_while_skipping_non_token_lines() {
        let content = concat!(
            r#"{"step_index":0,"type":"USER_INPUT","created_at":"2026-11-20T12:00:00Z","content":"hello"}"#,
            "\n",
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"RUNNING","created_at":"2026-11-20T12:00:01Z","model":"gemini-4-neon-high","input_tokens":100,"cache_read_tokens":50,"output_tokens":10}"#,
            "\n",
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T12:00:02Z","model":"gemini-4-neon-high","input_tokens":100,"cache_read_tokens":50,"output_tokens":25}"#,
            "\n",
            r#"{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T12:00:05Z","model":"claude-opus-4-6-thinking","input_tokens":200,"cache_write_tokens":100,"cache_read_tokens":300,"output_tokens":40}"#,
            "\n",
            "{not valid json\n"
        );

        let events = parse_jsonl(content);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].step_index, Some(1));
        assert_eq!(events[0].input, 100);
        assert_eq!(events[0].cache_read, 50);
        assert_eq!(events[0].output, 25);
        assert_eq!(events[0].total_tokens(), 175);

        assert_eq!(events[1].step_index, Some(2));
        assert_eq!(events[1].model.as_deref(), Some("claude-opus-4-6-thinking"));
        assert_eq!(events[1].cache_write, 100);
        assert_eq!(events[1].total_tokens(), 640);
    }

    #[test]
    fn scans_prices_known_models_and_keeps_unpriced_tokens_visible() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        let root = directory.path().join(".gemini").join("antigravity-cli");
        let now = local_noon(2026, 11, 21);
        let today_ts = (now - chrono::Duration::minutes(10)).to_rfc3339();
        let yesterday_ts = (now - chrono::Duration::days(1)).to_rfc3339();

        let body = format!(
            concat!(
                r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{today}","model":"gemini-4-neon-high","input_tokens":100000,"cache_read_tokens":200000,"output_tokens":10000}}"#,
                "\n",
                r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{today}","model":"claude-sonnet-4-6","input_tokens":100000,"cache_read_tokens":0,"output_tokens":10000}}"#,
                "\n",
                r#"{{"step_index":3,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{yesterday}","model":"gemini-mystery-preview","input_tokens":50000,"cache_read_tokens":0,"output_tokens":5000}}"#,
                "\n"
            ),
            today = today_ts,
            yesterday = yesterday_ts,
        );
        write_transcript(&root, "conv-1", "transcript.jsonl", &body);

        let pricing = test_bundled_pricing();
        let history =
            scan_roots(&storage, std::slice::from_ref(&root), now, &pricing, &[]).unwrap();

        let today = history.today.expect("today usage should exist");
        assert_eq!(today.tokens, 310_000);
        assert!(today.estimate_complete);
        assert!(today.unknown_models.is_empty());
        // gemini-4-neon: 0.1M * $3 + 0.2M * $0.15 + 0.01M * $15 = $0.30 + $0.03 + $0.15 = $0.48
        // claude-sonnet-4-6 is routed to Claude and excluded from Antigravity.
        let cost = today.estimated_cost_usd.expect("today cost should exist");
        assert!((cost - 0.48).abs() < 0.000_001, "unexpected cost: {cost}");
        let breakdown = today.model_breakdown.expect("breakdown should exist");
        assert_eq!(breakdown.source_note, USAGE_SOURCE_NOTE);
        assert_eq!(breakdown.models.len(), 1);
        let neon = breakdown
            .models
            .iter()
            .find(|entry| entry.model == "gemini-4-neon")
            .expect("gemini-4-neon family should exist");
        assert_eq!(neon.total_tokens, 310_000);
        let variants = neon
            .variants
            .as_deref()
            .expect("gemini-4-neon variants should exist");
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].model, "gemini-4-neon-high");

        let yesterday = history.yesterday.expect("yesterday usage should exist");
        assert_eq!(yesterday.tokens, 55_000);
        assert_eq!(yesterday.estimated_cost_usd, None);
        assert!(!yesterday.estimate_complete);
        assert_eq!(yesterday.unknown_models, ["gemini-mystery-preview"]);

        let last30 = history.last_30_days.expect("30d usage should exist");
        assert_eq!(last30.tokens, 365_000);
        assert!(!last30.estimate_complete);
        assert_eq!(last30.unknown_models, ["gemini-mystery-preview"]);

        let mut claude_accumulator = DailyUsageAccumulator::default();
        assert!(scan_routed_roots_into(
            &storage,
            std::slice::from_ref(&root),
            now,
            &pricing,
            "claude",
            &mut claude_accumulator,
        )
        .unwrap());
        let claude_history = claude_accumulator.build(now, "From Antigravity");
        let claude_today = claude_history
            .today
            .expect("routed Claude usage should exist");
        assert_eq!(claude_today.tokens, 110_000);
        // claude-sonnet-4-6: 0.1M * $3 + 0.01M * $15 = $0.45
        let claude_cost = claude_today
            .estimated_cost_usd
            .expect("routed Claude cost should exist");
        assert!(
            (claude_cost - 0.45).abs() < 0.000_001,
            "unexpected routed Claude cost: {claude_cost}"
        );
        assert!(claude_history.yesterday.is_none());
    }

    #[test]
    fn deduplicates_same_conversation_across_roots_preferring_more_complete_transcript() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        let now = local_noon(2026, 11, 21);
        let ts1 = (now - chrono::Duration::minutes(20)).to_rfc3339();
        let ts2 = (now - chrono::Duration::minutes(10)).to_rfc3339();

        let backup_root = directory.path().join(".gemini").join("antigravity-backup");
        let live_root = directory.path().join(".gemini").join("antigravity-cli");
        let step1 = format!(
            r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{ts1}","model":"gemini-4-neon-high","input_tokens":10000,"cache_read_tokens":0,"output_tokens":1000}}"#
        );
        let step2 = format!(
            r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{ts2}","model":"gemini-4-neon-high","input_tokens":20000,"cache_read_tokens":5000,"output_tokens":2000}}"#
        );

        write_transcript(&backup_root, "conv-shared", "transcript.jsonl", &step1);
        write_transcript(
            &live_root,
            "conv-shared",
            "transcript.jsonl",
            &format!("{step1}\n{step2}\n"),
        );

        let roots = [backup_root, live_root];
        let files = discover_transcript_files(&roots);
        assert_eq!(files.len(), 1);
        assert!(files[0].to_string_lossy().contains("antigravity-cli"));

        let history = scan_roots(&storage, &roots, now, &test_bundled_pricing(), &[]).unwrap();
        assert_eq!(history.today.unwrap().tokens, 38_000);
    }

    #[test]
    fn deduplicates_forked_history_events_across_conversations_while_keeping_new_steps() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        let root = directory.path().join(".gemini").join("antigravity-cli");
        let now = local_noon(2026, 11, 21);
        let ts_parent = (now - chrono::Duration::minutes(20)).to_rfc3339();
        let ts_fork = (now - chrono::Duration::minutes(5)).to_rfc3339();

        let shared_step = format!(
            r#"{{"step_index":1,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{ts_parent}","model":"gemini-4-neon-high","input_tokens":10000,"cache_read_tokens":0,"output_tokens":1000}}"#
        );
        let fork_step = format!(
            r#"{{"step_index":2,"type":"PLANNER_RESPONSE","status":"DONE","created_at":"{ts_fork}","model":"gemini-4-neon-high","input_tokens":5000,"cache_read_tokens":10000,"output_tokens":500}}"#
        );

        write_transcript(&root, "conv-parent", "transcript.jsonl", &shared_step);
        write_transcript(
            &root,
            "conv-fork",
            "transcript.jsonl",
            &format!("{shared_step}\n{fork_step}\n"),
        );

        let history = scan_roots(
            &storage,
            std::slice::from_ref(&root),
            now,
            &test_bundled_pricing(),
            &[],
        )
        .unwrap();
        // shared_step (11_000) counted once + fork_step (15_500) = 26_500.
        assert_eq!(history.today.unwrap().tokens, 26_500);
    }

    /// Pins the output of antigravity's per-event policy, so moving it into `card_usage` cannot
    /// change what any Card shows.
    mod characterization {
        use chrono::{DateTime, Duration, TimeZone, Utc};

        use super::super::{aggregate_into, aggregate_routed_into, AntigravityTokenEvent};
        use crate::{
            pricing::test_bundled_pricing,
            providers::{
                card_usage::test_support::{assert_pinned, relative_json},
                card_usage::DailyUsageAccumulator,
            },
        };

        fn now() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap()
        }

        fn event(
            days_ago: i64,
            model: Option<&str>,
            input: u64,
            output: u64,
        ) -> AntigravityTokenEvent {
            AntigravityTokenEvent {
                timestamp: now() - Duration::days(days_ago),
                step_index: None,
                model: model.map(str::to_owned),
                input,
                cache_write: 0,
                cache_read: 0,
                output,
            }
        }

        fn events() -> Vec<AntigravityTokenEvent> {
            let mut cached = event(0, Some("gemini-3-pro-preview"), 10_000, 500);
            cached.cache_write = 2_000;
            cached.cache_read = 6_000;
            let mut future = event(0, Some("gemini-3-pro"), 9_999, 9_999);
            future.timestamp = now() + Duration::hours(1);
            vec![
                // Home models whose label is a family plus a variant, or the family itself.
                cached,
                event(0, Some("gemini-3-pro"), 2_000, 200),
                event(0, Some("gemini-3.1-pro-high"), 2_000, 200),
                event(1, Some("gemini-3.5-flash-lite-tiered-high"), 1_000, 100),
                event(1, Some("gemini/gemini-3-pro-preview"), 1_000, 100),
                event(1, Some("  gemini-3.5-flash  "), 1_000, 100),
                // gpt-oss counts as Home on the antigravity Card.
                event(1, Some("gpt-oss-120b"), 500, 50),
                // A gemini-family model without a price, and a missing model.
                event(2, Some("gemini-99-unreleased"), 700, 70),
                event(2, None, 400, 40),
                // Routed to other Cards.
                event(3, Some("claude-sonnet-4-5"), 1_500, 150),
                event(3, Some("claude-opus-4-6"), 1_500, 150),
                event(3, Some("gpt-5.5"), 1_500, 150),
                event(3, Some("grok-4"), 800, 80),
                event(3, Some("glm-4.6"), 600, 60),
                event(3, Some("kimi-k2"), 600, 60),
                // Others: priced, unpriced, and local runtime.
                event(4, Some("deepseek-chat"), 900, 90),
                event(4, Some("totally-unknown-model"), 50, 5),
                event(4, Some("ollama/llama3"), 50, 5),
                // Zero tokens, guarded everywhere.
                event(5, Some("gemini-3-pro"), 0, 0),
                event(5, Some("deepseek-chat"), 0, 0),
                event(5, Some("claude-sonnet-4-5"), 0, 0),
                event(5, None, 0, 0),
                // Window edges.
                event(29, Some("gemini-3-pro"), 100, 10),
                event(29, Some("claude-sonnet-4-5"), 100, 10),
                event(30, Some("gemini-3-pro"), 100, 10),
                event(30, Some("claude-sonnet-4-5"), 100, 10),
                event(30, Some("deepseek-chat"), 100, 10),
                future,
            ]
        }

        #[test]
        fn native_card_output_is_pinned() {
            let mut accumulator = DailyUsageAccumulator::default();
            aggregate_into(&events(), now(), &test_bundled_pricing(), &mut accumulator);
            let history = accumulator.build(now(), "test");
            assert_pinned("native", &relative_json(&history, now()), NATIVE);
        }

        #[test]
        fn routed_output_is_pinned_per_card() {
            let actual = ["claude", "codex", "grok", "zai", "kimi", "minimax"]
                .map(|card_id| {
                    let mut accumulator = DailyUsageAccumulator::default();
                    let contributed = aggregate_routed_into(
                        &events(),
                        now(),
                        &test_bundled_pricing(),
                        card_id,
                        &mut accumulator,
                    );
                    let json = relative_json(&accumulator.build(now(), "test"), now());
                    format!("== {card_id} contributed={contributed}\n{json}")
                })
                .join("\n");
            assert_pinned("routed", &actual, ROUTED);
        }

        const NATIVE: &str = r#"
{
  "today": {
    "tokens": 22900,
    "estimatedCostUsd": 0.044000000000000004,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 20700,
          "costUsd": 0.0376,
          "variants": [
            {
              "model": "gemini-3-pro-preview",
              "totalTokens": 18500,
              "costUsd": 0.0312
            },
            {
              "model": "gemini-3-pro",
              "totalTokens": 2200,
              "costUsd": 0.0064
            }
          ]
        },
        {
          "model": "gemini-3.1-pro-preview",
          "totalTokens": 2200,
          "costUsd": 0.0064,
          "variants": [
            {
              "model": "gemini-3.1-pro-high",
              "totalTokens": 2200,
              "costUsd": 0.0064
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 3850,
    "estimatedCostUsd": 0.005705,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 1100,
          "costUsd": 0.0032,
          "variants": [
            {
              "model": "gemini/gemini-3-pro-preview",
              "totalTokens": 1100,
              "costUsd": 0.0032
            }
          ]
        },
        {
          "model": "gemini-3.5-flash",
          "totalTokens": 1100,
          "costUsd": 0.0024
        },
        {
          "model": "gpt-oss-120b",
          "totalTokens": 550,
          "costUsd": 0.000105
        },
        {
          "model": "gemini-3.5-flash-lite",
          "totalTokens": 1100,
          "costUsd": null,
          "variants": [
            {
              "model": "gemini-3.5-flash-lite-tiered-high",
              "totalTokens": 1100,
              "costUsd": null
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "gemini-3.5-flash-lite-tiered-high"
    ]
  },
  "last30Days": {
    "tokens": 28070,
    "estimatedCostUsd": 0.05002500000000001,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 21910,
          "costUsd": 0.041120000000000004,
          "variants": [
            {
              "model": "gemini-3-pro-preview",
              "totalTokens": 18500,
              "costUsd": 0.0312
            },
            {
              "model": "gemini-3-pro",
              "totalTokens": 2310,
              "costUsd": 0.00672
            },
            {
              "model": "gemini/gemini-3-pro-preview",
              "totalTokens": 1100,
              "costUsd": 0.0032
            }
          ]
        },
        {
          "model": "gemini-3.1-pro-preview",
          "totalTokens": 2200,
          "costUsd": 0.0064,
          "variants": [
            {
              "model": "gemini-3.1-pro-high",
              "totalTokens": 2200,
              "costUsd": 0.0064
            }
          ]
        },
        {
          "model": "Other",
          "totalTokens": 3960,
          "costUsd": null,
          "variants": [
            {
              "model": "gemini-3.5-flash",
              "totalTokens": 1100,
              "costUsd": 0.0024
            },
            {
              "model": "gpt-oss-120b",
              "totalTokens": 550,
              "costUsd": 0.000105
            },
            {
              "model": "gemini-3.5-flash-lite",
              "totalTokens": 1100,
              "costUsd": null
            },
            {
              "model": "gemini-99-unreleased",
              "totalTokens": 770,
              "costUsd": null
            },
            {
              "model": "Unattributed",
              "totalTokens": 440,
              "costUsd": null
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "Unattributed",
      "gemini-3.5-flash-lite-tiered-high",
      "gemini-99-unreleased"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 22900,
      "estimatedCostUsd": 0.044000000000000004,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 3850,
      "estimatedCostUsd": 0.005705,
      "estimateComplete": false
    },
    {
      "date": "today-2",
      "tokens": 1210,
      "estimatedCostUsd": null,
      "estimateComplete": false
    },
    {
      "date": "today-29",
      "tokens": 110,
      "estimatedCostUsd": 0.00032,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed",
    "gemini-3.5-flash-lite-tiered-high",
    "gemini-99-unreleased"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": null,
    "last30Days": {
      "models": {
        "deepseek-chat": {
          "tokens": 990,
          "pricedTokens": 990,
          "estimatedCostUsd": 0.0002898,
          "costEstimated": true
        },
        "ollama/llama3": {
          "tokens": 55,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        },
        "totally-unknown-model": {
          "tokens": 55,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        }
      },
      "tokens": 1100,
      "pricedTokens": 990,
      "estimatedCostUsd": 0.0002898,
      "costEstimated": true,
      "estimateComplete": false
    }
  }
}
"#;
        const ROUTED: &str = r#"
== claude contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 3410,
    "estimatedCostUsd": 0.018449999999999998,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 1650,
          "costUsd": 0.01125
        },
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 1760,
          "costUsd": 0.0072
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-3",
      "tokens": 3300,
      "estimatedCostUsd": 0.018,
      "estimateComplete": true
    },
    {
      "date": "today-29",
      "tokens": 110,
      "estimatedCostUsd": 0.00045,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== codex contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 1650,
    "estimatedCostUsd": 0.012,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gpt-5.5",
          "totalTokens": 1650,
          "costUsd": 0.012
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-3",
      "tokens": 1650,
      "estimatedCostUsd": 0.012,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== grok contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 880,
    "estimatedCostUsd": 0.0002,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-4",
          "totalTokens": 880,
          "costUsd": 0.0002
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-3",
      "tokens": 880,
      "estimatedCostUsd": 0.0002,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== zai contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 660,
    "estimatedCostUsd": 0.000384,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "glm-4.6",
          "totalTokens": 660,
          "costUsd": 0.000384
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-3",
      "tokens": 660,
      "estimatedCostUsd": 0.000384,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== kimi contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 660,
    "estimatedCostUsd": 0.00051,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "kimi-k2",
          "totalTokens": 660,
          "costUsd": 0.00051
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-3",
      "tokens": 660,
      "estimatedCostUsd": 0.00051,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== minimax contributed=false
{
  "today": null,
  "yesterday": null,
  "last30Days": null,
  "daily": [],
  "unknownModels": []
}
"#;
    }
}
