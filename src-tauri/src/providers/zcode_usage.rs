use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use zeroize::Zeroizing;

#[cfg(test)]
use crate::models::UsageHistory;
use crate::{
    pricing::{ModelPricing, ServiceTier, TokenBreakdown},
    providers::{
        daily_usage::DailyUsageAccumulator,
        model_scope::{
            card_for_model, is_local_runtime_provider, model_belongs_to_card,
            normalize_provider_model,
        },
    },
};

#[cfg(test)]
const SOURCE_NOTE: &str = "From your ZCode history (estimated)";
const REQUIRED_COLUMNS: &[&str] = &[
    "provider_id",
    "model_id",
    "variant",
    "started_at",
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "computed_total_tokens",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZCodeProvider {
    Antigravity,
    Claude,
    Codex,
    Grok,
    Kimi,
    MiniMax,
    Zai,
}

impl ZCodeProvider {
    pub(crate) fn from_card_id(card_id: &str) -> Option<Self> {
        match card_id {
            "antigravity" => Some(Self::Antigravity),
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "grok" => Some(Self::Grok),
            "kimi" => Some(Self::Kimi),
            "minimax" => Some(Self::MiniMax),
            "zai" => Some(Self::Zai),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Kimi => "kimi",
            Self::MiniMax => "minimax",
            Self::Zai => "zai",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ZCodeUsageScanner {
    database_path: PathBuf,
    config_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ZCodeUsageError {
    #[error("ZCode usage database could not be read")]
    Database,
    #[error("ZCode usage database has an unsupported schema")]
    Schema,
}

#[derive(Debug)]
struct UsageEvent {
    provider_id: String,
    model: String,
    variant: Option<String>,
    timestamp: DateTime<Utc>,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    total: u64,
}

#[derive(Default, Deserialize)]
struct ZCodeConfig {
    #[serde(default)]
    provider: HashMap<String, ZCodeProviderConfig>,
}

#[derive(Default, Deserialize)]
struct ZCodeProviderConfig {
    name: Option<String>,
    #[serde(default)]
    options: ZCodeProviderOptions,
}

#[derive(Default, Deserialize)]
struct ZCodeProviderOptions {
    #[serde(rename = "baseURL")]
    base_url: Option<String>,
}

impl ZCodeUsageScanner {
    pub(crate) fn new() -> Self {
        let home = home_directory();
        let root = crate::provider_environment::value("ZCODE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".zcode"));
        Self {
            database_path: root.join("cli").join("db").join("db.sqlite"),
            config_path: root.join("v2").join("config.json"),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_paths(database_path: PathBuf, config_path: PathBuf) -> Self {
        Self {
            database_path,
            config_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn scan(
        &self,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
        target: ZCodeProvider,
    ) -> Result<UsageHistory, ZCodeUsageError> {
        let mut accumulator = DailyUsageAccumulator::default();
        self.scan_into(now, pricing, target, &mut accumulator)?;
        Ok(accumulator.build(now, SOURCE_NOTE))
    }

    pub(crate) fn scan_into(
        &self,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
        target: ZCodeProvider,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<bool, ZCodeUsageError> {
        let configured = configured_providers(&self.config_path);
        let events = read_events(&self.database_path, now)?;
        let since = history_start(now);
        let mut contributed = false;

        for event in events {
            let date = event.timestamp.with_timezone(&Local).date_naive();
            if event.timestamp > now || date < since {
                continue;
            }
            if is_delegated_cli(&event.provider_id.trim().to_ascii_lowercase()) {
                continue;
            }
            let is_ollama = is_local_runtime_provider(&event.provider_id);
            let configured_owner =
                provider_kind(&event.provider_id, configured.get(&event.provider_id));
            if event.model.is_empty() {
                if configured_owner == Some(target) {
                    contributed = true;
                    accumulator.add_unpriced(date, event.total, "Unattributed");
                }
                continue;
            }
            let mut event = event;
            event.model = if configured_owner == Some(ZCodeProvider::Kimi) {
                normalize_provider_model("kimi", &event.model)
            } else {
                normalize_provider_model(&event.provider_id, &event.model)
            };
            let model_owner = if is_ollama {
                None
            } else {
                card_for_model(&event.model).and_then(ZCodeProvider::from_card_id)
            };
            if let Some(owner) = model_owner {
                if owner == target {
                    contributed = true;
                    add_event(accumulator, date, &event, pricing, target);
                }
                continue;
            }
            if configured_owner == Some(target)
                && model_belongs_to_card(target.as_str(), &event.model)
            {
                contributed = true;
                add_event(accumulator, date, &event, pricing, target);
                continue;
            }
            let other_owner = configured_owner.unwrap_or(ZCodeProvider::Zai);
            if other_owner == target {
                add_other_event(accumulator, date, &event, pricing, target, is_ollama);
            }
        }
        Ok(contributed)
    }
}

fn read_events(path: &Path, now: DateTime<Utc>) -> Result<Vec<UsageEvent>, ZCodeUsageError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        _ => return Err(ZCodeUsageError::Database),
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| ZCodeUsageError::Database)?;
    connection
        .busy_timeout(Duration::from_millis(150))
        .map_err(|_| ZCodeUsageError::Database)?;
    let Some(columns) = table_columns(&connection, "model_usage")? else {
        return Ok(Vec::new());
    };
    if !REQUIRED_COLUMNS
        .iter()
        .all(|column| columns.contains(*column))
    {
        return Err(ZCodeUsageError::Schema);
    }

    // This coarse UTC cutoff leaves room for every local timezone. Exact calendar filtering happens
    // after timestamps are decoded.
    let cutoff_ms = now
        .checked_sub_days(Days::new(32))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
        .timestamp_millis();
    let mut statement = connection
        .prepare(
            "SELECT provider_id, model_id, variant, started_at, input_tokens, output_tokens, \
                    cache_creation_input_tokens, cache_read_input_tokens, computed_total_tokens \
             FROM model_usage \
             WHERE started_at >= ?1 \
               AND (input_tokens > 0 OR output_tokens > 0 \
                    OR cache_creation_input_tokens > 0 OR cache_read_input_tokens > 0 \
                    OR computed_total_tokens > 0)",
        )
        .map_err(|_| ZCodeUsageError::Database)?;
    let rows = statement
        .query_map([cutoff_ms], |row| {
            let provider_id = row.get::<_, String>(0)?;
            let model = row.get::<_, String>(1)?;
            let variant = row.get::<_, Option<String>>(2)?;
            let started_at = row.get::<_, i64>(3)?;
            let input = non_negative(row.get::<_, i64>(4)?);
            let output = non_negative(row.get::<_, i64>(5)?);
            let cache_write = non_negative(row.get::<_, i64>(6)?);
            let cache_read = non_negative(row.get::<_, i64>(7)?);
            let computed_total = non_negative(row.get::<_, i64>(8)?);
            Ok((
                provider_id,
                model,
                variant,
                started_at,
                input,
                output,
                cache_write,
                cache_read,
                computed_total,
            ))
        })
        .map_err(|_| ZCodeUsageError::Database)?;

    let mut events = Vec::new();
    for row in rows {
        let (
            provider_id,
            model,
            variant,
            started_at,
            input,
            output,
            cache_write,
            cache_read,
            computed_total,
        ) = row.map_err(|_| ZCodeUsageError::Database)?;
        let Some(timestamp) = DateTime::<Utc>::from_timestamp_millis(started_at) else {
            continue;
        };
        let provider_id = provider_id.trim();
        let model = model.trim();
        if provider_id.is_empty() {
            continue;
        }
        let fallback_total = input.saturating_add(output);
        let total = computed_total.max(fallback_total);
        if total == 0 {
            continue;
        }
        events.push(UsageEvent {
            provider_id: provider_id.to_owned(),
            model: model.to_owned(),
            variant: variant.and_then(non_empty),
            timestamp,
            input,
            output,
            cache_write,
            cache_read,
            total,
        });
    }
    Ok(events)
}

fn resolve_rate_model<'a>(
    event: &'a UsageEvent,
    pricing: &ModelPricing,
    provider: &str,
) -> (Option<String>, &'a str) {
    let variant_model = event
        .variant
        .as_deref()
        .map(|variant| format!("{}-{variant}", event.model));
    let has_variant_rate = variant_model.as_deref().is_some_and(|model| {
        pricing
            .historical_rates(provider, model, event.timestamp, ServiceTier::Standard, 0)
            .is_some()
            || pricing.resolve(model).is_some()
    });
    if has_variant_rate {
        (variant_model, &event.model)
    } else {
        (None, &event.model)
    }
}

fn event_tokens(event: &UsageEvent) -> TokenBreakdown {
    let categorized_cache = event.cache_read.saturating_add(event.cache_write);
    TokenBreakdown {
        input: event.input.saturating_sub(categorized_cache),
        cache_write_5m: event.cache_write,
        cache_read: event.cache_read,
        output: event.output,
        ..TokenBreakdown::default()
    }
}

fn add_event(
    accumulator: &mut DailyUsageAccumulator,
    date: NaiveDate,
    event: &UsageEvent,
    pricing: &ModelPricing,
    target: ZCodeProvider,
) {
    let provider = target.as_str();
    let (variant_model, fallback) = resolve_rate_model(event, pricing, provider);
    let rate_model = variant_model.as_deref().unwrap_or(fallback);
    let tokens = event_tokens(event);
    let display_family = pricing.display_family(rate_model);
    if let Some(cost) =
        pricing.estimated_cost_at(provider, rate_model, event.timestamp, tokens, true)
    {
        if display_family != rate_model {
            accumulator.add_variant(date, event.total, cost, &display_family, rate_model);
        } else {
            accumulator.add(date, event.total, cost, &display_family);
        }
    } else {
        accumulator.add_unpriced(date, event.total, rate_model);
    }
}

fn add_other_event(
    accumulator: &mut DailyUsageAccumulator,
    date: NaiveDate,
    event: &UsageEvent,
    pricing: &ModelPricing,
    target: ZCodeProvider,
    is_local_runtime: bool,
) {
    let provider = target.as_str();
    let (variant_model, fallback) = resolve_rate_model(event, pricing, provider);
    let rate_model = variant_model.as_deref().unwrap_or(fallback);
    let tokens = event_tokens(event);
    let cost = (!is_local_runtime)
        .then(|| pricing.estimated_cost_at(provider, rate_model, event.timestamp, tokens, true))
        .flatten();
    accumulator.add_other(date, event.total, cost, true, rate_model);
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Option<HashSet<String>>, ZCodeUsageError> {
    let exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|_| ZCodeUsageError::Database)?;
    if !exists {
        return Ok(None);
    }
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| ZCodeUsageError::Database)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| ZCodeUsageError::Database)?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|_| ZCodeUsageError::Database)?;
    Ok(Some(columns))
}

fn configured_providers(path: &Path) -> HashMap<String, ZCodeProviderConfig> {
    fs::read(path)
        .ok()
        .map(Zeroizing::new)
        .and_then(|content| serde_json::from_slice::<ZCodeConfig>(&content).ok())
        .map(|config| config.provider)
        .unwrap_or_default()
}

fn provider_kind(provider_id: &str, config: Option<&ZCodeProviderConfig>) -> Option<ZCodeProvider> {
    let id = provider_id.trim().to_ascii_lowercase();
    if is_delegated_cli(&id) {
        return None;
    }
    if id.starts_with("builtin:bigmodel") || id.starts_with("builtin:zai") {
        return Some(ZCodeProvider::Zai);
    }
    for (marker, provider) in [
        ("anthropic", ZCodeProvider::Claude),
        ("claude", ZCodeProvider::Claude),
        ("openai", ZCodeProvider::Codex),
        ("codex", ZCodeProvider::Codex),
        ("antigravity", ZCodeProvider::Antigravity),
        ("google-antigravity", ZCodeProvider::Antigravity),
        ("google", ZCodeProvider::Antigravity),
        ("gemini", ZCodeProvider::Antigravity),
        ("minimax", ZCodeProvider::MiniMax),
        ("moonshot", ZCodeProvider::Kimi),
        ("kimi", ZCodeProvider::Kimi),
        ("grok", ZCodeProvider::Grok),
        ("xai", ZCodeProvider::Grok),
    ] {
        if id == marker || id == format!("builtin:{marker}") {
            return Some(provider);
        }
    }
    let config = config?;
    let name = config
        .name
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let base_url = config
        .options
        .base_url
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    provider_from_metadata(&name, &base_url)
}

fn provider_from_metadata(name: &str, base_url: &str) -> Option<ZCodeProvider> {
    let contains_any =
        |haystack: &str, needles: &[&str]| needles.iter().any(|needle| haystack.contains(needle));
    if contains_any(
        base_url,
        &["api.z.ai", "open.bigmodel.cn", "zcode.z.ai", "zhipuai.cn"],
    ) || contains_any(name, &["z.ai", "bigmodel", "zhipu"])
    {
        return Some(ZCodeProvider::Zai);
    }
    if contains_any(base_url, &["moonshot.cn", "moonshot.ai", "kimi.com"])
        || contains_any(name, &["moonshot", "kimi"])
    {
        return Some(ZCodeProvider::Kimi);
    }
    if contains_any(base_url, &["minimax.io", "minimaxi.com", "minimax.chat"])
        || name.contains("minimax")
    {
        return Some(ZCodeProvider::MiniMax);
    }
    if contains_any(base_url, &["api.x.ai", "grok.com"])
        || matches!(name.trim(), "xai" | "x.ai" | "grok")
    {
        return Some(ZCodeProvider::Grok);
    }
    if base_url.contains("api.anthropic.com") || matches!(name.trim(), "anthropic" | "claude") {
        return Some(ZCodeProvider::Claude);
    }
    if base_url.contains("api.openai.com") || matches!(name.trim(), "openai" | "codex") {
        return Some(ZCodeProvider::Codex);
    }
    if base_url.contains("generativelanguage.googleapis.com")
        || matches!(name.trim(), "google" | "gemini" | "antigravity")
    {
        return Some(ZCodeProvider::Antigravity);
    }
    None
}

fn is_delegated_cli(provider_id: &str) -> bool {
    ["codex-cli", "claude-cli", "gemini-cli", "agent-cli"]
        .iter()
        .any(|marker| provider_id.contains(marker))
}

fn history_start(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN)
}

fn non_negative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use chrono::{TimeZone, Utc};
    use rusqlite::{params, Connection};
    use tempfile::tempdir;

    use super::{provider_kind, ZCodeProvider, ZCodeUsageScanner};
    use crate::{pricing::test_bundled_pricing, providers::zcode_usage::ZCodeProviderConfig};

    fn create_database(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE model_usage (
                    provider_id TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    variant TEXT,
                    started_at INTEGER NOT NULL,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                    computed_total_tokens INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        connection
    }

    fn insert(
        connection: &Connection,
        provider: &str,
        model: &str,
        timestamp: i64,
        usage: [i64; 4],
    ) {
        connection
            .execute(
                "INSERT INTO model_usage VALUES (?1, ?2, 'max', ?3, ?4, ?5, 0, ?6, ?7)",
                params![provider, model, timestamp, usage[0], usage[1], usage[2], usage[3]],
            )
            .unwrap();
    }

    #[test]
    fn provider_mapping_uses_upstream_metadata_and_skips_delegated_clis() {
        let kimi: ZCodeProviderConfig = serde_json::from_str(
            r#"{"name":"Kimi mirror","options":{"baseURL":"https://api.kimi.com/coding/v1"}}"#,
        )
        .unwrap();
        assert_eq!(
            provider_kind("custom-id", Some(&kimi)),
            Some(ZCodeProvider::Kimi)
        );
        assert_eq!(
            provider_kind("builtin:zai-coding-plan", None),
            Some(ZCodeProvider::Zai)
        );
        assert_eq!(provider_kind("builtin:codex-cli", None), None);
        assert_eq!(provider_kind("builtin:gemini-cli", None), None);

        let openrouter: ZCodeProviderConfig = serde_json::from_str(
            r#"{"name":"OpenRouter mirror","options":{"baseURL":"https://openrouter.ai/api/v1"}}"#,
        )
        .unwrap();
        assert_eq!(provider_kind("openrouter", None), None);
        assert_eq!(provider_kind("custom-id", Some(&openrouter)), None);

        let unrelated: ZCodeProviderConfig = serde_json::from_str(
            r#"{"name":"Bailian","options":{"baseURL":"https://dashscope.aliyuncs.com/v1"}}"#,
        )
        .unwrap();
        assert_eq!(provider_kind("custom-id", Some(&unrelated)), None);
    }

    #[test]
    fn scanner_groups_provider_usage_and_keeps_token_bearing_error_attempts() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("db.sqlite");
        let config = directory.path().join("config.json");
        let connection = create_database(&database);
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        insert(
            &connection,
            "builtin:zai-coding-plan",
            "GLM-5.3-Flash",
            now.timestamp_millis(),
            [1_000_000, 100_000, 800_000, 1_100_000],
        );
        insert(
            &connection,
            "custom-openai",
            "gpt-5.6-sol",
            now.timestamp_millis(),
            [500_000, 50_000, 0, 550_000],
        );
        insert(
            &connection,
            "builtin:zai-coding-plan",
            "GLM-5.3-Flash",
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
                .unwrap()
                .timestamp_millis(),
            [1_000_000, 0, 0, 1_000_000],
        );
        fs::write(
            &config,
            r#"{"provider":{"custom-openai":{"name":"OpenAI","options":{"baseURL":"https://api.openai.com/v1"}}}}"#,
        )
        .unwrap();
        drop(connection);

        let scanner = ZCodeUsageScanner::for_paths(database, config);
        let pricing = test_bundled_pricing();
        let zai = scanner.scan(now, &pricing, ZCodeProvider::Zai).unwrap();
        let codex = scanner.scan(now, &pricing, ZCodeProvider::Codex).unwrap();
        assert_eq!(zai.today.as_ref().unwrap().tokens, 1_100_000);
        assert_eq!(
            zai.today.as_ref().unwrap().unknown_models,
            Vec::<String>::new()
        );
        assert!(
            (zai.today.as_ref().unwrap().estimated_cost_usd.unwrap() - 0.052).abs() < 0.000_001
        );
        assert_eq!(zai.daily.len(), 1);
        assert_eq!(codex.today.as_ref().unwrap().tokens, 550_000);
    }

    #[test]
    fn model_first_routing_handles_custom_relays_cross_provider_calls_and_others() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("db.sqlite");
        let config = directory.path().join("missing.json");
        let connection = create_database(&database);
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        for (provider, model, tokens) in [
            ("my-relay", "claude-sonnet-4-6", 100),
            ("my-relay", "gpt-5.4", 200),
            ("my-relay", "grok-4", 300),
            ("my-relay", "kimi-k3", 400),
            ("my-relay", "MiniMax-M2.7", 500),
            ("anthropic", "GLM-5.3-Flash", 600),
            ("my-relay", "gemini-3-pro", 650),
            ("openai", "", 250),
            ("builtin:codex-cli", "gpt-5.4", 9_999),
            ("anthropic", "deepseek-flash", 700),
            ("bailian", "qwen3-coder", 800),
        ] {
            insert(
                &connection,
                provider,
                model,
                now.timestamp_millis(),
                [tokens, 0, 0, tokens],
            );
        }
        drop(connection);

        let scanner = ZCodeUsageScanner::for_paths(database, config);
        let pricing = test_bundled_pricing();
        let antigravity = scanner
            .scan(now, &pricing, ZCodeProvider::Antigravity)
            .unwrap();
        let claude = scanner.scan(now, &pricing, ZCodeProvider::Claude).unwrap();
        let codex = scanner.scan(now, &pricing, ZCodeProvider::Codex).unwrap();
        let grok = scanner.scan(now, &pricing, ZCodeProvider::Grok).unwrap();
        let kimi = scanner.scan(now, &pricing, ZCodeProvider::Kimi).unwrap();
        let minimax = scanner.scan(now, &pricing, ZCodeProvider::MiniMax).unwrap();
        let zai = scanner.scan(now, &pricing, ZCodeProvider::Zai).unwrap();

        assert_eq!(antigravity.today.as_ref().unwrap().tokens, 650);
        assert_eq!(claude.today.as_ref().unwrap().tokens, 100);
        assert_eq!(codex.today.as_ref().unwrap().tokens, 450);
        assert_eq!(
            codex.today.as_ref().unwrap().unknown_models,
            ["Unattributed"]
        );
        assert_eq!(grok.today.as_ref().unwrap().tokens, 300);
        assert_eq!(kimi.today.as_ref().unwrap().tokens, 400);
        assert_eq!(minimax.today.as_ref().unwrap().tokens, 500);
        assert_eq!(zai.today.as_ref().unwrap().tokens, 600);

        // DeepSeek via `anthropic` lands in Claude's Others; unowned Bailian Qwen lands once in Zai's Others.
        assert_eq!(claude.other_usage.unwrap().today.unwrap().tokens, 700);
        assert_eq!(zai.other_usage.unwrap().today.unwrap().tokens, 800);
        assert!(codex.other_usage.is_none());
        assert!(grok.other_usage.is_none());
        assert!(kimi.other_usage.is_none());
        assert!(minimax.other_usage.is_none());
    }

    #[test]
    fn missing_database_is_an_empty_history_but_changed_schema_is_an_error() {
        let directory = tempdir().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        let pricing = test_bundled_pricing();
        let missing = ZCodeUsageScanner::for_paths(
            directory.path().join("missing.sqlite"),
            directory.path().join("missing.json"),
        );
        assert_eq!(
            missing.scan(now, &pricing, ZCodeProvider::Kimi).unwrap(),
            crate::models::UsageHistory::default()
        );

        let changed_path = directory.path().join("changed.sqlite");
        Connection::open(&changed_path)
            .unwrap()
            .execute("CREATE TABLE model_usage (provider_id TEXT)", [])
            .unwrap();
        let changed =
            ZCodeUsageScanner::for_paths(changed_path, directory.path().join("missing.json"));
        assert!(changed.scan(now, &pricing, ZCodeProvider::Kimi).is_err());
    }

    #[test]
    fn ollama_models_stay_unpriced_in_others_and_kimi_short_aliases_stay_in_kimi() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("zcode.sqlite");
        let config = directory.path().join("config.json");
        let connection = create_database(&database);
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap();
        insert(
            &connection,
            "ollama",
            "glm-4.7-flash",
            now.timestamp_millis(),
            [1000, 500, 0, 1500],
        );
        insert(
            &connection,
            "kimi-coding",
            "k3",
            now.timestamp_millis(),
            [2000, 500, 0, 2500],
        );
        drop(connection);

        let pricing = test_bundled_pricing();
        let scanner = ZCodeUsageScanner::for_paths(database, config);
        let zai = scanner.scan(now, &pricing, ZCodeProvider::Zai).unwrap();
        assert!(zai.today.is_none());
        let zai_other = zai.other_usage.unwrap().today.unwrap();
        assert_eq!(zai_other.tokens, 1500);
        assert_eq!(zai_other.estimated_cost_usd, None);

        let kimi = scanner.scan(now, &pricing, ZCodeProvider::Kimi).unwrap();
        assert_eq!(kimi.today.unwrap().tokens, 2500);
        assert!(kimi.other_usage.is_none());
    }
}
