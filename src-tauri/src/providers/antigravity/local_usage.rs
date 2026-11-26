use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    models::UsageHistory,
    pricing::{ModelPricing, TokenBreakdown},
    storage::Storage,
};

use super::AntigravityError;
use crate::providers::{
    daily_usage::DailyUsageAccumulator,
    log_usage::{load_or_parse_log, parse_log_timestamp},
    model_scope::card_for_model,
    opencode, pi_usage,
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

fn load_local_events(
    storage: &Storage,
    roots: &[PathBuf],
    since_date: NaiveDate,
) -> Result<Vec<AntigravityTokenEvent>, AntigravityError> {
    let paths = discover_transcript_files(roots);
    let mut seen_paths = HashSet::with_capacity(paths.len());
    let mut events = Vec::new();
    for path in paths {
        seen_paths.insert(path.clone());
        let Some(parsed) = load_or_parse_log(
            storage,
            "antigravity",
            &path,
            LOG_CACHE_SCHEMA_VERSION,
            parse_jsonl,
        )
        .map_err(|_| AntigravityError::LocalUsage)?
        else {
            continue;
        };
        events.extend(
            parsed
                .into_iter()
                .filter(|event| event.timestamp.with_timezone(&Local).date_naive() >= since_date),
        );
    }
    storage
        .prune_log_events("antigravity", &seen_paths)
        .map_err(|_| AntigravityError::LocalUsage)?;
    Ok(events)
}

pub(crate) fn scan_routed_usage_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, AntigravityError> {
    if card_id != "claude" {
        return Ok(false);
    }
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
    let since_date = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);
    let events = load_local_events(storage, roots, since_date)?;
    Ok(aggregate_routed_into(
        events,
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
    scan_roots(storage, &roots, now, pricing, true)
}

fn scan_roots(
    storage: &Storage,
    roots: &[PathBuf],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    fold_pi: bool,
) -> Result<UsageHistory, AntigravityError> {
    let since_date = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);
    let events = load_local_events(storage, roots, since_date)?;

    let mut accumulator = DailyUsageAccumulator::default();
    aggregate_into(events, now, pricing, &mut accumulator);
    let includes_pi = if fold_pi {
        match pi_usage::scan_into(storage, now, pricing, "antigravity", &mut accumulator) {
            Ok(includes_pi) => includes_pi,
            Err(_) => {
                crate::app_warn!(
                    "plugin:pi",
                    "pi/oh-my-pi usage history could not be folded into Antigravity"
                );
                pi_usage::PiUsageSources::default()
            }
        }
    } else {
        pi_usage::PiUsageSources::default()
    };
    let includes_opencode = if fold_pi {
        match opencode::scan_routed_usage_into(now, pricing, "antigravity", &mut accumulator) {
            Ok(includes_opencode) => includes_opencode,
            Err(_) => {
                crate::app_warn!(
                    "plugin:opencode",
                    "OpenCode usage history could not be folded into Antigravity"
                );
                false
            }
        }
    } else {
        false
    };
    let source_note = pi_usage::usage_source_note(
        "Antigravity transcripts",
        includes_pi,
        includes_opencode,
        false,
    );
    Ok(accumulator.build(now, &source_note))
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
    let mut seen = HashSet::new();
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
            if seen.insert(canonical.clone()) {
                paths.push(canonical);
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

fn add_priced_event(
    accumulator: &mut DailyUsageAccumulator,
    date: NaiveDate,
    event: &AntigravityTokenEvent,
    model_name: &str,
    provider: &str,
    pricing: &ModelPricing,
) {
    let total = event.total_tokens();
    let tokens = event.pricing_tokens();
    if let Some(cost) =
        pricing.estimated_cost_at(provider, model_name, event.timestamp, tokens, true)
    {
        let family = pricing.display_family(model_name);
        if family != model_name {
            accumulator.add_variant(date, total, cost, &family, model_name);
        } else {
            accumulator.add(date, total, cost, &family);
        }
    } else {
        accumulator.add_unpriced(date, total, model_name);
    }
}

fn aggregate_into(
    events: Vec<AntigravityTokenEvent>,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) {
    let since = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);

    for event in events {
        let date = event.timestamp.with_timezone(&Local).date_naive();
        if date < since || event.timestamp > now {
            continue;
        }
        let total = event.total_tokens();
        if total == 0 {
            continue;
        }
        let model_name = event
            .model
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());

        let Some(model_name) = model_name else {
            accumulator.add_unpriced(date, total, "Unattributed");
            continue;
        };
        if card_for_model(model_name) == Some("claude") {
            continue;
        }

        add_priced_event(
            accumulator,
            date,
            &event,
            model_name,
            "antigravity",
            pricing,
        );
    }
}

fn aggregate_routed_into(
    events: Vec<AntigravityTokenEvent>,
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
        let date = event.timestamp.with_timezone(&Local).date_naive();
        if date < since || event.timestamp > now || event.total_tokens() == 0 {
            continue;
        }
        let Some(model_name) = event
            .model
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if card_for_model(model_name) != Some(card_id) {
            continue;
        }

        add_priced_event(accumulator, date, &event, model_name, card_id, pricing);
        contributed = true;
    }
    contributed
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
        pricing::test_bundled_pricing, providers::daily_usage::DailyUsageAccumulator,
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
            scan_roots(&storage, std::slice::from_ref(&root), now, &pricing, false).unwrap();

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
}
