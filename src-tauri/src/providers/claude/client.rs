use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{auth::ClaudeOAuthConfig, ClaudeError};

type UsageResponse = (StatusCode, Value, Option<u64>, Option<i64>);

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
        })
    }

    fn with_timeout(timeout: std::time::Duration) -> Result<Self, ClaudeError> {
        Ok(Self {
            client: crate::http_client::ProviderClient::new("claude", move |builder| {
                builder.timeout(timeout)
            })
            .map_err(|_| ClaudeError::ConnectionFailed)?,
        })
    }

    pub fn for_provider(&self, provider_id: &str, settings: std::path::PathBuf) -> Self {
        Self {
            client: self
                .client
                .for_provider(provider_id)
                .with_claude_settings(settings),
        }
    }

    pub fn fetch_usage(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
    ) -> Result<UsageResponse, ClaudeError> {
        let primary = self.fetch_usage_once(
            token,
            &config.usage_url,
            &[("cedar_ember", "1")],
            USAGE_USER_AGENT,
            true,
        )?;
        if primary.0 == StatusCode::TOO_MANY_REQUESTS {
            if let Ok(fallback) = self.fetch_usage_once(
                token,
                &config.usage_url,
                &[("cedar_ember", "1")],
                LEGACY_USER_AGENT,
                false,
            ) {
                if fallback.0.is_success() {
                    return Ok(fallback);
                }
            }
        }
        Ok(primary)
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

    #[test]
    fn usage_sends_cedar_ember_cli_agent_and_falls_back_on_429() {
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
            for (status, body) in [
                (429, r#"{"error":"rate_limited"}"#),
                (200, r#"{"five_hour":{"utilization":12}}"#),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 4096];
                let count = stream.read(&mut buffer).unwrap();
                sender
                    .send(String::from_utf8_lossy(&buffer[..count]).into_owned())
                    .unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let base = format!("http://{address}");
        let (status, body, _, _) = ClaudeClient::new()
            .unwrap()
            .fetch_usage("secret-token", &config(&base))
            .unwrap();
        server.join().unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["five_hour"]["utilization"], 12);
        let first = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(first.starts_with("GET /usage?cedar_ember=1 HTTP/1.1"));
        assert!(first
            .to_ascii_lowercase()
            .contains("user-agent: claude-cli/2.1.322 (external, cli)"));
        assert!(first.to_ascii_lowercase().contains("x-app: cli"));
        assert!(second.starts_with("GET /usage?cedar_ember=1 HTTP/1.1"));
        assert!(second
            .to_ascii_lowercase()
            .contains("user-agent: claude-code/2.1.69"));
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
