//! Exercise the production claim state machine with scripted credential and network boundaries.
//! Including the module keeps these tests off real auth files, Keychain and reset-credit APIs,
//! without adding test-only hooks to the production service.
#![allow(dead_code)]

use std::{cell::RefCell, collections::VecDeque};

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::{json, Value};

#[macro_export]
macro_rules! app_error {
    ($($args:tt)*) => {};
}

type CodexError = ();

mod auth {
    #[derive(Clone)]
    pub struct CodexAuthState {
        pub access_token: String,
        pub account_id: Option<String>,
    }

    impl CodexAuthState {
        pub fn load_candidates() -> Result<Vec<Self>, super::CodexError> {
            Ok(super::SCRIPT.with(|script| script.borrow().logins.clone()))
        }

        pub fn account_identity(&self) -> Option<String> {
            self.account_id.clone()
        }
    }
}

mod client {
    pub struct CodexClient;

    impl CodexClient {
        pub fn new() -> Result<Self, super::CodexError> {
            Ok(Self)
        }

        pub fn fetch_reset_credits(
            &self,
            token: &str,
            account: Option<&str>,
        ) -> Result<super::Response, super::CodexError> {
            super::respond("list", token, account)
        }

        pub fn consume_reset_credit(
            &self,
            token: &str,
            account: Option<&str>,
            credit: &str,
            request: &str,
        ) -> Result<super::Response, super::CodexError> {
            assert_eq!(credit, "credit-a", "retry must keep the matched credit");
            assert_eq!(request, "request-a", "retry must keep the idempotency key");
            super::respond("consume", token, account)
        }
    }
}

#[path = "../src/providers/codex/reset_claim.rs"]
mod reset_claim;

use reset_claim::{CodexResetClaimService, ResetClaimOutcome};

struct Response {
    status: StatusCode,
    body: Value,
}

// None represents a transport error, rather than an HTTP error response.
type ExpectedRequest = (&'static str, &'static str, Option<u16>);

#[derive(Default)]
struct Script {
    logins: Vec<auth::CodexAuthState>,
    requests: VecDeque<ExpectedRequest>,
}

thread_local! {
    static SCRIPT: RefCell<Script> = RefCell::new(Script::default());
}

fn respond(method: &str, token: &str, account: Option<&str>) -> Result<Response, CodexError> {
    SCRIPT.with(|script| {
        let mut script = script.borrow_mut();
        let (expected_method, expected_token, status) = script
            .requests
            .pop_front()
            .expect("claim made an unexpected request");
        assert_eq!((method, token), (expected_method, expected_token));
        let login = script
            .logins
            .iter()
            .find(|login| login.access_token == token)
            .unwrap();
        assert_eq!(account, login.account_id.as_deref());
        let status = status.ok_or(())?;
        Ok(Response {
            status: StatusCode::from_u16(status).unwrap(),
            body: if method == "list" {
                json!({"credits": [{"id": "credit-a", "expires_at": expiry().timestamp()}]})
            } else {
                json!({"code": "reset"})
            },
        })
    })
}

fn expiry() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

fn scripted_service(requests: &[ExpectedRequest]) -> CodexResetClaimService {
    SCRIPT.with(|script| {
        *script.borrow_mut() = Script {
            // An unrelated account sits between the owner's two stored logins so an
            // overly broad fallback cannot pass merely by reaching the valid login.
            logins: [("stale-a", "a"), ("foreign-b", "b"), ("valid-a", "a")]
                .into_iter()
                .map(|(token, account)| auth::CodexAuthState {
                    access_token: token.into(),
                    account_id: Some(account.into()),
                })
                .collect(),
            requests: requests.iter().copied().collect(),
        };
    });
    CodexResetClaimService::new().unwrap()
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
fn cached_claim_retries_the_owners_other_login_after_a_transport_failure() {
    for rejected in [401, 403] {
        let service = scripted_service(&[
            ("list", "stale-a", Some(401)),
            ("list", "foreign-b", Some(401)),
            ("list", "valid-a", Some(200)),
            ("consume", "valid-a", None),
            // Retry must reuse the cached credit, skip the unrelated account, and
            // continue after either auth rejection instead of stopping at stale-a.
            ("consume", "stale-a", Some(rejected)),
            ("consume", "valid-a", Some(200)),
        ]);

        assert_eq!(
            service.claim(expiry(), "request-a"),
            ResetClaimOutcome::Failed
        );
        assert_eq!(
            service.claim(expiry(), "request-a"),
            ResetClaimOutcome::Success
        );
        assert_all_requests_made();
    }
}

#[test]
fn first_claim_prefers_the_matching_login_and_only_falls_back_within_its_owner() {
    let service = scripted_service(&[
        ("list", "stale-a", Some(401)),
        ("list", "foreign-b", Some(401)),
        ("list", "valid-a", Some(200)),
        ("consume", "valid-a", Some(200)),
    ]);
    assert_eq!(
        service.claim(expiry(), "request-a"),
        ResetClaimOutcome::Success
    );
    assert_all_requests_made();

    let service = scripted_service(&[
        ("list", "stale-a", Some(200)),
        ("consume", "stale-a", Some(403)),
        ("consume", "valid-a", Some(200)),
    ]);
    assert_eq!(
        service.claim(expiry(), "request-a"),
        ResetClaimOutcome::Success
    );
    assert_all_requests_made();
}

#[test]
fn cached_claim_does_not_consume_with_another_account_after_its_owner_disappears() {
    let service = scripted_service(&[("list", "stale-a", Some(200)), ("consume", "stale-a", None)]);
    assert_eq!(
        service.claim(expiry(), "request-a"),
        ResetClaimOutcome::Failed
    );
    SCRIPT.with(|script| {
        script
            .borrow_mut()
            .logins
            .retain(|login| login.account_id.as_deref() == Some("b"));
    });

    assert_eq!(
        service.claim(expiry(), "request-a"),
        ResetClaimOutcome::Failed
    );
    assert_all_requests_made();
}
