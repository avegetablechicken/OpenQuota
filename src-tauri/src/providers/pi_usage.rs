use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::{
    pricing::{ModelPricing, TokenBreakdown},
    storage::Storage,
};

use super::{
    card_usage::DailyUsageAccumulator,
    card_usage::{self, UsageObservation},
    log_usage::{load_or_parse_log, parse_log_timestamp, LoadedEventsMemo, LogCacheError},
    model_scope::{card_for_model, normalize_provider_model},
};

const LOG_CACHE_SCHEMA_VERSION: u8 = 10;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PiUsageEvent {
    id: Option<String>,
    timestamp: DateTime<Utc>,
    card_id: String,
    provider: String,
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
            ..TokenBreakdown::default()
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
    let files = paths
        .iter()
        .map(|(_, path)| path.clone())
        .collect::<Vec<_>>();
    // Events carry the client their file belongs to, which can change while the file does not.
    let sources = paths
        .iter()
        .map(|(source, _)| match source {
            PiUsageSource::Pi => 'p',
            PiUsageSource::OhMyPi => 'o',
        })
        .collect::<String>();
    let events = LOADED_EVENTS.load_in_context(storage, sources, files, NaiveDate::MIN, || {
        load_session_files(storage, paths)
    })?;

    Ok(aggregate_into(&events, card_id, now, pricing, accumulator))
}

static LOADED_EVENTS: LoadedEventsMemo<PiUsageEvent> = LoadedEventsMemo::new();

fn load_session_files(
    storage: &Storage,
    paths: Vec<(PiUsageSource, PathBuf)>,
) -> Result<Vec<PiUsageEvent>, LogCacheError> {
    let mut seen_paths = HashSet::with_capacity(paths.len());
    let mut events = Vec::new();
    for (source, path) in paths {
        if !seen_paths.insert(path.clone()) {
            continue;
        }
        let Some(parsed) =
            load_or_parse_log(storage, "pi", &path, LOG_CACHE_SCHEMA_VERSION, |content| {
                parse_jsonl(content, source)
            })?
        else {
            continue;
        };
        // The stored events keep the client the file belonged to when it was parsed.
        events.extend(
            parsed
                .into_iter()
                .map(|event| PiUsageEvent { source, ..event }),
        );
    }
    storage.prune_log_events("pi", &seen_paths)?;
    Ok(deduplicate(events))
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
    let pi_canonical = fs::canonicalize(&pi).unwrap_or_else(|_| pi.clone());
    let oh_my_pi_canonical = fs::canonicalize(&oh_my_pi).unwrap_or_else(|_| oh_my_pi.clone());
    if pi_canonical == oh_my_pi_canonical {
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
    let provider = message.get("provider")?.as_str()?;
    let raw_model = message
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let model = normalize_provider_model(provider, raw_model);
    let card_id = mapped_card(provider, &model)?;
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
        provider: provider.to_owned(),
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

fn mapped_card(provider: &str, model: &str) -> Option<&'static str> {
    let normalized_model = normalize_provider_model(provider, model);
    if let Some(card) = card_for_model(&normalized_model) {
        return Some(card);
    }
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude-agent-sdk" => Some("claude"),
        "openai" | "openai-codex" => Some("codex"),
        "xai" | "x-ai" | "xai-oauth" | "grok" => Some("grok"),
        "kimi" | "kimi-code" | "kimi-coding" | "moonshot" | "moonshot-cn" | "moonshotai" => {
            Some("kimi")
        }
        "minimax" => Some("minimax"),
        "zai" | "z-ai" | "z_ai" | "zhipu" | "zhipuai" | "bigmodel" => Some("zai"),
        "google-antigravity" | "antigravity" | "antigravity-cli" => Some("antigravity"),
        _ => None,
    }
}

fn deduplicate(events: Vec<PiUsageEvent>) -> Vec<PiUsageEvent> {
    let mut seen = HashSet::new();
    events
        .into_iter()
        .filter(|event| event.id.as_ref().is_none_or(|id| seen.insert(id.clone())))
        .collect()
}

/// Each event's Card was chosen at parse, from its model and then its provider, so only events
/// already on `card_id` are recorded, as that Card's own usage. A source counts when any of its
/// usage lands on the Card, Others included.
fn aggregate_into(
    events: &[PiUsageEvent],
    card_id: &str,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> PiUsageSources {
    let mut contributed = PiUsageSources::default();
    for event in events.iter().filter(|event| event.card_id == card_id) {
        let landed = card_usage::record(
            [observation(event)],
            card_id,
            card_id,
            now,
            pricing,
            accumulator,
        );
        if landed.own_series || landed.others {
            mark_source(&mut contributed, event.source);
        }
    }
    contributed
}

/// pi counts its reported total, and a carried cost of zero means none was recorded. The provider
/// tells whether a Local runtime served the usage.
fn observation(event: &PiUsageEvent) -> UsageObservation<'_> {
    UsageObservation {
        total_tokens: event.reported_total_tokens,
        provider_hint: Some(&event.provider),
        ..UsageObservation::new(
            event.timestamp,
            Some(&event.model),
            event.tokens.pricing_tokens(),
            event.carried_cost.filter(|cost| *cost > 0.0),
        )
    }
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
        sessions_directories, PiTokenBreakdown, PiUsageEvent, PiUsageSource, PiUsageSources,
    };
    use crate::{
        pricing::{ModelPricing, ModelRates, PricingCatalog, PricingSupplement},
        providers::card_usage::DailyUsageAccumulator,
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
        assert_eq!(mapped_card("openai-codex", "gpt-5.5"), Some("codex"));
        assert_eq!(mapped_card("custom", "gpt-5.6-sol"), Some("codex"));
        assert_eq!(mapped_card("custom", "claude-sonnet-5"), Some("claude"));
        assert_eq!(
            mapped_card("google-antigravity", "claude-sonnet-5"),
            Some("claude")
        );
        assert_eq!(
            mapped_card("google-antigravity", "gemini-3-pro"),
            Some("antigravity")
        );
        assert_eq!(
            mapped_card("antigravity-cli", "gemini-3-pro"),
            Some("antigravity")
        );
        assert_eq!(
            mapped_card("antigravity-cli", "claude-sonnet-5"),
            Some("claude")
        );
        assert_eq!(mapped_card("deepseek", "deepseek-v4"), None);
        assert_eq!(mapped_card("cursor", "cursor-small"), None);
        assert_eq!(mapped_card("zai", "glm-5"), Some("zai"));
        assert_eq!(mapped_card("zhipu", "glm-5"), Some("zai"));
        assert_eq!(mapped_card("z_ai", "future-model"), Some("zai"));
        assert_eq!(mapped_card("anthropic", "glm-5"), Some("zai"));
        assert_eq!(mapped_card("custom", "kimi-k3"), Some("kimi"));
        assert_eq!(mapped_card("kimi-code", "future-model"), Some("kimi"));
        assert_eq!(mapped_card("kimi-coding", "future-model"), Some("kimi"));
        assert_eq!(mapped_card("anthropic", "MiniMax-M2.7"), Some("minimax"));
        assert_eq!(mapped_card("custom", "grok-4"), Some("grok"));
        assert_eq!(mapped_card("xai-oauth", "future-model"), Some("grok"));
        assert_eq!(
            mapped_card("github-copilot", "gemini-3-pro"),
            Some("antigravity")
        );
        assert_eq!(mapped_card("google", "gemini-3-pro"), Some("antigravity"));
        assert_eq!(mapped_card("nvidia-nim", "deepseek-v4"), None);
    }

    #[test]
    fn missing_model_records_as_unattributed_on_main_card_for_all_providers() {
        for (provider, card) in [("anthropic", "claude"), ("openai", "codex")] {
            let event = parsed_line(&line("missing", provider, "", "0")).unwrap();
            assert_eq!(event.card_id, card);
            let mut accumulator = DailyUsageAccumulator::default();
            assert!(aggregate_into(&[event], card, now(), &pricing(), &mut accumulator,).pi);
            let history = accumulator.build(now(), "From pi");
            let today = history.today.unwrap();
            assert_eq!(today.tokens, 202);
            assert_eq!(today.unknown_models, ["Unattributed"]);
            assert!(history.other_usage.is_none());
        }
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
            provider: "openai".into(),
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

        assert!(aggregate_into(&[event], "codex", now(), &pricing(), &mut accumulator,).pi);
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
        assert!(aggregate_into(&events, "claude", now(), &pricing(), &mut accumulator,).pi);
        let history = accumulator.build(now(), "From pi");
        let period = history.today.unwrap();
        assert_eq!(period.tokens, 606);
        assert!(period.cost_estimated);
        assert!(!period.estimate_complete);
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
            aggregate_into(&[event], "claude", now(), &pricing(), &mut accumulator,),
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
    fn cached_sessions_follow_the_client_their_directory_now_belongs_to() {
        let directory = tempdir().unwrap();
        let sessions = directory.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("session.jsonl"),
            line("pi", "openai", "gpt-5.5", "0.25"),
        )
        .unwrap();
        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let scan = |source| {
            scan_directories_into(
                &storage,
                &[(source, sessions.clone())],
                now(),
                &pricing(),
                "codex",
                &mut DailyUsageAccumulator::default(),
            )
            .unwrap()
        };

        assert_eq!(
            scan(PiUsageSource::Pi),
            PiUsageSources {
                pi: true,
                oh_my_pi: false,
            }
        );
        // The file is unchanged, so both the loaded events and the stored parse are reused.
        assert_eq!(
            scan(PiUsageSource::OhMyPi),
            PiUsageSources {
                pi: false,
                oh_my_pi: true,
            }
        );
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
        assert!(history.other_usage.is_none());
    }

    #[test]
    fn pi_and_oh_my_pi_route_all_model_families_including_custom_relays() {
        let directory = tempdir().unwrap();
        let pi_sessions = directory.path().join("pi-sessions");
        let oh_my_pi_sessions = directory.path().join("omp-sessions");
        fs::create_dir_all(&pi_sessions).unwrap();
        fs::create_dir_all(&oh_my_pi_sessions).unwrap();
        fs::write(
            pi_sessions.join("pi.jsonl"),
            [
                line("pi-zai", "anthropic", "glm-5.3-flash", "0.2"),
                line("pi-kimi", "my-relay", "kimi-k3", "0.3"),
                line("pi-grok", "xai", "grok-4", "0.4"),
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            oh_my_pi_sessions.join("omp.jsonl"),
            [
                line("omp-minimax", "custom-cn", "MiniMax-M2.7", "0.5"),
                line(
                    "omp-antigravity",
                    "google-antigravity",
                    "gemini-3-pro",
                    "0.6",
                ),
                line(
                    "omp-ag-claude",
                    "google-antigravity",
                    "claude-sonnet-5",
                    "0.7",
                ),
            ]
            .join("\n"),
        )
        .unwrap();
        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let dirs = [
            (PiUsageSource::Pi, pi_sessions),
            (PiUsageSource::OhMyPi, oh_my_pi_sessions),
        ];

        for (card, expect_pi, expect_omp, expected_cost) in [
            ("zai", true, false, 0.2),
            ("kimi", true, false, 0.3),
            ("grok", true, false, 0.4),
            ("minimax", false, true, 0.5),
            ("antigravity", false, true, 0.6),
            ("claude", false, true, 0.7),
        ] {
            let mut accumulator = DailyUsageAccumulator::default();
            let sources =
                scan_directories_into(&storage, &dirs, now(), &pricing(), card, &mut accumulator)
                    .unwrap();
            assert_eq!(
                sources,
                PiUsageSources {
                    pi: expect_pi,
                    oh_my_pi: expect_omp,
                },
                "{card}"
            );
            let today = accumulator.build(now(), "test").today.unwrap();
            assert_eq!(today.tokens, 202, "{card}");
            assert!(
                (today.estimated_cost_usd.unwrap() - expected_cost).abs() < 1e-9,
                "{card}"
            );
        }
    }

    #[test]
    fn overlapping_directories_never_double_count_idless_events() {
        let directory = tempdir().unwrap();
        let outer = directory.path().join("sessions");
        let nested_omp = outer.join("omp");
        fs::create_dir_all(&nested_omp).unwrap();
        let idless = serde_json::json!({
            "type": "message",
            "timestamp": "2026-07-12T10:00:00.000Z",
            "message": {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input": 100,
                    "output": 50,
                    "cacheRead": 0,
                    "cacheWrite": 0,
                    "totalTokens": 150,
                    "cost": {"total": 0.4}
                }
            }
        })
        .to_string();
        fs::write(nested_omp.join("session.jsonl"), idless).unwrap();

        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let mut accumulator = DailyUsageAccumulator::default();
        let sources = scan_directories_into(
            &storage,
            &[
                (PiUsageSource::Pi, outer),
                (PiUsageSource::OhMyPi, nested_omp),
            ],
            now(),
            &pricing(),
            "claude",
            &mut accumulator,
        )
        .unwrap();

        assert_eq!(
            sources,
            PiUsageSources {
                pi: true,
                oh_my_pi: false,
            }
        );
        let today = accumulator.build(now(), "test").today.unwrap();
        assert_eq!(today.tokens, 150);
        assert!((today.estimated_cost_usd.unwrap() - 0.4).abs() < 1e-9);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_pi_and_omp_directories_collapse_to_single_source() {
        let directory = tempdir().unwrap();
        let home = directory.path();
        let omp_sessions = home.join(".omp").join("agent").join("sessions");
        let pi_agent = home.join(".pi").join("agent");
        fs::create_dir_all(&omp_sessions).unwrap();
        fs::create_dir_all(&pi_agent).unwrap();
        std::os::unix::fs::symlink(&omp_sessions, pi_agent.join("sessions")).unwrap();

        let dirs = sessions_directories(None, None, home);
        assert_eq!(dirs, [(PiUsageSource::OhMyPi, omp_sessions)]);
    }

    #[test]
    fn ollama_sessions_stay_unpriced_in_others_and_kimi_short_aliases_route_to_kimi() {
        let directory = tempdir().unwrap();
        let pi_sessions = directory.path().join("pi");
        fs::create_dir_all(&pi_sessions).unwrap();
        let at = now();
        let ts = at.to_rfc3339();
        let lines = [
            serde_json::json!({
                "type": "message",
                "id": "ollama-1",
                "timestamp": ts,
                "message": {
                    "role": "assistant",
                    "provider": "ollama",
                    "model": "glm-4.7-flash",
                    "usage": { "input": 1000, "output": 500, "totalTokens": 1500, "cost": { "total": 0.0 } }
                }
            }),
            serde_json::json!({
                "type": "message",
                "id": "kimi-short-1",
                "timestamp": ts,
                "message": {
                    "role": "assistant",
                    "provider": "moonshot-cn",
                    "model": "k2p5",
                    "usage": { "input": 1000, "output": 500, "totalTokens": 1500, "cost": { "total": 0.0 } }
                }
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(pi_sessions.join("session.jsonl"), lines).unwrap();

        let bundled = crate::pricing::test_bundled_pricing();
        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let mut zai_acc = DailyUsageAccumulator::default();
        let zai_sources = scan_directories_into(
            &storage,
            &[(PiUsageSource::Pi, pi_sessions.clone())],
            at,
            &bundled,
            "zai",
            &mut zai_acc,
        )
        .unwrap();
        assert!(zai_sources.pi);
        let zai_history = zai_acc.build(at, "test");
        assert!(zai_history.today.is_none());
        let zai_others = zai_history.other_usage.unwrap().today.unwrap();
        assert_eq!(zai_others.tokens, 1500);
        assert_eq!(zai_others.estimated_cost_usd, None);

        let mut kimi_acc = DailyUsageAccumulator::default();
        let kimi_sources = scan_directories_into(
            &storage,
            &[(PiUsageSource::Pi, pi_sessions)],
            at,
            &bundled,
            "kimi",
            &mut kimi_acc,
        )
        .unwrap();
        assert!(kimi_sources.pi);
        let kimi_history = kimi_acc.build(at, "test");
        let kimi_today = kimi_history.today.unwrap();
        assert_eq!(kimi_today.tokens, 1500);
        assert!(kimi_today.estimated_cost_usd.is_some());
        assert!(kimi_history.other_usage.is_none());
    }

    /// Pins the output of pi's per-event policy, so moving it into `card_usage` cannot change
    /// what any Card shows.
    mod characterization {
        use chrono::{DateTime, Duration, TimeZone, Utc};

        use super::super::{
            aggregate_into, mapped_card, normalize_provider_model, PiTokenBreakdown, PiUsageEvent,
            PiUsageSource,
        };
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

        /// Builds an event as the parser would, or `None` when the parser drops it.
        fn event(
            days_ago: i64,
            source: PiUsageSource,
            provider: &str,
            model: &str,
            input: u64,
            output: u64,
            carried_cost: Option<f64>,
        ) -> Option<PiUsageEvent> {
            let model = normalize_provider_model(provider, model.trim());
            let card_id = mapped_card(provider, &model)?;
            Some(PiUsageEvent {
                id: None,
                timestamp: now() - Duration::days(days_ago),
                card_id: card_id.to_owned(),
                provider: provider.to_owned(),
                model,
                carried_cost,
                tokens: PiTokenBreakdown {
                    input,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                    cache_read: 0,
                    output,
                },
                reported_total_tokens: input + output,
                source,
            })
        }

        fn events() -> Vec<PiUsageEvent> {
            use PiUsageSource::{OhMyPi, Pi};
            let mut cached = event(0, Pi, "anthropic", "claude-sonnet-4-5", 10_000, 500, None);
            if let Some(cached) = cached.as_mut() {
                cached.tokens.cache_write_5m = 2_000;
                cached.tokens.cache_write_1h = 500;
                cached.tokens.cache_read = 6_000;
                cached.reported_total_tokens = 20_000;
            }
            let mut future = event(0, Pi, "anthropic", "claude-sonnet-4-5", 9_999, 9_999, None);
            if let Some(future) = future.as_mut() {
                future.timestamp = now() + Duration::hours(1);
            }
            [
                // Cards chosen by model, then by provider; a reported total that differs.
                cached,
                event(0, Pi, "anthropic", "claude-opus-4-6", 2_000, 200, Some(0.3)),
                event(0, OhMyPi, "openai-codex", "gpt-5.5", 2_000, 200, None),
                event(0, OhMyPi, "openai", "gpt-5.5", 2_000, 200, Some(0.0)),
                event(1, Pi, "custom", "gpt-5.6-sol", 1_000, 100, None),
                event(
                    1,
                    Pi,
                    "google-antigravity",
                    "gemini-3-pro",
                    1_000,
                    100,
                    None,
                ),
                event(
                    1,
                    Pi,
                    "google-antigravity",
                    "claude-sonnet-4-5",
                    1_000,
                    100,
                    None,
                ),
                event(1, Pi, "kimi-coding", "k2p5", 1_000, 100, None),
                event(1, Pi, "xai", "grok-4", 1_000, 100, None),
                event(1, OhMyPi, "zai", "glm-4.6", 600, 60, None),
                event(1, OhMyPi, "minimax", "minimax-m2", 600, 60, None),
                // A provider-mapped model without a price, and missing models.
                event(2, Pi, "anthropic", "claude-unreleased-zz-9", 700, 70, None),
                event(2, Pi, "anthropic", "  ", 400, 40, Some(0.05)),
                event(2, OhMyPi, "openai", "", 300, 30, None),
                // Others under the provider's Card: estimated, carried, unpriced, gpt-oss.
                event(4, Pi, "openai", "deepseek-chat", 900, 90, None),
                event(4, Pi, "anthropic", "deepseek-chat", 100, 10, Some(0.01)),
                event(4, OhMyPi, "openai", "totally-unknown-model", 50, 5, None),
                event(4, Pi, "openai", "gpt-oss-120b", 500, 50, None),
                event(4, Pi, "google-antigravity", "gpt-oss-120b", 500, 50, None),
                event(4, Pi, "openai", "ollama/llama3", 50, 5, None),
                // Local runtime providers: unpriced Others on the model's Card, if one owns it.
                event(4, Pi, "ollama", "llama3", 300, 30, None),
                event(4, Pi, "ollama", "claude-sonnet-4-5", 300, 30, None),
                // Unmapped providers are dropped at parse too.
                event(4, Pi, "mystery", "deepseek-chat", 300, 30, None),
                // Zero tokens: dropped unless something was paid.
                event(5, Pi, "anthropic", "claude-sonnet-4-5", 0, 0, None),
                event(5, Pi, "anthropic", "deepseek-chat", 0, 0, None),
                event(5, Pi, "anthropic", "", 0, 0, None),
                event(5, Pi, "anthropic", "claude-sonnet-4-5", 0, 0, Some(0.2)),
                // Window edges: today-29 counts, today-30 does not.
                event(29, Pi, "anthropic", "claude-sonnet-4-5", 100, 10, None),
                event(30, Pi, "anthropic", "claude-sonnet-4-5", 100, 10, None),
                event(30, OhMyPi, "anthropic", "deepseek-chat", 100, 10, None),
                event(31, Pi, "anthropic", "claude-sonnet-4-5", 100, 10, None),
                future,
            ]
            .into_iter()
            .flatten()
            .collect()
        }

        #[test]
        fn output_is_pinned_per_card() {
            let actual = [
                "claude",
                "codex",
                "antigravity",
                "grok",
                "kimi",
                "zai",
                "minimax",
            ]
            .map(|card_id| {
                let mut accumulator = DailyUsageAccumulator::default();
                let sources = aggregate_into(
                    &events(),
                    card_id,
                    now(),
                    &test_bundled_pricing(),
                    &mut accumulator,
                );
                let json = relative_json(&accumulator.build(now(), "test"), now());
                format!(
                    "== {card_id} pi={} oh_my_pi={}\n{json}",
                    sources.pi, sources.oh_my_pi
                )
            })
            .join("\n");
            assert_pinned("per card", &actual, PER_CARD);
        }

        #[test]
        fn sources_count_others_and_unattributed_usage() {
            let cases = [
                event(
                    0,
                    PiUsageSource::Pi,
                    "openai",
                    "deepseek-chat",
                    100,
                    10,
                    None,
                ),
                event(0, PiUsageSource::Pi, "openai", "deepseek-chat", 0, 0, None),
                event(0, PiUsageSource::OhMyPi, "openai", "", 100, 10, None),
                event(0, PiUsageSource::OhMyPi, "openai", "", 0, 0, None),
                event(0, PiUsageSource::Pi, "openai", "gpt-5.5", 0, 0, None),
                event(
                    0,
                    PiUsageSource::Pi,
                    "openai",
                    "claude-sonnet-4-5",
                    100,
                    10,
                    None,
                ),
            ];
            let actual = cases
                .into_iter()
                .map(|event| {
                    let event = event.unwrap();
                    let mut accumulator = DailyUsageAccumulator::default();
                    let sources = aggregate_into(
                        std::slice::from_ref(&event),
                        "codex",
                        now(),
                        &test_bundled_pricing(),
                        &mut accumulator,
                    );
                    format!(
                        "{} {}: pi={} oh_my_pi={}",
                        event.model, event.reported_total_tokens, sources.pi, sources.oh_my_pi
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_pinned("sources", &actual, SOURCES);
        }

        const PER_CARD: &str = r#"
== claude pi=true oh_my_pi=false
{
  "today": {
    "tokens": 22200,
    "estimatedCostUsd": 0.3498,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 2200,
          "costUsd": 0.3
        },
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 20000,
          "costUsd": 0.0498
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 1100,
    "estimatedCostUsd": 0.0045,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 1100,
          "costUsd": 0.0045
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "last30Days": {
    "tokens": 24620,
    "estimatedCostUsd": 0.60475,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 2200,
          "costUsd": 0.3
        },
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 21210,
          "costUsd": 0.25475000000000003
        },
        {
          "model": "Other",
          "totalTokens": 1210,
          "costUsd": null,
          "variants": [
            {
              "model": "Unattributed",
              "totalTokens": 440,
              "costUsd": 0.05
            },
            {
              "model": "claude-unreleased-zz-9",
              "totalTokens": 770,
              "costUsd": null
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "claude-unreleased-zz-9"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 22200,
      "estimatedCostUsd": 0.3498,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 1100,
      "estimatedCostUsd": 0.0045,
      "estimateComplete": true
    },
    {
      "date": "today-2",
      "tokens": 1210,
      "estimatedCostUsd": 0.05,
      "estimateComplete": false
    },
    {
      "date": "today-5",
      "tokens": 0,
      "estimatedCostUsd": 0.2,
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
    "claude-unreleased-zz-9"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": null,
    "last30Days": {
      "models": {
        "claude-sonnet-4-5": {
          "tokens": 330,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        },
        "deepseek-chat": {
          "tokens": 110,
          "pricedTokens": 110,
          "estimatedCostUsd": 0.01,
          "costEstimated": false
        }
      },
      "tokens": 440,
      "pricedTokens": 110,
      "estimatedCostUsd": 0.01,
      "costEstimated": false,
      "estimateComplete": false
    }
  }
}
== codex pi=true oh_my_pi=true
{
  "today": {
    "tokens": 4400,
    "estimatedCostUsd": 0.032,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gpt-5.5",
          "totalTokens": 4400,
          "costUsd": 0.032
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 1100,
    "estimatedCostUsd": 0.008,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gpt-5.6-sol",
          "totalTokens": 1100,
          "costUsd": 0.008
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "last30Days": {
    "tokens": 5830,
    "estimatedCostUsd": 0.04,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "gpt-5.5",
          "totalTokens": 4400,
          "costUsd": 0.032
        },
        {
          "model": "gpt-5.6-sol",
          "totalTokens": 1100,
          "costUsd": 0.008
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
      "tokens": 4400,
      "estimatedCostUsd": 0.032,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 1100,
      "estimatedCostUsd": 0.008,
      "estimateComplete": true
    },
    {
      "date": "today-2",
      "tokens": 330,
      "estimatedCostUsd": null,
      "estimateComplete": false
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
      "tokens": 1650,
      "pricedTokens": 1540,
      "estimatedCostUsd": 0.0003948,
      "costEstimated": true,
      "estimateComplete": false
    }
  }
}
== antigravity pi=true oh_my_pi=false
{
  "today": null,
  "yesterday": {
    "tokens": 1100,
    "estimatedCostUsd": 0.0032,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 1100,
          "costUsd": 0.0032,
          "variants": [
            {
              "model": "gemini-3-pro",
              "totalTokens": 1100,
              "costUsd": 0.0032
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "last30Days": {
    "tokens": 1650,
    "estimatedCostUsd": 0.003305,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 1100,
          "costUsd": 0.0032,
          "variants": [
            {
              "model": "gemini-3-pro",
              "totalTokens": 1100,
              "costUsd": 0.0032
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
      "date": "today-1",
      "tokens": 1100,
      "estimatedCostUsd": 0.0032,
      "estimateComplete": true
    },
    {
      "date": "today-4",
      "tokens": 550,
      "estimatedCostUsd": 0.000105,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== grok pi=true oh_my_pi=false
{
  "today": null,
  "yesterday": {
    "tokens": 1100,
    "estimatedCostUsd": 0.00025,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-4",
          "totalTokens": 1100,
          "costUsd": 0.00025
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "last30Days": {
    "tokens": 1100,
    "estimatedCostUsd": 0.00025,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-4",
          "totalTokens": 1100,
          "costUsd": 0.00025
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "daily": [
    {
      "date": "today-1",
      "tokens": 1100,
      "estimatedCostUsd": 0.00025,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== kimi pi=true oh_my_pi=false
{
  "today": null,
  "yesterday": {
    "tokens": 1100,
    "estimatedCostUsd": 0.0009,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "moonshot/kimi-k2.5",
          "totalTokens": 1100,
          "costUsd": 0.0009,
          "variants": [
            {
              "model": "kimi-k2p5",
              "totalTokens": 1100,
              "costUsd": 0.0009
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "last30Days": {
    "tokens": 1100,
    "estimatedCostUsd": 0.0009,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "moonshot/kimi-k2.5",
          "totalTokens": 1100,
          "costUsd": 0.0009,
          "variants": [
            {
              "model": "kimi-k2p5",
              "totalTokens": 1100,
              "costUsd": 0.0009
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
      "date": "today-1",
      "tokens": 1100,
      "estimatedCostUsd": 0.0009,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== zai pi=false oh_my_pi=true
{
  "today": null,
  "yesterday": {
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
      "date": "today-1",
      "tokens": 660,
      "estimatedCostUsd": 0.000384,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
== minimax pi=false oh_my_pi=true
{
  "today": null,
  "yesterday": {
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
      "date": "today-1",
      "tokens": 660,
      "estimatedCostUsd": 0.000252,
      "estimateComplete": true
    }
  ],
  "unknownModels": []
}
"#;
        const SOURCES: &str = r#"
deepseek-chat 110: pi=true oh_my_pi=false
deepseek-chat 0: pi=true oh_my_pi=false
 110: pi=false oh_my_pi=true
 0: pi=false oh_my_pi=false
gpt-5.5 0: pi=false oh_my_pi=false
claude-sonnet-4-5 110: pi=false oh_my_pi=false
"#;
    }
}
