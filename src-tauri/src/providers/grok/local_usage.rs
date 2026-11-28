use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    models::UsageHistory,
    pricing::{ModelPricing, TokenBreakdown},
    providers::local_usage::{
        card_usage::DailyUsageAccumulator,
        card_usage::{self, UsageObservation},
        home_clients::{self, Card, FoldContext, HomeClient},
        log_usage::{parse_log_timestamp, LocalLogs, LogCacheError, LogFormat},
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
    /// Home clients folded into the Grok Card: all of them, or none in tests.
    clients: &'static [&'static dyn HomeClient],
}

impl GrokLogUsageScanner {
    pub fn new() -> Self {
        Self {
            path: default_log_path(),
            clients: &home_clients::CLIENTS,
        }
    }

    #[cfg(test)]
    pub fn for_path(path: PathBuf) -> Self {
        Self { path, clients: &[] }
    }

    pub fn scan(
        &self,
        storage: &Storage,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
    ) -> Result<UsageHistory, GrokError> {
        scan_path(storage, &self.path, self.clients, now, pricing)
    }
}

pub(crate) fn scan_routed_usage_into(
    storage: &Storage,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> Result<bool, GrokError> {
    let path = default_log_path();
    let events = load_events(storage, &path, now)?;
    Ok(aggregate_routed_into(
        &events,
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

struct GrokLogs;

impl LogFormat for GrokLogs {
    type Event = TokenEvent;
    type Context = ();
    const PROVIDER_ID: &'static str = "grok";
    const SCHEMA_VERSION: u8 = LOG_CACHE_SCHEMA_VERSION;
    /// Grok writes one log, so an unreadable log leaves nothing to show.
    const UNREADABLE_FAILS: bool = true;

    fn parse(content: &str) -> Vec<TokenEvent> {
        parse_jsonl(content)
    }

    fn timestamp(event: &TokenEvent) -> DateTime<Utc> {
        event.timestamp
    }
}

static LOGS: LocalLogs<GrokLogs> = LocalLogs::new();

fn load_events(
    storage: &Storage,
    path: &Path,
    now: DateTime<Utc>,
) -> Result<std::sync::Arc<Vec<TokenEvent>>, GrokError> {
    // No log yet means no usage; anything else at the path must be a readable log.
    let files = match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        _ => Some((path.to_path_buf(), ())),
    };
    LOGS.load(storage, now, files).map_err(|error| match error {
        LogCacheError::Storage(_) => GrokError::Storage,
        LogCacheError::Encode(_) | LogCacheError::Unreadable => GrokError::LocalUsage,
    })
}

fn scan_path(
    storage: &Storage,
    path: &Path,
    clients: &[&dyn HomeClient],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
) -> Result<UsageHistory, GrokError> {
    let events = load_events(storage, path, now)?;

    home_clients::card_history(
        clients,
        &FoldContext {
            storage,
            now,
            pricing,
        },
        Card {
            id: "grok",
            name: "Grok",
        },
        "Grok logs",
        |accumulator| Ok(aggregate_into(&events, now, pricing, accumulator)),
    )
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
fn aggregate(events: &[TokenEvent], now: DateTime<Utc>, pricing: &ModelPricing) -> UsageHistory {
    let mut accumulator = DailyUsageAccumulator::default();
    aggregate_into(events, now, pricing, &mut accumulator);
    accumulator.build(now, SOURCE_NOTE)
}

fn aggregate_into(
    events: &[TokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let landed = card_usage::record(
        events.iter().map(observation),
        "grok",
        "grok",
        now,
        pricing,
        accumulator,
    );
    landed.own_series || landed.others
}

fn aggregate_routed_into(
    events: &[TokenEvent],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    card_usage::record(
        events.iter().map(observation),
        "grok",
        card_id,
        now,
        pricing,
        accumulator,
    )
    .own_series
}

/// Grok counts reasoning tokens as output and reports cached prompt tokens inside the prompt.
fn observation(event: &TokenEvent) -> UsageObservation<'_> {
    UsageObservation {
        total_tokens: event
            .prompt
            .saturating_add(event.completion)
            .saturating_add(event.reasoning),
        ..UsageObservation::new(
            event.timestamp,
            event.model.as_deref(),
            TokenBreakdown {
                input: event.prompt.saturating_sub(event.cached_prompt),
                cache_read: event.cached_prompt,
                output: event.completion.saturating_add(event.reasoning),
                ..TokenBreakdown::default()
            },
            None,
        )
    }
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
            &parse_jsonl(include_str!("fixtures/usage.jsonl")),
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
        let history = aggregate(&parse_jsonl(content), now(), &test_bundled_pricing());

        assert!((history.today.unwrap().estimated_cost_usd.unwrap() - 3.36).abs() < 0.000_001);
    }

    #[test]
    fn unknown_and_missing_models_retain_tokens_as_incomplete() {
        let content = r#"{"ts":"2026-06-18T08:00:00Z","pid":1,"msg":"model changed","ctx":{"model":"grok-unknown-model"}}
{"ts":"2026-06-18T09:00:00Z","pid":1,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":1000000}}
{"ts":"2026-06-18T10:00:00Z","pid":2,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":1000000}}
{"ts":"2026-06-18T11:00:00Z","pid":3,"msg":"model changed","ctx":{"model":"grok-build"}}
{"ts":"2026-06-18T12:00:00Z","pid":3,"msg":"shell.turn.inference_done","ctx":{"prompt_tokens":500000}}"#;
        let history = aggregate(&parse_jsonl(content), now(), &test_bundled_pricing());

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
        let grok = aggregate(&events, now(), &pricing);
        assert_eq!(grok.today.as_ref().unwrap().tokens, 100_000);
        assert_eq!(grok.other_usage.unwrap().today.unwrap().tokens, 300_000);

        let mut claude =
            crate::providers::local_usage::card_usage::DailyUsageAccumulator::default();
        assert!(super::aggregate_routed_into(
            &events,
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
        let history = aggregate(&parse_jsonl(content), now(), &test_bundled_pricing());
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

    /// Pins the output of grok's per-event policy, so moving it into `card_usage` cannot change
    /// what any Card shows.
    mod characterization {
        use chrono::{DateTime, Duration, TimeZone, Utc};

        use super::super::{aggregate, aggregate_routed_into, TokenEvent};
        use crate::{
            pricing::test_bundled_pricing,
            providers::local_usage::{
                card_usage::test_support::{assert_pinned, relative_json},
                card_usage::DailyUsageAccumulator,
            },
        };

        fn now() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap()
        }

        fn event(days_ago: i64, model: Option<&str>, prompt: u64, completion: u64) -> TokenEvent {
            TokenEvent {
                timestamp: now() - Duration::days(days_ago),
                model: model.map(str::to_owned),
                prompt,
                cached_prompt: 0,
                completion,
                reasoning: 0,
            }
        }

        fn events() -> Vec<TokenEvent> {
            let mut cached = event(0, Some("grok-4"), 10_000, 500);
            cached.cached_prompt = 6_000;
            cached.reasoning = 300;
            let mut future = event(0, Some("grok-4"), 9_999, 9_999);
            future.timestamp = now() + Duration::hours(1);
            vec![
                // Home models, with cached prompt and reasoning tokens.
                cached,
                event(0, Some("grok-4"), 2_000, 200),
                event(1, Some("grok-code-fast-1"), 1_000, 100),
                event(1, Some("grok-4-fast-reasoning"), 1_000, 100),
                // A grok-family model without a price, and a missing model.
                event(2, Some("grok-99-unreleased"), 700, 70),
                event(2, None, 400, 40),
                // Routed to other Cards.
                event(3, Some("claude-sonnet-4-5"), 1_500, 150),
                event(3, Some("gpt-5.5"), 1_500, 150),
                event(3, Some("gemini-3-pro"), 800, 80),
                event(3, Some("glm-4.6"), 600, 60),
                event(3, Some("kimi-k2"), 600, 60),
                // Others: priced, unpriced, local runtime, and gpt-oss.
                event(4, Some("deepseek-chat"), 900, 90),
                event(4, Some("totally-unknown-model"), 50, 5),
                event(4, Some("ollama/llama3"), 50, 5),
                event(4, Some("gpt-oss-120b"), 500, 50),
                // Zero tokens, guarded everywhere.
                event(5, Some("grok-4"), 0, 0),
                event(5, Some("deepseek-chat"), 0, 0),
                event(5, Some("gpt-5.5"), 0, 0),
                event(5, None, 0, 0),
                // Window edges.
                event(29, Some("grok-4"), 100, 10),
                event(29, Some("gpt-5.5"), 100, 10),
                event(30, Some("grok-4"), 100, 10),
                event(30, Some("gpt-5.5"), 100, 10),
                event(30, Some("deepseek-chat"), 100, 10),
                future,
            ]
        }

        #[test]
        fn native_card_output_is_pinned() {
            let history = aggregate(&events(), now(), &test_bundled_pricing());
            assert_pinned("native", &relative_json(&history, now()), NATIVE);
        }

        #[test]
        fn routed_output_is_pinned_per_card() {
            let actual = ["claude", "codex", "antigravity", "zai", "kimi", "minimax"]
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
    "tokens": 13000,
    "estimatedCostUsd": 0.00182,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-4",
          "totalTokens": 13000,
          "costUsd": 0.00182
        }
      ],
      "sourceNote": "From your Grok logs (estimated)"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 2200,
    "estimatedCostUsd": 0.00145,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-build-0.1",
          "totalTokens": 1100,
          "costUsd": 0.0012,
          "variants": [
            {
              "model": "grok-code-fast-1",
              "totalTokens": 1100,
              "costUsd": 0.0012
            }
          ]
        },
        {
          "model": "grok-4-fast-reasoning",
          "totalTokens": 1100,
          "costUsd": 0.00025
        }
      ],
      "sourceNote": "From your Grok logs (estimated)"
    },
    "unknownModels": []
  },
  "last30Days": {
    "tokens": 16520,
    "estimatedCostUsd": 0.003295,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "grok-4",
          "totalTokens": 13110,
          "costUsd": 0.001845
        },
        {
          "model": "grok-build-0.1",
          "totalTokens": 1100,
          "costUsd": 0.0012,
          "variants": [
            {
              "model": "grok-code-fast-1",
              "totalTokens": 1100,
              "costUsd": 0.0012
            }
          ]
        },
        {
          "model": "grok-4-fast-reasoning",
          "totalTokens": 1100,
          "costUsd": 0.00025
        },
        {
          "model": "Other",
          "totalTokens": 1210,
          "costUsd": null,
          "variants": [
            {
              "model": "grok-99-unreleased",
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
      "sourceNote": "From your Grok logs (estimated)"
    },
    "unknownModels": [
      "Unattributed",
      "grok-99-unreleased"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 13000,
      "estimatedCostUsd": 0.00182,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 2200,
      "estimatedCostUsd": 0.00145,
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
      "estimatedCostUsd": 0.000025,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed",
    "grok-99-unreleased"
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
"#;
        const ROUTED: &str = r#"
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
          "costUsd": 0.00256,
          "variants": [
            {
              "model": "gemini-3-pro",
              "totalTokens": 880,
              "costUsd": 0.00256
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
      "tokens": 880,
      "estimatedCostUsd": 0.00256,
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
