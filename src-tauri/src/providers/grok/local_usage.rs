use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Days, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    models::UsageHistory,
    pricing::{ModelPricing, TokenBreakdown},
    providers::{
        antigravity, claude, codex,
        daily_usage::DailyUsageAccumulator,
        gemini_cli_usage, kimi,
        log_usage::{load_or_parse_log, parse_log_timestamp, LogCacheError},
        model_scope::{
            card_for_model, format_source_note, native_log_disposition, LocalModelDisposition,
        },
        opencode, pi_usage,
        zcode_usage::{ZCodeProvider, ZCodeUsageScanner},
    },
    storage::Storage,
};

use super::GrokError;

const LOG_CACHE_SCHEMA_VERSION: u8 = 1;
#[cfg(test)]
const SOURCE_NOTE: &str = "From your Grok logs (estimated)";

#[derive(Debug, Clone)]
pub struct GrokLogUsageScanner {
    path: PathBuf,
    zcode: ZCodeUsageScanner,
    fold_external: bool,
}

impl GrokLogUsageScanner {
    pub fn new() -> Self {
        Self {
            path: default_log_path(),
            zcode: ZCodeUsageScanner::new(),
            fold_external: true,
        }
    }

    #[cfg(test)]
    pub fn for_path(path: PathBuf) -> Self {
        Self {
            zcode: ZCodeUsageScanner::for_paths(
                path.with_extension("missing-zcode.sqlite"),
                path.with_extension("missing-zcode.json"),
            ),
            path,
            fold_external: false,
        }
    }

    pub fn scan(
        &self,
        storage: &Storage,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
    ) -> Result<UsageHistory, GrokError> {
        scan_path(
            storage,
            &self.path,
            &self.zcode,
            self.fold_external,
            now,
            pricing,
        )
    }
}

pub(crate) fn scan_routed_usage_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, GrokError> {
    if card_id == "grok" {
        return Ok(false);
    }
    let path = default_log_path();
    let events = load_events(storage, &path)?;
    Ok(aggregate_routed_into(
        events,
        now,
        pricing,
        card_id,
        accumulator,
    ))
}

fn default_log_path() -> PathBuf {
    log_path(
        &home_directory(),
        std::env::var("GROK_HOME")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .as_deref(),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TokenEvent {
    timestamp: DateTime<Utc>,
    model: Option<String>,
    prompt: u64,
    cached_prompt: u64,
    completion: u64,
    reasoning: u64,
}

fn load_events(storage: &Storage, path: &Path) -> Result<Vec<TokenEvent>, GrokError> {
    let (events, seen_paths) = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => (
            load_or_parse_log(storage, "grok", path, LOG_CACHE_SCHEMA_VERSION, parse_jsonl)
                .map_err(|error| match error {
                    LogCacheError::Storage(_) => GrokError::Storage,
                    LogCacheError::Encode(_) => GrokError::LocalUsage,
                })?
                .ok_or(GrokError::LocalUsage)?,
            HashSet::from([path.to_path_buf()]),
        ),
        Ok(_) => {
            crate::app_warn!("plugin:grok", "local usage log path is not a file");
            return Err(GrokError::LocalUsage);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), HashSet::new()),
        Err(_) => {
            crate::app_warn!("plugin:grok", "local usage log metadata could not be read");
            return Err(GrokError::LocalUsage);
        }
    };
    storage.prune_log_events("grok", &seen_paths)?;
    Ok(events)
}

fn scan_path(
    storage: &Storage,
    path: &Path,
    zcode: &ZCodeUsageScanner,
    fold_external: bool,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
) -> Result<UsageHistory, GrokError> {
    let events = load_events(storage, path)?;

    let mut accumulator = DailyUsageAccumulator::default();
    aggregate_into(events, now, pricing, &mut accumulator);
    let includes_pi = if fold_external {
        match pi_usage::scan_into(storage, now, pricing, "grok", &mut accumulator) {
            Ok(includes_pi) => includes_pi,
            Err(_) => {
                crate::app_warn!(
                    "plugin:pi",
                    "pi/oh-my-pi usage history could not be folded into Grok"
                );
                pi_usage::PiUsageSources::default()
            }
        }
    } else {
        pi_usage::PiUsageSources::default()
    };
    let includes_antigravity = if fold_external {
        match antigravity::local_usage::scan_routed_usage_into(
            storage,
            now,
            pricing,
            "grok",
            &mut accumulator,
        ) {
            Ok(includes_antigravity) => includes_antigravity,
            Err(_) => {
                crate::app_warn!(
                    "plugin:antigravity",
                    "Antigravity usage history could not be folded into Grok"
                );
                false
            }
        }
    } else {
        false
    };
    let includes_claude = if fold_external {
        match claude::local_usage::scan_routed_usage_into(
            storage,
            now,
            pricing,
            "grok",
            &mut accumulator,
        ) {
            Ok(includes_claude) => includes_claude,
            Err(_) => {
                crate::app_warn!(
                    "plugin:claude",
                    "Claude Code usage history could not be folded into Grok"
                );
                false
            }
        }
    } else {
        false
    };
    let includes_codex = if fold_external {
        match codex::local_usage::scan_routed_usage_into(
            storage,
            now,
            pricing,
            "grok",
            &mut accumulator,
        ) {
            Ok(includes_codex) => includes_codex,
            Err(_) => {
                crate::app_warn!(
                    "plugin:codex",
                    "Codex CLI usage history could not be folded into Grok"
                );
                false
            }
        }
    } else {
        false
    };
    let includes_kimi = if fold_external {
        match kimi::local_usage::scan_routed_usage_into(
            storage,
            now,
            pricing,
            "grok",
            &mut accumulator,
        ) {
            Ok(includes_kimi) => includes_kimi,
            Err(_) => {
                crate::app_warn!(
                    "plugin:kimi",
                    "Kimi Code usage history could not be folded into Grok"
                );
                false
            }
        }
    } else {
        false
    };
    let includes_gemini_cli = if fold_external {
        match gemini_cli_usage::scan_into(storage, now, pricing, "grok", &mut accumulator) {
            Ok(includes_gemini_cli) => includes_gemini_cli,
            Err(_) => {
                crate::app_warn!(
                    "plugin:gemini-cli",
                    "Gemini CLI usage history could not be folded into Grok"
                );
                false
            }
        }
    } else {
        false
    };
    let includes_opencode = if fold_external {
        match opencode::scan_routed_usage_into(now, pricing, "grok", &mut accumulator) {
            Ok(includes_opencode) => includes_opencode,
            Err(_) => {
                crate::app_warn!(
                    "plugin:opencode",
                    "OpenCode usage history could not be folded into Grok"
                );
                false
            }
        }
    } else {
        false
    };
    let includes_zcode = match zcode.scan_into(now, pricing, ZCodeProvider::Grok, &mut accumulator)
    {
        Ok(includes_zcode) => includes_zcode,
        Err(_) => {
            crate::app_warn!(
                "plugin:zcode",
                "ZCode usage history could not be folded into Grok"
            );
            false
        }
    };
    let mut extras = Vec::new();
    if includes_pi.pi {
        extras.push("pi");
    }
    if includes_pi.oh_my_pi {
        extras.push("oh-my-pi");
    }
    if includes_antigravity {
        extras.push("Antigravity");
    }
    if includes_claude {
        extras.push("Claude Code");
    }
    if includes_codex {
        extras.push("Codex");
    }
    if includes_kimi {
        extras.push("Kimi Code");
    }
    if includes_gemini_cli {
        extras.push("Gemini CLI");
    }
    if includes_opencode {
        extras.push("OpenCode");
    }
    if includes_zcode {
        extras.push("ZCode");
    }
    let source_note = format_source_note("Grok logs", &extras);
    Ok(accumulator.build(now, &source_note))
}

fn log_path(home: &Path, configured_home: Option<&str>) -> PathBuf {
    configured_home
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".grok"))
        .join("logs")
        .join("unified.jsonl")
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn parse_jsonl(content: &str) -> Vec<TokenEvent> {
    let mut model_by_pid = HashMap::<i64, String>::new();
    let mut events = Vec::new();

    for line in content.lines() {
        if !line.contains("inference_done") && !line.contains("model") {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(message) = object.get("msg").and_then(Value::as_str) else {
            continue;
        };
        let context = object
            .get("ctx")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let pid = object.get("pid").and_then(integer_i64);

        if let Some(model) = model_id(message, &context) {
            if let Some(pid) = pid {
                model_by_pid.insert(pid, model);
            }
            continue;
        }
        if message != "shell.turn.inference_done" {
            continue;
        }
        let Some(prompt) = context.get("prompt_tokens").and_then(integer_u64) else {
            continue;
        };
        let Some(timestamp) = object
            .get("ts")
            .and_then(Value::as_str)
            .and_then(parse_log_timestamp)
        else {
            continue;
        };
        let cached_prompt = context
            .get("cached_prompt_tokens")
            .and_then(integer_u64)
            .unwrap_or_default()
            .min(prompt);
        events.push(TokenEvent {
            timestamp,
            model: pid.and_then(|pid| model_by_pid.get(&pid).cloned()),
            prompt,
            cached_prompt,
            completion: context
                .get("completion_tokens")
                .and_then(integer_u64)
                .unwrap_or_default(),
            reasoning: context
                .get("reasoning_tokens")
                .and_then(integer_u64)
                .unwrap_or_default(),
        });
    }
    events
}

fn model_id(message: &str, context: &Map<String, Value>) -> Option<String> {
    let value = match message {
        "model changed" => context.get("model"),
        "model catalog: notifying clients" => context.get("current_model_id"),
        "backend_search: model switch" => context
            .get("model")
            .or_else(|| context.get("current_model_id"))
            .or_else(|| context.get("model_id")),
        "subagent model resolved" => context.get("model_id").or_else(|| context.get("model")),
        _ => None,
    }?;
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
fn aggregate(events: Vec<TokenEvent>, now: DateTime<Utc>, pricing: &ModelPricing) -> UsageHistory {
    let mut accumulator = DailyUsageAccumulator::default();
    aggregate_into(events, now, pricing, &mut accumulator);
    accumulator.build(now, SOURCE_NOTE)
}

fn aggregate_into(
    events: Vec<TokenEvent>,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) {
    let today = now.with_timezone(&Local).date_naive();
    let since = today.checked_sub_days(Days::new(30)).unwrap_or(today);

    for event in events {
        let date = event.timestamp.with_timezone(&Local).date_naive();
        if date < since || event.timestamp > now {
            continue;
        }
        let total = event
            .prompt
            .saturating_add(event.completion)
            .saturating_add(event.reasoning);
        let Some(model) = event
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        else {
            if total > 0 {
                accumulator.add_unpriced(date, total, "Unattributed");
            }
            continue;
        };
        let tokens = TokenBreakdown {
            input: event.prompt.saturating_sub(event.cached_prompt),
            cache_read: event.cached_prompt,
            output: event.completion.saturating_add(event.reasoning),
            ..TokenBreakdown::default()
        };
        if total == 0 {
            continue;
        }
        match native_log_disposition("grok", model) {
            LocalModelDisposition::Home => {}
            LocalModelDisposition::RoutedTo(_) => continue,
            LocalModelDisposition::Other => {
                let cost = pricing.estimated_cost_at("grok", model, event.timestamp, tokens, true);
                accumulator.add_other(date, total, cost, true, model);
                continue;
            }
        }
        if let Some(cost) = pricing.estimated_cost_at("grok", model, event.timestamp, tokens, true)
        {
            accumulator.add(date, total, cost, model);
        } else {
            accumulator.add_unpriced(date, total, model);
        }
    }
}

fn aggregate_routed_into(
    events: Vec<TokenEvent>,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let today = now.with_timezone(&Local).date_naive();
    let since = today.checked_sub_days(Days::new(29)).unwrap_or(today);
    let mut contributed = false;

    for event in events {
        let date = event.timestamp.with_timezone(&Local).date_naive();
        if date < since || event.timestamp > now {
            continue;
        }
        let Some(model) = event.model.as_deref() else {
            continue;
        };
        if card_for_model(model) != Some(card_id) {
            continue;
        }
        let tokens = TokenBreakdown {
            input: event.prompt.saturating_sub(event.cached_prompt),
            cache_read: event.cached_prompt,
            output: event.completion.saturating_add(event.reasoning),
            ..TokenBreakdown::default()
        };
        let total = event
            .prompt
            .saturating_add(event.completion)
            .saturating_add(event.reasoning);
        if total == 0 {
            continue;
        }
        if let Some(cost) = pricing.estimated_cost_at(card_id, model, event.timestamp, tokens, true)
        {
            accumulator.add(date, total, cost, model);
            contributed = true;
        } else {
            accumulator.add_unpriced(date, total, model);
            contributed = true;
        }
    }
    contributed
}

fn integer_u64(value: &Value) -> Option<u64> {
    finite_number(value)
        .filter(|number| *number >= 0.0)
        .map(|number| number.trunc() as u64)
}

fn integer_i64(value: &Value) -> Option<i64> {
    finite_number(value).map(|number| number.trunc() as i64)
}

fn finite_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .filter(|number: &f64| number.is_finite())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    use super::{aggregate, log_path, parse_jsonl, GrokLogUsageScanner};
    use crate::{pricing::test_bundled_pricing, providers::grok::GrokError, storage::Storage};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap()
    }

    #[test]
    fn fixture_tracks_models_per_process_and_prices_usage() {
        let history = aggregate(
            parse_jsonl(include_str!("fixtures/usage.jsonl")),
            now(),
            &test_bundled_pricing(),
        );

        let today = history.today.unwrap();
        let yesterday = history.yesterday.unwrap();
        assert_eq!(today.tokens, 2_000_000);
        assert!((today.estimated_cost_usd.unwrap() - 3.0).abs() < 0.000_001);
        assert_eq!(yesterday.tokens, 2_000_000);
        assert!((yesterday.estimated_cost_usd.unwrap() - 18.0).abs() < 0.000_001);
        assert!(today.cost_estimated);
    }

    #[test]
    fn mid_process_switch_and_cached_prompt_use_the_active_rates() {
        let content = r#"{"ts":"2026-06-18T08:00:00Z","pid":7,"msg":"model changed","ctx":{"model":"grok-build"}}
{"ts":"2026-06-18T09:00:00Z","pid":7,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":1000000,"cached_prompt_tokens":800000,"completion_tokens":0}}
{"ts":"2026-06-18T10:00:00Z","pid":7,"msg":"backend_search: model switch","ctx":{"model_id":"grok-composer-2.5-fast"}}
{"ts":"2026-06-18T11:00:00Z","pid":7,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":1000000,"completion_tokens":0}}"#;
        let history = aggregate(parse_jsonl(content), now(), &test_bundled_pricing());

        assert!((history.today.unwrap().estimated_cost_usd.unwrap() - 3.36).abs() < 0.000_001);
    }

    #[test]
    fn unknown_and_missing_models_retain_tokens_as_incomplete() {
        let content = r#"{"ts":"2026-06-18T08:00:00Z","pid":1,"msg":"model changed","ctx":{"model":"grok-unknown-model"}}
{"ts":"2026-06-18T09:00:00Z","pid":1,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":1000000}}
{"ts":"2026-06-18T10:00:00Z","pid":2,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":1000000}}
{"ts":"2026-06-18T11:00:00Z","pid":3,"msg":"model changed","ctx":{"model":"grok-build"}}
{"ts":"2026-06-18T12:00:00Z","pid":3,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":500000}}"#;
        let history = aggregate(parse_jsonl(content), now(), &test_bundled_pricing());

        assert_eq!(history.today.as_ref().unwrap().tokens, 2_500_000);
        assert_eq!(
            history.unknown_models,
            ["Unattributed", "grok-unknown-model"]
        );
        assert!(!history.today.unwrap().estimate_complete);
    }

    #[test]
    fn foreign_models_in_grok_logs_route_to_their_card_or_others() {
        let content = r#"{"ts":"2026-06-18T08:00:00Z","pid":1,"msg":"model changed","ctx":{"model":"grok-build"}}
{"ts":"2026-06-18T09:00:00Z","pid":1,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":100000}}
{"ts":"2026-06-18T09:10:00Z","pid":1,"msg":"model changed","ctx":{"model":"claude-sonnet-4-6"}}
{"ts":"2026-06-18T09:20:00Z","pid":1,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":200000}}
{"ts":"2026-06-18T09:30:00Z","pid":1,"msg":"model changed","ctx":{"model":"deepseek-v4"}}
{"ts":"2026-06-18T09:40:00Z","pid":1,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":300000}}"#;
        let events = parse_jsonl(content);
        let pricing = test_bundled_pricing();
        let grok = aggregate(events.clone(), now(), &pricing);
        assert_eq!(grok.today.as_ref().unwrap().tokens, 100_000);
        assert_eq!(grok.other_usage.unwrap().today.unwrap().tokens, 300_000);

        let mut claude = crate::providers::daily_usage::DailyUsageAccumulator::default();
        assert!(super::aggregate_routed_into(
            events,
            now(),
            &pricing,
            "claude",
            &mut claude
        ));
        assert_eq!(
            claude.build(now(), "test").today.as_ref().unwrap().tokens,
            200_000
        );
    }

    #[test]
    fn rows_outside_the_window_and_rows_without_prompt_tokens_are_ignored() {
        let content = r#"{"ts":"2026-05-01T08:00:00Z","pid":1,"msg":"model changed","ctx":{"model":"grok-build"}}
{"ts":"2026-05-01T09:00:00Z","pid":1,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":1000000}}
{"ts":"2026-06-18T09:00:00Z","pid":1,"msg":"shell.turn.inference_done","ctx":{"loop_index":3}}"#;
        let history = aggregate(parse_jsonl(content), now(), &test_bundled_pricing());
        assert!(history.today.is_none());
        assert!(history.daily.is_empty());
    }

    #[test]
    fn path_uses_override_or_cross_platform_home() {
        assert_eq!(
            log_path(Path::new("/users/me"), None),
            Path::new("/users/me/.grok/logs/unified.jsonl")
        );
        assert_eq!(
            log_path(Path::new("/ignored"), Some("/custom/grok")),
            Path::new("/custom/grok/logs/unified.jsonl")
        );
    }

    #[test]
    fn missing_log_is_honest_no_data_while_non_file_path_fails() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let log = directory.path().join("logs").join("unified.jsonl");
        let scanner = GrokLogUsageScanner::for_path(log.clone());
        let missing = scanner
            .scan(&storage, now(), &test_bundled_pricing())
            .unwrap();
        assert!(missing.daily.is_empty());

        fs::create_dir_all(&log).unwrap();
        let error = scanner
            .scan(&storage, now(), &test_bundled_pricing())
            .unwrap_err();
        assert!(matches!(error, GrokError::LocalUsage));
    }
}
