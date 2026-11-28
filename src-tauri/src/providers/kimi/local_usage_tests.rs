use std::{fs, path::Path};

use chrono::{TimeZone, Utc};
use tempfile::tempdir;

use super::{
    aggregate_into, kimi_roots, normalize_kimi_model, parse_jsonl, read_default_model,
    scan_roots_into, KimiLogs, KimiTokenEvent,
};
use crate::{
    pricing::test_bundled_pricing,
    providers::local_usage::{card_usage::DailyUsageAccumulator, log_usage::LogFormat},
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
    let parsed = parse_jsonl(&modern);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].model.as_deref(), Some("kimi-k3"));
    assert_eq!(parsed[0].input, 2_000);
    assert_eq!(parsed[0].output, 500);
    assert_eq!(parsed[0].cache_read, 20_000);
    assert_eq!(parsed[0].cache_write, 0);

    let legacy = legacy_kimi_wire(now.timestamp() as f64 + 0.5, None, [1_000, 250, 5_000, 100]);
    let mut parsed_legacy = parse_jsonl(&legacy);
    assert_eq!(parsed_legacy[0].model, None);
    KimiLogs::attach(
        &mut parsed_legacy[0],
        &Some("moonshot-cn/kimi-k2.7-code".to_owned()),
    );
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
    assert!(scan_roots_into(&storage, &roots, now, &pricing, "claude", &mut claude_acc).unwrap());
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
fn a_changed_default_model_reaches_cached_events_that_name_no_model() {
    let dir = tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("openquota.db")).unwrap();
    let pricing = test_bundled_pricing();
    let now = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
    let root = dir.path().join(".kimi");
    let wire = root
        .join("sessions")
        .join("hash")
        .join("11111111-2222-3333-4444-555555555555")
        .join("wire.jsonl");
    fs::create_dir_all(wire.parent().unwrap()).unwrap();
    fs::write(
        &wire,
        [
            serde_json::json!({"type":"usage.record","provider":"anthropic","model":"claude-sonnet-4-6","time":now.timestamp_millis()-1000,"usage":{"inputOther":200,"output":0}}).to_string(),
            legacy_kimi_wire(now.timestamp() as f64, None, [1_000, 0, 0, 0]),
        ].join("\n"),
    )
    .unwrap();
    let config = root.join("config.toml");
    let roots = [root];
    let tokens_on = |card: &str| {
        let mut accumulator = DailyUsageAccumulator::default();
        scan_roots_into(&storage, &roots, now, &pricing, card, &mut accumulator).unwrap();
        accumulator
            .build(now, "test")
            .today
            .map_or(0, |today| today.tokens)
    };

    fs::write(&config, "default_model = \"moonshot-cn/kimi-k2.7-code\"\n").unwrap();
    assert_eq!(tokens_on("kimi"), 1_000);
    assert_eq!(tokens_on("claude"), 200);

    fs::write(&config, "default_model = \"claude-sonnet-4-6\"\n").unwrap();
    assert_eq!(tokens_on("claude"), 1_200);
}

#[test]
fn provider_scoped_session_models_leave_raw_cache_events_and_default_fallback_intact() {
    let record = |provider: Option<&str>, model: Option<&str>, time: i64| {
        serde_json::json!({
            "type":"usage.record", "provider": provider, "model":model, "time":time,
            "usage":{"inputOther":100,"output":10}
        })
        .to_string()
    };
    let content = [
        record(Some("anthropic"), Some("claude-sonnet-4-6"), 1784808000000),
        record(Some("kimi"), None, 1784808001000),
        record(None, None, 1784808002000),
        record(Some("kimi"), Some("kimi-k2.5"), 1784808003000),
        record(Some("anthropic"), None, 1784808004000),
        record(Some("KIMI"), None, 1784808005000),
    ]
    .join("\n");
    let raw = parse_jsonl(&content);
    assert_eq!(raw.len(), 6);
    for index in [1, 2, 4, 5] {
        assert!(raw[index].model.is_none());
    }
    let mut attached = raw.clone();
    for event in &mut attached {
        KimiLogs::attach(event, &Some("kimi-k2.7-code".into()));
    }
    for index in [1, 2] {
        assert_eq!(attached[index].model.as_deref(), Some("kimi-k2.7-code"));
        assert!(!attached[index].model_inherited);
    }
    assert_eq!(attached[4].model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(attached[5].model.as_deref(), Some("kimi-k2.5"));
    assert!(attached[4].model_inherited && attached[5].model_inherited);
    let cached = serde_json::to_string(&raw).unwrap();
    let mut reloaded: Vec<KimiTokenEvent> = serde_json::from_str(&cached).unwrap();
    for event in &mut reloaded {
        KimiLogs::attach(event, &Some("kimi-k3".into()));
    }
    assert_eq!(reloaded[1].model.as_deref(), Some("kimi-k3"));
    assert_eq!(reloaded[2].model.as_deref(), Some("kimi-k3"));
}

#[test]
fn request_model_context_does_not_cross_an_explicit_provider_change() {
    let content = [
        serde_json::json!({"type":"llm.request","provider":"anthropic","model":"claude-sonnet-4-6"}),
        serde_json::json!({"type":"usage.record","provider":"kimi","time":1784808000000_i64,"usage":{"inputOther":100,"output":10}}),
        serde_json::json!({"timestamp":1784808001,"message":{"type":"StatusUpdate","payload":{"provider":"kimi","token_usage":{"input_other":100,"output":10}}}}),
    ].iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n");
    let mut events = parse_jsonl(&content);
    assert_eq!(events.len(), 2);
    for event in &mut events {
        assert!(event.model.is_none());
        KimiLogs::attach(event, &Some("kimi-k3".into()));
        assert_eq!(event.model.as_deref(), Some("kimi-k3"));
        assert!(!event.model_inherited);
    }
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
    assert!(scan_roots_into(&storage, &roots, now, &pricing, "kimi", &mut accumulator).unwrap());
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
    let providers = parse_jsonl(&content)
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
        session_model: None,
        model_inherited: false,
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
        providers::local_usage::{
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
            session_model: None,
            model_inherited: false,
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
            // Window edges: today-29 counts, today-30 does not.
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
          "costUsd": 0.0018,
          "variants": [
            {
              "model": "kimi-k2.5",
              "totalTokens": 2200,
              "costUsd": 0.0018
            }
          ]
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
          "costUsd": 0.0018,
          "variants": [
            {
              "model": "kimi-k2.5",
              "totalTokens": 2200,
              "costUsd": 0.0018
            }
          ]
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
