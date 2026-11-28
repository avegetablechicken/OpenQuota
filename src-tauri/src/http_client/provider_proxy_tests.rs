use std::time::Duration;

use crate::providers::test_http::serve_once;

use super::proxy_exit::{parse_proxy_exit, query_proxy_exit, PROXY_LOCATION_ERROR};
use super::{
    client_builder_for_proxy, validate_provider_proxy, validate_proxy_url, ProviderClient,
    DIRECT_PROXY,
};

#[test]
fn explicit_proxy_bypasses_loopback_but_routes_remote_requests() {
    let proxy = serve_once(200, &[], "remote-via-proxy");
    let client = client_builder_for_proxy(Some(&proxy))
        .unwrap()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    for hostname in ["127.0.0.1", "localhost"] {
        let local = serve_once(200, &[], "local-direct").replace("127.0.0.1", hostname);
        assert_eq!(
            client.get(local).send().unwrap().text().unwrap(),
            "local-direct"
        );
    }
    assert_eq!(
        client
            .get("http://unresolvable.invalid/usage")
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "remote-via-proxy"
    );
}

#[test]
fn codex_dotenv_proxy_does_not_affect_other_providers() {
    const CHILD: &str = "OPENQUOTA_TEST_CODEX_PROXY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "http_client::provider_proxy_tests::codex_dotenv_proxy_does_not_affect_other_providers",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CODEX_HOME", directory.path())
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("http_proxy", "http://127.0.0.1:9")
            // reqwest's non-macOS matcher treats wildcard domains separately from IPs.
            // Bypass the literal loopback address used by serve_once explicitly.
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let proxy = serve_once(200, &[], "codex-file-proxy");
    let directory = std::env::var_os("CODEX_HOME").unwrap();
    std::fs::write(
        std::path::Path::new(&directory).join(".env"),
        format!("HTTP_PROXY={proxy}\nhttp_proxy={proxy}\nNO_PROXY=\nno_proxy=\n"),
    )
    .unwrap();
    let codex =
        ProviderClient::new("codex", |builder| builder.timeout(Duration::from_secs(3))).unwrap();
    let response = codex
        .current_with_proxy(None)
        .unwrap()
        .get("http://unresolvable.invalid/usage")
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert_eq!(response, "codex-file-proxy");
    let claude = codex.for_provider("claude");
    let endpoint = serve_once(200, &[], "claude-direct");
    assert_eq!(
        claude
            .current_with_proxy(None)
            .unwrap()
            .get(endpoint)
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "claude-direct"
    );
    let claude_proxy = serve_once(200, &[], "claude-settings-proxy");
    let settings = std::path::Path::new(&directory).join("work-settings.json");
    std::fs::write(
        &settings,
        serde_json::json!({"env": {
            "HTTP_PROXY": claude_proxy, "http_proxy": claude_proxy, "NO_PROXY": "", "no_proxy": ""
        }})
        .to_string(),
    )
    .unwrap();
    let scoped = claude.for_provider("claude").with_claude_settings(settings);
    assert_eq!(
        scoped
            .current_with_proxy(None)
            .unwrap()
            .get("http://unresolvable.invalid/usage")
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "claude-settings-proxy"
    );
    let unrelated = scoped.for_provider("kimi");
    let endpoint = serve_once(200, &[], "kimi-direct");
    assert_eq!(
        unrelated
            .current_with_proxy(None)
            .unwrap()
            .get(endpoint)
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "kimi-direct"
    );
    let endpoint = serve_once(200, &[], "claude-explicit-direct");
    assert_eq!(
        scoped
            .current_with_proxy(Some(DIRECT_PROXY.into()))
            .unwrap()
            .get(endpoint)
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "claude-explicit-direct"
    );
    let endpoint = serve_once(200, &[], "codex-explicit-direct");
    assert_eq!(
        codex
            .current_with_proxy(Some(DIRECT_PROXY.into()))
            .unwrap()
            .get(endpoint)
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "codex-explicit-direct"
    );
}

#[test]
fn claude_settings_proxy_change_rebuilds_the_cached_client() {
    let directory = tempfile::tempdir().unwrap();
    let settings = directory.path().join("settings.json");
    let write_proxy = |proxy: &str| {
        std::fs::write(
            &settings,
            serde_json::json!({"env": {
                "HTTP_PROXY": proxy, "http_proxy": proxy, "NO_PROXY": "", "no_proxy": ""
            }})
            .to_string(),
        )
        .unwrap();
    };
    let fetch = |client: &ProviderClient| {
        client
            .current_with_proxy(None)
            .unwrap()
            .get("http://unresolvable.invalid/usage")
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    write_proxy(&serve_once(200, &[], "first-proxy"));
    let claude = ProviderClient::new("kimi", |builder| builder.timeout(Duration::from_secs(3)))
        .unwrap()
        .for_provider("claude")
        .with_claude_settings(settings.clone());
    assert_eq!(fetch(&claude), "first-proxy");

    write_proxy(&serve_once(200, &[], "second-proxy"));
    assert_eq!(fetch(&claude), "second-proxy");
}

#[test]
fn exit_location_requires_a_real_ip_and_country_not_the_datacenter() {
    let result = parse_proxy_exit("ip=203.0.113.7\ncolo=NRT\nloc=US\n").unwrap();
    assert_eq!(result.ip, "203.0.113.7");
    assert_eq!(result.country_code, "US");
    assert_eq!(
        parse_proxy_exit("ip=2001:db8::1\nloc=HK\n")
            .unwrap()
            .country_code,
        "HK"
    );
    for body in [
        "ip=bad\nloc=US",
        "ip=203.0.113.7\ncolo=NRT",
        "ip=203.0.113.7\nloc=XX",
        "ip=203.0.113.7\nloc=USA",
        "<html>error</html>",
    ] {
        assert!(parse_proxy_exit(body).is_none());
    }
}

#[test]
fn exit_probe_uses_the_explicit_proxy_and_direct_routes() {
    let proxy = serve_once(200, &[], "ip=203.0.113.7\nloc=JP\n");
    let proxied = client_builder_for_proxy(Some(&proxy))
        .unwrap()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert_eq!(
        query_proxy_exit(&proxied, "http://unresolvable.invalid/cdn-cgi/trace")
            .unwrap()
            .country_code,
        "JP"
    );
    let direct = client_builder_for_proxy(Some(DIRECT_PROXY))
        .unwrap()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let endpoint = serve_once(200, &[], "ip=2001:db8::1\nloc=DE\n");
    assert_eq!(
        query_proxy_exit(&direct, &endpoint).unwrap().country_code,
        "DE"
    );
    for endpoint in [
        serve_once(429, &[], "rate limited"),
        serve_once(200, &[], &"x".repeat(4097)),
        serve_once(200, &[], "ip=203.0.113.7\nloc=XX"),
    ] {
        assert_eq!(
            query_proxy_exit(&direct, &endpoint).unwrap_err(),
            PROXY_LOCATION_ERROR
        );
    }
}

#[test]
fn validates_supported_proxy_endpoints_without_echoing_credentials() {
    assert!(validate_provider_proxy(DIRECT_PROXY).is_ok());
    assert!(validate_proxy_url(DIRECT_PROXY).is_err());
    for value in [
        "http://127.0.0.1:7890",
        "https://proxy.example",
        "socks5://localhost:1080",
        "socks5h://user:pass@[::1]:1080",
    ] {
        assert!(validate_proxy_url(value).is_ok(), "{value}");
    }
    for value in [
        "localhost:7890",
        "ftp://proxy.example",
        "http://localhost:0",
        "http://localhost/path",
        "http://localhost?secret",
        "http://localhost#secret",
    ] {
        let error = validate_proxy_url(value).unwrap_err();
        assert!(!error.contains("secret"));
    }
}

#[test]
fn switches_proxy_connections_and_restores_default_client() {
    let client = ProviderClient::new("proxy-test", |builder| {
        builder.timeout(Duration::from_secs(2))
    })
    .unwrap();
    let other = client.for_provider("proxy-test-other");
    let first = serve_once(200, &[], "first");
    let second = serve_once(200, &[], "second");
    let target = "http://unresolvable.invalid/usage";
    assert_eq!(
        client
            .current_with_proxy(Some(first))
            .unwrap()
            .get(target)
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "first"
    );
    assert_eq!(
        client
            .current_with_proxy(Some(second))
            .unwrap()
            .get(target)
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "second"
    );
    let direct_target = serve_once(200, &[], "direct response");
    assert_eq!(
        client
            .current_with_proxy(Some(DIRECT_PROXY.into()))
            .unwrap()
            .get(direct_target)
            .send()
            .unwrap()
            .text()
            .unwrap(),
        "direct response"
    );
    assert_eq!(
        client.cached.lock().unwrap().as_ref().unwrap().0.as_deref(),
        Some(DIRECT_PROXY)
    );
    assert!(other.cached.lock().unwrap().is_none());
    client.current_with_proxy(None).unwrap();
    assert!(client.cached.lock().unwrap().as_ref().unwrap().0.is_none());
    assert!(client
        .current_with_proxy(Some("not a proxy URL".into()))
        .is_err());
}
