use std::{fs, path::Path};

use chrono::{TimeZone, Utc};
use tempfile::tempdir;

use super::{parse_chat_file, scan_tmp_directories_into, tmp_directories};
use crate::{
    pricing::test_bundled_pricing, providers::local_usage::card_usage::DailyUsageAccumulator,
    storage::Storage,
};

fn session_json(session_id: &str, messages: Value) -> String {
    serde_json::json!({
        "sessionId": session_id,
        "projectHash": "abc123",
        "startTime": "2026-11-20T10:00:00.000Z",
        "lastUpdated": "2026-11-20T10:05:00.000Z",
        "messages": messages,
    })
    .to_string()
}

use serde_json::Value;

#[test]
fn tmp_directories_respect_gemini_cli_home_and_deduplicate() {
    let dir = tempdir().unwrap();
    let nested_home = dir.path().join("custom-home");
    let nested_tmp = nested_home.join(".gemini").join("tmp");
    fs::create_dir_all(&nested_tmp).unwrap();
    assert_eq!(
        tmp_directories(Some(nested_home.to_str().unwrap()), dir.path()),
        [fs::canonicalize(&nested_tmp).unwrap()]
    );

    // Pointing directly at a `.gemini` directory also resolves `<dir>/tmp`.
    let direct_gemini = nested_home.join(".gemini");
    assert_eq!(
        tmp_directories(Some(direct_gemini.to_str().unwrap()), dir.path()),
        [fs::canonicalize(&nested_tmp).unwrap()]
    );

    assert_eq!(
        tmp_directories(None, Path::new("/home/tester")),
        [Path::new("/home/tester/.gemini/tmp")]
    );
}

#[test]
fn token_buckets_avoid_double_counting_cached_tokens_and_include_thoughts_and_tool() {
    let content = session_json(
        "sess-1",
        serde_json::json!([
            {
                "id": "u1",
                "timestamp": "2026-11-20T10:00:00.000Z",
                "type": "user",
                "content": "hi"
            },
            {
                "id": "m1",
                "timestamp": "2026-11-20T10:01:00.000Z",
                "type": "gemini",
                "model": "gemini-4-neon",
                "tokens": {
                    "input": 1_000_000,
                    "cached": 400_000,
                    "output": 80_000,
                    "thoughts": 20_000,
                    "tool": 50_000,
                    "total": 1_150_000
                }
            }
        ]),
    );
    let events = parse_chat_file(&content);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].input, 650_000);
    assert_eq!(events[0].cache_read, 400_000);
    assert_eq!(events[0].output, 100_000);
    assert_eq!(events[0].total_tokens(), 1_150_000);
}

#[test]
fn scanner_deduplicates_resumed_and_dual_format_sessions_and_skips_checkpoints() {
    let dir = tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
    let pricing = test_bundled_pricing();
    let now = Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap();

    let tmp_dir = dir.path().join(".gemini").join("tmp");
    let project = tmp_dir.join("proj_hash");
    let chats = project.join("chats");
    fs::create_dir_all(&chats).unwrap();

    let msg_a = serde_json::json!({
        "id": "msg-1111",
        "timestamp": "2026-11-20T10:00:00.000Z",
        "type": "gemini",
        "model": "gemini-4-neon",
        "tokens": {
            "input": 1_000_000,
            "cached": 0,
            "output": 100_000,
            "thoughts": 0,
            "tool": 0,
            "total": 1_100_000
        }
    });
    let msg_b = serde_json::json!({
        "id": "msg-2222",
        "timestamp": "2026-11-20T10:05:00.000Z",
        "type": "gemini",
        "model": "gemini-4-neon",
        "tokens": {
            "input": 500_000,
            "cached": 0,
            "output": 50_000,
            "thoughts": 0,
            "tool": 0,
            "total": 550_000
        }
    });

    // Original session file.
    fs::write(
        chats.join("session-2026-11-20T10-00-aaaa.json"),
        session_json("sess-a", serde_json::json!([msg_a.clone()])),
    )
    .unwrap();

    // Resumed/copied session repeating msg-1111 and adding msg-2222.
    fs::write(
        chats.join("session-2026-11-20T10-05-bbbb.json"),
        session_json("sess-b", serde_json::json!([msg_a.clone(), msg_b])),
    )
    .unwrap();

    // JSONL copy of msg-1111 + a subagent message with a distinct ID.
    let subagent_dir = chats.join("sess-a");
    fs::create_dir_all(&subagent_dir).unwrap();
    let subagent_jsonl = [
        msg_a.to_string(),
        serde_json::json!({
            "id": "msg-sub-3333",
            "timestamp": "2026-11-20T10:06:00.000Z",
            "type": "gemini",
            "model": "gemini-4-neon",
            "tokens": {
                "input": 200_000,
                "cached": 0,
                "output": 0,
                "thoughts": 0,
                "tool": 0,
                "total": 200_000
            }
        })
        .to_string(),
    ]
    .join("\n");
    fs::write(subagent_dir.join("subagent-1.jsonl"), subagent_jsonl).unwrap();

    // Checkpoints outside and inside chats/ must be ignored.
    let checkpoint_msg = serde_json::json!({
        "id": "msg-checkpoint-9999",
        "timestamp": "2026-11-20T10:07:00.000Z",
        "type": "gemini",
        "model": "gemini-4-neon",
        "tokens": { "input": 9_000_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 9_000_000 }
    });
    fs::write(
        project.join("checkpoint-saved.json"),
        session_json("chk-1", serde_json::json!([checkpoint_msg.clone()])),
    )
    .unwrap();
    fs::write(
        chats.join("checkpoint-inline.json"),
        session_json("chk-2", serde_json::json!([checkpoint_msg])),
    )
    .unwrap();

    let mut antigravity_acc = DailyUsageAccumulator::default();
    assert!(scan_tmp_directories_into(
        &storage,
        &[tmp_dir],
        now,
        &pricing,
        "antigravity",
        &mut antigravity_acc,
    )
    .unwrap());
    let history = antigravity_acc.build(now, "From your Gemini CLI history (estimated)");
    let today = history.today.unwrap();
    // msg-1111 (1.1M) + msg-2222 (0.55M) + msg-sub-3333 (0.2M) = 1.85M tokens.
    assert_eq!(today.tokens, 1_850_000);
    // gemini-4-neon: $3/M input, $15/M output -> 1.7M*$3 + 0.15M*$15 = $5.10 + $2.25 = $7.35.
    assert!((today.estimated_cost_usd.unwrap() - 7.35).abs() < 1e-6);
    assert!(today.unknown_models.is_empty());
}

#[test]
fn missing_model_in_gemini_cli_session_inherits_and_warns_as_unverified() {
    let dir = tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
    let pricing = test_bundled_pricing();
    let now = Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap();

    let tmp_dir = dir.path().join(".gemini").join("tmp");
    let chats = tmp_dir.join("proj_hash").join("chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(
        chats.join("session-inherit.json"),
        session_json(
            "sess-inherit",
            serde_json::json!([
                {
                    "id": "m-1",
                    "timestamp": "2026-11-20T10:00:00.000Z",
                    "type": "gemini",
                    "model": "gemini-4-neon",
                    "tokens": { "input": 100_000, "cached": 0, "output": 10_000, "thoughts": 0, "tool": 0, "total": 110_000 }
                },
                {
                    "id": "m-2",
                    "timestamp": "2026-11-20T10:01:00.000Z",
                    "type": "gemini",
                    "tokens": { "input": 50_000, "cached": 0, "output": 5_000, "thoughts": 0, "tool": 0, "total": 55_000 }
                }
            ]),
        ),
    )
    .unwrap();

    let mut antigravity_acc = DailyUsageAccumulator::default();
    assert!(scan_tmp_directories_into(
        &storage,
        &[tmp_dir],
        now,
        &pricing,
        "antigravity",
        &mut antigravity_acc,
    )
    .unwrap());
    let warnings = antigravity_acc.warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Gemini CLI"));
    assert!(warnings[0].contains("unverified"));
    let today = antigravity_acc.build(now, "test").today.unwrap();
    assert_eq!(today.tokens, 165_000);
    assert!(today.unknown_models.is_empty());
}

#[test]
fn scanner_routes_non_gemini_models_once_without_double_counting() {
    let dir = tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
    let pricing = test_bundled_pricing();
    let now = Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap();

    let tmp_dir = dir.path().join(".gemini").join("tmp");
    let chats = tmp_dir.join("proj_hash").join("chats");
    fs::create_dir_all(&chats).unwrap();
    fs::write(
        chats.join("session-mixed.json"),
        session_json(
            "sess-mixed",
            serde_json::json!([
                {
                    "id": "m-gemini",
                    "timestamp": "2026-11-20T10:00:00.000Z",
                    "type": "gemini",
                    "model": "gemini-4-neon",
                    "tokens": { "input": 100_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 100_000 }
                },
                {
                    "id": "m-claude",
                    "timestamp": "2026-11-20T10:01:00.000Z",
                    "type": "gemini",
                    "model": "claude-sonnet-4-6",
                    "tokens": { "input": 200_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 200_000 }
                },
                {
                    "id": "m-deepseek",
                    "timestamp": "2026-11-20T10:02:00.000Z",
                    "type": "gemini",
                    "model": "deepseek-flash",
                    "tokens": { "input": 300_000, "cached": 0, "output": 0, "thoughts": 0, "tool": 0, "total": 300_000 }
                }
            ]),
        ),
    )
    .unwrap();

    let roots = [tmp_dir];
    let mut antigravity_acc = DailyUsageAccumulator::default();
    assert!(scan_tmp_directories_into(
        &storage,
        &roots,
        now,
        &pricing,
        "antigravity",
        &mut antigravity_acc
    )
    .unwrap());
    let antigravity = antigravity_acc.build(now, "test");
    assert_eq!(antigravity.today.unwrap().tokens, 100_000);
    let other_today = antigravity.other_usage.unwrap().today.unwrap();
    assert_eq!(other_today.tokens, 300_000);
    assert!(other_today.estimated_cost_usd.is_some());
    assert!(other_today.estimate_complete);

    let mut claude_acc = DailyUsageAccumulator::default();
    assert!(
        scan_tmp_directories_into(&storage, &roots, now, &pricing, "claude", &mut claude_acc)
            .unwrap()
    );
    let claude = claude_acc.build(now, "test");
    assert_eq!(claude.today.unwrap().tokens, 200_000);
    assert!(claude.other_usage.is_none());
}

#[test]
fn single_line_wrapped_jsonl_preserves_message_and_timestamp() {
    let content = serde_json::json!({
        "sessionId": "single-line-session",
        "timestamp": "2026-11-20T10:05:00.000Z",
        "message": {
            "id": "m-single",
            "type": "gemini",
            "model": "gemini-4-neon",
            "tokens": { "input": 100, "cached": 0, "output": 50, "thoughts": 0, "tool": 0, "total": 150 }
        }
    })
    .to_string();
    let events = parse_chat_file(&content);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id.as_deref(), Some("single-line-session"));
    assert_eq!(events[0].total_tokens(), 150);
}

/// Pins the output of gemini CLI's per-event policy, so moving it into `card_usage` cannot
/// change what any Card shows.
mod characterization {
    use chrono::{DateTime, Duration, TimeZone, Utc};

    use super::super::{aggregate_into, GeminiCliTokenEvent};
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

    fn event(days_ago: i64, model: Option<&str>, input: u64, output: u64) -> GeminiCliTokenEvent {
        GeminiCliTokenEvent {
            id: None,
            session_id: None,
            timestamp: now() - Duration::days(days_ago),
            model: model.map(str::to_owned),
            model_inherited: false,
            input,
            cache_read: 0,
            output,
        }
    }

    fn events() -> Vec<GeminiCliTokenEvent> {
        let mut cached = event(0, Some("gemini-3-pro-preview"), 10_000, 500);
        cached.cache_read = 6_000;
        let mut future = event(0, Some("gemini-3-pro"), 9_999, 9_999);
        future.timestamp = now() + Duration::hours(1);
        vec![
            // Home models on the antigravity Card, labelled by family plus variant.
            cached,
            event(0, Some("gemini-3-pro"), 2_000, 200),
            event(0, Some("gemini-3.1-pro-high"), 2_000, 200),
            event(1, Some("gemini-3.5-flash-lite-tiered-high"), 1_000, 100),
            event(1, Some("gemini-3.5-flash"), 1_000, 100),
            event(1, Some("gpt-oss-120b"), 500, 50),
            // A gemini-family model without a price, and a missing model.
            event(2, Some("gemini-99-unreleased"), 700, 70),
            event(2, None, 400, 40),
            // Routed to other Cards.
            event(3, Some("claude-sonnet-4-5"), 1_500, 150),
            event(3, Some("gpt-5.5"), 1_500, 150),
            event(3, Some("grok-4"), 800, 80),
            event(3, Some("glm-4.6"), 600, 60),
            event(3, Some("kimi-k2"), 600, 60),
            // Others: priced, unpriced, and local runtime.
            event(4, Some("deepseek-chat"), 900, 90),
            event(4, Some("totally-unknown-model"), 50, 5),
            event(4, Some("ollama/llama3"), 50, 5),
            // Zero tokens, guarded everywhere.
            event(5, Some("gemini-3-pro"), 0, 0),
            event(5, Some("deepseek-chat"), 0, 0),
            event(5, Some("gpt-5.5"), 0, 0),
            event(5, None, 0, 0),
            // Window edges.
            event(29, Some("gemini-3-pro"), 100, 10),
            event(29, Some("gpt-5.5"), 100, 10),
            event(30, Some("gemini-3-pro"), 100, 10),
            event(30, Some("gpt-5.5"), 100, 10),
            event(30, Some("deepseek-chat"), 100, 10),
            future,
        ]
    }

    #[test]
    fn output_is_pinned_per_card() {
        let actual = [
            "antigravity",
            "claude",
            "codex",
            "grok",
            "kimi",
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
== antigravity contributed=true
{
  "today": {
    "tokens": 20900,
    "estimatedCostUsd": 0.04,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 18700,
          "costUsd": 0.0336,
          "variants": [
            {
              "model": "gemini-3-pro-preview",
              "totalTokens": 16500,
              "costUsd": 0.0272
            },
            {
              "model": "gemini-3-pro",
              "totalTokens": 2200,
              "costUsd": 0.0064
            }
          ]
        },
        {
          "model": "gemini-3.1-pro-preview",
          "totalTokens": 2200,
          "costUsd": 0.0064,
          "variants": [
            {
              "model": "gemini-3.1-pro-high",
              "totalTokens": 2200,
              "costUsd": 0.0064
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 2750,
    "estimatedCostUsd": 0.002505,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3.5-flash",
          "totalTokens": 1100,
          "costUsd": 0.0024
        },
        {
          "model": "gpt-oss-120b",
          "totalTokens": 550,
          "costUsd": 0.000105
        },
        {
          "model": "gemini-3.5-flash-lite",
          "totalTokens": 1100,
          "costUsd": null,
          "variants": [
            {
              "model": "gemini-3.5-flash-lite-tiered-high",
              "totalTokens": 1100,
              "costUsd": null
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "gemini-3.5-flash-lite-tiered-high"
    ]
  },
  "last30Days": {
    "tokens": 24970,
    "estimatedCostUsd": 0.042825,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "gemini-3-pro-preview",
          "totalTokens": 18810,
          "costUsd": 0.03392,
          "variants": [
            {
              "model": "gemini-3-pro-preview",
              "totalTokens": 16500,
              "costUsd": 0.0272
            },
            {
              "model": "gemini-3-pro",
              "totalTokens": 2310,
              "costUsd": 0.00672
            }
          ]
        },
        {
          "model": "gemini-3.1-pro-preview",
          "totalTokens": 2200,
          "costUsd": 0.0064,
          "variants": [
            {
              "model": "gemini-3.1-pro-high",
              "totalTokens": 2200,
              "costUsd": 0.0064
            }
          ]
        },
        {
          "model": "Other",
          "totalTokens": 3960,
          "costUsd": null,
          "variants": [
            {
              "model": "gemini-3.5-flash",
              "totalTokens": 1100,
              "costUsd": 0.0024
            },
            {
              "model": "gpt-oss-120b",
              "totalTokens": 550,
              "costUsd": 0.000105
            },
            {
              "model": "gemini-3.5-flash-lite",
              "totalTokens": 1100,
              "costUsd": null
            },
            {
              "model": "gemini-99-unreleased",
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
      "gemini-3.5-flash-lite-tiered-high",
      "gemini-99-unreleased"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 20900,
      "estimatedCostUsd": 0.04,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 2750,
      "estimatedCostUsd": 0.002505,
      "estimateComplete": false
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
      "estimatedCostUsd": 0.00032,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed",
    "gemini-3.5-flash-lite-tiered-high",
    "gemini-99-unreleased"
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
      "tokens": 1100,
      "pricedTokens": 990,
      "estimatedCostUsd": 0.0002898,
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
