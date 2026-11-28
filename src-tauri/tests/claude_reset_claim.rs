//! Exercise the Claude reset-claim state machine with scripted credentials and network boundaries.
//! Including the module keeps these tests completely off real auth files, Keychain, and live APIs.
#![allow(dead_code)]

use std::{cell::RefCell, collections::VecDeque};

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::{json, Value};

#[macro_export]
macro_rules! app_error {
    ($($args:tt)*) => {};
}

type ClaudeError = ();

mod models {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ResetCreditKind {
        Full,
        Fable,
        Partial,
        Unknown,
    }
}

mod config {
    use std::path::PathBuf;

    pub fn settings_path() -> PathBuf {
        PathBuf::new()
    }
}

mod mapper {
    use serde_json::Value;

    use super::models::ResetCreditKind;

    pub fn is_valid_grant_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 40
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    }

    pub fn classify_reset_kind(grant: &Value) -> ResetCreditKind {
        if grant.get("gives_back_weekly").and_then(Value::as_bool) == Some(true) {
            ResetCreditKind::Fable
        } else if let Some(clears) = grant.get("clears").and_then(Value::as_array) {
            let has_5h = clears.iter().any(|v| v.as_str() == Some("five_hour"));
            let has_7d = clears.iter().any(|v| v.as_str() == Some("seven_day"));
            if has_5h && has_7d {
                ResetCreditKind::Full
            } else {
                ResetCreditKind::Partial
            }
        } else {
            ResetCreditKind::Unknown
        }
    }
}

mod auth {
    #[derive(Debug, Clone)]
    pub struct ClaudeOAuthConfig {
        pub usage_url: String,
        pub refresh_url: String,
        pub client_id: String,
    }

    #[derive(Debug, Clone)]
    pub enum ClaudeCredentialScope {
        Standard,
    }

    #[derive(Clone)]
    pub struct ClaudeCredential {
        pub token: String,
        pub owner_byte: u8,
        pub inference_only: bool,
    }

    impl ClaudeCredential {
        pub fn access_token(&self) -> Option<&str> {
            Some(self.token.as_str())
        }

        pub fn has_profile_scope(&self) -> bool {
            !self.inference_only
        }

        pub fn fingerprint(&self) -> [u8; 32] {
            [self.owner_byte; 32]
        }
    }

    pub fn oauth_config() -> Result<ClaudeOAuthConfig, super::ClaudeError> {
        Ok(ClaudeOAuthConfig {
            usage_url: "http://127.0.0.1/api/oauth/usage".into(),
            refresh_url: "http://127.0.0.1/v1/oauth/token".into(),
            client_id: "test-client".into(),
        })
    }

    pub fn load_candidates(_scope: &ClaudeCredentialScope) -> Vec<ClaudeCredential> {
        super::SCRIPT.with(|script| script.borrow().logins.clone())
    }
}

mod accounts {
    pub fn identity_for_scope(_scope: &super::auth::ClaudeCredentialScope) -> Option<String> {
        super::SCRIPT.with(|script| script.borrow().active_identity.clone())
    }

    pub fn profile_matches_identity(profile: &serde_json::Value, expected: &str) -> bool {
        profile
            .pointer("/account/uuid")
            .and_then(|value| value.as_str())
            == Some(expected)
    }
}

mod client {
    use std::path::PathBuf;

    use reqwest::StatusCode;
    use serde_json::Value;

    use super::auth::ClaudeOAuthConfig;

    pub struct ClaudeClient;

    impl ClaudeClient {
        pub fn new_no_redirect() -> Result<Self, super::ClaudeError> {
            Ok(Self)
        }

        pub fn for_provider(&self, _provider_id: &str, _settings: PathBuf) -> Self {
            Self
        }

        pub fn fetch_reset_status(
            &self,
            token: &str,
            _config: &ClaudeOAuthConfig,
        ) -> Result<(StatusCode, Value), super::ClaudeError> {
            super::respond("status", token, None, None)
        }

        pub fn fetch_profile(
            &self,
            token: &str,
            _config: &ClaudeOAuthConfig,
        ) -> Result<(StatusCode, Value), super::ClaudeError> {
            super::respond("profile", token, None, None)
        }

        pub fn redeem_reset_credit(
            &self,
            token: &str,
            _config: &ClaudeOAuthConfig,
            org_uuid: &str,
            grant_id: &str,
            request_id: &str,
        ) -> Result<(StatusCode, Value), super::ClaudeError> {
            assert_eq!(org_uuid, super::ORG_UUID);
            super::respond("redeem", token, Some(grant_id), Some(request_id))
        }
    }
}

#[path = "../src/providers/claude/reset_claim.rs"]
mod reset_claim;

use models::ResetCreditKind;
use reset_claim::{ClaudeResetClaimOutcome, ClaudeResetClaimService};

const ACCOUNT_ID: &str = "account-a";
const ORG_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

type ExpectedRequest = (&'static str, &'static str, Option<(u16, Value)>);

#[derive(Default)]
struct Script {
    logins: Vec<auth::ClaudeCredential>,
    requests: VecDeque<ExpectedRequest>,
    active_identity: Option<String>,
    switch_on_status: bool,
}

thread_local! {
    static SCRIPT: RefCell<Script> = RefCell::new(Script::default());
}

fn respond(
    method: &str,
    token: &str,
    grant_id: Option<&str>,
    request_id: Option<&str>,
) -> Result<(StatusCode, Value), ClaudeError> {
    SCRIPT.with(|script| {
        let mut script = script.borrow_mut();
        let (expected_method, expected_token, reply) = script
            .requests
            .pop_front()
            .expect("claim made an unexpected request");
        assert_eq!((method, token), (expected_method, expected_token));
        if method == "redeem" {
            assert_eq!(grant_id, Some("fable_grant"));
            assert_eq!(request_id, Some("request-1"));
        }
        if method == "status" && script.switch_on_status {
            script.active_identity = Some("account-b".into());
        }
        let (status, body) = reply.ok_or(())?;
        Ok((StatusCode::from_u16(status).unwrap(), body))
    })
}

fn expiry() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2099-11-17T07:59:00Z")
        .unwrap()
        .to_utc()
}

fn status_body() -> Value {
    json!({
        "cedar_ember": {
            "eligible": true,
            "at_limit": false,
            "next_grant_id": "fable_grant",
            "grants": [{
                "id": "fable_grant",
                "resets_left": 1,
                "ends_at": "2099-11-17T07:59:00Z",
                "gives_back_weekly": true,
                "clears": ["five_hour", "seven_day_overage_included"],
                "paused": false,
                "usable_now": true,
                "use_requires_limit": false,
                "blocking": []
            }]
        }
    })
}

fn profile_body() -> Value {
    json!({"account": {"uuid": ACCOUNT_ID}, "organization": {"uuid": ORG_UUID}})
}

fn scripted_service(requests: Vec<ExpectedRequest>) -> ClaudeResetClaimService {
    SCRIPT.with(|script| {
        *script.borrow_mut() = Script {
            logins: vec![
                auth::ClaudeCredential {
                    token: "stale-token".into(),
                    owner_byte: 1,
                    inference_only: false,
                },
                auth::ClaudeCredential {
                    token: "valid-token".into(),
                    owner_byte: 2,
                    inference_only: false,
                },
            ],
            requests: requests.into(),
            active_identity: Some(ACCOUNT_ID.into()),
            switch_on_status: false,
        };
    });
    ClaudeResetClaimService::new().unwrap()
}

fn assert_all_requests_made() {
    SCRIPT.with(|script| {
        assert!(
            script.borrow().requests.is_empty(),
            "claim did not reach every expected request"
        );
    });
}

#[test]
fn claims_matched_grant_after_falling_back_past_stale_login() {
    let service = scripted_service(vec![
        ("profile", "stale-token", Some((401, Value::Null))),
        ("profile", "valid-token", Some((200, profile_body()))),
        ("status", "valid-token", Some((200, status_body()))),
        (
            "redeem",
            "valid-token",
            Some((200, json!({"result": "reset", "cleared": ["five_hour"]}))),
        ),
    ]);

    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Success
    );
    assert_all_requests_made();
}

#[test]
fn uncertain_outcomes_block_retries_even_with_a_new_request_id() {
    for reply in [
        None,
        Some((502, Value::Null)),
        Some((200, json!({"result":"unknown"}))),
    ] {
        let service = scripted_service(vec![
            ("profile", "stale-token", Some((401, Value::Null))),
            ("profile", "valid-token", Some((200, profile_body()))),
            ("status", "valid-token", Some((200, status_body()))),
            ("redeem", "valid-token", reply),
        ]);

        assert_eq!(
            service.claim(
                Some(expiry()),
                Some(ResetCreditKind::Fable),
                ACCOUNT_ID,
                "request-1"
            ),
            ClaudeResetClaimOutcome::Uncertain
        );
        // Second call with the same request id must not make any network call.
        assert_eq!(
            service.claim(
                Some(expiry()),
                Some(ResetCreditKind::Fable),
                ACCOUNT_ID,
                "request-1"
            ),
            ClaudeResetClaimOutcome::Uncertain
        );
        assert_eq!(
            service.claim(
                Some(expiry()),
                Some(ResetCreditKind::Fable),
                ACCOUNT_ID,
                "new-request"
            ),
            ClaudeResetClaimOutcome::Uncertain
        );
        assert_all_requests_made();
    }
}

#[test]
fn maps_cooldown_and_not_limited_outcomes_without_retrying() {
    for (result, expected) in [
        ("cooldown", ClaudeResetClaimOutcome::Cooldown),
        ("not_limited", ClaudeResetClaimOutcome::NothingToReset),
        ("already_used", ClaudeResetClaimOutcome::Success),
    ] {
        let service = scripted_service(vec![
            ("profile", "stale-token", Some((200, profile_body()))),
            ("status", "stale-token", Some((200, status_body()))),
            (
                "redeem",
                "stale-token",
                Some((200, json!({"result": result}))),
            ),
        ]);
        assert_eq!(
            service.claim(
                Some(expiry()),
                Some(ResetCreditKind::Fable),
                ACCOUNT_ID,
                "request-1"
            ),
            expected
        );
        assert_all_requests_made();
    }
}

#[test]
fn refuses_a_stale_card_after_the_cli_switches_accounts() {
    let service = scripted_service(vec![]);
    SCRIPT.with(|script| script.borrow_mut().active_identity = Some("account-b".into()));
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Failed
    );
    assert_all_requests_made();
}

#[test]
fn skips_credentials_of_another_account_even_when_its_grant_would_match() {
    let mut other_profile = profile_body();
    other_profile["account"]["uuid"] = json!("account-b");
    let service = scripted_service(vec![
        ("profile", "stale-token", Some((200, other_profile))),
        ("profile", "valid-token", Some((200, profile_body()))),
        ("status", "valid-token", Some((200, status_body()))),
        (
            "redeem",
            "valid-token",
            Some((200, json!({"result":"reset"}))),
        ),
    ]);
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Success
    );
    assert_all_requests_made();
}

#[test]
fn refuses_login_switch_during_the_status_request() {
    let service = scripted_service(vec![
        ("profile", "stale-token", Some((200, profile_body()))),
        ("status", "stale-token", Some((200, status_body()))),
    ]);
    SCRIPT.with(|script| script.borrow_mut().switch_on_status = true);
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Failed
    );
    assert_all_requests_made();
}

#[test]
fn retries_the_pinned_grant_after_same_account_token_rotation() {
    let service = scripted_service(vec![
        ("profile", "stale-token", Some((200, profile_body()))),
        ("status", "stale-token", Some((200, status_body()))),
        ("redeem", "stale-token", Some((401, Value::Null))),
    ]);
    SCRIPT.with(|script| script.borrow_mut().logins.truncate(1));
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Failed
    );
    SCRIPT.with(|script| {
        let mut script = script.borrow_mut();
        script.logins[0].token = "renewed-token".into();
        script.logins[0].owner_byte = 9;
        script.requests.extend([
            ("profile", "renewed-token", Some((200, profile_body()))),
            (
                "redeem",
                "renewed-token",
                Some((200, json!({"result":"reset"}))),
            ),
        ]);
    });
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Success
    );
    assert_all_requests_made();
}

#[test]
fn retries_another_login_of_the_same_account_after_redeem_unauthorized() {
    let service = scripted_service(vec![
        ("profile", "stale-token", Some((200, profile_body()))),
        ("status", "stale-token", Some((200, status_body()))),
        ("redeem", "stale-token", Some((401, Value::Null))),
        ("profile", "valid-token", Some((200, profile_body()))),
        (
            "redeem",
            "valid-token",
            Some((200, json!({"result":"reset"}))),
        ),
    ]);
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Success
    );
    assert_all_requests_made();
}

#[test]
fn refuses_a_different_organization_on_retry() {
    let mut other_profile = profile_body();
    other_profile["organization"]["uuid"] = json!("650e8400-e29b-41d4-a716-446655440000");
    let service = scripted_service(vec![
        ("profile", "stale-token", Some((200, profile_body()))),
        ("status", "stale-token", Some((200, status_body()))),
        ("redeem", "stale-token", Some((401, Value::Null))),
        ("profile", "valid-token", Some((200, other_profile))),
    ]);
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Failed
    );
    assert_all_requests_made();
}

#[test]
fn profile_failures_do_not_hide_later_renewed_credentials_for_a_pinned_grant() {
    for reply in [
        None,
        Some((429, Value::Null)),
        Some((503, Value::Null)),
        Some((200, json!({"account":{"uuid":ACCOUNT_ID}}))),
        Some((
            200,
            json!({"account":{"uuid":ACCOUNT_ID},"organization":{"uuid":"invalid"}}),
        )),
    ] {
        let service = scripted_service(vec![
            ("profile", "stale-token", Some((200, profile_body()))),
            ("status", "stale-token", Some((200, status_body()))),
            ("redeem", "stale-token", Some((401, Value::Null))),
        ]);
        SCRIPT.with(|script| script.borrow_mut().logins.truncate(1));
        assert_eq!(
            service.claim(
                Some(expiry()),
                Some(ResetCreditKind::Fable),
                ACCOUNT_ID,
                "request-1"
            ),
            ClaudeResetClaimOutcome::Failed
        );
        SCRIPT.with(|script| {
            let mut script = script.borrow_mut();
            script.logins.push(auth::ClaudeCredential {
                token: "renewed-token".into(),
                owner_byte: 9,
                inference_only: false,
            });
            script.requests.extend([
                ("profile", "stale-token", reply),
                ("profile", "renewed-token", Some((200, profile_body()))),
                (
                    "redeem",
                    "renewed-token",
                    Some((200, json!({"result":"reset"}))),
                ),
            ]);
        });
        assert_eq!(
            service.claim(
                Some(expiry()),
                Some(ResetCreditKind::Fable),
                ACCOUNT_ID,
                "request-1"
            ),
            ClaudeResetClaimOutcome::Success
        );
        assert_all_requests_made();
    }
}

#[test]
fn exhausted_profile_candidates_fail_without_redeeming() {
    let service = scripted_service(vec![
        ("profile", "stale-token", Some((429, Value::Null))),
        ("profile", "valid-token", Some((503, Value::Null))),
    ]);
    assert_eq!(
        service.claim(
            Some(expiry()),
            Some(ResetCreditKind::Fable),
            ACCOUNT_ID,
            "request-1"
        ),
        ClaudeResetClaimOutcome::Failed
    );
    assert_all_requests_made();
}
