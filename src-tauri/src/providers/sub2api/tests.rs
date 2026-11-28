use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

use chrono::{TimeZone, Utc};
use reqwest::StatusCode;
use tempfile::tempdir;

use super::{
    connection_target, credential_for_save, default_display_name_for_slot, definition_for,
    metric_template, normalize_base_url, normalize_refreshed_quotas,
    normalized_codex_provider_base_url_text, session_scope, should_apply_metric_template,
    StoredConfig, Sub2ApiClient, Sub2ApiConfigInput, Sub2ApiError, Sub2ApiProvider,
    Sub2ApiProviders,
};
use crate::providers::{
    codex::{client::UsageResponse, mapper::map_usage},
    UsageProvider,
};
use crate::{models::ProviderSnapshot, storage::Storage};

const QUOTA_DATA: &str = r#"{
  "user_id":"user-redacted",
  "account_id":"account-redacted",
  "email":"upstream@example.com",
  "plan_type":"prolite",
  "rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":38,"limit_window_seconds":604800,"reset_after_seconds":378553,"reset_at":1787810131}},
  "additional_rate_limits":[{"limit_name":"GPT-5.3-Codex-Spark","metered_feature":"codex_bengalfox","rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_after_seconds":18000,"reset_at":1787449578},"secondary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_after_seconds":604800,"reset_at":1788036378}}}],
  "rate_limit_reset_credits":{"available_count":1,"credits":[{"expires_at":"2026-09-21T00:19:21.387776Z"}]},
  "fetched_at":1787431578
}"#;

const CLAUDE_USAGE_DATA: &str = r#"{
  "source":"active",
  "five_hour":{"utilization":12.5,"resets_at":"2026-08-23T05:00:00Z","remaining_seconds":123},
  "seven_day":{"utilization":34,"resets_at":"2026-08-29T00:00:00Z"},
  "seven_day_sonnet":{"utilization":8},
  "seven_day_fable":{"utilization":3},
  "subscription_tier":"PRO"
}"#;

fn usage_data(_account: i64, now: chrono::DateTime<Utc>) -> String {
    let today = now.with_timezone(&chrono::Local).date_naive();
    serde_json::json!({"code":0,"data":{
        "history":[
            {"date":(today-chrono::Days::new(1)).to_string(),"tokens":1000,"cost":1.0,"actual_cost":45.0},
            {"date":today.to_string(),"tokens":2500,"cost":2.0,"actual_cost":60.0}
        ],
        "models":[
            {"model":"claude-haiku-4-5","total_tokens":1000,"cost":1.0,"actual_cost":45.0},
            {"model":"claude-sonnet-5","total_tokens":2500,"cost":2.0,"actual_cost":60.0}
        ]
    }}).to_string()
}

pub(super) fn serve_sequence(
    responses: Vec<String>,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    serve_sequence_with_status(responses.into_iter().map(|body| (200, body)).collect())
}

fn serve_sequence_with_status(
    responses: Vec<(u16, String)>,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 2048];
                let count = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&request);
                let Some(header_end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let content_length = text[..header_end]
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            sender
                .send(String::from_utf8_lossy(&request).into_owned())
                .unwrap();
            let response = format!(
                "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (format!("http://{address}"), receiver, worker)
}

#[test]
fn response_body_timeout_and_truncation_are_network_errors() {
    for stall in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\n\r\n{",
            ).unwrap();
            if stall {
                let _ = released.recv_timeout(Duration::from_secs(5));
            }
        });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let response = client.get(format!("http://{address}")).send().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = super::decode_response::<serde_json::Value>("test history", response);
        let _ = release.send(());
        worker.join().unwrap();
        assert_eq!(result, Err(Sub2ApiError::ConnectionFailed), "stall={stall}");
    }
}

#[test]
fn response_decoder_preserves_json_and_schema_failures() {
    let (base, _requests, worker) = serve_sequence(vec![
        "<html>gateway response</html>".into(),
        r#"{"code":0,"data":{"items":[],"total":"wrong-type"}}"#.into(),
        r#"{"code":0,"data":{"items":[],"total":0}}"#.into(),
    ]);
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap();
    for expected_valid in [false, false, true] {
        let response = client.get(&base).send().unwrap();
        let result = super::decode_response::<super::Envelope<super::AccountPage>>(
            "test accounts",
            response,
        );
        if expected_valid {
            assert_eq!(result.unwrap().data.unwrap().total, 0);
        } else {
            assert!(matches!(result, Err(Sub2ApiError::InvalidResponse)));
        }
    }
    worker.join().unwrap();
}

#[test]
fn metric_support_follows_the_track() {
    use super::track_shows_metric;

    assert!(!track_shows_metric("sub2api@2", "sub2api@2.extra", None));
    assert!(track_shows_metric("sub2api@2", "sub2api@2.spark", None));
    assert!(track_shows_metric(
        "sub2api@2",
        "sub2api@2.spark",
        Some("codex")
    ));
    assert!(!track_shows_metric(
        "sub2api@2",
        "sub2api@2.spark",
        Some("claude")
    ));
    assert!(!track_shows_metric(
        "sub2api@2",
        "sub2api@2.rateLimitResets",
        Some("claude")
    ));
    assert!(track_shows_metric(
        "sub2api@2",
        "sub2api@2.fable",
        Some("claude")
    ));
    assert!(!track_shows_metric(
        "sub2api@2",
        "sub2api@2.fable",
        Some("codex")
    ));
    assert!(track_shows_metric(
        "sub2api@2",
        "sub2api@2.session",
        Some("codex")
    ));
    assert!(track_shows_metric("sub2api", "sub2api@2.extra", None));
    assert!(!track_shows_metric("sub2api@2", "sub2api@2.claude", None));
    for suffix in ["session", "weekly", "claude", "claudeWeekly", "last30"] {
        assert!(track_shows_metric(
            "sub2api@2",
            &format!("sub2api@2.{suffix}"),
            Some("antigravity")
        ));
    }
    for suffix in ["sonnet", "spark", "monthly", "extra"] {
        assert!(!track_shows_metric(
            "sub2api@2",
            &format!("sub2api@2.{suffix}"),
            Some("antigravity")
        ));
    }
    for suffix in ["weekly", "monthly", "trend", "today"] {
        assert!(track_shows_metric(
            "sub2api@2",
            &format!("sub2api@2.{suffix}"),
            Some("grok")
        ));
    }
    for suffix in ["session", "claude", "daily", "expires", "extra"] {
        assert!(!track_shows_metric(
            "sub2api@2",
            &format!("sub2api@2.{suffix}"),
            Some("grok")
        ));
    }
    assert!(!track_shows_metric(
        "sub2api@2",
        "sub2api@2.claude",
        Some("claude")
    ));
}

#[test]
fn definition_exposes_one_fixed_sub2api_provider() {
    let definition = definition_for("sub2api", "Sub2API");
    assert_eq!(definition.id, "sub2api");
    assert_eq!(definition.display_name, "Sub2API");
    assert_eq!(definition.metrics[0].id, "sub2api.session");
    assert!(definition
        .metrics
        .iter()
        .any(|metric| metric.id == "sub2api.trend"));
    assert!(definition
        .metrics
        .iter()
        .any(|metric| metric.id == "sub2api.last30"));
    assert!(!definition.fallback_enabled);
}

#[test]
fn metric_templates_match_the_supported_native_provider_defaults() {
    for (upstream, native, unsupported) in [
        (
            super::Sub2ApiUpstream::Codex,
            crate::providers::codex::definition(),
            "credits",
        ),
        (
            super::Sub2ApiUpstream::Claude,
            crate::providers::claude::definition(),
            "extra",
        ),
        (
            super::Sub2ApiUpstream::Antigravity,
            crate::providers::antigravity::definition(),
            "",
        ),
        (
            super::Sub2ApiUpstream::Grok,
            crate::providers::grok::definition(),
            "payAsYouGo",
        ),
    ] {
        let template = metric_template("sub2api@2", upstream, super::Sub2ApiMode::Admin);
        for definition in native.metrics {
            let suffix = match definition.id.split_once('.').unwrap().1 {
                "geminiPro" => "session",
                "geminiWeekly" => "weekly",
                suffix => suffix,
            };
            if suffix == unsupported {
                continue;
            }
            assert!(
                super::track_shows_metric(
                    "sub2api@2",
                    &format!("sub2api@2.{suffix}"),
                    Some(upstream.provider_id())
                ),
                "{upstream} {suffix}"
            );
            let metric = template
                .iter()
                .find(|metric| metric.id == format!("sub2api@2.{suffix}"))
                .unwrap();
            assert_eq!(
                metric.enabled, definition.default_enabled,
                "{upstream} {suffix}"
            );
            assert_eq!(
                metric.section, definition.default_section,
                "{upstream} {suffix}"
            );
            assert_eq!(
                metric.pinned, definition.default_pinned,
                "{upstream} {suffix}"
            );
        }
    }

    let claude = metric_template(
        "sub2api@2",
        super::Sub2ApiUpstream::Claude,
        super::Sub2ApiMode::Admin,
    );
    assert!(claude
        .iter()
        .filter(|metric| metric.id.ends_with("spark")
            || metric.id.ends_with("sparkWeekly")
            || metric.id.ends_with("rateLimitResets"))
        .all(|metric| !metric.enabled));
    let codex = metric_template(
        "sub2api@2",
        super::Sub2ApiUpstream::Codex,
        super::Sub2ApiMode::Admin,
    );
    assert!(codex
        .iter()
        .filter(|metric| metric.id.ends_with("sonnet") || metric.id.ends_with("fable"))
        .all(|metric| !metric.enabled));
    assert!(claude
        .iter()
        .chain(codex.iter())
        .filter(|metric| metric.id.ends_with("extra"))
        .all(|metric| !metric.enabled));
    // Every template lists every metric, and enables only what its upstream reports.
    let definition = definition_for("sub2api@2", "Sub2API 2");
    for upstream in [
        super::Sub2ApiUpstream::Codex,
        super::Sub2ApiUpstream::Claude,
        super::Sub2ApiUpstream::Antigravity,
        super::Sub2ApiUpstream::Grok,
    ] {
        let template = metric_template("sub2api@2", upstream, super::Sub2ApiMode::Admin);
        for known in &definition.metrics {
            let suffix = known.id.split_once('.').unwrap().1;
            assert!(
                template.iter().any(|metric| metric.id == known.id),
                "{upstream} {suffix}"
            );
        }
        for metric in template {
            assert!(
                definition.metrics.iter().any(|known| known.id == metric.id),
                "{upstream} {}",
                metric.id
            );
            if metric.enabled {
                assert!(
                    super::track_shows_metric(
                        "sub2api@2",
                        &metric.id,
                        Some(upstream.provider_id())
                    ),
                    "{upstream} {}",
                    metric.id
                );
            }
        }
    }
}

#[test]
fn claude_refresh_omits_missing_and_resets_expired_quotas() {
    let body = serde_json::json!({
        "limits": [{
            "kind": "weekly_scoped",
            "scope": {"model": {"display_name": "Fable"}},
            "percent": 58,
            "resets_at": "2026-08-29T06:00:00Z"
        }]
    });
    let mapped = crate::providers::claude::map_sub2api_usage(&body).unwrap();

    let quotas = normalize_refreshed_quotas(
        super::Sub2ApiUpstream::Claude,
        mapped.quotas,
        Utc.with_ymd_and_hms(2026, 9, 1, 18, 0, 0).unwrap(),
    );

    assert_eq!(
        quotas
            .iter()
            .map(|quota| (quota.id.as_str(), quota.used_percent))
            .collect::<Vec<_>>(),
        [("fable", 0.0)]
    );
    assert!(quotas.iter().all(|quota| quota.resets_at.is_none()));
}

#[test]
fn codex_refresh_omits_missing_quotas() {
    let response = UsageResponse {
        status: StatusCode::OK,
        headers: Default::default(),
        body: serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 38,
                    "limit_window_seconds": 604800,
                    "reset_at": 1787810131
                }
            }
        }),
    };
    let now = Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap();
    let mapped = map_usage(&response, None, now).unwrap();

    let quotas = normalize_refreshed_quotas(super::Sub2ApiUpstream::Codex, mapped.quotas, now);

    assert_eq!(
        quotas
            .iter()
            .map(|quota| (quota.id.as_str(), quota.used_percent))
            .collect::<Vec<_>>(),
        [("weekly", 38.0)]
    );
}

#[test]
fn metric_template_runs_once_per_version_or_upstream_change() {
    let mut config = StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://sub2api.example.com".into(),
        codex_provider: String::new(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "secret".into(),
        upstream: super::Sub2ApiUpstream::Codex,
        metric_template_version: 0,
    };

    assert!(should_apply_metric_template(
        Some(&config),
        super::Sub2ApiMode::Admin,
        super::Sub2ApiUpstream::Codex
    ));
    config.metric_template_version = super::METRIC_TEMPLATE_VERSION;
    assert!(!should_apply_metric_template(
        Some(&config),
        super::Sub2ApiMode::Admin,
        super::Sub2ApiUpstream::Codex
    ));
    assert!(should_apply_metric_template(
        Some(&config),
        super::Sub2ApiMode::Admin,
        super::Sub2ApiUpstream::Claude
    ));
    assert!(should_apply_metric_template(
        None,
        super::Sub2ApiMode::Admin,
        super::Sub2ApiUpstream::Claude
    ));
    assert!(should_apply_metric_template(
        Some(&config),
        super::Sub2ApiMode::Subscription,
        config.upstream
    ));
    config.mode = super::Sub2ApiMode::Subscription;
    assert!(!should_apply_metric_template(
        Some(&config),
        config.mode,
        super::Sub2ApiUpstream::Claude
    ));
}

#[test]
fn cached_sessions_are_scoped_to_the_saved_password() {
    let mut config = StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://sub2api.example.com".into(),
        codex_provider: String::new(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "first-password".into(),
        upstream: super::Sub2ApiUpstream::Codex,
        metric_template_version: super::METRIC_TEMPLATE_VERSION,
    };
    let first = session_scope(&config);

    config.password = "second-password".into();

    assert_ne!(session_scope(&config), first);
}

#[test]
fn provider_slots_have_independent_ids_metrics_and_credentials() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    let providers = Sub2ApiProviders::new(directory.path().join("markers"), storage);
    let first = providers.provider("sub2api").unwrap();
    let second = providers.provider("sub2api@2").unwrap();

    assert_eq!(first.definition().metrics[0].id, "sub2api.session");
    assert_eq!(
        second.definition().display_name,
        default_display_name_for_slot("sub2api@2")
    );
    assert_eq!(second.definition().metrics[0].id, "sub2api@2.session");
    assert_eq!(first.credential_account(), "connection");
    assert_eq!(second.credential_account(), "sub2api@2");
}

#[test]
fn unconfigured_slot_names_are_generated_from_the_slot_id() {
    assert_eq!(default_display_name_for_slot("sub2api"), "Sub2API");
    assert_eq!(
        default_display_name_for_slot("sub2api@2"),
        format!("Sub2API {}", 2)
    );
}

#[test]
fn only_configured_slots_expose_account_names() {
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api@2".into(),
        default_display_name_for_slot("sub2api@2"),
        directory.path().join("sub2api@2.configured"),
        false,
    );

    assert!(!provider.has_local_credentials());
    assert!(provider.account_identity().is_none());

    let config = admin_config(
        "https://sub2api.example".into(),
        super::Sub2ApiUpstream::Grok,
    );
    provider.set_configuration_marker(Some(&config)).unwrap();
    assert!(provider.has_local_credentials());
    assert!(provider.supports_account_names());
    assert_eq!(provider.account_identity(), Some("sub2api@2"));

    provider.set_configuration_marker(None).unwrap();
    assert!(provider.account_identity().is_none());
}

#[test]
fn the_track_survives_restarts_without_reading_the_credential_store() {
    let directory = tempdir().unwrap();
    let marker = directory.path().join("sub2api@2.configured");
    let slot = |configured| {
        Sub2ApiProvider::new(
            "sub2api@2".into(),
            default_display_name_for_slot("sub2api@2"),
            marker.clone(),
            configured,
        )
    };
    let provider = slot(false);
    assert_eq!(provider.upstream_provider_id(), None);

    let config = admin_config(
        "https://sub2api.example".into(),
        super::Sub2ApiUpstream::Antigravity,
    );
    provider.set_configuration_marker(Some(&config)).unwrap();
    assert_eq!(provider.upstream_provider_id(), Some("antigravity"));
    assert_eq!(
        provider.cache_identity(),
        crate::providers::CacheIdentity::Resolved(super::ADMIN_CACHE_KEY)
    );
    assert_eq!(slot(true).upstream_provider_id(), Some("antigravity"));

    // Markers written before tracks were recorded catch up on the next credential read.
    fs::write(&marker, super::ADMIN_CACHE_KEY).unwrap();
    let legacy = slot(true);
    assert_eq!(legacy.upstream_provider_id(), None);
    legacy.remember_track(&config);
    assert_eq!(legacy.upstream_provider_id(), Some("antigravity"));
    assert_eq!(slot(true).upstream_provider_id(), Some("antigravity"));

    // A subscription's cache key already names its track.
    fs::write(&marker, super::SUBSCRIPTION_CACHE_KEY).unwrap();
    let subscription = slot(true);
    assert_eq!(subscription.upstream_provider_id(), Some("subscription"));
    assert_eq!(
        subscription.cache_identity(),
        crate::providers::CacheIdentity::Resolved(super::SUBSCRIPTION_CACHE_KEY)
    );

    // A refresh that read the configuration before a save or removal changes nothing.
    let grok = admin_config(
        "https://sub2api.example".into(),
        super::Sub2ApiUpstream::Grok,
    );
    provider.set_configuration_marker(Some(&grok)).unwrap();
    provider.remember_track(&config);
    assert_eq!(provider.upstream_provider_id(), Some("grok"));
    assert_eq!(slot(true).upstream_provider_id(), Some("grok"));
    provider.set_configuration_marker(None).unwrap();
    provider.remember_track(&grok);
    assert!(!marker.exists());

    provider.set_configuration_marker(None).unwrap();
    assert_eq!(provider.upstream_provider_id(), None);
    assert_eq!(slot(false).upstream_provider_id(), None);
}

#[test]
fn existing_snapshots_migrate_once_without_resurrecting_deleted_markers() {
    let directory = tempdir().unwrap();
    let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
    storage
        .save_snapshot(&ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: "sub2api".into(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        })
        .unwrap();
    let markers = directory.path().join("markers");
    let first = Sub2ApiProviders::new(markers.clone(), storage.clone());
    assert!(first.provider("sub2api").unwrap().has_local_credentials());
    assert!(markers.join("sub2api.configured").is_file());

    std::fs::remove_file(markers.join("sub2api.configured")).unwrap();
    let restarted = Sub2ApiProviders::new(markers, storage);
    assert!(!restarted
        .provider("sub2api")
        .unwrap()
        .has_local_credentials());
}

#[test]
fn old_admin_billing_markers_never_restore_mixed_price_snapshots() {
    let directory = tempdir().unwrap();
    let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
    storage
        .save_snapshot(&ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: "sub2api@2".into(),
            plan: Some("Old plan".into()),
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: Default::default(),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        })
        .unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api@2".into(),
        default_display_name_for_slot("sub2api@2"),
        directory.path().join("sub2api@2.configured"),
        false,
    );

    assert!(storage
        .load_snapshot_for_identity("sub2api@2", provider.cache_identity())
        .unwrap()
        .is_none());
    let old_snapshot = storage
        .load_snapshot_for_identity("sub2api@2", crate::providers::CacheIdentity::Unscoped)
        .unwrap()
        .unwrap();
    for key in [
        "sub2api-upstream-price-v1",
        "sub2api-upstream-price-v2",
        "sub2api-server-price-v1",
    ] {
        storage
            .save_snapshot_for_identity(&old_snapshot, Some(key))
            .unwrap();
        provider.write_configuration_marker(key, None).unwrap();
        provider
            .configured
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            provider.cache_identity(),
            crate::providers::CacheIdentity::Resolved(super::ADMIN_CACHE_KEY)
        );
        assert!(storage
            .load_snapshot_for_identity("sub2api@2", provider.cache_identity())
            .unwrap()
            .is_none());
    }
}

#[test]
fn switching_modes_preserves_both_profiles_in_one_credential_record() {
    use super::{StoredConnections, Sub2ApiMode};
    let legacy = r#"{"base_url":"https://admin.example","email":"admin@example.com","password":"test-password","upstream":"claude","billing_mode":"server"}"#;
    let admin: StoredConnections = serde_json::from_str(legacy).unwrap();
    let subscription: StoredConfig = serde_json::from_str(r#"{"mode":"subscription","base_url":"https://user.example","api_key":"test-user-key","email":"","password":""}"#).unwrap();
    let saved = StoredConnections::with_active(subscription, Some(admin));
    let saved: StoredConnections =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    let admin = saved.for_mode(Sub2ApiMode::Admin).unwrap().clone();
    assert_eq!(admin.password, "test-password");
    assert_eq!(admin.email, "admin@example.com");
    assert_eq!(admin.upstream, super::Sub2ApiUpstream::Claude);
    assert_eq!(admin.cache_key(), super::ADMIN_CACHE_KEY);
    let active = StoredConnections::with_active(admin, Some(saved));
    let saved = StoredConnections::with_active(active.active.clone(), Some(active));
    assert_eq!(
        saved.for_mode(Sub2ApiMode::Subscription).unwrap().api_key,
        "test-user-key"
    );
    let state = saved.state();
    assert_eq!(state.mode, Sub2ApiMode::Admin);
    assert_eq!(
        state.other_mode.as_ref().unwrap().base_url,
        "https://user.example"
    );
    let public = serde_json::to_string(&state).unwrap();
    assert!(!public.contains("test-user-key") && !public.contains("test-password"));
}

#[test]
fn relay_authentication_error_is_distinct_from_rejected_admin_login() {
    let (url, _requests, worker) = serve_sequence_with_status(vec![
        (
            401,
            r#"{"error":{"message":"An API Bearer token is required."}}"#.into(),
        ),
        (401, r#"{"message":"Login failed"}"#.into()),
    ]);
    let client = Sub2ApiClient::new(&url).unwrap();
    assert!(matches!(
        client.login("admin@example.com", "secret"),
        Err(Sub2ApiError::RelayRequiresKey)
    ));
    assert!(matches!(
        client.login("admin@example.com", "secret"),
        Err(Sub2ApiError::LoginRejected)
    ));
    worker.join().unwrap();
}

#[test]
fn subscription_reads_key_usage_and_caches_its_advertised_model_families() {
    let day = chrono::Local::now().date_naive();
    let body = serde_json::json!({
        "planName": "Example plan",
        "subscription": {
            "daily_usage_usd": 2.5, "daily_limit_usd": 10,
            "weekly_usage_usd": 0, "weekly_limit_usd": 50,
            "monthly_usage_usd": 11, "monthly_limit_usd": null,
            "expires_at": "2028-01-01T00:00:00Z"
        },
        "daily_usage": [{"date": day, "total_tokens": 120, "actual_cost": 0, "cost": 4}]
    });
    let (base, requests, worker) = serve_sequence(vec![
        body.to_string(),
        r#"{"data":[{"id":"gpt-test"},{"id":"o3-mini"}]}"#.into(),
        body.to_string(),
    ]);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api".into(),
        "Sub2API".into(),
        directory.path().join("marker"),
        false,
    );
    let config: StoredConfig = serde_json::from_value(serde_json::json!({
        "base_url": format!("{base}/relay/v1"), "mode": "subscription", "api_key": "user-test-key", "email": "", "password": ""
    })).unwrap();
    let snapshot = provider.refresh_snapshot(&config).unwrap();
    let again = provider.refresh_snapshot(&config).unwrap();
    assert_eq!(again.upstream_provider_id, snapshot.upstream_provider_id);
    worker.join().unwrap();
    let request = requests.recv().unwrap();
    assert!(request.starts_with("GET /relay/v1/usage?"));
    assert!(request
        .to_ascii_lowercase()
        .contains("authorization: bearer user-test-key"));
    assert!(requests
        .recv()
        .unwrap()
        .starts_with("GET /relay/v1/models "));
    assert!(requests.recv().unwrap().starts_with("GET /relay/v1/usage?"));
    assert_eq!(snapshot.upstream_provider_id.as_deref(), Some("codex"));
    assert_eq!(snapshot.plan.as_deref(), Some("Example plan"));
    assert!(snapshot.accounts.is_none());
    assert_eq!(snapshot.quotas.len(), 3);
    assert!(snapshot.value_metrics.is_empty());
    assert_eq!(
        snapshot.quotas[2].format,
        crate::models::QuotaFormat::UnlimitedDollars
    );
    assert_eq!(snapshot.quotas[2].used_value, Some(11.0));
    assert_eq!(snapshot.quotas[2].limit_value, None);
    assert_eq!(
        snapshot.quotas[0].format,
        crate::models::QuotaFormat::Percent
    );
    assert_eq!(snapshot.quotas[0].used_percent, 25.0);
    assert_eq!(snapshot.quotas[1].used_value, Some(0.0));
    assert!(snapshot.status_metrics.is_empty());
    assert_eq!(snapshot.quotas[2].label, "Monthly");
    let mut cached = snapshot.clone();
    cached.quotas[2].label = "Monthly quota".into();
    cached.quotas[0].format = crate::models::QuotaFormat::Dollars;
    super::restore_subscription_windows(&mut cached);
    assert_eq!(cached.quotas, snapshot.quotas);
    let today = snapshot.usage_histories.account.unwrap().today.unwrap();
    assert_eq!(today.tokens, 120);
    assert_eq!(today.estimated_cost_usd, Some(0.0));
    assert!(!serde_json::to_string(&config.state())
        .unwrap()
        .contains("user-test-key"));
    assert!(!provider.cache_path().exists());
}

#[test]
fn antigravity_and_grok_subscriptions_are_marked_by_their_chosen_upstream() {
    let body = serde_json::json!({
        "planName": "antigravity",
        "subscription": {"weekly_usage_usd": 0, "weekly_limit_usd": 0},
        "daily_usage": []
    });
    for upstream in ["antigravity", "grok"] {
        // Only the usage request is served: the mixed catalog is never requested.
        let (base, requests, worker) = serve_sequence(vec![body.to_string()]);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api".into(),
            "Sub2API".into(),
            directory.path().join("marker"),
            false,
        );
        let config: StoredConfig = serde_json::from_value(serde_json::json!({
            "base_url": base, "mode": "subscription", "api_key": "user-test-key",
            "email": "", "password": "", "upstream": upstream, "custom_base_url": true
        }))
        .unwrap();
        let snapshot = provider.refresh_snapshot(&config).unwrap();
        worker.join().unwrap();

        assert_eq!(snapshot.upstream_provider_id.as_deref(), Some(upstream));
        assert!(requests.recv().unwrap().starts_with("GET /v1/usage?"));
        assert!(requests.try_recv().is_err());
        let state = config.state();
        assert_eq!(state.upstream.provider_id(), upstream);
        assert!(state.custom_base_url);
    }
}

#[test]
fn empty_upstream_list_returns_no_account_cards() {
    let responses = vec![
        r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600}}"#.into(),
        r#"{"code":0,"message":"success","data":{"items":[],"total":0,"page":1,"page_size":1,"pages":0}}"#.into(),
    ];
    let (base_url, _requests, worker) = serve_sequence(responses);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api@2".into(),
        default_display_name_for_slot("sub2api@2"),
        directory.path().join("sub2api@2.configured"),
        false,
    );
    let config = StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url,
        codex_provider: String::new(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "secret-password".into(),
        upstream: super::Sub2ApiUpstream::Codex,
        metric_template_version: super::METRIC_TEMPLATE_VERSION,
    };

    assert!(provider
        .refresh_snapshot(&config)
        .unwrap()
        .accounts
        .unwrap()
        .is_empty());
    worker.join().unwrap();
}

#[test]
fn base_url_accepts_http_and_strips_the_api_prefix() {
    assert_eq!(
        normalize_base_url(" https://quota.example.test/api/v1/ ")
            .unwrap()
            .as_str(),
        "https://quota.example.test/"
    );
    assert!(normalize_base_url("file:///tmp/sub2api").is_err());
    assert!(normalize_base_url("https://user:secret@example.test").is_err());
}

#[test]
fn codex_provider_urls_are_converted_from_openai_to_instance_roots() {
    assert_eq!(
        normalized_codex_provider_base_url_text("https://quota.example.test/v1/").unwrap(),
        "https://quota.example.test"
    );
    assert_eq!(
        normalized_codex_provider_base_url_text("https://quota.example.test/sub2/v1").unwrap(),
        "https://quota.example.test/sub2"
    );
}

#[test]
fn codex_connection_targets_cannot_override_resolved_provider_urls() {
    let input = Sub2ApiConfigInput {
        provider_source: None,
        use_provider_key: false,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://ignored.example.com".into(),
        codex_provider: " exact-provider ".into(),
        custom_base_url: false,
        email: "admin@example.com".into(),
        password: "secret".into(),
        upstream: super::Sub2ApiUpstream::Codex,
    };

    let target = connection_target(
        &input,
        |provider| {
            assert_eq!(provider, "exact-provider");
            Ok("https://resolved.example.com".into())
        },
        |_| unreachable!(),
    )
    .unwrap();

    assert_eq!(
        target,
        (
            "https://resolved.example.com".into(),
            "exact-provider".into(),
            false
        )
    );
}

#[test]
fn changing_the_tracked_platform_preserves_the_provider_used_for_the_address() {
    let input: Sub2ApiConfigInput = serde_json::from_value(serde_json::json!({
        "mode":"admin","upstream":"claude","providerSource":"codex","codexProvider":"work",
        "baseUrl":"https://relay.example","email":"admin@example.com","password":""
    }))
    .unwrap();
    assert_eq!(
        connection_target(
            &input,
            |name| {
                assert_eq!(name, "work");
                Ok("https://relay.example".to_owned())
            },
            |_| unreachable!()
        )
        .unwrap()
        .0,
        "https://relay.example"
    );
}

#[test]
fn custom_codex_urls_reject_provider_names() {
    let input = Sub2ApiConfigInput {
        provider_source: None,
        use_provider_key: false,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://custom.example.com".into(),
        codex_provider: "provider-must-be-empty".into(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "secret".into(),
        upstream: super::Sub2ApiUpstream::Codex,
    };

    assert_eq!(
        connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
        Sub2ApiError::ProviderWithCustomBaseUrl
    );
}

#[test]
fn antigravity_and_grok_connections_always_use_an_entered_base_url() {
    for upstream in [
        super::Sub2ApiUpstream::Antigravity,
        super::Sub2ApiUpstream::Grok,
    ] {
        let mut input = Sub2ApiConfigInput {
            provider_source: None,
            use_provider_key: false,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://relay.example.com/api/v1/".into(),
            codex_provider: String::new(),
            custom_base_url: false,
            email: "admin@example.com".into(),
            password: "secret".into(),
            upstream,
        };
        assert_eq!(
            connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap(),
            ("https://relay.example.com".into(), String::new(), true)
        );
        input.codex_provider = "work".into();
        assert_eq!(
            connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
            Sub2ApiError::ProviderWithCustomBaseUrl
        );

        let config = admin_config("https://relay.example.com".into(), upstream);
        let stored: StoredConfig = serde_json::from_value(serde_json::json!({
            "base_url": config.base_url,
            "email": config.email,
            "password": config.password,
            "upstream": upstream.provider_id(),
            "custom_base_url": false
        }))
        .unwrap();
        assert_eq!(stored.upstream, upstream);
        assert!(stored.state().custom_base_url);

        // The instance address can still come from a Codex or Claude provider.
        input.provider_source = Some(super::Sub2ApiUpstream::Codex);
        assert_eq!(
            connection_target(
                &input,
                |name| Ok(format!("https://{name}.example.com")),
                |_| unreachable!()
            )
            .unwrap(),
            ("https://work.example.com".into(), "work".into(), false)
        );
        let mut stored = admin_config("https://work.example.com".into(), upstream);
        stored.custom_base_url = false;
        stored.codex_provider = "work".into();
        stored.provider_source = Some(super::Sub2ApiUpstream::Codex);
        assert!(!stored.state().custom_base_url);
    }
}

#[test]
fn legacy_claude_connections_expose_custom_mode_without_changing_named_providers() {
    let mut config: StoredConfig = serde_json::from_str(
        r#"{"base_url":"https://saved.example.com","email":"admin@example.com","password":"secret","upstream":"claude","custom_base_url":false}"#,
    ).unwrap();
    let state = config.state();
    assert!(state.custom_base_url);
    assert_eq!(state.base_url, "https://saved.example.com");
    config.codex_provider = "work".into();
    assert!(!config.state().custom_base_url);
    config.codex_provider.clear();
    config.upstream = super::Sub2ApiUpstream::Codex;
    assert!(!config.state().custom_base_url);
}

#[test]
fn the_state_shows_what_a_save_must_match_to_keep_the_secret() {
    let config =
        |value: serde_json::Value| -> StoredConfig { serde_json::from_value(value).unwrap() };
    let scope = |value: serde_json::Value| {
        serde_json::to_value(config(value).state().reusable_credential).unwrap()
    };
    assert_eq!(
        scope(serde_json::json!({
            "mode":"admin", "base_url":"https://relay.example", "upstream":"claude",
            "email":"admin@example.com", "password":"secret"
        })),
        serde_json::json!({"baseUrl":"https://relay.example"})
    );
    assert_eq!(
        scope(serde_json::json!({
            "mode":"subscription", "api_key":"saved", "codex_provider":"relay",
            "base_url":"https://relay.example", "custom_base_url":false, "email":"", "password":""
        })),
        serde_json::json!({
            "baseUrl":"https://relay.example", "customBaseUrl":false,
            "providerSource":"codex", "codexProvider":"relay"
        })
    );
    assert_eq!(
        scope(serde_json::json!({
            "mode":"subscription", "api_key":"saved", "upstream":"grok",
            "base_url":"https://relay.example", "custom_base_url":true, "email":"", "password":""
        })),
        serde_json::json!({
            "baseUrl":"https://relay.example", "customBaseUrl":true, "codexProvider":""
        })
    );
    // A subscription that reads the provider's key has no secret of its own to keep.
    assert_eq!(
        scope(serde_json::json!({
            "mode":"subscription", "codex_provider":"relay",
            "base_url":"https://relay.example", "custom_base_url":false, "email":"", "password":""
        })),
        serde_json::Value::Null
    );
}

#[test]
fn an_older_claude_subscription_keeps_its_key_at_the_address_the_editor_shows() {
    // Saved as a named connection without a name, so the editor shows an entered address.
    let previous: StoredConfig = serde_json::from_value(serde_json::json!({
        "mode":"subscription", "api_key":"saved", "upstream":"claude",
        "base_url":"https://relay.example", "custom_base_url":false, "email":"", "password":""
    }))
    .unwrap();
    assert!(previous.state().custom_base_url);
    let input: Sub2ApiConfigInput = serde_json::from_value(serde_json::json!({
        "mode":"subscription", "upstream":"claude", "baseUrl":"https://relay.example",
        "customBaseUrl":true, "codexProvider":"", "email":"", "password":""
    }))
    .unwrap();
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        "saved"
    );
}

#[test]
fn named_subscription_prefers_entered_then_saved_key_and_defaults_to_provider() {
    let previous: StoredConfig = serde_json::from_value(serde_json::json!({
        "mode":"subscription", "api_key":"saved", "codex_provider":"relay",
        "base_url":"https://relay.example", "custom_base_url":false, "email":"", "password":""
    }))
    .unwrap();
    let mut input: Sub2ApiConfigInput = serde_json::from_value(serde_json::json!({
        "mode":"subscription", "codexProvider":"relay", "baseUrl":"https://relay.example", "email":"", "password":""
    })).unwrap();
    assert_eq!(
        credential_for_save(&input, &input.base_url, None).unwrap(),
        ""
    );
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        "saved"
    );
    input.upstream = super::Sub2ApiUpstream::Grok;
    input.provider_source = Some(super::Sub2ApiUpstream::Codex);
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        "saved"
    );
    input.provider_source = Some(super::Sub2ApiUpstream::Claude);
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        ""
    );
    input.provider_source = Some(super::Sub2ApiUpstream::Codex);
    input.use_provider_key = true;
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        ""
    );
    input.api_key = "  override  ".into();
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        "override"
    );
    input.api_key.clear();
    input.use_provider_key = false;
    input.codex_provider = "another-provider".into();
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        ""
    );
}

#[test]
fn an_entered_subscription_key_follows_a_changed_upstream_mark() {
    let previous: StoredConfig = serde_json::from_value(serde_json::json!({
        "mode":"subscription", "api_key":"saved", "upstream":"codex",
        "base_url":"https://relay.example", "custom_base_url":true, "email":"", "password":""
    }))
    .unwrap();
    let mut input: Sub2ApiConfigInput = serde_json::from_value(serde_json::json!({
        "mode":"subscription", "upstream":"antigravity", "baseUrl":"https://relay.example",
        "customBaseUrl":true, "codexProvider":"", "email":"", "password":""
    }))
    .unwrap();
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        "saved"
    );
    // A key read with a named provider stays scoped to that provider's upstream.
    input.custom_base_url = false;
    input.upstream = super::Sub2ApiUpstream::Claude;
    input.codex_provider = "relay".into();
    assert_eq!(
        credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
        ""
    );
}

#[test]
fn admin_password_follows_the_instance_while_subscription_keys_stay_scoped() {
    let previous = StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://claude.example.com".into(),
        codex_provider: String::new(),
        custom_base_url: true,
        email: "old@example.com".into(),
        password: "old-password".into(),
        upstream: super::Sub2ApiUpstream::Claude,
        metric_template_version: super::METRIC_TEMPLATE_VERSION,
    };
    let mut same_upstream = Sub2ApiConfigInput {
        provider_source: None,
        use_provider_key: false,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://claude.example.com".into(),
        codex_provider: String::new(),
        custom_base_url: true,
        email: "new@example.com".into(),
        password: String::new(),
        upstream: super::Sub2ApiUpstream::Claude,
    };
    assert_eq!(
        credential_for_save(&same_upstream, &same_upstream.base_url, Some(&previous)).unwrap(),
        "old-password"
    );

    assert_eq!(
        credential_for_save(&same_upstream, "https://other.example", Some(&previous)),
        Err(Sub2ApiError::InvalidConfig)
    );
    same_upstream.upstream = super::Sub2ApiUpstream::Codex;
    assert_eq!(
        credential_for_save(&same_upstream, &previous.base_url, Some(&previous)).unwrap(),
        "old-password"
    );
    same_upstream.mode = super::Sub2ApiMode::Subscription;
    assert_eq!(
        credential_for_save(&same_upstream, &previous.base_url, Some(&previous)),
        Err(Sub2ApiError::MissingApiKey)
    );

    let changed_upstream = Sub2ApiConfigInput {
        provider_source: None,
        use_provider_key: false,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://codex.example.com".into(),
        codex_provider: String::new(),
        custom_base_url: true,
        email: "new@example.com".into(),
        password: String::new(),
        upstream: super::Sub2ApiUpstream::Codex,
    };
    assert_eq!(
        credential_for_save(
            &changed_upstream,
            &changed_upstream.base_url,
            Some(&previous)
        ),
        Err(Sub2ApiError::InvalidConfig)
    );
}

#[test]
fn claude_provider_is_required_and_resolved_again_on_save() {
    let mut input = Sub2ApiConfigInput {
        provider_source: None,
        use_provider_key: false,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url: "https://ignored.example.com".into(),
        codex_provider: " work ".into(),
        custom_base_url: false,
        email: "admin@example.com".into(),
        password: "secret".into(),
        upstream: super::Sub2ApiUpstream::Claude,
    };
    assert_eq!(
        connection_target(
            &input,
            |_| unreachable!(),
            |name| {
                assert_eq!(name, "work");
                Ok("https://selected.example.com".into())
            }
        )
        .unwrap(),
        ("https://selected.example.com".into(), "work".into(), false)
    );
    input.codex_provider.clear();
    assert_eq!(
        connection_target(
            &input,
            |_| unreachable!(),
            |_| Err(super::ClaudeConfigError::MissingName.into())
        )
        .unwrap_err(),
        Sub2ApiError::ClaudeConfig(super::ClaudeConfigError::MissingName)
    );
    input.custom_base_url = true;
    input.base_url = "   ".into();
    assert_eq!(
        connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
        Sub2ApiError::InvalidConfig
    );
    input.base_url = "https://manual.example.com/api/v1".into();
    assert_eq!(
        connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap(),
        ("https://manual.example.com".into(), String::new(), true)
    );
    input.codex_provider = "work".into();
    assert_eq!(
        connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
        Sub2ApiError::ProviderWithCustomBaseUrl
    );
}

#[test]
fn discovery_reads_every_page_and_rejects_non_advancing_pages() {
    for repeated in [false, true] {
        let second = if repeated { 1 } else { 2 };
        let (base_url, requests, worker) = serve_sequence(vec![
            r#"{"code":0,"data":{"items":[{"id":1,"name":"First"}],"total":2}}"#.into(),
            format!(
                r#"{{"code":0,"data":{{"items":[{{"id":{second},"name":"Second"}}],"total":2}}}}"#
            ),
        ]);
        let result = Sub2ApiClient::new(&base_url)
            .unwrap()
            .accounts("token", super::Sub2ApiUpstream::Codex);
        if repeated {
            assert!(matches!(result, Err(Sub2ApiError::InvalidResponse)));
        } else {
            assert_eq!(
                result.unwrap().iter().map(|a| a.id).collect::<Vec<_>>(),
                [1, 2]
            );
        }
        worker.join().unwrap();
        assert!(requests.recv().unwrap().contains("page=1&page_size=100"));
        assert!(requests.recv().unwrap().contains("page=2&page_size=100"));
    }
}

#[test]
fn refresh_keeps_all_accounts_and_rediscovers_with_a_cached_token() {
    let now = Utc::now();
    let quota = format!(r#"{{"code":0,"data":{CLAUDE_USAGE_DATA}}}"#);
    let (base_url, _requests, worker) = serve_sequence(vec![
        r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
        r#"{"code":0,"data":{"items":[{"id":7,"name":"Same name"},{"id":8,"name":"Same name"}],"total":2}}"#.into(),
        // All quotas come first so they can be shown before usage history loads.
        quota.clone(), quota, usage_data(7, now), usage_data(8, now),
        r#"{"code":0,"data":{"items":[{"id":9,"name":"Unavailable"}],"total":1}}"#.into(),
        r#"{"code":1}"#.into(),
        r#"{"code":0,"data":{"items":[],"total":0}}"#.into(),
    ]);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api".into(),
        "Sub2API".into(),
        directory.path().join("configured"),
        true,
    );
    let config = StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url,
        codex_provider: String::new(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "password".into(),
        upstream: super::Sub2ApiUpstream::Claude,
        metric_template_version: super::METRIC_TEMPLATE_VERSION,
    };
    let snapshot = provider.refresh_snapshot(&config).unwrap();
    let accounts = snapshot.accounts.unwrap();
    assert_eq!(
        accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        ["7", "8"]
    );
    assert!(accounts
        .iter()
        .all(|a| a.name == "Sub2API · Claude · Same name" && !a.snapshot.quotas.is_empty()));
    let history = snapshot.usage_histories.account.unwrap();
    assert_eq!(history.daily.len(), 2);
    assert_eq!(history.last_30_days.as_ref().unwrap().tokens, 7000);
    assert_eq!(
        history
            .last_30_days
            .unwrap()
            .model_breakdown
            .unwrap()
            .models
            .len(),
        2
    );
    assert_eq!(
        provider.refresh_snapshot(&config).unwrap_err(),
        Sub2ApiError::InvalidResponse
    );
    assert!(provider
        .refresh_snapshot(&config)
        .unwrap()
        .accounts
        .unwrap()
        .is_empty());
    worker.join().unwrap();
}

#[test]
fn quotas_of_every_account_are_previewed_before_any_usage_history_request() {
    let now = Utc::now();
    let quota = format!(r#"{{"code":0,"data":{CLAUDE_USAGE_DATA}}}"#);
    let (base_url, requests, worker) = serve_sequence(vec![
        r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
        r#"{"code":0,"data":{"items":[{"id":7,"name":"First"},{"id":8,"name":"Second"}],"total":2}}"#.into(),
        quota.clone(), quota, usage_data(7, now), usage_data(8, now),
    ]);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api".into(),
        "Sub2API".into(),
        directory.path().join("configured"),
        true,
    );
    let config = StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url,
        codex_provider: String::new(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "password".into(),
        upstream: super::Sub2ApiUpstream::Claude,
        metric_template_version: super::METRIC_TEMPLATE_VERSION,
    };
    let previews = std::sync::Mutex::new(Vec::new());
    let snapshot = provider
        .refresh_snapshot_with_preview(&config, &|preview| {
            let sent: Vec<String> = requests.try_iter().collect();
            assert!(sent.iter().all(|request| !request.contains("/admin/usage")));
            previews.lock().unwrap().push(preview);
        })
        .unwrap();
    worker.join().unwrap();

    let previews = previews.into_inner().unwrap();
    assert_eq!(previews.len(), 1);
    let accounts = previews[0].accounts.as_ref().unwrap();
    assert_eq!(
        accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
        ["7", "8"]
    );
    assert!(accounts.iter().all(|a| !a.snapshot.quotas.is_empty()
        && a.snapshot.usage_histories.account == Some(super::UsageHistory::default())));
    assert!(accounts.iter().all(|a| a.snapshot.notices.is_empty()));
    assert!(snapshot.notices.is_empty());
    assert_eq!(
        snapshot
            .usage_histories
            .account
            .unwrap()
            .last_30_days
            .unwrap()
            .tokens,
        7000
    );
}

fn admin_config(base_url: String, upstream: super::Sub2ApiUpstream) -> StoredConfig {
    StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url,
        codex_provider: String::new(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "password".into(),
        upstream,
        metric_template_version: super::METRIC_TEMPLATE_VERSION,
    }
}

#[test]
fn antigravity_accounts_show_both_pools_and_keep_unusable_accounts() {
    let now = Utc::now();
    let five_hours = (now + chrono::Duration::hours(3)).to_rfc3339();
    let week = (now + chrono::Duration::days(3)).to_rfc3339();
    let quota = serde_json::json!({"code":0,"data":{
        "updated_at": now.to_rfc3339(),
        "subscription_tier": "PRO",
        "antigravity_quota_groups": [
            {"key":"gemini","windows":[
                {"window":"5h","utilization":20,"resets_at":five_hours},
                {"window":"weekly","utilization":5,"resets_at":week}]},
            {"key":"3p","windows":[
                {"window":"5h","utilization":40,"resets_at":five_hours},
                {"window":"weekly","utilization":52,"resets_at":week}]}
        ]
    }})
    .to_string();
    let (base_url, requests, worker) = serve_sequence(vec![
        r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
        r#"{"code":0,"data":{"items":[{"id":6,"name":"agy"},{"id":9,"name":"banned"}],"total":2}}"#.into(),
        quota,
        r#"{"code":0,"data":{"is_forbidden":true,"forbidden_reason":"Google disabled the account"}}"#.into(),
        usage_data(6, now), usage_data(9, now),
    ]);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api".into(),
        "Sub2API".into(),
        directory.path().join("configured"),
        true,
    );
    let config = admin_config(base_url, super::Sub2ApiUpstream::Antigravity);
    let snapshot = provider.refresh_snapshot(&config).unwrap();
    worker.join().unwrap();

    let sent: Vec<String> = requests.try_iter().collect();
    assert!(sent[1].contains("platform=antigravity"));
    assert!(sent[2].starts_with("GET /api/v1/admin/accounts/6/usage "));
    assert!(sent[3].starts_with("GET /api/v1/admin/accounts/9/usage "));
    let accounts = snapshot.accounts.unwrap();
    assert_eq!(accounts[0].name, "Sub2API · Antigravity · agy");
    assert_eq!(accounts[0].snapshot.plan.as_deref(), Some("Pro"));
    assert_eq!(
        accounts[0]
            .snapshot
            .quotas
            .iter()
            .map(|quota| (quota.id.as_str(), quota.used_percent))
            .collect::<Vec<_>>(),
        [
            ("session", 20.0),
            ("weekly", 5.0),
            ("claude", 40.0),
            ("claudeWeekly", 52.0)
        ]
    );
    assert!(accounts[1].snapshot.quotas.is_empty());
    assert_eq!(
        accounts[1].snapshot.warnings,
        ["Google disabled the account"]
    );
    assert!(accounts[1].snapshot.usage_histories.account.is_some());
}

#[test]
fn grok_accounts_show_their_billing_period() {
    let now = Utc::now();
    let quota = serde_json::json!({"code":0,"data":{
        "seven_day": {"utilization": 88, "resets_at": (now + chrono::Duration::days(4)).to_rfc3339()},
        "grok_billing": {"period_type":"weekly","usage_percent":88}
    }})
    .to_string();
    let (base_url, requests, worker) = serve_sequence(vec![
        r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
        r#"{"code":0,"data":{"items":[{"id":7,"name":"grok"}],"total":1}}"#.into(),
        quota,
        usage_data(7, now),
    ]);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api".into(),
        "Sub2API".into(),
        directory.path().join("configured"),
        true,
    );
    let config = admin_config(base_url, super::Sub2ApiUpstream::Grok);
    let snapshot = provider.refresh_snapshot(&config).unwrap();
    worker.join().unwrap();

    let sent: Vec<String> = requests.try_iter().collect();
    assert!(sent[1].contains("platform=grok"));
    assert!(sent[2].starts_with("GET /api/v1/admin/accounts/7/usage "));
    let accounts = snapshot.accounts.unwrap();
    assert_eq!(accounts[0].name, "Sub2API · Grok · grok");
    let weekly = &accounts[0].snapshot.quotas;
    assert_eq!(weekly.len(), 1);
    assert_eq!(
        (weekly[0].id.as_str(), weekly[0].used_percent),
        ("weekly", 88.0)
    );
    assert!(weekly[0].resets_at.is_some());
}

#[tokio::test]
async fn failed_refresh_preserves_all_cached_accounts_on_disk_and_recovers() {
    use crate::{
        models::{ProviderDefinition, SnapshotSource},
        providers::{ProviderError, ProviderRegistry},
        service::ProviderService,
    };

    struct ConfiguredProvider {
        provider: Sub2ApiProvider,
        config: StoredConfig,
    }
    impl UsageProvider for ConfiguredProvider {
        fn definition(&self) -> ProviderDefinition {
            let mut definition = self.provider.definition();
            definition.fallback_enabled = true;
            definition
        }
        fn has_local_credentials(&self) -> bool {
            true
        }
        fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
            self.provider
                .refresh_snapshot(&self.config)
                .map_err(ProviderError::from)
        }
    }

    // Discovery, a later account's quota, and history must all preserve the same cache.
    for failed_stage in 0..3 {
        let page = r#"{"code":0,"data":{"items":[{"id":7,"name":"First"},{"id":8,"name":"Second"}],"total":2}}"#.to_owned();
        let quota = format!(r#"{{"code":0,"data":{CLAUDE_USAGE_DATA}}}"#);
        let now = Utc::now();
        let mut responses = vec![
            (
                200,
                r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
            ),
            (200, page.clone()),
            (200, quota.clone()),
            (200, quota.clone()),
            (200, usage_data(7, now)),
        ];

        responses.push((200, usage_data(8, now)));

        if failed_stage > 0 {
            responses.extend([(200, page), (200, quota.clone())]);
            if failed_stage == 2 {
                responses.extend([(200, quota.clone()), (200, usage_data(7, now))]);
            }
        }
        responses.push((503, "temporary outage".into()));
        responses.extend([
            (
                200,
                r#"{"code":0,"data":{"items":[{"id":8,"name":"Recovered"}],"total":1}}"#.into(),
            ),
            (200, quota),
            (200, usage_data(8, now)),
            (200, r#"{"code":0,"data":{"items":[],"total":0}}"#.into()),
        ]);
        let (base_url, _requests, worker) = serve_sequence_with_status(responses);
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("cache.db")).unwrap());
        let registry = Arc::new(
            ProviderRegistry::new(vec![Arc::new(ConfiguredProvider {
                provider: Sub2ApiProvider::new(
                    "sub2api".into(),
                    "Sub2API · Claude".into(),
                    directory.path().join("configured"),
                    true,
                ),
                config: StoredConfig {
                    provider_source: None,
                    mode: super::Sub2ApiMode::Admin,
                    api_key: String::new(),
                    base_url,
                    codex_provider: String::new(),
                    custom_base_url: true,
                    email: "admin@example.com".into(),
                    password: "password".into(),
                    upstream: super::Sub2ApiUpstream::Claude,
                    metric_template_version: super::METRIC_TEMPLATE_VERSION,
                },
            })])
            .unwrap(),
        );
        let service = Arc::new(ProviderService::new(registry.clone(), storage.clone()));
        let successful = service.refresh("sub2api", true).await;
        assert!(successful.error.is_none());
        let cached = successful.snapshot.unwrap();
        assert_eq!(cached.accounts.as_ref().unwrap().len(), 2);
        assert_eq!(
            cached
                .usage_histories
                .account
                .as_ref()
                .unwrap()
                .last_30_days
                .as_ref()
                .unwrap()
                .tokens,
            7000
        );

        let failed = service.refresh("sub2api", true).await;
        assert_eq!(
            failed.error_kind,
            Some(crate::models::ProviderErrorKind::Network)
        );
        assert_eq!(failed.snapshot.as_ref(), Some(&cached));
        drop(service);
        let restarted = Arc::new(ProviderService::new(registry, storage));
        let restored = restarted.state().providers.remove("sub2api").unwrap();
        assert_eq!(restored.source, SnapshotSource::Cache);
        assert_eq!(restored.snapshot.as_ref(), Some(&cached));

        let recovered = restarted.refresh("sub2api", true).await;
        assert!(recovered.error.is_none());
        assert_eq!(recovered.snapshot.unwrap().accounts.unwrap().len(), 1);
        let empty = restarted.refresh("sub2api", true).await;
        assert!(empty.error.is_none());
        assert!(empty.snapshot.unwrap().accounts.unwrap().is_empty());
        worker.join().unwrap();
    }
}

#[test]
fn login_discovers_one_codex_account_and_maps_its_quota() {
    let responses = vec![
        r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600,"token_type":"Bearer"}}"#.into(),
        r#"{"code":0,"message":"success","data":{"items":[{"id":1,"name":"Codex upstream"}],"total":1,"page":1,"page_size":1,"pages":1}}"#.into(),
        format!(r#"{{"code":0,"message":"success","data":{QUOTA_DATA}}}"#),
    ];
    let (base_url, requests, worker) = serve_sequence(responses);
    let client = Sub2ApiClient::new(&base_url).unwrap();
    let login = client
        .login("admin@example.com", "secret-password")
        .unwrap();
    let accounts = client
        .accounts(login.value.as_str(), super::Sub2ApiUpstream::Codex)
        .unwrap();
    let body = client
        .usage(
            login.value.as_str(),
            accounts[0].id,
            super::Sub2ApiUpstream::Codex,
        )
        .unwrap();
    worker.join().unwrap();

    assert_eq!(accounts[0].name, "Codex upstream");
    assert_eq!(accounts.len(), 1);
    let login_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(login_request.starts_with("POST /api/v1/auth/login HTTP/1.1"));
    assert!(login_request.contains("\"email\":\"admin@example.com\""));
    let accounts_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(accounts_request.starts_with("GET /api/v1/admin/accounts?"));
    assert!(accounts_request
        .to_ascii_lowercase()
        .contains("authorization: bearer admin-jwt"));
    let quota_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(quota_request.starts_with("GET /api/v1/admin/openai/accounts/1/quota HTTP/1.1"));

    let mapped = map_usage(
        &UsageResponse {
            status: StatusCode::OK,
            headers: Default::default(),
            body,
        },
        None,
        Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap(),
    )
    .unwrap();
    assert_eq!(mapped.plan.as_deref(), Some("Pro 5x"));
    assert_eq!(
        mapped
            .quotas
            .iter()
            .map(|quota| (quota.id.as_str(), quota.used_percent))
            .collect::<Vec<_>>(),
        [("weekly", 38.0), ("spark", 0.0), ("sparkWeekly", 0.0)]
    );
    assert_eq!(mapped.value_metrics[0].id, "rateLimitResets");
    assert_eq!(mapped.value_metrics[0].values[0].number, 1.0);
}

#[test]
fn discovers_a_claude_account_and_maps_its_usage() {
    let responses = vec![
        r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600}}"#.into(),
        r#"{"code":0,"message":"success","data":{"items":[{"id":1,"name":"Claude upstream"}],"total":1,"page":1,"page_size":1,"pages":1}}"#.into(),
        format!(r#"{{"code":0,"message":"success","data":{CLAUDE_USAGE_DATA}}}"#),
    ];
    let (base_url, requests, worker) = serve_sequence(responses);
    let client = Sub2ApiClient::new(&base_url).unwrap();
    let login = client
        .login("admin@example.com", "secret-password")
        .unwrap();
    let accounts = client
        .accounts(login.value.as_str(), super::Sub2ApiUpstream::Claude)
        .unwrap();
    let body = client
        .usage(
            login.value.as_str(),
            accounts[0].id,
            super::Sub2ApiUpstream::Claude,
        )
        .unwrap();
    worker.join().unwrap();

    assert_eq!(accounts[0].name, "Claude upstream");
    assert_eq!(accounts.len(), 1);
    let _login_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    let accounts_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(accounts_request.contains("platform=anthropic"));
    assert!(accounts_request.contains("type=oauth"));
    let usage_request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(usage_request.starts_with("GET /api/v1/admin/accounts/1/usage HTTP/1.1"));

    let mapped = crate::providers::claude::map_sub2api_usage(&body).unwrap();
    assert_eq!(mapped.plan.as_deref(), Some("Pro"));
    assert_eq!(
        mapped
            .quotas
            .iter()
            .map(|quota| (quota.id.as_str(), quota.used_percent))
            .collect::<Vec<_>>(),
        [
            ("session", 12.5),
            ("weekly", 34.0),
            ("sonnet", 8.0),
            ("fable", 3.0),
        ]
    );
}

#[test]
fn stats_endpoint_reads_only_the_summary_and_uses_standard_cost() {
    let now = Utc::now();
    let (base_url, requests, worker) = serve_sequence(vec![usage_data(42, now)]);
    let client = Sub2ApiClient::new(&base_url).unwrap();
    let history = client
        .stats("admin-jwt", 42)
        .unwrap()
        .into_standard_history(now);
    worker.join().unwrap();
    let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(request.starts_with("GET /api/v1/admin/accounts/42/stats?days=30 HTTP/1.1"));
    assert!(request
        .to_ascii_lowercase()
        .contains("authorization: bearer admin-jwt"));
    assert_eq!(history.today.unwrap().estimated_cost_usd, Some(2.0));
    let monthly = history.last_30_days.unwrap();
    assert_eq!(monthly.tokens, 3500);
    assert_eq!(monthly.estimated_cost_usd, Some(3.0));
    assert!(!monthly.cost_estimated);
    assert_eq!(
        monthly.model_breakdown.unwrap().models[0].model,
        "claude-sonnet-5"
    );
}

#[test]
fn repeated_refreshes_use_summaries_without_opening_the_old_detail_cache() {
    let now = Utc::now();
    let accounts =
        r#"{"code":0,"data":{"items":[{"id":10,"name":"upstream"}],"total":1}}"#.to_owned();
    let quota = format!(r#"{{"code":0,"data":{QUOTA_DATA}}}"#);
    let (base_url, requests, worker) = serve_sequence(vec![
        r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
        accounts.clone(),
        quota.clone(),
        usage_data(10, now),
        accounts,
        quota,
        usage_data(10, now),
    ]);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api".into(),
        "Codex".into(),
        directory.path().join("configured"),
        true,
    );
    let legacy = b"legacy detail cache must not be opened or changed";
    fs::write(provider.cache_path(), legacy).unwrap();
    let config = admin_config(base_url, super::Sub2ApiUpstream::Codex);
    for _ in 0..2 {
        let snapshot = provider.refresh_snapshot(&config).unwrap();
        assert_eq!(
            snapshot
                .usage_histories
                .account
                .unwrap()
                .last_30_days
                .unwrap()
                .estimated_cost_usd,
            Some(3.0)
        );
    }
    worker.join().unwrap();
    assert_eq!(fs::read(provider.cache_path()).unwrap(), legacy);
    let requests: Vec<_> = requests.try_iter().collect();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("GET /api/v1/admin/accounts/10/stats?days=30 "))
            .count(),
        2
    );
    assert!(requests
        .iter()
        .all(|request| !request.contains("/admin/usage")));
}

#[test]
fn stats_negotiate_gzip_and_accept_uncompressed_servers() {
    let body = usage_data(10, Utc::now());
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(body.as_bytes()).unwrap();
    let compressed = encoder.finish().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        for (encoding, bytes) in [
            ("Content-Encoding: gzip\r\n", compressed),
            ("", body.into_bytes()),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut chunk = [0; 2048];
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            assert!(String::from_utf8(request)
                .unwrap()
                .to_ascii_lowercase()
                .lines()
                .any(|line| line.starts_with("accept-encoding:") && line.contains("gzip")));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{encoding}Connection: close\r\n\r\n", bytes.len()).unwrap();
            stream.write_all(&bytes).unwrap();
        }
    });
    let client = Sub2ApiClient::new(&base).unwrap();
    let compressed = client.stats("test-token", 10).unwrap();
    let plain = client.stats("test-token", 10).unwrap();
    assert!(compressed == plain);
    worker.join().unwrap();
}

#[test]
fn claude_refresh_combines_quota_with_standard_cost_summary() {
    let now = Utc::now();
    let responses = vec![
        r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600}}"#.into(),
        r#"{"code":0,"message":"success","data":{"items":[{"id":7,"name":"Claude upstream"}],"total":1,"page":1,"page_size":1,"pages":1}}"#.into(),
        format!(r#"{{"code":0,"message":"success","data":{CLAUDE_USAGE_DATA}}}"#),
        usage_data(7, now),
    ];
    let (base_url, requests, worker) = serve_sequence(responses);
    let directory = tempdir().unwrap();
    let provider = Sub2ApiProvider::new(
        "sub2api@2".into(),
        default_display_name_for_slot("sub2api@2"),
        directory.path().join("sub2api@2.configured"),
        false,
    );
    let config = StoredConfig {
        provider_source: None,
        mode: super::Sub2ApiMode::Admin,
        api_key: String::new(),
        base_url,
        codex_provider: String::new(),
        custom_base_url: true,
        email: "admin@example.com".into(),
        password: "secret-password".into(),
        upstream: super::Sub2ApiUpstream::Claude,
        metric_template_version: super::METRIC_TEMPLATE_VERSION,
    };

    let snapshot = provider.refresh_snapshot(&config).unwrap();
    worker.join().unwrap();

    assert_eq!(
        snapshot
            .quotas
            .iter()
            .map(|quota| quota.id.as_str())
            .collect::<Vec<_>>(),
        ["session", "weekly", "sonnet", "fable"]
    );
    let history = snapshot.usage_histories.account.unwrap();
    assert_eq!(history.daily.len(), 2);
    assert_eq!(history.last_30_days.unwrap().tokens, 3500);
    let request_paths = (0..4)
        .map(|_| {
            requests
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(request_paths[3].starts_with("GET /api/v1/admin/accounts/7/stats?days=30 "));
}

#[test]
fn stored_connections_without_an_upstream_remain_codex_connections() {
    let config: StoredConfig = serde_json::from_str(
        r#"{"base_url":"https://sub2api.example.com","email":"admin@example.com","password":"secret"}"#,
    )
    .unwrap();

    assert_eq!(config.mode, super::Sub2ApiMode::Admin);
    assert_eq!(
        super::Sub2ApiConfigState::default().mode,
        super::Sub2ApiMode::Subscription
    );
    assert_eq!(config.upstream, super::Sub2ApiUpstream::Codex);
    assert_eq!(config.cache_key(), super::ADMIN_CACHE_KEY);
    assert!(config.codex_provider.is_empty());
    assert!(config.custom_base_url);
    assert_eq!(config.metric_template_version, 0);
}

#[test]
fn connection_errors_never_include_credentials() {
    let error = match Sub2ApiClient::new("not-a-url") {
        Ok(_) => panic!("invalid URL should be rejected"),
        Err(error) => error,
    };
    assert_eq!(error, Sub2ApiError::InvalidConfig);
    assert!(!error.to_string().contains("secret"));
}
