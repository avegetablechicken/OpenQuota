use std::{fs, path::Path};

use chrono::{TimeZone, Utc};
use rusqlite::{params, Connection};
use tempfile::tempdir;

use super::{provider_kind, ZCodeProvider, ZCodeUsageScanner};
use crate::{pricing::test_bundled_pricing, providers::local_usage::zcode::ZCodeProviderConfig};

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

fn insert(connection: &Connection, provider: &str, model: &str, timestamp: i64, usage: [i64; 4]) {
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
    assert!((zai.today.as_ref().unwrap().estimated_cost_usd.unwrap() - 0.052).abs() < 0.000_001);
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
    let changed = ZCodeUsageScanner::for_paths(changed_path, directory.path().join("missing.json"));
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
        providers::local_usage::{
            card_usage::test_support::{assert_pinned, relative_json},
            card_usage::DailyUsageAccumulator,
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
      "tokens": 1430,
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
