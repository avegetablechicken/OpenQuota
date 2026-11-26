use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    models::{QuotaFormat, QuotaWindow},
    providers::antigravity::started_reset,
};

use super::{client::UsageResponse, OpenCodeError};

const ROLLING_PERIOD_SECONDS: u64 = 5 * 60 * 60;
const WEEKLY_PERIOD_SECONDS: u64 = 7 * 24 * 60 * 60;

pub(super) fn map_go_usage(
    response: UsageResponse,
    now: DateTime<Utc>,
) -> Result<Vec<QuotaWindow>, OpenCodeError> {
    match response.status.as_u16() {
        200..=299 => {}
        401 => return Err(OpenCodeError::InvalidAuth),
        403 if response.body.pointer("/error/type").and_then(Value::as_str)
            == Some("EntitlementError") =>
        {
            return Err(OpenCodeError::GoSubscriptionRequired);
        }
        403 => return Err(OpenCodeError::RequestFailed(403)),
        status => return Err(OpenCodeError::RequestFailed(status)),
    }
    let usage = response
        .body
        .get("usage")
        .and_then(Value::as_object)
        .ok_or(OpenCodeError::InvalidResponse)?;
    [
        quota(
            usage.get("rolling"),
            "session",
            "Session",
            ROLLING_PERIOD_SECONDS,
        ),
        quota(
            usage.get("weekly"),
            "weekly",
            "Weekly",
            WEEKLY_PERIOD_SECONDS,
        ),
        quota(usage.get("monthly"), "monthly", "Monthly", 0),
    ]
    .into_iter()
    .map(|quota| quota.map(|quota| unstarted_session_without_reset(quota, now)))
    .collect()
}

/// An unused rolling window still reports a reset a whole window from now, which would read as
/// a running session. It starts with the first request, which fixes the reset. The percent is
/// whole, so a started window can still read 0%; only the reset's distance tells them apart.
fn unstarted_session_without_reset(mut quota: QuotaWindow, now: DateTime<Utc>) -> QuotaWindow {
    if quota.id == "session" {
        quota.resets_at = started_reset(
            1.0 - quota.used_percent / 100.0,
            quota.resets_at,
            ROLLING_PERIOD_SECONDS,
            now,
        );
    }
    quota
}

fn quota(
    value: Option<&Value>,
    id: &str,
    label: &str,
    period_seconds: u64,
) -> Result<QuotaWindow, OpenCodeError> {
    let value = value
        .and_then(Value::as_object)
        .ok_or(OpenCodeError::InvalidResponse)?;
    let used_percent = value
        .get("percent")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or(OpenCodeError::InvalidResponse)?
        .clamp(0.0, 100.0);
    let resets_at = value
        .get("resetsAt")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    Ok(QuotaWindow {
        clock_offset_seconds: None,
        id: id.into(),
        label: label.into(),
        used_percent,
        resets_at,
        period_seconds,
        format: QuotaFormat::Percent,
        used_value: None,
        limit_value: None,
        unit: None,
        estimated: false,
        source_note: None,
    })
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use serde_json::json;

    use super::super::OpenCodeError;
    use super::{map_go_usage, UsageResponse};
    use crate::providers::test_http;

    #[test]
    fn maps_authoritative_go_usage_windows() {
        let response = UsageResponse {
            status: StatusCode::OK,
            body: json!({"usage": {
                "rolling": {"percent": 31, "resetsAt": "2026-08-12T12:00:00Z", "status": "ok"},
                "weekly": {"percent": 100, "resetsAt": "2026-08-17T00:00:00Z", "status": "rate-limited"},
                "monthly": {"percent": 72, "resetsAt": "2026-09-05T00:00:00Z", "status": "ok"}
            }}),
        };
        let quotas = map_go_usage(response, test_now()).unwrap();
        assert_eq!(quotas.len(), 3);
        assert_eq!(quotas[0].id, "session");
        assert_eq!(quotas[0].period_seconds, 5 * 60 * 60);
        assert_eq!(quotas[1].period_seconds, 7 * 24 * 60 * 60);
        assert_eq!(quotas[2].period_seconds, 0);
        assert_eq!(quotas[1].used_percent, 100.0);
        assert!(!quotas.iter().any(|quota| quota.estimated));
    }

    fn test_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-12T07:00:00Z")
            .unwrap()
            .to_utc()
    }

    #[test]
    fn started_rolling_window_below_one_percent_keeps_its_reset() {
        let response = UsageResponse {
            status: StatusCode::OK,
            body: json!({"usage": {
                "rolling": {"percent": 0, "resetsAt": "2026-08-12T10:30:00Z", "status": "ok"},
                "weekly": {"percent": 0, "resetsAt": "2026-08-17T00:00:00Z", "status": "ok"},
                "monthly": {"percent": 0, "resetsAt": "2026-09-05T00:00:00Z", "status": "ok"}
            }}),
        };
        let quotas = map_go_usage(response, test_now()).unwrap();
        assert_eq!(quotas[0].used_percent, 0.0);
        assert_eq!(
            quotas[0].resets_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-08-12T10:30:00Z")
                    .unwrap()
                    .to_utc()
            )
        );
    }

    #[test]
    fn unused_rolling_window_has_no_reset_while_other_windows_keep_theirs() {
        let response = UsageResponse {
            status: StatusCode::OK,
            body: json!({"usage": {
                "rolling": {"percent": 0, "resetsAt": "2026-08-12T12:00:00Z", "status": "ok"},
                "weekly": {"percent": 0, "resetsAt": "2026-08-17T00:00:00Z", "status": "ok"},
                "monthly": {"percent": 0, "resetsAt": "2026-09-05T00:00:00Z", "status": "ok"}
            }}),
        };
        let quotas = map_go_usage(response, test_now()).unwrap();
        assert_eq!(quotas[0].used_percent, 0.0);
        assert_eq!(quotas[0].resets_at, None);
        assert!(quotas[1..].iter().all(|quota| quota.resets_at.is_some()));
    }

    #[test]
    fn maps_missing_go_subscription_as_entitlement_error() {
        let response = fetch_response(
            403,
            r#"{"error":{"type":"EntitlementError","message":"OpenCode Go subscription required."}}"#,
        );

        let error = map_go_usage(response, test_now()).unwrap_err();
        assert_eq!(error, OpenCodeError::GoSubscriptionRequired);
    }

    #[test]
    fn maps_non_entitlement_forbidden_as_request_failure() {
        let response = fetch_response(
            403,
            r#"{"error":{"type":"PermissionError","message":"Forbidden"}}"#,
        );

        let error = map_go_usage(response, test_now()).unwrap_err();
        assert_eq!(error, OpenCodeError::RequestFailed(403));
    }

    #[test]
    fn maps_invalid_key_as_authentication_error() {
        let response = fetch_response(401, r#"{"type":"error"}"#);

        let error = map_go_usage(response, test_now()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenCode Go login data is invalid or expired. Sign in to OpenCode Go again."
        );
    }

    #[test]
    fn maps_rate_limit_response_as_rate_limited_error() {
        let response = fetch_response(429, r#"{"type":"error"}"#);

        let error = map_go_usage(response, test_now()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenCode Go usage request failed (HTTP 429)."
        );
    }

    #[test]
    fn client_fetches_usage_from_a_test_endpoint() {
        let response = fetch_response(200, r#"{"usage":{}}"#);

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body["usage"], json!({}));
    }

    fn fetch_response(status: u16, body: &str) -> UsageResponse {
        let url = test_http::serve_once(status, &[], body);
        let client =
            super::super::client::OpenCodeClient::for_test(&url, std::time::Duration::from_secs(1));
        client.fetch_go_usage("test-key").unwrap()
    }
}
