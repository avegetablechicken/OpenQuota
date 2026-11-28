use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
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
        card_usage::DailyUsageAccumulator,
        card_usage::{self, UsageObservation},
        log_usage::{parse_log_timestamp, LocalLogs, LogCacheError, LogFormat},
        session_model::{SessionModelCli, SessionModelTracker},
    },
    storage::Storage,
};

const LOG_CACHE_SCHEMA_VERSION: u8 = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct KimiTokenEvent {
    timestamp: DateTime<Utc>,
    provider: Option<String>,
    model: Option<String>,
    /// Same-provider session context, kept separate from the request model in the cache.
    #[serde(default)]
    session_model: Option<String>,
    #[serde(default)]
    model_inherited: bool,
    input: u64,
    cache_write: u64,
    cache_read: u64,
    output: u64,
}

impl KimiTokenEvent {
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

/// Scans Kimi Code (`~/.kimi-code`) and legacy Kimi CLI (`~/.kimi`) session logs into `accumulator`.
///
/// For `card_id == "kimi"`, native Kimi models land in the main series, foreign models owned by
/// another first-party card are skipped so they route once to their owner, and unowned foreign
/// models land in `other_usage`. For any other `card_id`, only models owned by `card_id` are
/// routed into `accumulator`.
pub(crate) fn scan_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, LogCacheError> {
    let roots = default_roots();
    scan_roots_into(storage, &roots, now, pricing, card_id, accumulator)
}

fn default_roots() -> Vec<PathBuf> {
    let home = home_directory();
    kimi_roots(
        crate::provider_environment::value("KIMI_CODE_HOME").as_deref(),
        crate::provider_environment::value("KIMI_SHARE_DIR").as_deref(),
        &home,
    )
}

fn kimi_roots(
    code_override: Option<&str>,
    legacy_override: Option<&str>,
    home: &Path,
) -> Vec<PathBuf> {
    let code_root = code_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| expand_home(value, home))
        .unwrap_or_else(|| home.join(".kimi-code"));
    let legacy_root = legacy_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| expand_home(value, home))
        .unwrap_or_else(|| home.join(".kimi"));
    let mut roots = vec![code_root];
    if !roots.iter().any(|existing| existing == &legacy_root) {
        roots.push(legacy_root);
    }
    roots
}

fn scan_roots_into(
    storage: &Storage,
    roots: &[PathBuf],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, LogCacheError> {
    let events = load_local_events(storage, roots, now)?;
    Ok(aggregate_into(&events, now, pricing, card_id, accumulator))
}

struct KimiLogs;

impl LogFormat for KimiLogs {
    type Event = KimiTokenEvent;
    /// The default model in the file's Kimi root config, for events that name none.
    type Context = Option<String>;
    const PROVIDER_ID: &'static str = "kimi";
    const SCHEMA_VERSION: u8 = LOG_CACHE_SCHEMA_VERSION;

    fn parse(content: &str) -> Vec<KimiTokenEvent> {
        parse_jsonl(content)
    }

    fn timestamp(event: &KimiTokenEvent) -> DateTime<Utc> {
        event.timestamp
    }

    /// Resolve same-provider session context after loading the raw cache, then the current
    /// configured default. Configuration changes must not require reparsing the wire file.
    fn attach(event: &mut KimiTokenEvent, fallback_model: &Option<String>) {
        if event.model.is_none() {
            event.model_inherited = event.session_model.is_some();
            event.model = event.session_model.clone().or_else(|| {
                fallback_model
                    .as_deref()
                    .map(|model| normalize_kimi_model(model, event.provider.as_deref()))
            });
        }
    }

    fn deduplicate(events: Vec<KimiTokenEvent>) -> Vec<KimiTokenEvent> {
        deduplicate(events)
    }
}

static LOGS: LocalLogs<KimiLogs> = LocalLogs::new();

fn load_local_events(
    storage: &Storage,
    roots: &[PathBuf],
    now: DateTime<Utc>,
) -> Result<std::sync::Arc<Vec<KimiTokenEvent>>, LogCacheError> {
    let mut default_models = HashMap::<PathBuf, Option<String>>::new();
    let files = discover_wire_files(roots)
        .into_iter()
        .map(|(root, path)| {
            let fallback_model = default_models
                .entry(root)
                .or_insert_with_key(|root| read_default_model(&root.join("config.toml")))
                .clone();
            (path, fallback_model)
        })
        .collect::<Vec<_>>();
    LOGS.load(storage, now, files)
}

fn deduplicate(events: Vec<KimiTokenEvent>) -> Vec<KimiTokenEvent> {
    let mut seen = HashSet::with_capacity(events.len());
    events
        .into_iter()
        .filter(|event| {
            seen.insert((
                event.timestamp,
                event.model.clone(),
                event.input,
                event.cache_write,
                event.cache_read,
                event.output,
            ))
        })
        .collect()
}

fn discover_wire_files(roots: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut discovered = Vec::new();
    let mut seen_roots = HashSet::new();
    let mut seen_sessions = HashSet::new();

    for root in roots {
        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if !seen_roots.insert(canonical_root.clone()) {
            continue;
        }
        let sessions_dir = canonical_root.join("sessions");
        if !sessions_dir.is_dir() {
            continue;
        }
        let mut root_files = WalkDir::new(&sessions_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry.path().file_name() == Some(OsStr::new("wire.jsonl"))
            })
            .map(|entry| entry.into_path())
            .collect::<Vec<_>>();
        root_files.sort();
        root_files.dedup();

        // Track session ids seen in this root separately from previous roots so multiple agents
        // (e.g. main + subagents) within the same Kimi Code session are all kept, while a legacy
        // ~/.kimi copy of an already-migrated session is skipped.
        let mut root_sessions = HashSet::new();
        for path in root_files {
            if let Some(session_key) = session_dedup_key(&path, &sessions_dir) {
                if seen_sessions.contains(&session_key) {
                    continue;
                }
                root_sessions.insert(session_key);
            }
            discovered.push((canonical_root.clone(), path));
        }
        seen_sessions.extend(root_sessions);
    }
    discovered
}

fn session_dedup_key(path: &Path, sessions_dir: &Path) -> Option<String> {
    let relative = path.strip_prefix(sessions_dir).ok()?;
    for component in relative.components() {
        let name = component.as_os_str().to_str()?;
        let candidate = name
            .strip_prefix("session_")
            .or_else(|| name.strip_prefix("ses_"))
            .unwrap_or(name);
        if looks_like_uuid(candidate) {
            return Some(candidate.to_ascii_lowercase());
        }
    }
    None
}

fn looks_like_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn read_default_model(config_path: &Path) -> Option<String> {
    let content = fs::read_to_string(config_path).ok()?;
    let mut in_top_level = true;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_top_level = false;
            continue;
        }
        if !in_top_level {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "default_model" {
            continue;
        }
        let trimmed = value
            .split('#')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

#[derive(Default)]
struct LastRequestContext {
    provider: Option<String>,
    model: Option<String>,
    model_alias: Option<String>,
}

impl LastRequestContext {
    fn model_for(&self, provider: Option<&str>) -> Option<String> {
        if provider.is_some_and(|provider| {
            !self
                .provider
                .as_deref()
                .is_some_and(|last| provider.eq_ignore_ascii_case(last))
        }) {
            return None;
        }
        self.model_alias.clone().or_else(|| self.model.clone())
    }

    /// The last request's provider, for a usage line that names no provider of its own. A line
    /// naming a different model belongs to another request, so it does not inherit it.
    fn provider_for(&self, model: Option<&str>) -> Option<String> {
        let matches_last = model.is_none_or(|model| {
            self.model_alias.as_deref() == Some(model) || self.model.as_deref() == Some(model)
        });
        matches_last.then(|| self.provider.clone()).flatten()
    }
}

fn parse_jsonl(content: &str) -> Vec<KimiTokenEvent> {
    let mut events = Vec::new();
    let mut last_request = LastRequestContext::default();

    for line in content.lines() {
        if !line.contains("usage.record")
            && !line.contains("llm.request")
            && !line.contains("StatusUpdate")
            && !line.contains("token_usage")
        {
            continue;
        }
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(map) = object.as_object() else {
            continue;
        };

        if let Some(event_type) = map.get("type").and_then(Value::as_str) {
            if event_type == "llm.request" {
                last_request = LastRequestContext {
                    provider: non_empty_str(map.get("provider")),
                    model: non_empty_str(map.get("model")),
                    model_alias: non_empty_str(map.get("modelAlias")),
                };
                continue;
            }
            if event_type == "usage.record" {
                if let Some(event) = parse_usage_record(map, &last_request) {
                    events.push(event);
                }
                continue;
            }
        }

        if let Some(event) = parse_legacy_status_update(map, &last_request) {
            events.push(event);
        }
    }

    let mut trackers = HashMap::<String, SessionModelTracker>::new();
    for event in &mut events {
        // Unknown providers are not a shared identity; never infer across their records.
        let Some(provider) = event.provider.as_deref() else {
            continue;
        };
        let resolved = trackers
            .entry(provider.trim().to_ascii_lowercase())
            .or_default()
            .resolve(event.model.clone());
        if resolved.inherited {
            event.session_model = resolved.model;
        }
    }
    events
}

fn parse_usage_record(
    map: &Map<String, Value>,
    last_request: &LastRequestContext,
) -> Option<KimiTokenEvent> {
    if let Some(scope) = map.get("usageScope").and_then(Value::as_str) {
        let normalized_scope = scope.trim().to_ascii_lowercase();
        if matches!(normalized_scope.as_str(), "session" | "summary" | "total") {
            return None;
        }
    }
    let timestamp = parse_timestamp_value(map.get("time").or_else(|| map.get("timestamp")))?;
    let usage = map.get("usage").and_then(Value::as_object)?;
    let (input, cache_write, cache_read, output) = extract_token_counts(usage)?;
    let explicit_provider = non_empty_str(map.get("provider"));
    let raw_model = non_empty_str(map.get("model"))
        .or_else(|| last_request.model_for(explicit_provider.as_deref()));
    let provider = explicit_provider.or_else(|| last_request.provider_for(raw_model.as_deref()));
    let model = raw_model
        .as_deref()
        .map(|model| normalize_kimi_model(model, provider.as_deref()));

    Some(KimiTokenEvent {
        timestamp,
        provider,
        model,
        session_model: None,
        model_inherited: false,
        input,
        cache_write,
        cache_read,
        output,
    })
}

fn parse_legacy_status_update(
    map: &Map<String, Value>,
    last_request: &LastRequestContext,
) -> Option<KimiTokenEvent> {
    let message = map.get("message").and_then(Value::as_object).unwrap_or(map);
    if message.get("type").and_then(Value::as_str) != Some("StatusUpdate") {
        return None;
    }
    let payload = message.get("payload").and_then(Value::as_object)?;
    let usage = payload
        .get("token_usage")
        .or_else(|| payload.get("tokenUsage"))
        .and_then(Value::as_object)?;
    let (input, cache_write, cache_read, output) = extract_token_counts(usage)?;
    let timestamp = parse_timestamp_value(
        map.get("timestamp")
            .or_else(|| map.get("time"))
            .or_else(|| payload.get("timestamp"))
            .or_else(|| payload.get("time")),
    )?;
    let explicit_provider = non_empty_str(payload.get("provider"));
    let raw_model = non_empty_str(payload.get("model"))
        .or_else(|| non_empty_str(message.get("model")))
        .or_else(|| last_request.model_for(explicit_provider.as_deref()));
    let provider = explicit_provider.or_else(|| last_request.provider_for(raw_model.as_deref()));
    let model = raw_model
        .as_deref()
        .map(|model| normalize_kimi_model(model, provider.as_deref()));

    Some(KimiTokenEvent {
        timestamp,
        provider,
        model,
        session_model: None,
        model_inherited: false,
        input,
        cache_write,
        cache_read,
        output,
    })
}

fn extract_token_counts(usage: &Map<String, Value>) -> Option<(u64, u64, u64, u64)> {
    let input = first_u64(
        usage,
        &["inputOther", "input_other", "input_tokens", "input"],
    );
    let cache_read = first_u64(
        usage,
        &[
            "inputCacheRead",
            "input_cache_read",
            "cache_read_input_tokens",
            "cacheRead",
        ],
    );
    let cache_write = first_u64(
        usage,
        &[
            "inputCacheCreation",
            "input_cache_creation",
            "cache_creation_input_tokens",
            "cacheWrite",
        ],
    );
    let output = first_u64(usage, &["output", "output_tokens"]);
    let total = input
        .saturating_add(cache_write)
        .saturating_add(cache_read)
        .saturating_add(output);
    (total > 0).then_some((input, cache_write, cache_read, output))
}

fn first_u64(map: &Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(unsigned_u64))
        .unwrap_or_default()
}

fn unsigned_u64(value: &Value) -> Option<u64> {
    match value {
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

fn parse_timestamp_value(value: Option<&Value>) -> Option<DateTime<Utc>> {
    match value? {
        Value::String(text) => parse_log_timestamp(text),
        Value::Number(number) => {
            if let Some(raw) = number.as_i64() {
                timestamp_from_numeric(raw as f64)
            } else {
                timestamp_from_numeric(number.as_f64()?)
            }
        }
        _ => None,
    }
}

fn timestamp_from_numeric(raw: f64) -> Option<DateTime<Utc>> {
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let millis = if raw >= 1_000_000_000_000.0 {
        raw.trunc() as i64
    } else {
        (raw * 1000.0).round() as i64
    };
    DateTime::<Utc>::from_timestamp_millis(millis)
}

fn normalize_kimi_model(raw: &str, provider: Option<&str>) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some((prefix, slug)) = trimmed.split_once('/') {
        let prefix_lower = prefix.trim().to_ascii_lowercase();
        let slug_trimmed = slug.trim();
        if is_kimi_provider_name(&prefix_lower) {
            return normalize_kimi_slug(slug_trimmed);
        }
    }
    if provider
        .map(|value| is_kimi_provider_name(&value.trim().to_ascii_lowercase()))
        .unwrap_or(false)
    {
        return normalize_kimi_slug(trimmed);
    }
    trimmed.to_owned()
}

fn is_kimi_provider_name(name: &str) -> bool {
    matches!(
        name,
        "kimi" | "kimi-code" | "kimi-coding" | "moonshot" | "moonshot-cn" | "moonshotai"
    )
}

fn normalize_kimi_slug(slug: &str) -> String {
    let lower = slug.to_ascii_lowercase();
    if is_short_kimi_series(&lower) {
        format!("kimi-{lower}")
    } else {
        slug.to_owned()
    }
}

fn is_short_kimi_series(slug_lower: &str) -> bool {
    let bytes = slug_lower.as_bytes();
    bytes.len() >= 2 && bytes[0] == b'k' && bytes[1].is_ascii_digit()
}

fn aggregate_into(
    events: &[KimiTokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    card_usage::record(
        events.iter().map(observation),
        "kimi",
        card_id,
        now,
        pricing,
        accumulator,
    )
    .own_series
}

fn observation(event: &KimiTokenEvent) -> UsageObservation<'_> {
    UsageObservation {
        provider_hint: event.provider.as_deref(),
        session_model_cli: event.model_inherited.then_some(SessionModelCli::KimiCode),
        ..UsageObservation::new(
            event.timestamp,
            event.model.as_deref(),
            event.pricing_tokens(),
            None,
        )
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
#[path = "local_usage_tests.rs"]
mod tests;
