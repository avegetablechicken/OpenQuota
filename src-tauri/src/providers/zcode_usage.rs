use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Days, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use zeroize::Zeroizing;

#[cfg(test)]
use crate::models::UsageHistory;
use crate::{
    pricing::{ModelPricing, ServiceTier, TokenBreakdown},
    providers::{
        card_usage::{self, LabelMode, LocalRuntimeMode, TransitionalPolicy, UsageObservation},
        daily_usage::DailyUsageAccumulator,
        model_scope::normalize_provider_model,
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
        let target_card = target.as_str();
        let policy = TransitionalPolicy {
            window_start_days_ago: 30,
            zero_token_guard: true,
            label: LabelMode::FamilyWithVariant,
            local_runtime: LocalRuntimeMode::ProviderKeepsHome,
        };
        let mut contributed = false;
        for mut event in events {
            if is_delegated_cli(&event.provider_id.trim().to_ascii_lowercase()) {
                continue;
            }
            let configured_owner =
                provider_kind(&event.provider_id, configured.get(&event.provider_id));
            if !event.model.is_empty() {
                event.model = if configured_owner == Some(ZCodeProvider::Kimi) {
                    normalize_provider_model("kimi", &event.model)
                } else {
                    normalize_provider_model(&event.provider_id, &event.model)
                };
            }
            // Rows from a provider no Card is configured for keep their Others on Zai.
            let home = configured_owner.unwrap_or(ZCodeProvider::Zai);
            let rate_model = (!event.model.is_empty())
                .then(|| resolve_rate_model(&event, pricing, target_card).0)
                .flatten();
            let observation = UsageObservation {
                rate_model: rate_model.as_deref(),
                home_is_fallback: configured_owner.is_none(),
                provider_hint: Some(&event.provider_id),
                total_tokens: event.total,
                ..UsageObservation::new(
                    event.timestamp,
                    Some(&event.model),
                    event_tokens(&event),
                    None,
                )
            };
            contributed |= card_usage::record(
                [observation],
                home.as_str(),
                target_card,
                now,
                pricing,
                policy,
                accumulator,
            )
            .own_series;
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

    /// Pins the output of zcode's per-row policy, so moving it into `card_usage` cannot change
    /// what any Card shows.
    mod characterization {
        use chrono::{DateTime, Duration, TimeZone, Utc};
        use rusqlite::params;
        use tempfile::tempdir;

        use super::{create_database, ZCodeProvider, ZCodeUsageScanner};
        use crate::{
            pricing::test_bundled_pricing,
            providers::{
                card_usage::test_support::{assert_pinned, relative_json},
                daily_usage::DailyUsageAccumulator,
            },
        };

        fn now() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap()
        }

        /// `(provider, model, variant, days_ago, [input, output, cache write, cache read, total])`
        type Row = (
            &'static str,
            &'static str,
            Option<&'static str>,
            i64,
            [i64; 5],
        );

        const ROWS: &[Row] = &[
            // Zai's own provider: priced with and without a variant, unpriced, Others, no model.
            (
                "builtin:zai",
                "glm-4.6",
                Some("max"),
                0,
                [2_000, 200, 0, 0, 0],
            ),
            ("builtin:zai", "glm-4.6", None, 0, [2_000, 200, 300, 500, 0]),
            (
                "builtin:zai",
                "glm-99-unreleased",
                None,
                1,
                [700, 70, 0, 0, 0],
            ),
            ("builtin:zai", "deepseek-chat", None, 1, [900, 90, 0, 0, 0]),
            ("builtin:zai", "", None, 1, [400, 40, 0, 0, 0]),
            // A provider configured as claude: own models, a routed model, Others, no model.
            (
                "my-anthropic",
                "claude-sonnet-4-5",
                Some("thinking"),
                0,
                [1_500, 150, 0, 0, 0],
            ),
            (
                "my-anthropic",
                "claude-opus-4-6",
                None,
                0,
                [1_500, 150, 0, 0, 2_000],
            ),
            ("my-anthropic", "gpt-5.5", None, 2, [1_000, 100, 0, 0, 0]),
            ("my-anthropic", "deepseek-chat", None, 2, [100, 10, 0, 0, 0]),
            ("my-anthropic", "", None, 2, [300, 30, 0, 0, 0]),
            // Built-in providers for other Cards, including gpt-oss on antigravity.
            ("builtin:kimi", "k2p5", None, 3, [600, 60, 0, 0, 0]),
            (
                "builtin:google",
                "gemini-3-pro",
                None,
                3,
                [800, 80, 0, 0, 0],
            ),
            (
                "builtin:google",
                "gpt-oss-120b",
                None,
                3,
                [500, 50, 0, 0, 0],
            ),
            ("builtin:minimax", "minimax-m2", None, 3, [600, 60, 0, 0, 0]),
            ("builtin:xai", "grok-4", None, 3, [600, 60, 0, 0, 0]),
            // An unconfigured provider: owned models route, the rest go to Zai's Others.
            (
                "openrouter",
                "claude-sonnet-4-5",
                None,
                4,
                [1_000, 100, 0, 0, 0],
            ),
            ("openrouter", "deepseek-chat", None, 4, [500, 50, 0, 0, 0]),
            ("openrouter", "", None, 4, [200, 20, 0, 0, 0]),
            // Ollama, configured as claude in one scenario and unconfigured in the other.
            ("ollama", "llama3", None, 5, [300, 30, 0, 0, 0]),
            ("ollama", "claude-sonnet-4-5", None, 5, [300, 30, 0, 0, 0]),
            ("ollama", "glm-4.6", None, 5, [300, 30, 0, 0, 0]),
            ("ollama", "", None, 5, [300, 30, 0, 0, 0]),
            // Delegated CLIs are skipped.
            (
                "builtin:codex-cli",
                "gpt-5.5",
                None,
                5,
                [1_000, 100, 0, 0, 0],
            ),
            // Window edges.
            ("builtin:zai", "glm-4.6", None, 29, [100, 10, 0, 0, 0]),
            (
                "my-anthropic",
                "claude-sonnet-4-5",
                None,
                29,
                [100, 10, 0, 0, 0],
            ),
            ("builtin:zai", "glm-4.6", None, 30, [100, 10, 0, 0, 0]),
            ("builtin:zai", "deepseek-chat", None, 30, [100, 10, 0, 0, 0]),
            ("builtin:zai", "glm-4.6", None, 31, [100, 10, 0, 0, 0]),
            ("builtin:zai", "glm-4.6", None, -1, [9_999, 9_999, 0, 0, 0]),
        ];

        fn scan_all_cards(config: &str) -> String {
            let directory = tempdir().unwrap();
            let database = directory.path().join("db.sqlite");
            let config_path = directory.path().join("config.json");
            fs_write(&config_path, config);
            let connection = create_database(&database);
            for (provider, model, variant, days_ago, usage) in ROWS {
                let started_at = (now() - Duration::days(*days_ago)).timestamp_millis();
                connection
                    .execute(
                        "INSERT INTO model_usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            provider, model, variant, started_at, usage[0], usage[1], usage[2],
                            usage[3], usage[4]
                        ],
                    )
                    .unwrap();
            }
            drop(connection);
            let scanner = ZCodeUsageScanner::for_paths(database, config_path);
            [
                ZCodeProvider::Zai,
                ZCodeProvider::Claude,
                ZCodeProvider::Codex,
                ZCodeProvider::Antigravity,
                ZCodeProvider::Kimi,
                ZCodeProvider::MiniMax,
                ZCodeProvider::Grok,
            ]
            .map(|target| {
                let mut accumulator = DailyUsageAccumulator::default();
                let contributed = scanner
                    .scan_into(now(), &test_bundled_pricing(), target, &mut accumulator)
                    .unwrap();
                let json = relative_json(&accumulator.build(now(), "test"), now());
                format!("== {} contributed={contributed}\n{json}", target.as_str())
            })
            .join("\n")
        }

        fn fs_write(path: &std::path::Path, content: &str) {
            std::fs::write(path, content).unwrap();
        }

        #[test]
        fn output_is_pinned_with_ollama_configured_as_claude() {
            let actual = scan_all_cards(
                r#"{"provider":{"my-anthropic":{"name":"anthropic"},"ollama":{"name":"claude"}}}"#,
            );
            assert_pinned("configured", &actual, CONFIGURED);
        }

        #[test]
        fn output_is_pinned_with_ollama_unconfigured() {
            let actual = scan_all_cards(r#"{"provider":{"my-anthropic":{"name":"anthropic"}}}"#);
            assert_pinned("unconfigured", &actual, UNCONFIGURED);
        }

        const CONFIGURED: &str = r#"
== zai contributed=true
{
  "today": {
    "tokens": 4400,
    "estimatedCostUsd": 0.0023575000000000002,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "glm-4.6",
          "totalTokens": 4400,
          "costUsd": 0.0023575000000000002
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 1210,
    "estimatedCostUsd": null,
    "costEstimated": false,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "glm-99-unreleased",
          "totalTokens": 770,
          "costUsd": null
        },
        {
          "model": "Other",
          "totalTokens": 440,
          "costUsd": null,
          "variants": [
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
      "glm-99-unreleased"
    ]
  },
  "last30Days": {
    "tokens": 5720,
    "estimatedCostUsd": 0.0024215,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "glm-4.6",
          "totalTokens": 4510,
          "costUsd": 0.0024215
        },
        {
          "model": "glm-99-unreleased",
          "totalTokens": 770,
          "costUsd": null
        },
        {
          "model": "Other",
          "totalTokens": 440,
          "costUsd": null,
          "variants": [
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
      "glm-99-unreleased"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 4400,
      "estimatedCostUsd": 0.0023575000000000002,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 1210,
      "estimatedCostUsd": null,
      "estimateComplete": false
    },
    {
      "date": "today-29",
      "tokens": 110,
      "estimatedCostUsd": 0.000064,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed",
    "glm-99-unreleased"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": {
      "models": {
        "deepseek-chat": {
          "tokens": 990,
          "pricedTokens": 990,
          "estimatedCostUsd": 0.0002898,
          "costEstimated": true
        }
      },
      "tokens": 990,
      "pricedTokens": 990,
      "estimatedCostUsd": 0.0002898,
      "costEstimated": true,
      "estimateComplete": true
    },
    "last30Days": {
      "models": {
        "deepseek-chat": {
          "tokens": 1540,
          "pricedTokens": 1540,
          "estimatedCostUsd": 0.0004508,
          "costEstimated": true
        }
      },
      "tokens": 1540,
      "pricedTokens": 1540,
      "estimatedCostUsd": 0.0004508,
      "costEstimated": true,
      "estimateComplete": true
    }
  }
}
== claude contributed=true
{
  "today": {
    "tokens": 3650,
    "estimatedCostUsd": 0.018,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 2000,
          "costUsd": 0.01125
        },
        {
          "model": "claude-sonnet-4-5-thinking",
          "totalTokens": 1650,
          "costUsd": 0.00675
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": null,
  "last30Days": {
    "tokens": 5850,
    "estimatedCostUsd": 0.0243,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 2000,
          "costUsd": 0.01125
        },
        {
          "model": "claude-sonnet-4-5-thinking",
          "totalTokens": 1650,
          "costUsd": 0.00675
        },
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 1540,
          "costUsd": 0.0063
        },
        {
          "model": "Other",
          "totalTokens": 660,
          "costUsd": null,
          "variants": [
            {
              "model": "Unattributed",
              "totalTokens": 660,
              "costUsd": null
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "Unattributed"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 3650,
      "estimatedCostUsd": 0.018,
      "estimateComplete": true
    },
    {
      "date": "today-2",
      "tokens": 330,
      "estimatedCostUsd": null,
      "estimateComplete": false
    },
    {
      "date": "today-4",
      "tokens": 1100,
      "estimatedCostUsd": 0.0045,
      "estimateComplete": true
    },
    {
      "date": "today-5",
      "tokens": 660,
      "estimatedCostUsd": 0.00135,
      "estimateComplete": false
    },
    {
      "date": "today-29",
      "tokens": 110,
      "estimatedCostUsd": 0.00045,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": null,
    "last30Days": {
      "models": {
        "deepseek-chat": {
          "tokens": 110,
          "pricedTokens": 110,
          "estimatedCostUsd": 0.000032200000000000003,
          "costEstimated": true
        },
        "glm-4.6": {
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
        }
      },
      "tokens": 770,
      "pricedTokens": 110,
      "estimatedCostUsd": 0.000032200000000000003,
      "costEstimated": true,
      "estimateComplete": false
    }
  }
}
== codex contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 1100,
    "estimatedCostUsd": 0.008,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gpt-5.5",
          "totalTokens": 1100,
          "costUsd": 0.008
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-2",
      "tokens": 1100,
      "estimatedCostUsd": 0.008,
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
    "tokens": 1430,
    "estimatedCostUsd": 0.0026650000000000003,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 880,
          "costUsd": 0.00256,
          "variants": [
            {
              "model": "gemini-3-pro",
              "totalTokens": 880,
              "costUsd": 0.00256
            }
          ]
        },
        {
          "model": "Other",
          "totalTokens": 550,
          "costUsd": 0.000105,
          "variants": [
            {
              "model": "gpt-oss-120b",
              "totalTokens": 550,
              "costUsd": 0.000105
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-3",
      "tokens": 1430,
      "estimatedCostUsd": 0.0026650000000000003,
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
    "estimatedCostUsd": 0.00054,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "moonshot/kimi-k2.5",
          "totalTokens": 660,
          "costUsd": 0.00054,
          "variants": [
            {
              "model": "kimi-k2p5",
              "totalTokens": 660,
              "costUsd": 0.00054
            }
          ]
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
      "estimatedCostUsd": 0.00054,
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
== grok contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 660,
    "estimatedCostUsd": 0.00015,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-4",
          "totalTokens": 660,
          "costUsd": 0.00015
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
      "estimatedCostUsd": 0.00015,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
"#;
        const UNCONFIGURED: &str = r#"
== zai contributed=true
{
  "today": {
    "tokens": 4400,
    "estimatedCostUsd": 0.0023575000000000002,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "glm-4.6",
          "totalTokens": 4400,
          "costUsd": 0.0023575000000000002
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 1210,
    "estimatedCostUsd": null,
    "costEstimated": false,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "glm-99-unreleased",
          "totalTokens": 770,
          "costUsd": null
        },
        {
          "model": "Other",
          "totalTokens": 440,
          "costUsd": null,
          "variants": [
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
      "glm-99-unreleased"
    ]
  },
  "last30Days": {
    "tokens": 5720,
    "estimatedCostUsd": 0.0024215,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "glm-4.6",
          "totalTokens": 4510,
          "costUsd": 0.0024215
        },
        {
          "model": "glm-99-unreleased",
          "totalTokens": 770,
          "costUsd": null
        },
        {
          "model": "Other",
          "totalTokens": 440,
          "costUsd": null,
          "variants": [
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
      "glm-99-unreleased"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 4400,
      "estimatedCostUsd": 0.0023575000000000002,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 1210,
      "estimatedCostUsd": null,
      "estimateComplete": false
    },
    {
      "date": "today-29",
      "tokens": 110,
      "estimatedCostUsd": 0.000064,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed",
    "glm-99-unreleased"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": {
      "models": {
        "deepseek-chat": {
          "tokens": 990,
          "pricedTokens": 990,
          "estimatedCostUsd": 0.0002898,
          "costEstimated": true
        }
      },
      "tokens": 990,
      "pricedTokens": 990,
      "estimatedCostUsd": 0.0002898,
      "costEstimated": true,
      "estimateComplete": true
    },
    "last30Days": {
      "models": {
        "claude-sonnet-4-5": {
          "tokens": 330,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        },
        "deepseek-chat": {
          "tokens": 1540,
          "pricedTokens": 1540,
          "estimatedCostUsd": 0.0004508,
          "costEstimated": true
        },
        "glm-4.6": {
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
        }
      },
      "tokens": 2530,
      "pricedTokens": 1540,
      "estimatedCostUsd": 0.0004508,
      "costEstimated": true,
      "estimateComplete": false
    }
  }
}
== claude contributed=true
{
  "today": {
    "tokens": 3650,
    "estimatedCostUsd": 0.018,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 2000,
          "costUsd": 0.01125
        },
        {
          "model": "claude-sonnet-4-5-thinking",
          "totalTokens": 1650,
          "costUsd": 0.00675
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": null,
  "last30Days": {
    "tokens": 5190,
    "estimatedCostUsd": 0.022949999999999998,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 2000,
          "costUsd": 0.01125
        },
        {
          "model": "claude-sonnet-4-5-thinking",
          "totalTokens": 1650,
          "costUsd": 0.00675
        },
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 1210,
          "costUsd": 0.0049499999999999995
        },
        {
          "model": "Other",
          "totalTokens": 330,
          "costUsd": null,
          "variants": [
            {
              "model": "Unattributed",
              "totalTokens": 330,
              "costUsd": null
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "Unattributed"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 3650,
      "estimatedCostUsd": 0.018,
      "estimateComplete": true
    },
    {
      "date": "today-2",
      "tokens": 330,
      "estimatedCostUsd": null,
      "estimateComplete": false
    },
    {
      "date": "today-4",
      "tokens": 1100,
      "estimatedCostUsd": 0.0045,
      "estimateComplete": true
    },
    {
      "date": "today-29",
      "tokens": 110,
      "estimatedCostUsd": 0.00045,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": null,
    "last30Days": {
      "models": {
        "deepseek-chat": {
          "tokens": 110,
          "pricedTokens": 110,
          "estimatedCostUsd": 0.000032200000000000003,
          "costEstimated": true
        }
      },
      "tokens": 110,
      "pricedTokens": 110,
      "estimatedCostUsd": 0.000032200000000000003,
      "costEstimated": true,
      "estimateComplete": true
    }
  }
}
== codex contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 1100,
    "estimatedCostUsd": 0.008,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gpt-5.5",
          "totalTokens": 1100,
          "costUsd": 0.008
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-2",
      "tokens": 1100,
      "estimatedCostUsd": 0.008,
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
    "tokens": 1430,
    "estimatedCostUsd": 0.0026650000000000003,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 880,
          "costUsd": 0.00256,
          "variants": [
            {
              "model": "gemini-3-pro",
              "totalTokens": 880,
              "costUsd": 0.00256
            }
          ]
        },
        {
          "model": "Other",
          "totalTokens": 550,
          "costUsd": 0.000105,
          "variants": [
            {
              "model": "gpt-oss-120b",
              "totalTokens": 550,
              "costUsd": 0.000105
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-3",
      "tokens": 1430,
      "estimatedCostUsd": 0.0026650000000000003,
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
    "estimatedCostUsd": 0.00054,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "moonshot/kimi-k2.5",
          "totalTokens": 660,
          "costUsd": 0.00054,
          "variants": [
            {
              "model": "kimi-k2p5",
              "totalTokens": 660,
              "costUsd": 0.00054
            }
          ]
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
      "estimatedCostUsd": 0.00054,
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
== grok contributed=true
{
  "today": null,
  "yesterday": null,
  "last30Days": {
    "tokens": 660,
    "estimatedCostUsd": 0.00015,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-4",
          "totalTokens": 660,
          "costUsd": 0.00015
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
      "estimatedCostUsd": 0.00015,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
"#;
    }
}
