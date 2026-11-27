use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
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
        card_usage::DailyUsageAccumulator,
        card_usage::{self, LabelMode, LocalRuntimeMode, TransitionalPolicy, UsageObservation},
        log_usage::{load_or_parse_log, parse_log_timestamp, LoadedEventsMemo, LogCacheError},
    },
    storage::Storage,
};

const LOG_CACHE_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct KimiTokenEvent {
    timestamp: DateTime<Utc>,
    provider: Option<String>,
    model: Option<String>,
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

pub(crate) fn scan_routed_usage_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, LogCacheError> {
    if card_id == "kimi" {
        return Ok(false);
    }
    scan_into(storage, now, pricing, card_id, accumulator)
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
    let since_date = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);
    let events = load_local_events(storage, roots, since_date)?;
    Ok(aggregate_into(&events, now, pricing, card_id, accumulator))
}

static LOADED_EVENTS: LoadedEventsMemo<KimiTokenEvent> = LoadedEventsMemo::new();

fn load_local_events(
    storage: &Storage,
    roots: &[PathBuf],
    since_date: NaiveDate,
) -> Result<std::sync::Arc<Vec<KimiTokenEvent>>, LogCacheError> {
    let discovered = discover_wire_files(roots);
    let paths = discovered
        .iter()
        .map(|(_, path)| path.clone())
        .collect::<Vec<_>>();
    LOADED_EVENTS.load(storage, paths, since_date, || {
        load_wire_files(storage, discovered, since_date)
    })
}

fn load_wire_files(
    storage: &Storage,
    discovered: Vec<(PathBuf, PathBuf)>,
    since_date: NaiveDate,
) -> Result<Vec<KimiTokenEvent>, LogCacheError> {
    let mut seen_paths = HashSet::with_capacity(discovered.len());
    let mut default_models = HashMap::<PathBuf, Option<String>>::new();
    let mut events = Vec::new();

    for (root, path) in discovered {
        seen_paths.insert(path.clone());
        let fallback_model = default_models
            .entry(root.clone())
            .or_insert_with(|| read_default_model(&root.join("config.toml")))
            .clone();
        let Some(parsed) = load_or_parse_log(
            storage,
            "kimi",
            &path,
            LOG_CACHE_SCHEMA_VERSION,
            |content| parse_jsonl(content, fallback_model.as_deref()),
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
    storage.prune_log_events("kimi", &seen_paths)?;
    Ok(deduplicate(events))
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
    /// The last request's provider, for a usage line that names no provider of its own. A line
    /// naming a different model belongs to another request, so it does not inherit it.
    fn provider_for(&self, model: Option<&str>) -> Option<String> {
        let matches_last = model.is_none_or(|model| {
            self.model_alias.as_deref() == Some(model) || self.model.as_deref() == Some(model)
        });
        matches_last.then(|| self.provider.clone()).flatten()
    }
}

fn parse_jsonl(content: &str, fallback_model: Option<&str>) -> Vec<KimiTokenEvent> {
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
                if let Some(event) = parse_usage_record(map, &last_request, fallback_model) {
                    events.push(event);
                }
                continue;
            }
        }

        if let Some(event) = parse_legacy_status_update(map, &last_request, fallback_model) {
            events.push(event);
        }
    }

    events
}

fn parse_usage_record(
    map: &Map<String, Value>,
    last_request: &LastRequestContext,
    fallback_model: Option<&str>,
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
    let raw_model = non_empty_str(map.get("model"))
        .or_else(|| last_request.model_alias.clone())
        .or_else(|| last_request.model.clone())
        .or_else(|| fallback_model.map(str::to_owned));
    let provider = non_empty_str(map.get("provider"))
        .or_else(|| last_request.provider_for(raw_model.as_deref()));
    let model = raw_model
        .as_deref()
        .map(|model| normalize_kimi_model(model, provider.as_deref()));

    Some(KimiTokenEvent {
        timestamp,
        provider,
        model,
        input,
        cache_write,
        cache_read,
        output,
    })
}

fn parse_legacy_status_update(
    map: &Map<String, Value>,
    last_request: &LastRequestContext,
    fallback_model: Option<&str>,
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
    let raw_model = non_empty_str(payload.get("model"))
        .or_else(|| non_empty_str(message.get("model")))
        .or_else(|| last_request.model_alias.clone())
        .or_else(|| last_request.model.clone())
        .or_else(|| fallback_model.map(str::to_owned));
    let provider = non_empty_str(payload.get("provider"))
        .or_else(|| last_request.provider_for(raw_model.as_deref()));
    let model = raw_model
        .as_deref()
        .map(|model| normalize_kimi_model(model, provider.as_deref()));

    Some(KimiTokenEvent {
        timestamp,
        provider,
        model,
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
        TransitionalPolicy {
            window_start_days_ago: 29,
            zero_token_guard: true,
            label: LabelMode::Family,
            local_runtime: LocalRuntimeMode::ModelSlugOrProvider,
        },
        accumulator,
    )
    .own_series
}

fn observation(event: &KimiTokenEvent) -> UsageObservation<'_> {
    UsageObservation {
        provider_hint: event.provider.as_deref(),
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
mod tests {
    use std::{fs, path::Path};

    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    use super::{
        aggregate_into, kimi_roots, normalize_kimi_model, parse_jsonl, read_default_model,
        scan_roots_into, KimiTokenEvent,
    };
    use crate::{
        pricing::test_bundled_pricing, providers::card_usage::DailyUsageAccumulator,
        storage::Storage,
    };

    fn kimi_code_wire(ts_ms: i64, model: &str, usage: [u64; 4]) -> String {
        [
            serde_json::json!({
                "type": "llm.request",
                "kind": "loop",
                "provider": "kimi",
                "model": "k3",
                "modelAlias": model,
                "time": ts_ms - 1000
            })
            .to_string(),
            serde_json::json!({
                "type": "usage.record",
                "model": model,
                "usage": {
                    "inputOther": usage[0],
                    "output": usage[1],
                    "inputCacheRead": usage[2],
                    "inputCacheCreation": usage[3]
                },
                "usageScope": "turn",
                "time": ts_ms
            })
            .to_string(),
        ]
        .join("\n")
    }

    fn legacy_kimi_wire(ts_secs: f64, model: Option<&str>, usage: [u64; 4]) -> String {
        let mut payload = serde_json::json!({
            "token_usage": {
                "input_other": usage[0],
                "output": usage[1],
                "input_cache_read": usage[2],
                "input_cache_creation": usage[3]
            }
        });
        if let Some(model) = model {
            payload["model"] = serde_json::json!(model);
        }
        serde_json::json!({
            "timestamp": ts_secs,
            "message": {
                "type": "StatusUpdate",
                "payload": payload
            }
        })
        .to_string()
    }

    #[test]
    fn roots_resolve_overrides_defaults_and_deduplicate() {
        let home = Path::new("/home/tester");
        assert_eq!(
            kimi_roots(None, None, home),
            [home.join(".kimi-code"), home.join(".kimi")]
        );
        assert_eq!(
            kimi_roots(Some("~/custom-kimi-code"), Some("~/legacy-kimi"), home),
            [home.join("custom-kimi-code"), home.join("legacy-kimi")]
        );
        assert_eq!(
            kimi_roots(Some("~/.kimi"), None, home),
            [home.join(".kimi")]
        );
    }

    #[test]
    fn model_normalization_maps_kimi_code_short_aliases_and_keeps_foreign_models() {
        assert_eq!(
            normalize_kimi_model("kimi-code/k3", Some("kimi")),
            "kimi-k3"
        );
        assert_eq!(normalize_kimi_model("kimi-code/k2.5", None), "kimi-k2.5");
        assert_eq!(
            normalize_kimi_model("kimi-code/k3.1-pro", Some("kimi")),
            "kimi-k3.1-pro"
        );
        assert_eq!(
            normalize_kimi_model("moonshot-cn/kimi-k2.7-code", Some("kimi")),
            "kimi-k2.7-code"
        );
        assert_eq!(normalize_kimi_model("k3", Some("kimi")), "kimi-k3");
        assert_eq!(
            normalize_kimi_model("anthropic/claude-sonnet-4-6", Some("anthropic")),
            "anthropic/claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_kimi_model("glm-5.3-flash", Some("zai")),
            "glm-5.3-flash"
        );
    }

    #[test]
    fn parser_reads_kimi_code_usage_records_and_legacy_status_updates() {
        let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
        let modern = kimi_code_wire(
            now.timestamp_millis(),
            "kimi-code/k3",
            [2_000, 500, 20_000, 0],
        );
        let parsed = parse_jsonl(&modern, None);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].model.as_deref(), Some("kimi-k3"));
        assert_eq!(parsed[0].input, 2_000);
        assert_eq!(parsed[0].output, 500);
        assert_eq!(parsed[0].cache_read, 20_000);
        assert_eq!(parsed[0].cache_write, 0);

        let legacy = legacy_kimi_wire(now.timestamp() as f64 + 0.5, None, [1_000, 250, 5_000, 100]);
        let parsed_legacy = parse_jsonl(&legacy, Some("moonshot-cn/kimi-k2.7-code"));
        assert_eq!(parsed_legacy.len(), 1);
        assert_eq!(parsed_legacy[0].model.as_deref(), Some("kimi-k2.7-code"));
        assert_eq!(parsed_legacy[0].pricing_tokens().total_tokens(), 6_350);
    }

    #[test]
    fn scanner_prices_kimi_code_usage_routes_foreign_models_and_deduplicates_migrated_sessions() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
        let pricing = test_bundled_pricing();
        let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();

        let code_root = dir.path().join(".kimi-code");
        let legacy_root = dir.path().join(".kimi");
        let uuid = "baca9151-4943-46ee-a9af-4aa1de017e8d";

        let code_wire = code_root
            .join("sessions")
            .join("wd_project_abc")
            .join(format!("session_{uuid}"))
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        fs::create_dir_all(code_wire.parent().unwrap()).unwrap();
        let content = [
            kimi_code_wire(
                now.timestamp_millis(),
                "kimi-code/k3",
                [1_000_000, 100_000, 2_000_000, 0],
            ),
            serde_json::json!({
                "type": "usage.record",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "inputOther": 100_000,
                    "output": 10_000,
                    "inputCacheRead": 0,
                    "inputCacheCreation": 0
                },
                "usageScope": "turn",
                "time": now.timestamp_millis()
            })
            .to_string(),
            serde_json::json!({
                "type": "usage.record",
                "model": "GLM-5.3-Flash",
                "usage": {
                    "inputOther": 50_000,
                    "output": 5_000,
                    "inputCacheRead": 0,
                    "inputCacheCreation": 0
                },
                "usageScope": "turn",
                "time": now.timestamp_millis()
            })
            .to_string(),
            serde_json::json!({
                "type": "usage.record",
                "model": "deepseek-flash",
                "usage": {
                    "inputOther": 40_000,
                    "output": 4_000,
                    "inputCacheRead": 0,
                    "inputCacheCreation": 0
                },
                "usageScope": "turn",
                "time": now.timestamp_millis()
            })
            .to_string(),
            // Session summary scope is ignored to avoid double-counting.
            serde_json::json!({
                "type": "usage.record",
                "model": "kimi-code/k3",
                "usage": {
                    "inputOther": 9_999_999,
                    "output": 0,
                    "inputCacheRead": 0,
                    "inputCacheCreation": 0
                },
                "usageScope": "session",
                "time": now.timestamp_millis()
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(&code_wire, content).unwrap();

        // A legacy ~/.kimi session with the same UUID must be skipped (already migrated).
        let migrated_legacy = legacy_root
            .join("sessions")
            .join("hash")
            .join(uuid)
            .join("wire.jsonl");
        fs::create_dir_all(migrated_legacy.parent().unwrap()).unwrap();
        fs::write(
            &migrated_legacy,
            legacy_kimi_wire(now.timestamp() as f64, Some("kimi-k3"), [500_000, 0, 0, 0]),
        )
        .unwrap();

        // A distinct legacy ~/.kimi session (different UUID) must still be counted and use config.toml fallback.
        fs::write(
            legacy_root.join("config.toml"),
            "default_model = \"moonshot-cn/kimi-k2.7-code\"\n[thinking]\nenabled = true\n",
        )
        .unwrap();
        assert_eq!(
            read_default_model(&legacy_root.join("config.toml")).as_deref(),
            Some("moonshot-cn/kimi-k2.7-code")
        );
        let unmigrated_legacy = legacy_root
            .join("sessions")
            .join("hash")
            .join("11111111-2222-3333-4444-555555555555")
            .join("wire.jsonl");
        fs::create_dir_all(unmigrated_legacy.parent().unwrap()).unwrap();
        fs::write(
            &unmigrated_legacy,
            legacy_kimi_wire(now.timestamp() as f64, None, [1_000_000, 0, 0, 0]),
        )
        .unwrap();

        let roots = [code_root, legacy_root];

        // 1. Kimi card gets kimi-k3 (from ~/.kimi-code) + kimi-k2.7-code (from unmigrated ~/.kimi),
        //    and deepseek-flash in other_usage; claude & glm are excluded from Kimi.
        let mut kimi_acc = DailyUsageAccumulator::default();
        assert!(scan_roots_into(&storage, &roots, now, &pricing, "kimi", &mut kimi_acc).unwrap());
        let kimi = kimi_acc.build(now, "From your Kimi Code history (estimated)");
        let today = kimi.today.unwrap();
        assert_eq!(today.tokens, 4_100_000);
        // kimi-k3: 1M*3 + 0.1M*15 + 2M*0.3 = 3 + 1.5 + 0.6 = 5.10
        // kimi-k2.7-code: 1M*0.95 = 0.95 -> total = 6.05
        assert!((today.estimated_cost_usd.unwrap() - 6.05).abs() < 1e-6);
        assert!(today.unknown_models.is_empty());
        assert_eq!(kimi.other_usage.unwrap().today.unwrap().tokens, 44_000);

        // 2. Claude card receives the routed claude-sonnet-4-6 usage.
        let mut claude_acc = DailyUsageAccumulator::default();
        assert!(
            scan_roots_into(&storage, &roots, now, &pricing, "claude", &mut claude_acc).unwrap()
        );
        assert_eq!(claude_acc.build(now, "test").today.unwrap().tokens, 110_000);

        // 3. Z.ai card receives the routed GLM-5.3-Flash usage.
        let mut zai_acc = DailyUsageAccumulator::default();
        assert!(scan_roots_into(&storage, &roots, now, &pricing, "zai", &mut zai_acc).unwrap());
        assert_eq!(zai_acc.build(now, "test").today.unwrap().tokens, 55_000);
    }

    #[test]
    fn subagent_wire_files_in_same_session_are_both_counted_and_cached() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
        let pricing = test_bundled_pricing();
        let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();

        let code_root = dir.path().join(".kimi-code");
        let session_dir = code_root
            .join("sessions")
            .join("wd_repo")
            .join("session_3aaa942b-2021-4bb9-8a75-302238866260")
            .join("agents");
        let main_wire = session_dir.join("main").join("wire.jsonl");
        let sub_wire = session_dir.join("subagent-1").join("wire.jsonl");
        fs::create_dir_all(main_wire.parent().unwrap()).unwrap();
        fs::create_dir_all(sub_wire.parent().unwrap()).unwrap();
        fs::write(
            &main_wire,
            kimi_code_wire(
                now.timestamp_millis(),
                "kimi-code/k3",
                [10_000, 1_000, 0, 0],
            ),
        )
        .unwrap();
        fs::write(
            &sub_wire,
            kimi_code_wire(
                now.timestamp_millis(),
                "kimi-code/k3",
                [20_000, 2_000, 0, 0],
            ),
        )
        .unwrap();

        let roots = [code_root];
        let mut first = DailyUsageAccumulator::default();
        assert!(scan_roots_into(&storage, &roots, now, &pricing, "kimi", &mut first).unwrap());
        assert_eq!(first.build(now, "test").today.unwrap().tokens, 33_000);

        // Second scan hits persistent cache and produces identical totals.
        let mut second = DailyUsageAccumulator::default();
        assert!(scan_roots_into(&storage, &roots, now, &pricing, "kimi", &mut second).unwrap());
        assert_eq!(second.build(now, "test").today.unwrap().tokens, 33_000);

        // Removing the subagent file prunes its cached rows.
        fs::remove_file(&sub_wire).unwrap();
        let mut third = DailyUsageAccumulator::default();
        assert!(scan_roots_into(&storage, &roots, now, &pricing, "kimi", &mut third).unwrap());
        assert_eq!(third.build(now, "test").today.unwrap().tokens, 11_000);
    }

    #[test]
    fn ses_prefixed_sessions_deduplicate_against_legacy_kimi_root() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
        let pricing = test_bundled_pricing();
        let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();

        let code_root = dir.path().join(".kimi-code");
        let legacy_root = dir.path().join(".kimi");
        let uuid = "423f0afc-b11b-423e-8164-4f21b2c862d8";

        let code_wire = code_root
            .join("sessions")
            .join("wd_repo")
            .join(format!("ses_{uuid}"))
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        let legacy_wire = legacy_root
            .join("sessions")
            .join("hash_a")
            .join(uuid)
            .join("wire.jsonl");
        fs::create_dir_all(code_wire.parent().unwrap()).unwrap();
        fs::create_dir_all(legacy_wire.parent().unwrap()).unwrap();
        fs::write(
            &code_wire,
            kimi_code_wire(
                now.timestamp_millis(),
                "kimi-code/k3",
                [10_000, 1_000, 0, 0],
            ),
        )
        .unwrap();
        fs::write(
            &legacy_wire,
            legacy_kimi_wire(
                now.timestamp() as f64 + 1.0,
                Some("kimi-code/k3"),
                [10_000, 1_000, 0, 0],
            ),
        )
        .unwrap();

        let roots = [code_root, legacy_root];
        let mut accumulator = DailyUsageAccumulator::default();
        assert!(
            scan_roots_into(&storage, &roots, now, &pricing, "kimi", &mut accumulator).unwrap()
        );
        assert_eq!(accumulator.build(now, "test").today.unwrap().tokens, 11_000);
    }

    #[test]
    fn duplicate_events_in_same_or_copied_wire_files_are_counted_once() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
        let pricing = test_bundled_pricing();
        let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
        let ts_ms = now.timestamp_millis();

        let code_root = dir.path().join(".kimi-code");
        let wire_a = code_root
            .join("sessions")
            .join("wd_a")
            .join("custom-dir-a")
            .join("wire.jsonl");
        let wire_b = code_root
            .join("sessions")
            .join("wd_b")
            .join("custom-dir-b")
            .join("wire.jsonl");
        fs::create_dir_all(wire_a.parent().unwrap()).unwrap();
        fs::create_dir_all(wire_b.parent().unwrap()).unwrap();

        let record_and_legacy = [
            kimi_code_wire(ts_ms, "kimi-code/k3", [50_000, 5_000, 0, 0]),
            legacy_kimi_wire(
                ts_ms as f64 / 1000.0,
                Some("kimi-code/k3"),
                [50_000, 5_000, 0, 0],
            ),
        ]
        .join("\n");
        fs::write(&wire_a, &record_and_legacy).unwrap();
        fs::write(&wire_b, &record_and_legacy).unwrap();

        let mut accumulator = DailyUsageAccumulator::default();
        assert!(scan_roots_into(
            &storage,
            std::slice::from_ref(&code_root),
            now,
            &pricing,
            "kimi",
            &mut accumulator,
        )
        .unwrap());
        assert_eq!(accumulator.build(now, "test").today.unwrap().tokens, 55_000);
    }

    #[test]
    #[ignore = "reads the developer's local ~/.kimi-code directory; pass KIMI_SCAN_AS_OF=<rfc3339> for historical data"]
    fn inspect_real_kimi_code_directory() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
        let pricing = test_bundled_pricing();
        let now = std::env::var("KIMI_SCAN_AS_OF")
            .ok()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value.trim()).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);
        let roots = super::default_roots();
        let mut accumulator = DailyUsageAccumulator::default();
        let contributed =
            scan_roots_into(&storage, &roots, now, &pricing, "kimi", &mut accumulator).unwrap();
        let history = accumulator.build(now, "From your Kimi Code history (estimated)");
        println!(
            "contributed={contributed} roots={roots:?} now={now} daily={:?} last30={:?}",
            history.daily, history.last_30_days
        );
        assert!(contributed);
        let last30 = history.last_30_days.expect("expected last30 usage");
        assert!(last30.tokens > 0);
        assert!(last30.estimated_cost_usd.is_some_and(|cost| cost > 0.0));
        assert!(last30.unknown_models.is_empty());
    }

    #[test]
    fn legacy_status_updates_inherit_the_request_provider_only_for_its_model() {
        let request = serde_json::json!({
            "type": "llm.request",
            "provider": "ollama",
            "model": "glm-4.7",
            "time": 1_784_800_000_000_i64
        })
        .to_string();
        let content = [
            request,
            legacy_kimi_wire(1_784_800_001.0, None, [10, 1, 0, 0]),
            legacy_kimi_wire(1_784_800_002.0, Some("glm-4.7"), [20, 2, 0, 0]),
            legacy_kimi_wire(1_784_800_003.0, Some("kimi-k3"), [30, 3, 0, 0]),
        ]
        .join("\n");
        let providers = parse_jsonl(&content, None)
            .into_iter()
            .map(|event| (event.input, event.provider))
            .collect::<Vec<_>>();
        assert_eq!(
            providers,
            [
                (10, Some("ollama".to_owned())),
                (20, Some("ollama".to_owned())),
                (30, None),
            ]
        );
    }

    #[test]
    fn local_runtime_usage_stays_unpriced_in_kimi_others() {
        let pricing = test_bundled_pricing();
        let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
        let event = |model: &str| KimiTokenEvent {
            timestamp: now,
            provider: Some("ollama".to_owned()),
            model: Some(model.to_owned()),
            input: 1_000_000,
            cache_write: 0,
            cache_read: 0,
            output: 0,
        };
        let events = vec![event("kimi-k3"), event("glm-4.7")];

        let mut kimi_acc = DailyUsageAccumulator::default();
        assert!(!aggregate_into(
            &events,
            now,
            &pricing,
            "kimi",
            &mut kimi_acc
        ));
        let kimi = kimi_acc.build(now, "test");
        let other = kimi.other_usage.unwrap().today.unwrap();
        assert_eq!(other.tokens, 2_000_000);
        assert!(other.estimated_cost_usd.unwrap_or_default().abs() < 1e-9);

        let mut zai_acc = DailyUsageAccumulator::default();
        assert!(!aggregate_into(&events, now, &pricing, "zai", &mut zai_acc));
        assert!(zai_acc.build(now, "test").today.is_none());
    }

    /// Pins the output of kimi's per-event policy, so moving it into `card_usage` cannot change
    /// what any Card shows.
    mod characterization {
        use chrono::{DateTime, Duration, TimeZone, Utc};

        use super::super::{aggregate_into, KimiTokenEvent};
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
            provider: Option<&str>,
            model: Option<&str>,
            input: u64,
            output: u64,
        ) -> KimiTokenEvent {
            KimiTokenEvent {
                timestamp: now() - Duration::days(days_ago),
                provider: provider.map(str::to_owned),
                model: model.map(str::to_owned),
                input,
                cache_write: 0,
                cache_read: 0,
                output,
            }
        }

        fn events() -> Vec<KimiTokenEvent> {
            let kimi = Some("kimi-code");
            let ollama = Some("ollama");
            let mut cached = event(0, kimi, Some("kimi-k2"), 10_000, 500);
            cached.cache_write = 2_000;
            cached.cache_read = 6_000;
            let mut future = event(0, kimi, Some("kimi-k2"), 9_999, 9_999);
            future.timestamp = now() + Duration::hours(1);
            vec![
                // Home models, labelled by family without a variant when priced.
                cached,
                event(0, kimi, Some("kimi-k2.5"), 2_000, 200),
                event(1, kimi, Some("kimi-k2-thinking"), 1_000, 100),
                event(1, None, Some("moonshot-v1-8k"), 1_000, 100),
                // A kimi-family model without a price, and a missing model.
                event(2, kimi, Some("kimi-k99-unreleased"), 700, 70),
                event(2, kimi, None, 400, 40),
                // Routed to other Cards.
                event(3, kimi, Some("claude-sonnet-4-5"), 1_500, 150),
                event(3, kimi, Some("gpt-5.5"), 1_500, 150),
                event(3, kimi, Some("gemini-3-pro"), 800, 80),
                event(3, kimi, Some("grok-4"), 800, 80),
                event(3, kimi, Some("glm-4.6"), 600, 60),
                event(3, kimi, Some("minimax-m2"), 600, 60),
                // Others: priced, unpriced, and gpt-oss.
                event(4, kimi, Some("deepseek-chat"), 900, 90),
                event(4, kimi, Some("totally-unknown-model"), 50, 5),
                event(4, kimi, Some("gpt-oss-120b"), 500, 50),
                // Local runtime by provider: Others on kimi whatever the model, skipped elsewhere.
                event(4, ollama, Some("llama3"), 300, 30),
                event(4, ollama, Some("claude-sonnet-4-5"), 300, 30),
                event(4, ollama, Some("kimi-k2"), 300, 30),
                event(4, ollama, None, 300, 30),
                // Local runtime by slug only.
                event(4, kimi, Some("ollama/llama3"), 50, 5),
                // Zero tokens, guarded everywhere.
                event(5, kimi, Some("kimi-k2"), 0, 0),
                event(5, kimi, Some("deepseek-chat"), 0, 0),
                event(5, kimi, Some("gpt-5.5"), 0, 0),
                event(5, ollama, Some("llama3"), 0, 0),
                event(5, kimi, None, 0, 0),
                // Window edges: kimi already uses 30 days including today everywhere.
                event(29, kimi, Some("kimi-k2"), 100, 10),
                event(29, kimi, Some("gpt-5.5"), 100, 10),
                event(30, kimi, Some("kimi-k2"), 100, 10),
                event(30, kimi, Some("gpt-5.5"), 100, 10),
                event(30, kimi, Some("deepseek-chat"), 100, 10),
                future,
            ]
        }

        #[test]
        fn output_is_pinned_per_card() {
            let actual = [
                "kimi",
                "claude",
                "codex",
                "antigravity",
                "grok",
                "zai",
                "minimax",
            ]
            .map(|card_id| {
                let mut accumulator = DailyUsageAccumulator::default();
                let contributed = aggregate_into(
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
            assert_pinned("per card", &actual, PER_CARD);
        }

        const PER_CARD: &str = r#"
== kimi contributed=true
{
  "today": {
    "tokens": 20700,
    "estimatedCostUsd": 0.01061,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "kimi-k2",
          "totalTokens": 18500,
          "costUsd": 0.00881
        },
        {
          "model": "moonshot/kimi-k2.5",
          "totalTokens": 2200,
          "costUsd": 0.0018
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 2200,
    "estimatedCostUsd": 0.00125,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "kimi-k2-thinking",
          "totalTokens": 1100,
          "costUsd": 0.00085
        },
        {
          "model": "moonshot-v1-8k",
          "totalTokens": 1100,
          "costUsd": 0.0004
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "last30Days": {
    "tokens": 24220,
    "estimatedCostUsd": 0.011944999999999999,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "kimi-k2",
          "totalTokens": 18610,
          "costUsd": 0.008895
        },
        {
          "model": "moonshot/kimi-k2.5",
          "totalTokens": 2200,
          "costUsd": 0.0018
        },
        {
          "model": "Other",
          "totalTokens": 3410,
          "costUsd": null,
          "variants": [
            {
              "model": "kimi-k2-thinking",
              "totalTokens": 1100,
              "costUsd": 0.00085
            },
            {
              "model": "moonshot-v1-8k",
              "totalTokens": 1100,
              "costUsd": 0.0004
            },
            {
              "model": "kimi-k99-unreleased",
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
      "kimi-k99-unreleased"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 20700,
      "estimatedCostUsd": 0.01061,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 2200,
      "estimatedCostUsd": 0.00125,
      "estimateComplete": true
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
      "estimatedCostUsd": 0.000085,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed",
    "kimi-k99-unreleased"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": null,
    "last30Days": {
      "models": {
        "Unattributed": {
          "tokens": 330,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        },
        "claude-sonnet-4-5": {
          "tokens": 330,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        },
        "deepseek-chat": {
          "tokens": 990,
          "pricedTokens": 990,
          "estimatedCostUsd": 0.0002898,
          "costEstimated": true
        },
        "gpt-oss-120b": {
          "tokens": 550,
          "pricedTokens": 550,
          "estimatedCostUsd": 0.000105,
          "costEstimated": true
        },
        "kimi-k2": {
          "tokens": 330,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        },
        "llama3": {
          "tokens": 330,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
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
      "tokens": 2970,
      "pricedTokens": 1540,
      "estimatedCostUsd": 0.0003948,
      "costEstimated": true,
      "estimateComplete": false
    }
  }
}
== claude contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 1650,
    "estimatedCostUsd": 0.00675,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 1650,
          "costUsd": 0.00675
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
      "estimatedCostUsd": 0.00675,
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
    "tokens": 1760,
    "estimatedCostUsd": 0.0128,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gpt-5.5",
          "totalTokens": 1760,
          "costUsd": 0.0128
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
    },
    {
      "date": "today-29",
      "tokens": 110,
      "estimatedCostUsd": 0.0008,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== antigravity contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 880,
    "estimatedCostUsd": 0.00256,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 880,
          "costUsd": 0.00256
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
      "estimatedCostUsd": 0.00256,
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
== minimax contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 660,
    "estimatedCostUsd": 0.000252,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "minimax-m2",
          "totalTokens": 660,
          "costUsd": 0.000252
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
      "estimatedCostUsd": 0.000252,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
"#;
    }
}
