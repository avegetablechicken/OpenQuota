mod auth;
mod client;
mod discovery;
mod mapper;

use std::path::PathBuf;

use chrono::Utc;
use serde_json::{json, Value};
use thiserror::Error;

use crate::models::{
    MetricDefinition, MetricSection, ProviderDefinition, ProviderSnapshot, UsageHistories,
};

use self::{
    auth::{
        credential_fingerprint, load_token, write_back_renewal, AccessTokenCache, SharedLogin,
        SystemLogin,
    },
    client::{AntigravityClient, CloudOutcome, CloudUserAgent, RefreshOutcome},
    discovery::discover,
    mapper::{
        build_legacy_quotas, has_gemini_pool, merge_model_pools, parse_cloud_models,
        parse_command_model_configs, parse_plan, parse_quota_buckets, parse_quota_summary,
        parse_user_status,
    },
};

const QUOTA_SUMMARY_PATH: &str = "/v1internal:retrieveUserQuotaSummary";
const FETCH_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
const LOAD_CODE_ASSIST_PATH: &str = "/v1internal:loadCodeAssist";
const RETRIEVE_QUOTA_PATH: &str = "/v1internal:retrieveUserQuota";

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "antigravity".into(),
        display_name: "Antigravity".into(),
        short_name: "A".into(),
        fallback_enabled: false,
        local_usage_source_note: None,
        links: vec![],
        // Antigravity reports its available pools explicitly, including idle five-hour pools.
        // Opt every window out of the starts-on-use availability heuristic.
        metrics: vec![
            MetricDefinition::quota(
                "antigravity.geminiPro",
                "Session",
                "geminiPro",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::quota(
                "antigravity.geminiWeekly",
                "Weekly",
                "geminiWeekly",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "W",
            ),
            MetricDefinition::quota(
                "antigravity.claude",
                "Claude",
                "claude",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "C",
            ),
            MetricDefinition::quota(
                "antigravity.claudeWeekly",
                "Claude Weekly",
                "claudeWeekly",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "CW",
            ),
        ],
    }
}

#[derive(Debug, Error)]
pub enum AntigravityError {
    #[error("Start Antigravity or run `agy` and try again.")]
    NotSignedIn,
    #[error("Antigravity sign-in expired. Open Antigravity or run `agy` to refresh.")]
    AuthExpired,
    #[error("Antigravity credentials could not be read from secure storage.")]
    CredentialStoreUnreadable,
    #[error("Antigravity credentials are invalid. Sign in again in Antigravity or `agy`.")]
    InvalidCredentialData,
    #[error("Antigravity usage is temporarily unavailable. Try again shortly.")]
    Unavailable,
}

pub struct AntigravityProvider {
    client: AntigravityClient,
    access_token_cache: AccessTokenCache,
    login: Box<dyn SharedLogin>,
}

impl AntigravityProvider {
    pub fn new(cache_path: PathBuf) -> Result<Self, AntigravityError> {
        Ok(Self {
            client: AntigravityClient::new()?,
            access_token_cache: AccessTokenCache::new(cache_path),
            login: Box::new(SystemLogin),
        })
    }

    fn refresh_inner(&self) -> Result<ProviderSnapshot, AntigravityError> {
        if let Some(server) = discover() {
            if let Some(summary) = self
                .client
                .call_language_server(&server, "RetrieveUserQuotaSummary")
            {
                if let Some(quotas) = parse_quota_summary(&summary) {
                    let plan = self
                        .client
                        .call_language_server(&server, "GetUserStatus")
                        .as_ref()
                        .and_then(parse_plan);
                    return Ok(snapshot(plan, quotas));
                }
            }

            if let Some(status) = self.client.call_language_server(&server, "GetUserStatus") {
                if let Some((plan, configs)) = parse_user_status(&status) {
                    let quotas = build_legacy_quotas(configs);
                    if !quotas.is_empty() {
                        return Ok(snapshot(plan, quotas));
                    }
                }
                if let Some(configs) = self
                    .client
                    .call_language_server(&server, "GetCommandModelConfigs")
                    .as_ref()
                    .and_then(parse_command_model_configs)
                {
                    let quotas = build_legacy_quotas(configs);
                    if !quotas.is_empty() {
                        return Ok(snapshot(None, quotas));
                    }
                }
            }
        }
        self.refresh_from_login()
    }

    fn refresh_from_login(&self) -> Result<ProviderSnapshot, AntigravityError> {
        let keychain = match load_token(self.login.as_ref())? {
            Some(token) => token,
            None => {
                self.access_token_cache.discard();
                return Err(AntigravityError::NotSignedIn);
            }
        };
        let now = Utc::now();
        let cached = self
            .access_token_cache
            .load(keychain.refresh_token.as_deref(), now);
        let (access_tokens, access_is_expired) = access_token_candidates(&keychain, cached, now);

        let mut saw_auth_failure = access_is_expired;
        let mut saw_rejection = false;
        let mut saw_unavailable = false;
        for access_token in &access_tokens {
            match self.fetch_remote(access_token) {
                Ok(snapshot) => return Ok(snapshot),
                Err(AntigravityError::AuthExpired) => {
                    saw_auth_failure = true;
                    saw_rejection = true;
                }
                Err(AntigravityError::Unavailable) => saw_unavailable = true,
                Err(error) => return Err(error),
            }
        }
        // Another program may have renewed the login since it was read; use its token rather
        // than renewing again.
        if let Some(access_token) = saw_rejection
            .then(|| self.renewed_elsewhere(keychain.refresh_token.as_deref(), &access_tokens))
            .flatten()
        {
            match self.fetch_remote(&access_token) {
                Ok(snapshot) => return Ok(snapshot),
                Err(AntigravityError::AuthExpired) => {}
                Err(AntigravityError::Unavailable) => saw_unavailable = true,
                Err(error) => return Err(error),
            }
        }

        let refresh_token = keychain
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty());
        if let Some(refresh_token) = refresh_token.filter(|_| {
            should_refresh_access_token(saw_auth_failure, !access_tokens.is_empty(), true)
        }) {
            match self.client.refresh_google_token(refresh_token) {
                RefreshOutcome::Refreshed {
                    access_token,
                    expires_in_seconds,
                } => {
                    crate::app_info!("auth:antigravity", "token refresh succeeded");
                    let now = Utc::now();
                    self.access_token_cache.store(
                        &access_token,
                        expires_in_seconds,
                        Some(refresh_token),
                        now,
                    );
                    write_back_renewal(
                        self.login.as_ref(),
                        refresh_token,
                        &access_token,
                        expires_in_seconds,
                        now,
                    );
                    return match self.fetch_remote(&access_token) {
                        Err(AntigravityError::AuthExpired) => {
                            self.access_token_cache.discard();
                            Err(AntigravityError::AuthExpired)
                        }
                        result => result,
                    };
                }
                RefreshOutcome::AuthFailed => {
                    self.access_token_cache.discard();
                    return Err(AntigravityError::AuthExpired);
                }
                RefreshOutcome::Unavailable => return Err(AntigravityError::Unavailable),
            }
        }
        Err(credential_failure(
            saw_auth_failure,
            saw_unavailable,
            !access_tokens.is_empty(),
        ))
    }

    /// The shared login's access token when it was renewed for the same sign-in after `tried`
    /// were read and has not expired.
    fn renewed_elsewhere(&self, refresh_token: Option<&str>, tried: &[String]) -> Option<String> {
        let current = load_token(self.login.as_ref()).ok().flatten()?;
        if credential_fingerprint(current.refresh_token.as_deref())?
            != credential_fingerprint(refresh_token)?
        {
            return None;
        }
        access_token_candidates(&current, None, Utc::now())
            .0
            .into_iter()
            .find(|token| !tried.contains(token))
    }

    fn fetch_remote(&self, token: &str) -> Result<ProviderSnapshot, AntigravityError> {
        let summary = match self.client.cloud_code(
            QUOTA_SUMMARY_PATH,
            token,
            json!({}),
            CloudUserAgent::Antigravity,
        ) {
            CloudOutcome::Ok(value) => parse_quota_summary(&value),
            CloudOutcome::AuthFailed => return Err(AntigravityError::AuthExpired),
            CloudOutcome::Unavailable => None,
        };
        // An account whose Gemini models share no pool in the summary (Google AI Pro, say)
        // reports their five-hour window per model instead.
        if let Some(quotas) = summary.as_ref().filter(|quotas| has_gemini_pool(quotas)) {
            return Ok(snapshot(self.load_remote_plan(token), quotas.clone()));
        }

        let models = match self.client.cloud_code(
            FETCH_MODELS_PATH,
            token,
            json!({}),
            CloudUserAgent::Antigravity,
        ) {
            CloudOutcome::Ok(value) => build_legacy_quotas(parse_cloud_models(&value)),
            CloudOutcome::AuthFailed => return Err(AntigravityError::AuthExpired),
            CloudOutcome::Unavailable => Vec::new(),
        };
        let quotas = match summary {
            Some(summary) => merge_model_pools(summary, models),
            None => models,
        };
        if !quotas.is_empty() {
            return Ok(snapshot(self.load_remote_plan(token), quotas));
        }

        let (plan, project) = match self.client.cloud_code(
            LOAD_CODE_ASSIST_PATH,
            token,
            json!({}),
            CloudUserAgent::Antigravity,
        ) {
            CloudOutcome::Ok(value) => (
                remote_plan(&value),
                value
                    .get("cloudaicompanionProject")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned),
            ),
            CloudOutcome::AuthFailed => return Err(AntigravityError::AuthExpired),
            CloudOutcome::Unavailable => (None, None),
        };

        let body = project
            .as_ref()
            .map(|project| json!({"project": project}))
            .unwrap_or_else(|| json!({}));
        let mut quota =
            self.client
                .cloud_code(RETRIEVE_QUOTA_PATH, token, body, CloudUserAgent::Agy);
        if matches!(quota, CloudOutcome::Unavailable) && project.is_some() {
            quota =
                self.client
                    .cloud_code(RETRIEVE_QUOTA_PATH, token, json!({}), CloudUserAgent::Agy);
        }
        match quota {
            CloudOutcome::Ok(value) => {
                let quotas = build_legacy_quotas(parse_quota_buckets(&value));
                if !quotas.is_empty() {
                    return Ok(snapshot(plan, quotas));
                }
            }
            CloudOutcome::AuthFailed => return Err(AntigravityError::AuthExpired),
            CloudOutcome::Unavailable => {}
        }
        Err(AntigravityError::Unavailable)
    }

    fn load_remote_plan(&self, token: &str) -> Option<String> {
        match self.client.cloud_code(
            LOAD_CODE_ASSIST_PATH,
            token,
            json!({}),
            CloudUserAgent::Antigravity,
        ) {
            CloudOutcome::Ok(value) => remote_plan(&value),
            _ => None,
        }
    }
}

fn access_token_candidates(
    source: &auth::AntigravityToken,
    cached: Option<String>,
    now: chrono::DateTime<Utc>,
) -> (Vec<String>, bool) {
    let access_is_expired = source.access_token.as_ref().is_some_and(|_| {
        source
            .expiry
            .is_some_and(|expiry| expiry <= now + chrono::Duration::seconds(60))
    });
    let mut candidates = Vec::new();
    if !access_is_expired {
        if let Some(access_token) = source
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            candidates.push(access_token.to_owned());
        }
    }
    if let Some(cached) = cached.filter(|token| !token.trim().is_empty()) {
        if !candidates.iter().any(|token| token == &cached) {
            candidates.push(cached);
        }
    }
    (candidates, access_is_expired)
}

fn should_refresh_access_token(
    saw_auth_failure: bool,
    has_access_candidate: bool,
    has_refresh_token: bool,
) -> bool {
    has_refresh_token && (saw_auth_failure || !has_access_candidate)
}

fn credential_failure(
    saw_auth_failure: bool,
    saw_unavailable: bool,
    tried_access_token: bool,
) -> AntigravityError {
    if saw_auth_failure {
        AntigravityError::AuthExpired
    } else if saw_unavailable || tried_access_token {
        AntigravityError::Unavailable
    } else {
        AntigravityError::NotSignedIn
    }
}

fn remote_plan(value: &Value) -> Option<String> {
    let tier = ["/paidTier", "/currentTier"]
        .iter()
        .filter_map(|path| value.pointer(path))
        .find(|tier| tier.is_object())?;
    let raw = ["name", "id"]
        .iter()
        .find_map(|key| tier.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|raw| !raw.is_empty())?;
    for tier in ["Ultra", "Pro", "Free"] {
        if raw
            .to_ascii_lowercase()
            .contains(&tier.to_ascii_lowercase())
        {
            return Some(tier.into());
        }
    }
    Some(raw.strip_prefix("Google AI ").unwrap_or(raw).into())
}

fn snapshot(plan: Option<String>, quotas: Vec<crate::models::QuotaWindow>) -> ProviderSnapshot {
    ProviderSnapshot {
        accounts: None,
        provider_id: "antigravity".into(),
        plan,
        quotas,
        value_metrics: Vec::new(),
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage_histories: UsageHistories::default(),
        warnings: Vec::new(),
        refreshed_at: Utc::now(),
    }
}

fn has_refresh_source(
    has_credentials: bool,
    local_server_available: impl FnOnce() -> bool,
) -> bool {
    has_credentials || local_server_available()
}

impl crate::providers::UsageProvider for AntigravityProvider {
    fn definition(&self) -> ProviderDefinition {
        definition()
    }

    fn has_local_credentials(&self) -> bool {
        has_refresh_source(auth::has_local_credentials(self.login.as_ref()), || {
            discover().is_some()
        })
    }

    fn refresh(&self) -> Result<ProviderSnapshot, crate::providers::ProviderError> {
        self.refresh_inner().map_err(|error| {
            use crate::models::ProviderErrorKind as Kind;

            let kind = match error {
                AntigravityError::NotSignedIn
                | AntigravityError::AuthExpired
                | AntigravityError::InvalidCredentialData => Kind::Authentication,
                AntigravityError::CredentialStoreUnreadable => Kind::CredentialStorage,
                AntigravityError::Unavailable => Kind::Network,
            };
            crate::providers::ProviderError::from_display(kind, error)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
    };

    use base64::{engine::general_purpose::STANDARD, Engine};
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use serde_json::{json, Value};
    use tempfile::{tempdir, TempDir};

    use super::{
        access_token_candidates,
        auth::{AccessTokenCache, AntigravityToken, SharedLogin},
        client::AntigravityClient,
        credential_failure, has_refresh_source, should_refresh_access_token, AntigravityError,
        AntigravityProvider,
    };

    /// An in-memory shared login: each read takes the next scripted value, the last one staying;
    /// a write replaces them all unless writes fail.
    struct FakeLogin {
        reads: Mutex<Vec<Vec<u8>>>,
        writes: Mutex<Vec<Vec<u8>>>,
        fail_writes: bool,
    }

    impl FakeLogin {
        fn new(reads: Vec<Vec<u8>>, fail_writes: bool) -> Arc<Self> {
            Arc::new(Self {
                reads: Mutex::new(reads),
                writes: Mutex::new(Vec::new()),
                fail_writes,
            })
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl SharedLogin for Arc<FakeLogin> {
        fn read(&self) -> Result<Option<Vec<u8>>, String> {
            let mut reads = self.reads.lock().unwrap();
            Ok(match reads.len() {
                0 => None,
                1 => Some(reads[0].clone()),
                _ => Some(reads.remove(0)),
            })
        }

        fn write(&self, value: &[u8]) -> Result<(), String> {
            if self.fail_writes {
                return Err("The credential store could not be updated.".into());
            }
            self.writes.lock().unwrap().push(value.to_vec());
            *self.reads.lock().unwrap() = vec![value.to_vec()];
            Ok(())
        }
    }

    /// A local stand-in for Cloud Code and Google's token endpoint. Cloud Code accepts only
    /// `accepted`; the token endpoint renews to `renewed`. Every request is recorded as
    /// `<path> <bearer token>`.
    fn fake_google(
        accepted: &'static str,
        renewed: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                let head_end = loop {
                    let Ok(read) = stream.read(&mut chunk) else {
                        break None;
                    };
                    if read == 0 {
                        break None;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        break Some(end + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
                let header = |name: &str| {
                    head.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_owned())
                    })
                };
                let length = header("content-length")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                while request.len() < head_end + length {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&chunk[..read]),
                    }
                }
                let path = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let bearer = header("authorization")
                    .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned))
                    .unwrap_or_default();
                recorded.lock().unwrap().push(format!("{path} {bearer}"));
                let (status, body) = if path == "/token" {
                    (
                        200,
                        json!({"access_token": renewed, "expires_in": 3599}).to_string(),
                    )
                } else if bearer != accepted {
                    (401, "{}".to_owned())
                } else if path == super::QUOTA_SUMMARY_PATH {
                    (200, include_str!("fixtures/quota_summary.json").to_owned())
                } else {
                    (200, "{}".to_owned())
                };
                let reason = if status == 200 { "OK" } else { "Unauthorized" };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (base, requests)
    }

    fn provider(base: &str, login: &Arc<FakeLogin>) -> (AntigravityProvider, TempDir) {
        let directory = tempdir().unwrap();
        let provider = AntigravityProvider {
            client: AntigravityClient::with_endpoints(
                vec![base.to_owned()],
                format!("{base}/token"),
                std::time::Duration::from_secs(5),
            )
            .unwrap(),
            access_token_cache: AccessTokenCache::new(directory.path().join("auth.json")),
            login: Box::new(login.clone()),
        };
        (provider, directory)
    }

    /// agy's shared login, as go-keyring stores it.
    fn agy_login(access: &str, refresh: &str, expiry: DateTime<chrono::FixedOffset>) -> Vec<u8> {
        let json = json!({
            "token": {
                "access_token": access,
                "token_type": "Bearer",
                "refresh_token": refresh,
                "expiry": expiry.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                "expires_in": 3599
            },
            "auth_method": "oauth",
            "id_token_email": "user@example.com"
        })
        .to_string();
        format!("go-keyring-base64:{}", STANDARD.encode(json)).into_bytes()
    }

    fn decode_login(raw: &[u8]) -> Value {
        let text = std::str::from_utf8(raw).unwrap();
        let encoded = text.strip_prefix("go-keyring-base64:").unwrap();
        serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap()
    }

    fn offset(hours: i32) -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(hours * 3_600).unwrap()
    }

    fn token_requests(requests: &Arc<Mutex<Vec<String>>>) -> usize {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("/token "))
            .count()
    }

    #[test]
    fn renewal_is_written_back_in_the_shared_login_format() {
        let (base, requests) = fake_google("renewed", "renewed");
        let expired = (Utc::now() - Duration::hours(1)).with_timezone(&offset(8));
        let login = FakeLogin::new(vec![agy_login("expired", "refresh", expired)], false);
        let (provider, directory) = provider(&base, &login);

        let before = Utc::now();
        provider.refresh_from_login().unwrap();

        assert_eq!(token_requests(&requests), 1);
        let writes = login.writes();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].starts_with(b"go-keyring-base64:"));
        let mut saved = decode_login(&writes[0]);
        let expiry = saved["token"]["expiry"].as_str().unwrap().to_owned();
        let parsed = DateTime::parse_from_rfc3339(&expiry).unwrap();
        assert_eq!(parsed.offset(), &offset(8));
        assert!(parsed.to_utc() >= before + Duration::seconds(3_599));
        assert!(parsed.to_utc() <= Utc::now() + Duration::seconds(3_599));
        saved["token"]["expiry"] = json!("<expiry>");
        assert_eq!(
            saved,
            json!({
                "token": {
                    "access_token": "renewed",
                    "token_type": "Bearer",
                    "refresh_token": "refresh",
                    "expiry": "<expiry>",
                    "expires_in": 3599
                },
                "auth_method": "oauth",
                "id_token_email": "user@example.com"
            })
        );
        let cache = AccessTokenCache::new(directory.path().join("auth.json"));
        assert_eq!(
            cache.load(Some("refresh"), Utc::now()).as_deref(),
            Some("renewed")
        );
    }

    #[test]
    fn renewal_never_overwrites_a_different_sign_in() {
        let (base, requests) = fake_google("renewed", "renewed");
        let expired = (Utc::now() - Duration::hours(1)).fixed_offset();
        let other = agy_login("other-access", "other-refresh", expired);
        let login = FakeLogin::new(
            vec![agy_login("expired", "refresh", expired), other.clone()],
            false,
        );
        let (provider, _directory) = provider(&base, &login);

        provider.refresh_from_login().unwrap();

        assert_eq!(token_requests(&requests), 1);
        assert!(login.writes().is_empty());
        assert_eq!(login.read().unwrap(), Some(other));
    }

    #[test]
    fn renewal_never_overwrites_a_newer_token() {
        let (base, requests) = fake_google("renewed", "renewed");
        let expired = (Utc::now() - Duration::hours(1)).fixed_offset();
        let newer = agy_login(
            "newer-access",
            "refresh",
            (Utc::now() + Duration::hours(2)).fixed_offset(),
        );
        let login = FakeLogin::new(
            vec![agy_login("expired", "refresh", expired), newer.clone()],
            false,
        );
        let (provider, _directory) = provider(&base, &login);

        provider.refresh_from_login().unwrap();

        assert_eq!(token_requests(&requests), 1);
        assert!(login.writes().is_empty());
        assert_eq!(login.read().unwrap(), Some(newer));
    }

    #[test]
    fn rejected_token_is_replaced_by_one_renewed_elsewhere_without_renewing() {
        let (base, requests) = fake_google("renewed-by-agy", "unused");
        let valid = (Utc::now() + Duration::minutes(30)).fixed_offset();
        let login = FakeLogin::new(
            vec![
                agy_login("revoked", "refresh", valid),
                agy_login("renewed-by-agy", "refresh", valid),
            ],
            false,
        );
        let (provider, _directory) = provider(&base, &login);

        let snapshot = provider.refresh_from_login().unwrap();

        assert!(!snapshot.quotas.is_empty());
        assert_eq!(token_requests(&requests), 0);
        assert!(login.writes().is_empty());
        let requests = requests.lock().unwrap();
        assert!(requests.iter().any(|request| request.ends_with(" revoked")));
        assert!(requests
            .iter()
            .any(|request| request.ends_with(" renewed-by-agy")));
    }

    #[test]
    fn failed_write_back_keeps_the_quota_and_the_private_cache() {
        let (base, requests) = fake_google("renewed", "renewed");
        let expired = (Utc::now() - Duration::hours(1)).fixed_offset();
        let original = agy_login("expired", "refresh", expired);
        let login = FakeLogin::new(vec![original.clone()], true);
        let (provider, directory) = provider(&base, &login);

        let snapshot = provider.refresh_from_login().unwrap();

        assert!(!snapshot.quotas.is_empty());
        assert_eq!(token_requests(&requests), 1);
        assert_eq!(login.read().unwrap(), Some(original));
        let cache = AccessTokenCache::new(directory.path().join("auth.json"));
        assert_eq!(
            cache.load(Some("refresh"), Utc::now()).as_deref(),
            Some("renewed")
        );
    }

    #[test]
    fn keychain_access_token_precedes_a_distinct_cached_token() {
        let now = Utc.with_ymd_and_hms(2026, 7, 18, 12, 0, 0).unwrap();
        let source = AntigravityToken {
            access_token: Some("keychain-access".into()),
            refresh_token: Some("refresh".into()),
            expiry: Some(now + Duration::hours(1)),
        };

        let (candidates, expired) =
            access_token_candidates(&source, Some("cached-access".into()), now);
        assert_eq!(candidates, ["keychain-access", "cached-access"]);
        assert!(!expired);

        let (deduplicated, _) =
            access_token_candidates(&source, Some("keychain-access".into()), now);
        assert_eq!(deduplicated, ["keychain-access"]);
    }

    #[test]
    fn expiring_keychain_tokens_are_skipped_but_count_as_auth_evidence() {
        let now = Utc.with_ymd_and_hms(2026, 7, 18, 12, 0, 0).unwrap();
        let source = AntigravityToken {
            access_token: Some("expiring".into()),
            refresh_token: Some("refresh".into()),
            expiry: Some(now + Duration::seconds(30)),
        };

        let (candidates, expired) = access_token_candidates(&source, None, now);
        assert!(candidates.is_empty());
        assert!(expired);
    }

    #[test]
    fn refresh_and_terminal_error_decisions_preserve_auth_network_distinction() {
        assert!(!should_refresh_access_token(false, true, true));
        assert!(should_refresh_access_token(true, true, true));
        assert!(should_refresh_access_token(false, false, true));
        assert!(!should_refresh_access_token(true, true, false));

        assert!(matches!(
            credential_failure(true, false, true),
            AntigravityError::AuthExpired
        ));
        assert!(matches!(
            credential_failure(false, true, true),
            AntigravityError::Unavailable
        ));
        assert!(matches!(
            credential_failure(false, false, false),
            AntigravityError::NotSignedIn
        ));
    }

    #[test]
    fn running_language_server_is_a_refresh_source_without_credentials() {
        assert!(has_refresh_source(false, || true));
        assert!(!has_refresh_source(false, || false));
    }

    #[test]
    fn credentials_skip_language_server_discovery() {
        let discovery_called = Cell::new(false);

        assert!(has_refresh_source(true, || {
            discovery_called.set(true);
            false
        }));
        assert!(!discovery_called.get());
    }
}
