use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{auth::ClaudeOAuthConfig, ClaudeError};

type UsageResponse = (StatusCode, Value, Option<u64>, Option<i64>);
// Isolate logins and OAuth endpoints without retaining raw access tokens.
type UsageKey = (String, [u8; 32]);

#[derive(Clone)]
struct UsageCooldown {
    until: Instant,
    response: UsageResponse,
}

const OAUTH_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const OAUTH_BETA: &str = "oauth-2025-04-20";
// Banked resets (`cedar_ember`) require a Claude Code 2.1.280+ CLI User-Agent;
// 2.1.322 also exposes Fable weekly-refund metadata (`gives_back_weekly`).
const USAGE_USER_AGENT: &str = "claude-cli/2.1.322 (external, cli)";
const LEGACY_USER_AGENT: &str = "claude-code/2.1.69";

#[derive(Debug, Deserialize)]
pub struct ClaudeRefreshResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<f64>,
    /// Claude Code records when the refresh token itself expires.
    #[serde(default)]
    pub refresh_token_expires_in: Option<f64>,
    /// The scopes granted, which Claude Code records.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Clone)]
pub struct ClaudeClient {
    client: crate::http_client::ProviderClient,
    primary_usage_retry_at: Arc<Mutex<HashMap<UsageKey, UsageCooldown>>>,
}

impl ClaudeClient {
    pub fn new() -> Result<Self, ClaudeError> {
        Self::with_timeout(std::time::Duration::from_secs(15))
    }

    pub fn new_no_redirect() -> Result<Self, ClaudeError> {
        let timeout = std::time::Duration::from_secs(30);
        Ok(Self {
            client: crate::http_client::ProviderClient::new("claude", move |builder| {
                builder
                    .timeout(timeout)
                    .redirect(reqwest::redirect::Policy::none())
            })
            .map_err(|_| ClaudeError::ConnectionFailed)?,
            primary_usage_retry_at: Default::default(),
        })
    }

    fn with_timeout(timeout: std::time::Duration) -> Result<Self, ClaudeError> {
        Ok(Self {
            client: crate::http_client::ProviderClient::new("claude", move |builder| {
                builder.timeout(timeout)
            })
            .map_err(|_| ClaudeError::ConnectionFailed)?,
            primary_usage_retry_at: Default::default(),
        })
    }

    pub fn for_provider(&self, provider_id: &str, settings: std::path::PathBuf) -> Self {
        Self {
            client: self
                .client
                .for_provider(provider_id)
                .with_claude_settings(settings),
            primary_usage_retry_at: self.primary_usage_retry_at.clone(),
        }
    }

    pub fn fetch_usage(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
    ) -> Result<UsageResponse, ClaudeError> {
        let key = (
            config.usage_url.clone(),
            Sha256::digest(token.trim()).into(),
        );
        let now = Instant::now();
        let primary_paused = self
            .primary_usage_retry_at
            .lock()
            .map(|mut pauses| {
                pauses.retain(|_, cooldown| cooldown.until > now);
                pauses.get(&key).cloned()
            })
            .unwrap_or(None);
        if let Some(cooldown) = primary_paused {
            let mut primary = cooldown.response;
            primary.2 = Some(
                cooldown
                    .until
                    .saturating_duration_since(now)
                    .as_secs()
                    .max(1),
            );
            return self.fetch_usage_fallback(token, config, primary);
        }
        let primary = self.fetch_usage_once(
            token,
            &config.usage_url,
            &[("cedar_ember", "1")],
            USAGE_USER_AGENT,
            true,
        )?;
        if primary.0 == StatusCode::TOO_MANY_REQUESTS {
            // The fallback's success must not erase the primary's rate limit.
            // Also retain it if the fallback fails, so the next poll avoids the
            // throttled agent. Existing provider-level backoff still applies.
            let pause = primary
                .2
                .unwrap_or(5 * 60)
                .clamp(5 * 60, MAX_RETRY_AFTER_SECONDS);
            if let Ok(mut pauses) = self.primary_usage_retry_at.lock() {
                pauses.insert(
                    key,
                    UsageCooldown {
                        until: Instant::now() + Duration::from_secs(pause),
                        response: primary.clone(),
                    },
                );
            }
            return self.fetch_usage_fallback(token, config, primary);
        }
        Ok(primary)
    }

    /// The legacy agent is optional: only success may replace the primary's known 429.
    /// In particular, its 401/403 must never trigger an OAuth renewal.
    fn fetch_usage_fallback(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
        primary: UsageResponse,
    ) -> Result<UsageResponse, ClaudeError> {
        match self.fetch_usage_once(
            token,
            &config.usage_url,
            &[("cedar_ember", "1")],
            LEGACY_USER_AGENT,
            false,
        ) {
            Ok(fallback) if fallback.0.is_success() => Ok(fallback),
            _ => Ok(primary),
        }
    }

    fn fetch_usage_once(
        &self,
        token: &str,
        url: &str,
        query: &[(&str, &str)],
        user_agent: &str,
        cli_app_header: bool,
    ) -> Result<UsageResponse, ClaudeError> {
        let started = std::time::Instant::now();
        let mut parsed_url = reqwest::Url::parse(url).map_err(|_| ClaudeError::InvalidOAuthUrl)?;
        parsed_url.query_pairs_mut().extend_pairs(query);
        let mut request = self
            .client
            .current()
            .map_err(|_| ClaudeError::ConnectionFailed)?
            .get(parsed_url)
            .bearer_auth(token.trim())
            .header("Accept", "application/json")
            .header("anthropic-beta", OAUTH_BETA)
            .header("User-Agent", user_agent);
        if cli_app_header {
            request = request.header("x-app", "cli");
        }
        let response = request.send().map_err(|error| {
            crate::app_warn!("http", "claude usage request failed (transport)");
            crate::http_client::certificate_restart_hint(&error)
                .map(ClaudeError::CertificateFailed)
                .unwrap_or(ClaudeError::ConnectionFailed)
        })?;
        let clock_offset = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| clock_offset_seconds(value, Utc::now()));
        let status = response.status();
        crate::app_debug!(
            "http",
            "claude usage HTTP {} ({}ms)",
            status.as_u16(),
            started.elapsed().as_millis()
        );
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, Utc::now()));
        let body = response.json().or_else(|_| {
            crate::app_warn!(
                "http",
                "claude usage response is not JSON (HTTP {})",
                status.as_u16()
            );
            if status.is_success() {
                Err(ClaudeError::InvalidResponse)
            } else {
                // Preserve HTTP failures, including authentication and rate-limit handling,
                // even when a proxy returns an empty body or an HTML error page.
                Ok(Value::Null)
            }
        })?;
        Ok((status, body, retry_after, clock_offset))
    }

    pub fn fetch_reset_status(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
    ) -> Result<(StatusCode, Value), ClaudeError> {
        let (status, body, _, _) = self.fetch_usage_once(
            token,
            &config.usage_url,
            &[("cedar_ember", "1"), ("skip_spend", "1")],
            USAGE_USER_AGENT,
            true,
        )?;
        Ok((status, body))
    }

    pub fn fetch_profile(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
    ) -> Result<(StatusCode, Value), ClaudeError> {
        let started = std::time::Instant::now();
        let response = self
            .client
            .current()
            .map_err(|_| ClaudeError::ConnectionFailed)?
            .get(config.profile_url())
            .bearer_auth(token.trim())
            .header("Accept", "application/json")
            .header("anthropic-beta", OAUTH_BETA)
            .header("x-app", "cli")
            .header("User-Agent", USAGE_USER_AGENT)
            .send()
            .map_err(|error| {
                crate::app_warn!("http", "claude profile request failed (transport)");
                crate::http_client::certificate_restart_hint(&error)
                    .map(ClaudeError::CertificateFailed)
                    .unwrap_or(ClaudeError::ConnectionFailed)
            })?;
        let status = response.status();
        crate::app_debug!(
            "http",
            "claude profile HTTP {} ({}ms)",
            status.as_u16(),
            started.elapsed().as_millis()
        );
        let body = response.json().unwrap_or(Value::Null);
        Ok((status, body))
    }

    pub fn redeem_reset_credit(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
        organization_uuid: &str,
        grant_id: &str,
        request_id: &str,
    ) -> Result<(StatusCode, Value), ClaudeError> {
        let started = std::time::Instant::now();
        let response = self
            .client
            .current()
            .map_err(|_| ClaudeError::ConnectionFailed)?
            .post(config.reset_rate_limits_url(organization_uuid))
            .bearer_auth(token.trim())
            .header("Accept", "application/json")
            .header("anthropic-beta", OAUTH_BETA)
            .header("x-app", "cli")
            .header("User-Agent", USAGE_USER_AGENT)
            .json(&json!({
                "program": "cedar_ember",
                "grant_id": grant_id,
                "request_id": request_id,
            }))
            .send()
            .map_err(|error| {
                crate::app_warn!("http", "claude reset-credit redeem failed (transport)");
                crate::http_client::certificate_restart_hint(&error)
                    .map(ClaudeError::CertificateFailed)
                    .unwrap_or(ClaudeError::ConnectionFailed)
            })?;
        let status = response.status();
        crate::app_debug!(
            "http",
            "claude reset-credit redeem HTTP {} ({}ms)",
            status.as_u16(),
            started.elapsed().as_millis()
        );
        let body = response.json().unwrap_or(Value::Null);
        Ok((status, body))
    }

    pub fn refresh_token(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
    ) -> Result<ClaudeRefreshResponse, ClaudeError> {
        let started = std::time::Instant::now();
        crate::app_info!("auth:claude", "token refresh attempt");
        let response = self
            .client
            .current()
            .map_err(|_| ClaudeError::ConnectionFailed)?
            .post(&config.refresh_url)
            .json(&json!({
                "grant_type": "refresh_token",
                "refresh_token": token,
                "client_id": config.client_id,
                "scope": OAUTH_SCOPES
            }))
            .send()
            .map_err(|error| {
                crate::app_warn!("auth:claude", "token refresh failed (transport)");
                crate::http_client::certificate_restart_hint(&error)
                    .map(ClaudeError::CertificateFailed)
                    .unwrap_or(ClaudeError::ConnectionFailed)
            })?;
        let status = response.status();
        crate::app_debug!(
            "http",
            "claude token refresh HTTP {} ({}ms)",
            status.as_u16(),
            started.elapsed().as_millis()
        );
        if !status.is_success() {
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::BAD_REQUEST) {
                let body = response.json::<Value>().ok();
                let code = body.as_ref().and_then(|body| {
                    body.get("error")
                        .and_then(Value::as_str)
                        .or_else(|| body.get("error_description").and_then(Value::as_str))
                });
                if code == Some("invalid_grant") {
                    return Err(ClaudeError::SessionExpired);
                }
            }
            return Err(ClaudeError::RequestFailed(status.as_u16()));
        }
        let refreshed: ClaudeRefreshResponse =
            response.json().map_err(|_| ClaudeError::InvalidResponse)?;
        if refreshed.access_token.trim().is_empty() {
            return Err(ClaudeError::TokenExpired);
        }
        crate::app_info!("auth:claude", "token refresh succeeded");
        Ok(refreshed)
    }
}

fn clock_offset_seconds(value: &str, now: DateTime<Utc>) -> Option<i64> {
    let server = DateTime::parse_from_rfc2822(value.trim()).ok()?.to_utc();
    Some(server.signed_duration_since(now).num_seconds())
}

/// Longest cooldown honored from `Retry-After`; larger values would overflow
/// chrono arithmetic and are never meaningful for a polling client.
const MAX_RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;

fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<u64> {
    let value = value.trim();
    let seconds = if let Ok(seconds) = value.parse::<u64>() {
        seconds
    } else {
        let date = DateTime::parse_from_rfc2822(value).ok()?.to_utc();
        let milliseconds = date.signed_duration_since(now).num_milliseconds().max(0) as u64;
        milliseconds.div_ceil(1000)
    };
    Some(seconds.min(MAX_RETRY_AFTER_SECONDS))
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use chrono::{TimeZone, Utc};

    use super::{parse_retry_after, ClaudeClient};
    use crate::providers::{
        claude::{auth::ClaudeOAuthConfig, ClaudeError},
        test_http,
    };

    fn config(base: &str) -> ClaudeOAuthConfig {
        ClaudeOAuthConfig {
            usage_url: format!("{base}/usage"),
            refresh_url: format!("{base}/token"),
            client_id: "test-client".into(),
        }
    }

    #[test]
    fn server_clock_offsets_and_time_zones() {
        let now = Utc.with_ymd_and_hms(2026, 10, 30, 12, 0, 0).unwrap();
        for date in [
            "Fri, 30 Oct 2026 18:00:00 GMT",
            "Sat, 31 Oct 2026 02:00:00 +0800",
        ] {
            assert_eq!(super::clock_offset_seconds(date, now), Some(6 * 3600));
        }
        assert_eq!(
            super::clock_offset_seconds("Fri, 30 Oct 2026 06:00:00 GMT", now),
            Some(-6 * 3600)
        );
        assert_eq!(super::clock_offset_seconds("invalid", now), None);
    }

    #[test]
    fn usage_response_preserves_server_clock_offset() {
        let server = Utc::now() + chrono::Duration::hours(6);
        let date = server.to_rfc2822();
        let base = test_http::serve_once(
            200,
            &[("date", &date)],
            r#"{"five_hour":{"utilization":2}}"#,
        );
        let (_, _, _, offset) = ClaudeClient::new()
            .unwrap()
            .fetch_usage("test-token", &config(&base))
            .unwrap();
        assert!((offset.unwrap() - 6 * 3600).abs() <= 2);
    }

    #[test]
    fn usage_success_reads_json_and_retry_after() {
        let base = test_http::serve_once(200, &[("retry-after", "120")], r#"{"plan":"max"}"#);
        let (status, body, retry_after, _) = ClaudeClient::new()
            .unwrap()
            .fetch_usage("secret-token", &config(&base))
            .unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["plan"], "max");
        assert_eq!(retry_after, Some(120));
    }

    #[test]
    fn malformed_usage_and_timeout_return_safe_errors() {
        let malformed = test_http::serve_once(200, &[], "secret-token: not-json");
        let error = ClaudeClient::new()
            .unwrap()
            .fetch_usage("secret-token", &config(&malformed))
            .unwrap_err();
        assert!(matches!(error, ClaudeError::InvalidResponse));
        assert!(!error.to_string().contains("secret-token"));

        let timeout = test_http::serve_once_after(
            test_http::TIMEOUT_TEST_RESPONSE_DELAY,
            200,
            &[],
            r#"{"plan":"max"}"#,
        );
        let error = ClaudeClient::with_timeout(test_http::TIMEOUT_TEST_CLIENT_LIMIT)
            .unwrap()
            .fetch_usage("secret-token", &config(&timeout))
            .unwrap_err();
        assert!(matches!(error, ClaudeError::ConnectionFailed));
        assert!(!error.to_string().contains("secret-token"));
    }

    #[test]
    fn non_json_usage_errors_preserve_http_status_and_retry_after() {
        for (code, response) in [
            (502, ""),
            (401, "unauthorized"),
            (403, "<html>denied</html>"),
            (429, "busy"),
        ] {
            let base = test_http::serve_once(code, &[("retry-after", "120")], response);
            let (status, body, retry_after, _) = ClaudeClient::new()
                .unwrap()
                .fetch_usage("fake-access-token", &config(&base))
                .unwrap();
            assert_eq!(status.as_u16(), code);
            assert_eq!(body, serde_json::Value::Null);
            assert_eq!(retry_after, Some(120));
        }
    }

    #[test]
    fn refresh_distinguishes_auth_failures_and_rate_limits() {
        let forbidden = test_http::serve_once(400, &[], r#"{"error":"invalid_grant"}"#);
        assert!(matches!(
            ClaudeClient::new()
                .unwrap()
                .refresh_token("secret-refresh", &config(&forbidden)),
            Err(ClaudeError::SessionExpired)
        ));

        let limited = test_http::serve_once(429, &[], r#"{"error":"slow_down"}"#);
        assert!(matches!(
            ClaudeClient::new()
                .unwrap()
                .refresh_token("secret-refresh", &config(&limited)),
            Err(ClaudeError::RequestFailed(429))
        ));
    }

    #[test]
    fn refresh_does_not_misclassify_an_unrecognized_400_as_expired() {
        let base = test_http::serve_once(400, &[], "<html>Bad Gateway</html>");
        assert!(matches!(
            ClaudeClient::new()
                .unwrap()
                .refresh_token("secret-refresh", &config(&base)),
            Err(ClaudeError::RequestFailed(400))
        ));
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = Utc.with_ymd_and_hms(2015, 10, 21, 7, 27, 0).unwrap();
        assert_eq!(parse_retry_after("120", now), Some(120));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", now),
            Some(60)
        );
        assert_eq!(
            parse_retry_after("100000000000000", now),
            Some(super::MAX_RETRY_AFTER_SECONDS)
        );
        assert_eq!(
            parse_retry_after("Fri, 31 Dec 9999 23:59:59 GMT", now),
            Some(super::MAX_RETRY_AFTER_SECONDS)
        );
    }

    /// Captures each request so cooldown tests verify which agent reached the server.
    fn usage_server(
        responses: Vec<(u16, Option<u64>)>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::mpsc,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            for (status, retry) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                sender.send(String::from_utf8(request).unwrap()).unwrap();
                let body = r#"{"five_hour":{"utilization":12}}"#;
                let header = retry
                    .map(|seconds| format!("Retry-After: {seconds}\r\n"))
                    .unwrap_or_default();
                write!(stream,
                    "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n{header}\r\n{body}",
                    body.len()
                ).unwrap();
            }
        });
        (format!("http://{address}"), receiver, server)
    }

    #[test]
    fn usage_primary_cooldown_survives_success_is_scoped_and_expires() {
        use sha2::{Digest, Sha256};
        use std::time::{Duration, Instant};
        let (base, receiver, server) = usage_server(vec![
            (429, None),
            (200, None),
            (200, None),
            (200, None),
            (200, None),
            (200, None),
        ]);
        let config = config(&base);
        let client = ClaudeClient::new().unwrap();
        let before = Instant::now();
        let (status, body, _, _) = client.fetch_usage("secret-token", &config).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["five_hour"]["utilization"], 12);
        let key = (
            config.usage_url.clone(),
            Sha256::digest("secret-token").into(),
        );
        let until = client.primary_usage_retry_at.lock().unwrap()[&key].until;
        assert!(until >= before + Duration::from_secs(300));
        assert!(until <= Instant::now() + Duration::from_secs(300));
        // Provider-specific clients and clones must retain the same cooldown.
        let dir = tempfile::tempdir().unwrap();
        let derived = client
            .clone()
            .for_provider("claude", dir.path().join("settings.json"));
        derived.fetch_usage("secret-token", &config).unwrap();
        client.fetch_usage("other-token", &config).unwrap();
        let other_endpoint = ClaudeOAuthConfig {
            usage_url: format!("{base}/other"),
            ..config.clone()
        };
        client.fetch_usage("secret-token", &other_endpoint).unwrap();
        client
            .primary_usage_retry_at
            .lock()
            .unwrap()
            .get_mut(&key)
            .unwrap()
            .until = Instant::now() - Duration::from_secs(1);
        client.fetch_usage("secret-token", &config).unwrap();
        assert!(!client
            .primary_usage_retry_at
            .lock()
            .unwrap()
            .contains_key(&key));
        server.join().unwrap();
        let requests: Vec<_> = receiver.try_iter().collect();
        assert_eq!(requests.len(), 6);
        for (index, agent) in [
            super::USAGE_USER_AGENT,
            super::LEGACY_USER_AGENT,
            super::LEGACY_USER_AGENT,
            super::USAGE_USER_AGENT,
            super::USAGE_USER_AGENT,
            super::USAGE_USER_AGENT,
        ]
        .iter()
        .enumerate()
        {
            let request = requests[index].to_ascii_lowercase();
            assert!(
                request.contains(&format!("user-agent: {agent}")),
                "request {index}: {request}"
            );
            assert_eq!(
                request.contains("x-app: cli"),
                *agent == super::USAGE_USER_AGENT
            );
        }
        assert!(requests[0].starts_with("GET /usage?cedar_ember=1 HTTP/1.1"));
        assert!(requests[4].starts_with("GET /other?cedar_ember=1 HTTP/1.1"));
    }

    #[test]
    fn usage_cooldown_keeps_retry_after_when_fallback_is_also_limited() {
        use std::time::{Duration, Instant};
        let (base, receiver, server) =
            usage_server(vec![(429, Some(5400)), (429, Some(60)), (429, Some(120))]);
        let client = ClaudeClient::new().unwrap();
        let before = Instant::now();
        let (status, _, retry, _) = client.fetch_usage("test-token", &config(&base)).unwrap();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry, Some(5400));
        let until = client
            .primary_usage_retry_at
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .until;
        assert!(until >= before + Duration::from_secs(5400));
        assert!(until <= Instant::now() + Duration::from_secs(5400));
        let (status, _, retry, _) = client.fetch_usage("test-token", &config(&base)).unwrap();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(matches!(retry, Some(seconds) if (5395..=5400).contains(&seconds)));
        server.join().unwrap();
        let requests: Vec<_> = receiver.try_iter().collect();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].contains(super::LEGACY_USER_AGENT));
    }

    #[test]
    fn failed_legacy_responses_never_replace_the_primary_rate_limit() {
        for fallback_status in [401, 403, 429, 500, 503] {
            let (base, requests, server) = usage_server(vec![
                (429, Some(1800)),
                (fallback_status, Some(1)),
                (fallback_status, Some(2)),
            ]);
            let client = ClaudeClient::new().unwrap();
            let config = config(&base);
            let primary = client.fetch_usage("same-token", &config).unwrap();
            assert_eq!(primary.0, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(primary.2, Some(1800));
            let during_cooldown = client.fetch_usage("same-token", &config).unwrap();
            assert_eq!(during_cooldown.0, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(during_cooldown.1, primary.1);
            assert!(matches!(during_cooldown.2, Some(seconds) if (1795..=1800).contains(&seconds)));
            server.join().unwrap();
            assert_eq!(requests.try_iter().count(), 3);
        }
    }

    #[test]
    fn a_failed_legacy_transport_keeps_the_primary_response_during_cooldown() {
        let (base, _requests, server) = usage_server(vec![(429, Some(1800))]);
        let client = ClaudeClient::new().unwrap();
        let config = config(&base);
        let primary = client.fetch_usage("same-token", &config).unwrap();
        server.join().unwrap();
        assert_eq!(primary.0, StatusCode::TOO_MANY_REQUESTS);
        let during_cooldown = client.fetch_usage("same-token", &config).unwrap();
        assert_eq!(during_cooldown.0, primary.0);
        assert_eq!(during_cooldown.1, primary.1);
        assert!(during_cooldown.2.unwrap() > 1700);
    }

    #[test]
    fn failed_legacy_responses_preserve_the_primary_rate_limit() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        for fallback_status in [401, 403, 429, 500, 503] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                for (status, body) in [
                    (429, r#"{"error":"primary_rate_limit"}"#),
                    (fallback_status, r#"{"error":"legacy_failure"}"#),
                ] {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                        let mut chunk = [0_u8; 4096];
                        let count = stream.read(&mut chunk).unwrap();
                        assert!(count > 0);
                        request.extend_from_slice(&chunk[..count]);
                    }
                    write!(stream,"HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nRetry-After: 1800\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                }
            });
            let (status, body, retry, _) = ClaudeClient::new()
                .unwrap()
                .fetch_usage("test-token", &config(&base))
                .unwrap();
            server.join().unwrap();
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(body["error"], "primary_rate_limit");
            assert_eq!(retry, Some(1800));
        }
    }

    #[test]
    fn reset_redeem_sends_cedar_ember_payload_to_organization_endpoint() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::mpsc,
            thread,
            time::Duration,
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
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
            let body = r#"{"result":"reset","cleared":["five_hour","seven_day"]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let base = format!("http://{address}");
        let (status, body) = ClaudeClient::new_no_redirect()
            .unwrap()
            .redeem_reset_credit(
                "secret-token",
                &config(&base),
                "550e8400-e29b-41d4-a716-446655440000",
                "grant_1",
                "req-123",
            )
            .unwrap();
        server.join().unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], "reset");
        let request = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request.starts_with(
            "POST /api/organizations/550e8400-e29b-41d4-a716-446655440000/reset_rate_limits HTTP/1.1"
        ));
        assert!(request.contains("\"program\":\"cedar_ember\""));
        assert!(request.contains("\"grant_id\":\"grant_1\""));
        assert!(request.contains("\"request_id\":\"req-123\""));
        assert!(request
            .to_ascii_lowercase()
            .contains("anthropic-beta: oauth-2025-04-20"));
        assert!(request.to_ascii_lowercase().contains("x-app: cli"));
    }
}
