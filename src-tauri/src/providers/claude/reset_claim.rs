use std::{collections::HashMap, sync::Mutex};

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;

use crate::models::ResetCreditKind;

use super::{
    accounts::{identity_for_scope, profile_matches_identity},
    auth::{load_candidates, oauth_config, ClaudeCredentialScope, ClaudeOAuthConfig},
    client::ClaudeClient,
    config,
    mapper::{classify_reset_kind, is_valid_grant_id},
};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeResetClaimOutcome {
    Success,
    NothingToReset,
    NoCredit,
    Cooldown,
    Uncertain,
    Failed,
}

pub struct ClaudeResetClaimService {
    client: ClaudeClient,
    matched_grants: Mutex<HashMap<String, MatchedGrant>>,
}

/// A grant matched for a claim, pinned to its organization and owner account so retries
/// with the same `request_id` never spend another grant or target another login.
#[derive(Debug, Clone)]
struct MatchedGrant {
    grant_id: String,
    organization_uuid: String,
    owner_identity: String,
    expiry: Option<DateTime<Utc>>,
    kind: Option<ResetCreditKind>,
    uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GrantSelection {
    Matched(String),
    NotLimited,
    Cooldown,
    NoCredit,
}

impl ClaudeResetClaimService {
    pub fn new() -> Result<Self, super::ClaudeError> {
        let client =
            ClaudeClient::new_no_redirect()?.for_provider("claude", config::settings_path());
        Ok(Self {
            client,
            matched_grants: Mutex::new(HashMap::new()),
        })
    }

    pub fn claim(
        &self,
        expiry: Option<DateTime<Utc>>,
        kind: Option<ResetCreditKind>,
        claim_account: &str,
        redeem_request_id: &str,
    ) -> ClaudeResetClaimOutcome {
        let redeem_request_id = redeem_request_id.trim();
        let scope = ClaudeCredentialScope::Standard;
        let owner_is_current = || identity_for_scope(&scope).as_deref() == Some(claim_account);
        if !is_valid_request_id(redeem_request_id)
            || claim_account.is_empty()
            || !owner_is_current()
        {
            return ClaudeResetClaimOutcome::Failed;
        }
        let config = match oauth_config() {
            Ok(config) => config,
            Err(_) => {
                crate::app_error!("provider:claude", "reset claim has invalid OAuth settings");
                return ClaudeResetClaimOutcome::Failed;
            }
        };
        let candidates = load_candidates(&scope)
            .into_iter()
            .filter(|candidate| !candidate.inference_only && candidate.has_profile_scope())
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            crate::app_error!("provider:claude", "reset claim has no usable credentials");
            return ClaudeResetClaimOutcome::Failed;
        }

        let mut matched = {
            let Ok(cache) = self.matched_grants.lock() else {
                return ClaudeResetClaimOutcome::Failed;
            };
            // A new UI request ID must not bypass an unresolved debit for this same offering.
            if cache.values().any(|grant| {
                grant.uncertain
                    && grant.owner_identity == claim_account
                    && grant.expiry == expiry
                    && grant.kind == kind
            }) {
                return ClaudeResetClaimOutcome::Uncertain;
            }
            cache.get(redeem_request_id).cloned()
        };
        if let Some(cached) = &matched {
            if cached.owner_identity != claim_account {
                return ClaudeResetClaimOutcome::Failed;
            }
            if cached.uncertain {
                return ClaudeResetClaimOutcome::Uncertain;
            }
        }

        // A token fingerprint identifies a credential generation, not an account. Verify each
        // candidate's server profile so renewed tokens can retry the pinned grant safely.
        for candidate in candidates {
            let Some(token) = candidate.access_token() else {
                continue;
            };
            let organization_uuid =
                match self.fetch_organization_uuid(token, &config, claim_account) {
                    Ok(Some(uuid)) => uuid,
                    // Profile requests are read-only; an unusable candidate must not hide
                    // a later renewed token for the same pinned owner and grant.
                    Ok(None) | Err(()) => continue,
                };
            if let Some(cached) = &matched {
                if cached.organization_uuid != organization_uuid {
                    continue;
                }
            } else {
                let (status, body) = match self.client.fetch_reset_status(token, &config) {
                    Ok(response) => response,
                    Err(_) => {
                        crate::app_error!("provider:claude", "reset claim status request failed");
                        return ClaudeResetClaimOutcome::Failed;
                    }
                };
                if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                    continue;
                }
                if !status.is_success() || !body.is_object() {
                    crate::app_error!(
                        "provider:claude",
                        "reset claim status request returned HTTP {}",
                        status.as_u16()
                    );
                    return ClaudeResetClaimOutcome::Failed;
                }
                let grant_id = match select_grant(&body, expiry, kind, Utc::now()) {
                    GrantSelection::Matched(grant_id) => grant_id,
                    GrantSelection::NotLimited => return ClaudeResetClaimOutcome::NothingToReset,
                    GrantSelection::Cooldown => return ClaudeResetClaimOutcome::Cooldown,
                    GrantSelection::NoCredit => return ClaudeResetClaimOutcome::NoCredit,
                };
                let grant = MatchedGrant {
                    grant_id,
                    organization_uuid: organization_uuid.clone(),
                    owner_identity: claim_account.to_owned(),
                    expiry,
                    kind,
                    uncertain: false,
                };
                let Ok(mut cache) = self.matched_grants.lock() else {
                    return ClaudeResetClaimOutcome::Failed;
                };
                if cache.len() >= 256 {
                    cache.retain(|_, grant| grant.uncertain);
                    if cache.len() >= 256 {
                        return ClaudeResetClaimOutcome::Failed;
                    }
                }
                cache.insert(redeem_request_id.to_owned(), grant.clone());
                matched = Some(grant);
            }
            // The CLI may have switched logins while the read-only requests were in flight.
            if !owner_is_current() {
                return ClaudeResetClaimOutcome::Failed;
            }
            let Some(grant) = &matched else {
                return ClaudeResetClaimOutcome::Failed;
            };
            let response = self.client.redeem_reset_credit(
                token,
                &config,
                &grant.organization_uuid,
                &grant.grant_id,
                redeem_request_id,
            );
            let (status, body) = match response {
                Ok(response) => response,
                Err(_) => {
                    self.mark_uncertain(redeem_request_id);
                    crate::app_error!("provider:claude", "reset claim redeem request failed");
                    return ClaudeResetClaimOutcome::Uncertain;
                }
            };
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                continue;
            }
            let (outcome, uncertain) = outcome_from_redeem(status, &body);
            if uncertain {
                self.mark_uncertain(redeem_request_id);
            }
            if outcome == ClaudeResetClaimOutcome::Failed {
                crate::app_error!(
                    "provider:claude",
                    "reset claim redeem returned an unsupported response (HTTP {})",
                    status.as_u16()
                );
            }
            return outcome;
        }
        ClaudeResetClaimOutcome::Failed
    }

    fn fetch_organization_uuid(
        &self,
        token: &str,
        config: &ClaudeOAuthConfig,
        expected_identity: &str,
    ) -> Result<Option<String>, ()> {
        let (status, body) = self.client.fetch_profile(token, config).map_err(|_| {
            crate::app_error!("provider:claude", "reset claim profile request failed");
        })?;
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Ok(None);
        }
        if !status.is_success() {
            crate::app_error!(
                "provider:claude",
                "reset claim profile request returned HTTP {}",
                status.as_u16()
            );
            return Err(());
        }
        if !profile_matches_identity(&body, expected_identity) {
            return Ok(None);
        }
        let Some(uuid) = extract_organization_uuid(&body) else {
            crate::app_error!(
                "provider:claude",
                "reset claim profile missing organization UUID"
            );
            return Err(());
        };
        Ok(Some(uuid))
    }

    fn mark_uncertain(&self, redeem_request_id: &str) {
        if let Ok(mut cache) = self.matched_grants.lock() {
            if let Some(entry) = cache.get_mut(redeem_request_id) {
                entry.uncertain = true;
            }
        }
    }
}

fn is_valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn extract_organization_uuid(profile: &Value) -> Option<String> {
    let raw = profile.pointer("/organization/uuid")?.as_str()?.trim();
    is_valid_uuid(raw).then(|| raw.to_ascii_lowercase())
}

fn is_valid_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    let bytes = value.as_bytes();
    let dashes = [8, 13, 18, 23];
    for (index, &byte) in bytes.iter().enumerate() {
        if dashes.contains(&index) {
            if byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    !value.bytes().all(|byte| byte == b'0' || byte == b'-')
}

fn select_grant(
    body: &Value,
    expiry: Option<DateTime<Utc>>,
    kind: Option<ResetCreditKind>,
    now: DateTime<Utc>,
) -> GrantSelection {
    let Some(block) = body.get("cedar_ember").filter(|value| value.is_object()) else {
        return GrantSelection::NoCredit;
    };
    if block.get("eligible").and_then(Value::as_bool) != Some(true) {
        return GrantSelection::NoCredit;
    }
    if block
        .get("cooldown_until")
        .and_then(parse_timestamp)
        .is_some_and(|until| until > now)
    {
        return GrantSelection::Cooldown;
    }
    let Some(grants) = block.get("grants").and_then(Value::as_array) else {
        return GrantSelection::NoCredit;
    };
    let at_limit = block
        .get("at_limit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next_grant_id = block
        .get("next_grant_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut matched_requires_limit = false;
    for grant in grants {
        let Some(id) = grant
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| is_valid_grant_id(id))
        else {
            continue;
        };
        if grant.get("paused").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let left = grant
            .get("resets_left")
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .filter(|value| value.is_finite() && *value >= 1.0);
        if left.is_none() {
            continue;
        }
        if grant
            .get("starts_at")
            .and_then(parse_timestamp)
            .is_some_and(|start| start > now)
        {
            continue;
        }
        let ends_at = grant.get("ends_at").and_then(parse_timestamp);
        if ends_at.is_some_and(|end| end <= now) {
            continue;
        }
        if !expiry_matches(ends_at, expiry) {
            continue;
        }
        if let Some(expected_kind) = kind {
            if classify_reset_kind(grant) != expected_kind {
                continue;
            }
        }
        if let Some(next) = next_grant_id {
            if id != next {
                continue;
            }
        }
        if grant.get("usable_now").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        if grant
            .get("blocking")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        {
            continue;
        }
        if grant.get("use_requires_limit").and_then(Value::as_bool) == Some(true) && !at_limit {
            matched_requires_limit = true;
            continue;
        }
        return GrantSelection::Matched(id.to_owned());
    }
    if matched_requires_limit {
        GrantSelection::NotLimited
    } else {
        GrantSelection::NoCredit
    }
}

fn expiry_matches(candidate: Option<DateTime<Utc>>, target: Option<DateTime<Utc>>) -> bool {
    match (candidate, target) {
        (Some(candidate), Some(target)) => {
            candidate
                .signed_duration_since(target)
                .num_milliseconds()
                .abs()
                < 1_000
        }
        (None, None) => true,
        _ => false,
    }
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .map(|date| date.to_utc())
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|date| date.and_utc())
            });
    }
    let seconds = value.as_f64().filter(|value| value.is_finite())?;
    let seconds = if seconds.abs() < 10_000_000_000.0 {
        seconds
    } else {
        seconds / 1000.0
    };
    DateTime::from_timestamp_millis((seconds * 1_000.0).round() as i64)
}

/// Maps Anthropic's `/api/organizations/{org}/reset_rate_limits` response to an outcome and
/// whether the server outcome is uncertain (and must block retries with the same request ID).
fn outcome_from_redeem(status: StatusCode, body: &Value) -> (ClaudeResetClaimOutcome, bool) {
    if !status.is_success() {
        return (ClaudeResetClaimOutcome::Uncertain, true);
    }
    let reason = body.get("reason").and_then(Value::as_str);
    if matches!(reason, Some("stamp_indeterminate" | "reset_unconfirmed")) {
        return (ClaudeResetClaimOutcome::Uncertain, true);
    }
    match body.get("result").and_then(Value::as_str) {
        Some("reset" | "already_used") => (ClaudeResetClaimOutcome::Success, false),
        Some("not_limited") => (ClaudeResetClaimOutcome::NothingToReset, false),
        Some("cooldown") => (ClaudeResetClaimOutcome::Cooldown, false),
        Some("ineligible") => {
            let outcome = match reason {
                Some("no_grant" | "unknown_grant" | "not_next_grant") => {
                    ClaudeResetClaimOutcome::NoCredit
                }
                _ => ClaudeResetClaimOutcome::Failed,
            };
            (outcome, false)
        }
        Some("unavailable") => (ClaudeResetClaimOutcome::Failed, false),
        _ => (ClaudeResetClaimOutcome::Uncertain, true),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use reqwest::StatusCode;
    use serde_json::json;

    use crate::models::ResetCreditKind;

    use super::{
        extract_organization_uuid, is_valid_request_id, outcome_from_redeem, select_grant,
        ClaudeResetClaimOutcome, GrantSelection,
    };

    #[test]
    fn validates_request_ids_and_organization_uuids() {
        assert!(is_valid_request_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!is_valid_request_id(""));
        assert!(!is_valid_request_id("bad/id"));
        assert!(!is_valid_request_id(&"a".repeat(65)));

        assert_eq!(
            extract_organization_uuid(&json!({
                "organization": {"uuid": "550e8400-e29b-41d4-a716-446655440000"}
            }))
            .as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert!(extract_organization_uuid(&json!({
            "organization": {"uuid": "../other"}
        }))
        .is_none());
        assert!(extract_organization_uuid(&json!({
            "organization": {"uuid": "00000000-0000-0000-0000-000000000000"}
        }))
        .is_none());
    }

    #[test]
    fn selects_only_the_matching_usable_next_grant() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap();
        let fable_expiry = Utc.with_ymd_and_hms(2026, 11, 17, 7, 59, 0).unwrap();
        let full_expiry = Utc.with_ymd_and_hms(2026, 12, 10, 22, 0, 0).unwrap();
        let body = json!({
            "cedar_ember": {
                "eligible": true,
                "at_limit": false,
                "next_grant_id": "fable_grant",
                "grants": [
                    {
                        "id": "fable_grant",
                        "resets_left": 1,
                        "ends_at": "2026-11-17T07:59:00+00:00",
                        "gives_back_weekly": true,
                        "clears": ["five_hour", "seven_day_overage_included"],
                        "paused": false,
                        "usable_now": true,
                        "use_requires_limit": false,
                        "blocking": []
                    },
                    {
                        "id": "full_grant",
                        "resets_left": 1,
                        "ends_at": "2026-12-10T22:00:00+00:00",
                        "gives_back_weekly": false,
                        "clears": ["five_hour", "seven_day"],
                        "paused": false,
                        "usable_now": true,
                        "use_requires_limit": false,
                        "blocking": []
                    }
                ]
            }
        });

        assert_eq!(
            select_grant(&body, Some(fable_expiry), Some(ResetCreditKind::Fable), now),
            GrantSelection::Matched("fable_grant".into())
        );
        // A grant that is not `next_grant_id` cannot be claimed out of order.
        assert_eq!(
            select_grant(&body, Some(full_expiry), Some(ResetCreditKind::Full), now),
            GrantSelection::NoCredit
        );
        // A mismatched kind is rejected even at the same expiry.
        assert_eq!(
            select_grant(&body, Some(fable_expiry), Some(ResetCreditKind::Full), now),
            GrantSelection::NoCredit
        );
    }

    #[test]
    fn enforces_limit_requirement_and_cooldown() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap();
        let expiry = Utc.with_ymd_and_hms(2026, 11, 17, 7, 59, 0).unwrap();
        let requires_limit = json!({
            "cedar_ember": {
                "eligible": true,
                "at_limit": false,
                "next_grant_id": "grant_1",
                "grants": [{
                    "id": "grant_1",
                    "resets_left": 1,
                    "ends_at": "2026-11-17T07:59:00Z",
                    "clears": ["five_hour", "seven_day"],
                    "paused": false,
                    "usable_now": true,
                    "use_requires_limit": true,
                    "blocking": []
                }]
            }
        });
        assert_eq!(
            select_grant(&requires_limit, Some(expiry), None, now),
            GrantSelection::NotLimited
        );

        let cooldown = json!({
            "cedar_ember": {
                "eligible": true,
                "cooldown_until": "2026-11-11T01:00:00Z",
                "grants": []
            }
        });
        assert_eq!(
            select_grant(&cooldown, Some(expiry), None, now),
            GrantSelection::Cooldown
        );
    }

    #[test]
    fn maps_redeem_outcomes_and_flags_uncertain_responses() {
        assert_eq!(
            outcome_from_redeem(StatusCode::OK, &json!({"result": "reset"})),
            (ClaudeResetClaimOutcome::Success, false)
        );
        assert_eq!(
            outcome_from_redeem(StatusCode::OK, &json!({"result": "already_used"})),
            (ClaudeResetClaimOutcome::Success, false)
        );
        assert_eq!(
            outcome_from_redeem(StatusCode::OK, &json!({"result": "not_limited"})),
            (ClaudeResetClaimOutcome::NothingToReset, false)
        );
        assert_eq!(
            outcome_from_redeem(StatusCode::OK, &json!({"result": "cooldown"})),
            (ClaudeResetClaimOutcome::Cooldown, false)
        );
        assert_eq!(
            outcome_from_redeem(
                StatusCode::OK,
                &json!({"result": "ineligible", "reason": "not_next_grant"})
            ),
            (ClaudeResetClaimOutcome::NoCredit, false)
        );
        assert_eq!(
            outcome_from_redeem(
                StatusCode::OK,
                &json!({"result": "reset", "reason": "stamp_indeterminate"})
            ),
            (ClaudeResetClaimOutcome::Uncertain, true)
        );
        assert_eq!(
            outcome_from_redeem(StatusCode::BAD_GATEWAY, &json!({"result": "reset"})),
            (ClaudeResetClaimOutcome::Uncertain, true)
        );
    }
}
