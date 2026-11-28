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
    pricing::{HistoricalRates, ModelPricing, ServiceTier, TokenBreakdown},
    providers::local_usage::{
        card_usage::{self, DailyUsageAccumulator, UsageObservation},
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
    /// The provider for `card_id`, when ZCode can serve that Card.
    pub(crate) fn from_card_id(card_id: &str) -> Option<Self> {
        Some(match card_id {
            "antigravity" => Self::Antigravity,
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "grok" => Self::Grok,
            "kimi" => Self::Kimi,
            "minimax" => Self::MiniMax,
            "zai" => Self::Zai,
            _ => return None,
        })
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
        let target_card = target.as_str();
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
                .then(|| resolve_rate_model(&event, pricing, target_card))
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

/// The variant's own model name when `provider` prices it, as `model-variant`.
fn resolve_rate_model(
    event: &UsageEvent,
    pricing: &ModelPricing,
    provider: &str,
) -> Option<String> {
    let variant_model = event
        .variant
        .as_deref()
        .map(|variant| format!("{}-{variant}", event.model));
    let has_variant_rate = variant_model.as_deref().is_some_and(|model| {
        pricing.historical_rates(provider, model, event.timestamp, ServiceTier::Standard, 0)
            != HistoricalRates::Untracked
            || pricing.resolve(model).is_some()
    });
    variant_model.filter(|_| has_variant_rate)
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
mod tests;
