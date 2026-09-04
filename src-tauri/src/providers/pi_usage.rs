use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    pricing::{ModelPricing, TokenBreakdown},
    storage::Storage,
};

use super::{
    daily_usage::DailyUsageAccumulator,
    log_usage::{load_or_parse_log, parse_log_timestamp, LogCacheError},
    model_scope::model_belongs_to_card,
};

const LOG_CACHE_SCHEMA_VERSION: u8 = 4;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum PiUsageSource {
    Pi,
    OhMyPi,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PiUsageSources {
    pub pi: bool,
    pub oh_my_pi: bool,
}

pub fn usage_source_note(
    base: &str,
    pi_sources: PiUsageSources,
    includes_opencode: bool,
    includes_zcode: bool,
) -> String {
    let mut sources = Vec::new();
    if pi_sources.pi {
        sources.push("pi");
    }
    if pi_sources.oh_my_pi {
        sources.push("oh-my-pi");
    }
    if includes_opencode {
        sources.push("OpenCode");
    }
    if includes_zcode {
        sources.push("ZCode");
    }
    let suffix = match sources.as_slice() {
        [] => String::new(),
        [source] => format!(" and {source}"),
        [leading @ .., last] => format!(", {}, and {last}", leading.join(", ")),
    };
    format!("From your {base}{suffix} (estimated)")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PiUsageEvent {
    id: Option<String>,
    timestamp: DateTime<Utc>,
    card_id: String,
    model: String,
    carried_cost: Option<f64>,
    tokens: PiTokenBreakdown,
    reported_total_tokens: u64,
    source: PiUsageSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct PiTokenBreakdown {
    input: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_read: u64,
    output: u64,
}

impl PiTokenBreakdown {
    fn pricing_tokens(self) -> TokenBreakdown {
        TokenBreakdown {
            input: self.input,
            cache_write_5m: self.cache_write_5m,
            cache_write_1h: self.cache_write_1h,
            cache_read: self.cache_read,
            output: self.output,
            is_fast: false,
        }
    }
}

/// Folds usage written by pi-compatible clients into the card of the underlying model provider.
/// Returns whether each client contributed usage or an unknown-model marker to that card.
pub fn scan_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<PiUsageSources, LogCacheError> {
    let home = home_directory();
    let directories = sessions_directories(
        env_text("PI_CODING_AGENT_SESSION_DIR").as_deref(),
        env_text("PI_CODING_AGENT_DIR").as_deref(),
        &home,
    );
    scan_directories_into(storage, &directories, now, pricing, card_id, accumulator)
}

fn scan_directories_into(
    storage: &Storage,
    directories: &[(PiUsageSource, PathBuf)],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<PiUsageSources, LogCacheError> {
    let paths = directories
        .iter()
        .flat_map(|(source, directory)| {
            discover_files(directory)
                .into_iter()
                .map(|path| (*source, path))
        })
        .collect::<Vec<_>>();
    let mut seen_paths = HashSet::with_capacity(paths.len());
    let mut events = Vec::new();
    for (source, path) in paths {
        seen_paths.insert(path.clone());
        let Some(parsed) =
            load_or_parse_log(storage, "pi", &path, LOG_CACHE_SCHEMA_VERSION, |content| {
                parse_jsonl(content, source)
            })?
        else {
            continue;
        };
        events.extend(parsed);
    }
    storage.prune_log_events("pi", &seen_paths)?;

    let since = now
        .with_timezone(&Local)
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap_or(NaiveDate::MIN);
    Ok(aggregate_into(
        deduplicate(events),
        card_id,
        since,
        now,
        pricing,
        accumulator,
    ))
}

fn sessions_directories(
    session_override: Option<&str>,
    config_override: Option<&str>,
    home: &Path,
) -> Vec<(PiUsageSource, PathBuf)> {
    let pi = if let Some(path) = session_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        expand_home(path, home)
    } else if let Some(path) = config_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        expand_home(path, home).join("sessions")
    } else {
        home.join(".pi").join("agent").join("sessions")
    };
    let oh_my_pi = home.join(".omp").join("agent").join("sessions");
    if pi == oh_my_pi {
        vec![(PiUsageSource::OhMyPi, oh_my_pi)]
    } else {
        vec![(PiUsageSource::Pi, pi), (PiUsageSource::OhMyPi, oh_my_pi)]
    }
}

fn discover_files(directory: &Path) -> Vec<PathBuf> {
    let directory = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    let mut paths = WalkDir::new(directory)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
}

fn parse_jsonl(content: &str, source: PiUsageSource) -> Vec<PiUsageEvent> {
    content
        .lines()
        .filter(|line| line.contains("\"usage\""))
        .filter_map(|line| parse_line(line, source))
        .collect()
}

fn parse_line(line: &str, source: PiUsageSource) -> Option<PiUsageEvent> {
    let object = serde_json::from_str::<Value>(line).ok()?;
    if object.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let timestamp = parse_log_timestamp(object.get("timestamp")?.as_str()?)?;
    let message = object.get("message")?.as_object()?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let provider = message.get("provider")?.as_str()?;
    let card_id = mapped_card(provider, &model, source)?;
    let usage = message.get("usage")?.as_object()?;
    let cache_write = unsigned_number(usage.get("cacheWrite")).unwrap_or_default();
    let cache_write_1h = unsigned_number(usage.get("cacheWrite1h")).unwrap_or_default();
    let carried_cost = usage
        .get("cost")
        .and_then(Value::as_object)
        .and_then(|cost| finite_number(cost.get("total")));

    Some(PiUsageEvent {
        id: object.get("id").and_then(Value::as_str).map(str::to_owned),
        timestamp,
        card_id: card_id.to_owned(),
        model,
        carried_cost,
        tokens: PiTokenBreakdown {
            input: unsigned_number(usage.get("input")).unwrap_or_default(),
            cache_write_5m: cache_write.saturating_sub(cache_write_1h),
            cache_write_1h,
            cache_read: unsigned_number(usage.get("cacheRead")).unwrap_or_default(),
            output: unsigned_number(usage.get("output")).unwrap_or_default(),
        },
        reported_total_tokens: unsigned_number(usage.get("totalTokens")).unwrap_or_default(),
        source,
    })
}

fn mapped_card(provider: &str, model: &str, source: PiUsageSource) -> Option<&'static str> {
    if matches!(provider, "anthropic" | "claude-agent-sdk") || is_claude_model(model) {
        return Some("claude");
    }
    if source == PiUsageSource::OhMyPi {
        return Some("codex");
    }
    match provider {
        "openai" | "openai-codex" => Some("codex"),
        "cursor" => Some("cursor"),
        "zai" | "zhipu" => Some("zai"),
        "google-antigravity" => Some("antigravity"),
        "github-copilot" => Some("copilot"),
        _ if model_belongs_to_card("codex", model) => Some("codex"),
        _ => None,
    }
}

fn is_claude_model(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model.starts_with("claude") || model.contains("/claude")
}

fn deduplicate(events: Vec<PiUsageEvent>) -> Vec<PiUsageEvent> {
    let mut seen = HashSet::new();
    events
        .into_iter()
        .filter(|event| event.id.as_ref().is_none_or(|id| seen.insert(id.clone())))
        .collect()
}

fn aggregate_into(
    events: Vec<PiUsageEvent>,
    card_id: &str,
    since: NaiveDate,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> PiUsageSources {
    let mut contributed = PiUsageSources::default();
    for event in events {
        if event.card_id != card_id || event.timestamp > now {
            continue;
        }
        let date = event.timestamp.with_timezone(&Local).date_naive();
        if date < since {
            continue;
        }
        let model = event.model.trim();
        let display_model = if model.is_empty() {
            "Unattributed"
        } else {
            model
        };
        if !model_belongs_to_card(card_id, model) {
            let carried_cost = event.carried_cost.filter(|cost| *cost > 0.0);
            let estimated_cost =
                pricing.estimated_cost_dollars(model, event.tokens.pricing_tokens(), true);
            accumulator.add_other(
                date,
                event.reported_total_tokens,
                carried_cost.or(estimated_cost),
                carried_cost.is_none(),
            );
            mark_source(&mut contributed, event.source);
            continue;
        }
        if let Some(cost) = event.carried_cost.filter(|cost| *cost > 0.0) {
            accumulator.add_exact(date, event.reported_total_tokens, cost, display_model);
            mark_source(&mut contributed, event.source);
        } else if !model.is_empty() {
            if let Some(cost) =
                pricing.estimated_cost_dollars(model, event.tokens.pricing_tokens(), true)
            {
                accumulator.add(date, event.reported_total_tokens, cost, model);
                mark_source(&mut contributed, event.source);
            } else if event.reported_total_tokens > 0 {
                accumulator.add_unknown_model(date, model);
                mark_source(&mut contributed, event.source);
            }
        }
    }
    contributed
}

fn mark_source(sources: &mut PiUsageSources, source: PiUsageSource) {
    match source {
        PiUsageSource::Pi => sources.pi = true,
        PiUsageSource::OhMyPi => sources.oh_my_pi = true,
    }
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Number(value) => value.as_f64()?,
        Value::String(value) => value.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

fn unsigned_number(value: Option<&Value>) -> Option<u64> {
    let number = finite_number(value)?;
    if number < 0.0 || number > u64::MAX as f64 {
        return None;
    }
    Some(number.trunc() as u64)
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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
    use std::{collections::HashMap, fs, path::Path};

    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    use super::{
        aggregate_into, deduplicate, mapped_card, parse_line, scan_directories_into,
        sessions_directories, usage_source_note, PiTokenBreakdown, PiUsageEvent, PiUsageSource,
        PiUsageSources,
    };
    use crate::{
        pricing::{ModelPricing, ModelRates, PricingCatalog, PricingSupplement},
        providers::daily_usage::DailyUsageAccumulator,
        storage::Storage,
    };

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 12, 12, 0, 0).unwrap()
    }

    fn line(id: &str, provider: &str, model: &str, cost: &str) -> String {
        let cost = serde_json::from_str::<serde_json::Value>(cost).unwrap();
        serde_json::json!({
            "type": "message",
            "id": id,
            "timestamp": "2026-07-12T10:00:00.000Z",
            "message": {
                "role": "assistant",
                "provider": provider,
                "model": model,
                "usage": {
                    "input": 100,
                    "output": 50,
                    "cacheRead": 10,
                    "cacheWrite": 30,
                    "cacheWrite1h": 12,
                    "totalTokens": 202,
                    "cost": {"total": cost}
                }
            }
        })
        .to_string()
    }

    fn parsed_line(line: &str) -> Option<PiUsageEvent> {
        parse_line(line, PiUsageSource::Pi)
    }

    fn pricing() -> ModelPricing {
        ModelPricing::new(
            PricingSupplement::default(),
            PricingCatalog {
                entries: HashMap::from([("priced-model".into(), ModelRates::new(10.0, 20.0))]),
                ..PricingCatalog::default()
            },
            PricingCatalog::default(),
        )
    }

    #[test]
    fn path_resolution_uses_session_then_config_then_default() {
        let home = Path::new("/home/tester");
        assert_eq!(
            sessions_directories(Some("~/pi-sessions"), Some("~/ignored"), home),
            [
                (PiUsageSource::Pi, home.join("pi-sessions")),
                (
                    PiUsageSource::OhMyPi,
                    home.join(".omp").join("agent").join("sessions")
                ),
            ]
        );
        assert_eq!(
            sessions_directories(None, Some("~/pi-config"), home)[0].1,
            home.join("pi-config").join("sessions")
        );
        assert_eq!(
            sessions_directories(Some("  "), Some("  "), home)[0].1,
            home.join(".pi").join("agent").join("sessions")
        );
    }

    #[test]
    fn parser_maps_cards_splits_cache_and_accepts_numeric_strings() {
        let event = parsed_line(&line("one", "anthropic", "claude-model", "\"0.5\"")).unwrap();
        assert_eq!(event.card_id, "claude");
        assert_eq!(event.carried_cost, Some(0.5));
        assert_eq!(event.tokens.cache_write_5m, 18);
        assert_eq!(event.tokens.cache_write_1h, 12);
        assert_eq!(event.reported_total_tokens, 202);
        assert_eq!(
            mapped_card("openai-codex", "gpt-5.5", PiUsageSource::Pi),
            Some("codex")
        );
        assert_eq!(
            mapped_card("custom", "gpt-5.6-sol", PiUsageSource::Pi),
            Some("codex")
        );
        assert_eq!(
            mapped_card("custom", "claude-sonnet-5", PiUsageSource::OhMyPi),
            Some("claude")
        );
        assert_eq!(
            mapped_card("deepseek", "deepseek-v4", PiUsageSource::OhMyPi),
            Some("codex")
        );
        assert_eq!(
            mapped_card("nvidia-nim", "deepseek-v4", PiUsageSource::Pi),
            None
        );
    }

    #[test]
    fn parser_rejects_unmapped_and_non_assistant_messages() {
        assert!(parsed_line(&line("one", "nvidia-nim", "model", "1")).is_none());
        assert!(parsed_line(&line("mlx", "openai-codex", "qwen3.8:27b - mlx", "0")).is_some());
        let user = r#"{"type":"message","timestamp":"2026-07-12T10:00:00Z","message":{"role":"user","provider":"anthropic","usage":{}}}"#;
        assert!(parsed_line(user).is_none());
    }

    #[test]
    fn cached_non_codex_events_are_filtered_during_aggregation() {
        let event = PiUsageEvent {
            id: Some("mlx".into()),
            timestamp: now(),
            card_id: "codex".into(),
            model: "qwen3.8:27b - mlx".into(),
            carried_cost: Some(0.0),
            tokens: PiTokenBreakdown {
                input: 100,
                cache_write_5m: 0,
                cache_write_1h: 0,
                cache_read: 0,
                output: 50,
            },
            reported_total_tokens: 150,
            source: PiUsageSource::Pi,
        };
        let mut accumulator = DailyUsageAccumulator::default();

        assert!(
            aggregate_into(
                vec![event],
                "codex",
                chrono::NaiveDate::MIN,
                now(),
                &pricing(),
                &mut accumulator,
            )
            .pi
        );
        let history = accumulator.build(now(), "From pi");
        assert!(history.today.is_none());
        let other = history.other_usage.unwrap().today.unwrap();
        assert_eq!(other.tokens, 150);
        assert_eq!(other.priced_tokens, 0);
    }

    #[test]
    fn carried_cost_is_exact_zero_cost_is_priced_and_unknowns_are_reported() {
        let events = vec![
            parsed_line(&line("exact", "anthropic", "claude-unknown-exact", "0.5")).unwrap(),
            parsed_line(&line("priced", "anthropic", "claude-priced-model", "0")).unwrap(),
            parsed_line(&line("unknown", "anthropic", "claude-missing-model", "0")).unwrap(),
        ];
        let mut accumulator = DailyUsageAccumulator::default();
        assert!(
            aggregate_into(
                events,
                "claude",
                chrono::NaiveDate::MIN,
                now(),
                &pricing(),
                &mut accumulator,
            )
            .pi
        );
        let history = accumulator.build(now(), "From pi");
        let period = history.today.unwrap();
        assert_eq!(period.tokens, 404);
        assert!(period.cost_estimated);
        assert!((period.estimated_cost_usd.unwrap() - 0.502_43).abs() < 0.000_001);
        assert_eq!(period.unknown_models, ["claude-missing-model"]);
    }

    #[test]
    fn future_dated_events_do_not_contribute_usage() {
        let mut event =
            parsed_line(&line("future", "anthropic", "claude-priced-model", "0.5")).unwrap();
        event.timestamp = now() + chrono::Duration::seconds(1);
        let mut accumulator = DailyUsageAccumulator::default();

        assert_eq!(
            aggregate_into(
                vec![event],
                "claude",
                chrono::NaiveDate::MIN,
                now(),
                &pricing(),
                &mut accumulator,
            ),
            PiUsageSources::default()
        );
        assert!(accumulator.build(now(), "From pi").today.is_none());
    }

    #[test]
    fn repeated_message_ids_are_counted_once_across_files() {
        let event = parsed_line(&line("duplicate", "anthropic", "claude-model", "0.5")).unwrap();
        assert_eq!(deduplicate(vec![event.clone(), event]).len(), 1);
    }

    #[test]
    fn recursive_scan_uses_cache_and_folds_only_the_requested_card() {
        let directory = tempdir().unwrap();
        let sessions = directory.path().join("sessions");
        let log = sessions.join("project").join("session.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(
            &log,
            [
                line("claude", "anthropic", "claude-model", "0.5"),
                line("codex", "openai-codex", "gpt-5.5", "0.25"),
            ]
            .join("\n"),
        )
        .unwrap();
        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();

        assert!(
            scan_directories_into(
                &storage,
                &[(PiUsageSource::OhMyPi, sessions.clone())],
                now(),
                &pricing(),
                "claude",
                &mut accumulator,
            )
            .unwrap()
            .oh_my_pi
        );
        let history = accumulator.build(now(), "From pi");
        assert_eq!(history.today.unwrap().tokens, 202);

        let mut second = DailyUsageAccumulator::default();
        assert!(
            scan_directories_into(
                &storage,
                &[(PiUsageSource::OhMyPi, sessions)],
                now(),
                &pricing(),
                "codex",
                &mut second,
            )
            .unwrap()
            .oh_my_pi
        );
        assert_eq!(second.build(now(), "From pi").today.unwrap().tokens, 202);
    }

    #[test]
    fn oh_my_pi_custom_upstreams_are_scanned_and_attributed() {
        let directory = tempdir().unwrap();
        let pi_sessions = directory.path().join("pi-sessions");
        let oh_my_pi_sessions = directory.path().join("omp-sessions");
        fs::create_dir_all(&pi_sessions).unwrap();
        fs::create_dir_all(&oh_my_pi_sessions).unwrap();
        fs::write(
            pi_sessions.join("pi.jsonl"),
            line("pi", "openai", "gpt-5.5", "0.25"),
        )
        .unwrap();
        fs::write(
            oh_my_pi_sessions.join("omp.jsonl"),
            [
                line("omp", "private-upstream", "gpt-5.6-sol", "0.75"),
                line("deepseek", "deepseek", "deepseek-v4", "0.5"),
            ]
            .join("\n"),
        )
        .unwrap();
        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();

        let sources = scan_directories_into(
            &storage,
            &[
                (PiUsageSource::Pi, pi_sessions),
                (PiUsageSource::OhMyPi, oh_my_pi_sessions),
            ],
            now(),
            &pricing(),
            "codex",
            &mut accumulator,
        )
        .unwrap();

        assert_eq!(
            sources,
            PiUsageSources {
                pi: true,
                oh_my_pi: true,
            }
        );
        let history = accumulator.build(now(), "From clients");
        let today = history.today.unwrap();
        assert_eq!(today.tokens, 404);
        assert_eq!(today.estimated_cost_usd, Some(1.0));
        let other = history.other_usage.unwrap().today.unwrap();
        assert_eq!(other.tokens, 202);
        assert_eq!(other.priced_tokens, 202);
        assert_eq!(other.estimated_cost_usd, Some(0.5));
    }

    #[test]
    fn source_note_names_only_contributing_local_clients() {
        assert_eq!(
            usage_source_note(
                "Codex logs",
                PiUsageSources {
                    pi: true,
                    oh_my_pi: true,
                },
                true,
                false,
            ),
            "From your Codex logs, pi, oh-my-pi, and OpenCode (estimated)"
        );
        assert_eq!(
            usage_source_note("Codex logs", PiUsageSources::default(), false, false),
            "From your Codex logs (estimated)"
        );
    }
}
