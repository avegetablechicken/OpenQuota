use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use chrono::Utc;
use reqwest::{blocking::Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use crate::models::{
    MetricDefinition, MetricSection, ProviderDefinition, ProviderSnapshot, UsageHistory,
};
use crate::storage::Storage;

use super::{
    codex::{client::UsageResponse, mapper::map_usage},
    credential_store::{delete_owned_password, read_owned_password, write_owned_password},
    CacheIdentity, ProviderError, UsageProvider,
};

const PROVIDER_ID: &str = "sub2api";
const PROVIDER_SLOTS: usize = 8;
const CREDENTIAL_SERVICE: &str = "io.github.deviffyy.openquota.sub2api";

fn definition_for(provider_id: &str, display_name: &str) -> ProviderDefinition {
    let metric_id = |suffix: &str| format!("{provider_id}.{suffix}");
    ProviderDefinition {
        id: provider_id.into(),
        display_name: display_name.into(),
        short_name: "S2".into(),
        fallback_enabled: false,
        local_usage_source_note: None,
        links: Vec::new(),
        metrics: vec![
            MetricDefinition::quota(
                &metric_id("session"),
                "Session",
                "session",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::quota(
                &metric_id("weekly"),
                "Weekly",
                "weekly",
                false,
                true,
                MetricSection::AlwaysVisible,
                true,
                "W",
            ),
            MetricDefinition::quota(
                &metric_id("spark"),
                "Spark",
                "spark",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "Sp",
            ),
            MetricDefinition::quota(
                &metric_id("sparkWeekly"),
                "Spark Weekly",
                "sparkWeekly",
                false,
                true,
                MetricSection::OnDemand,
                false,
                "SW",
            ),
            MetricDefinition::value(
                &metric_id("rateLimitResets"),
                "Rate Limit Resets",
                "sub2apiRateLimitResets",
                true,
                MetricSection::OnDemand,
                false,
                "R",
                Some("resets"),
            ),
        ],
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sub2ApiConfigInput {
    pub base_url: String,
    pub email: String,
    pub password: String,
}

impl Drop for Sub2ApiConfigInput {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sub2ApiConfigState {
    pub configured: bool,
    pub base_url: String,
    pub email: String,
}

#[derive(Serialize, Deserialize)]
struct StoredConfig {
    base_url: String,
    email: String,
    password: String,
}

impl Clone for StoredConfig {
    fn clone(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            email: self.email.clone(),
            password: self.password.clone(),
        }
    }
}

impl Drop for StoredConfig {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl StoredConfig {
    fn state(&self) -> Sub2ApiConfigState {
        Sub2ApiConfigState {
            configured: true,
            base_url: self.base_url.clone(),
            email: self.email.clone(),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
enum Sub2ApiError {
    #[error("Configure the Sub2API connection in Customize.")]
    MissingConfig,
    #[error("Enter a valid Sub2API Base URL, email, and password.")]
    InvalidConfig,
    #[error("The Sub2API connection could not be read or updated securely.")]
    CredentialStorage,
    #[error("Sub2API rejected the administrator email or password.")]
    Authentication,
    #[error("This Sub2API login does not have administrator access.")]
    Permission,
    #[error("Sub2API requires two-factor authentication, which is not supported yet.")]
    TwoFactorRequired,
    #[error("No active Codex upstream account was found in Sub2API.")]
    NoCodexAccount,
    #[error("Sub2API is rate limiting requests. Try again later.")]
    RateLimited,
    #[error("Sub2API request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("Could not reach Sub2API. Check the Base URL and connection.")]
    ConnectionFailed,
    #[error("Sub2API returned an invalid response.")]
    InvalidResponse,
}

impl From<Sub2ApiError> for ProviderError {
    fn from(error: Sub2ApiError) -> Self {
        use crate::models::ProviderErrorKind as Kind;

        let kind = match error {
            Sub2ApiError::MissingConfig
            | Sub2ApiError::Authentication
            | Sub2ApiError::TwoFactorRequired => Kind::Authentication,
            Sub2ApiError::Permission => Kind::Permission,
            Sub2ApiError::CredentialStorage => Kind::CredentialStorage,
            Sub2ApiError::RateLimited => Kind::RateLimited,
            Sub2ApiError::RequestFailed(_) | Sub2ApiError::ConnectionFailed => Kind::Network,
            Sub2ApiError::InvalidConfig => Kind::Internal,
            Sub2ApiError::NoCodexAccount | Sub2ApiError::InvalidResponse => Kind::InvalidResponse,
        };
        ProviderError::from_display(kind, error)
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    code: i64,
    data: Option<T>,
}

#[derive(Deserialize)]
struct LoginData {
    access_token: Option<String>,
    expires_in: Option<u64>,
    #[serde(default)]
    requires_2fa: bool,
}

#[derive(Deserialize)]
struct AccountPage {
    items: Vec<Sub2ApiAccount>,
    total: usize,
}

#[derive(Clone, Deserialize)]
struct Sub2ApiAccount {
    id: i64,
    name: String,
}

struct LoginToken {
    value: Zeroizing<String>,
    expires_in: u64,
}

struct CachedSession {
    scope: String,
    token: Zeroizing<String>,
    expires_at: Instant,
    account: Sub2ApiAccount,
    account_count: usize,
}

struct Sub2ApiClient {
    client: Client,
    base_url: Url,
}

impl Sub2ApiClient {
    fn new(base_url: &str) -> Result<Self, Sub2ApiError> {
        let base_url = normalize_base_url(base_url)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("OpenQuota/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| Sub2ApiError::ConnectionFailed)?;
        Ok(Self { client, base_url })
    }

    fn login(&self, email: &str, password: &str) -> Result<LoginToken, Sub2ApiError> {
        #[derive(Serialize)]
        struct LoginRequest<'a> {
            email: &'a str,
            password: &'a str,
        }

        let response = self
            .client
            .post(self.endpoint(&["api", "v1", "auth", "login"])?)
            .header("Accept", "application/json")
            .json(&LoginRequest { email, password })
            .send()
            .map_err(|_| Sub2ApiError::ConnectionFailed)?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }
        let envelope = response
            .json::<Envelope<LoginData>>()
            .map_err(|_| Sub2ApiError::InvalidResponse)?;
        if envelope.code != 0 {
            return Err(Sub2ApiError::Authentication);
        }
        let data = envelope.data.ok_or(Sub2ApiError::InvalidResponse)?;
        if data.requires_2fa {
            return Err(Sub2ApiError::TwoFactorRequired);
        }
        let token = data
            .access_token
            .map(Zeroizing::new)
            .filter(|value| !value.trim().is_empty())
            .ok_or(Sub2ApiError::InvalidResponse)?;
        Ok(LoginToken {
            value: token,
            expires_in: data.expires_in.unwrap_or(15 * 60),
        })
    }

    fn first_codex_account(&self, token: &str) -> Result<(Sub2ApiAccount, usize), Sub2ApiError> {
        let mut url = self.endpoint(&["api", "v1", "admin", "accounts"])?;
        url.query_pairs_mut()
            .append_pair("page", "1")
            .append_pair("page_size", "1")
            .append_pair("platform", "openai")
            .append_pair("type", "oauth")
            .append_pair("status", "active")
            .append_pair("sort_by", "name")
            .append_pair("sort_order", "asc");
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .map_err(|_| Sub2ApiError::ConnectionFailed)?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }
        let envelope = response
            .json::<Envelope<AccountPage>>()
            .map_err(|_| Sub2ApiError::InvalidResponse)?;
        if envelope.code != 0 {
            return Err(Sub2ApiError::InvalidResponse);
        }
        let mut page = envelope.data.ok_or(Sub2ApiError::InvalidResponse)?;
        let account = page
            .items
            .drain(..)
            .next()
            .ok_or(Sub2ApiError::NoCodexAccount)?;
        Ok((account, page.total))
    }

    fn quota(&self, token: &str, account_id: i64) -> Result<Value, Sub2ApiError> {
        let account_id = account_id.to_string();
        let response = self
            .client
            .get(self.endpoint(&[
                "api",
                "v1",
                "admin",
                "openai",
                "accounts",
                &account_id,
                "quota",
            ])?)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .map_err(|_| Sub2ApiError::ConnectionFailed)?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }
        let envelope = response
            .json::<Envelope<Value>>()
            .map_err(|_| Sub2ApiError::InvalidResponse)?;
        if envelope.code != 0 {
            return Err(Sub2ApiError::InvalidResponse);
        }
        envelope
            .data
            .filter(Value::is_object)
            .ok_or(Sub2ApiError::InvalidResponse)
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, Sub2ApiError> {
        let mut url = self.base_url.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| Sub2ApiError::InvalidConfig)?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
        drop(path);
        Ok(url)
    }
}

fn normalize_base_url(value: &str) -> Result<Url, Sub2ApiError> {
    let mut url = Url::parse(value.trim()).map_err(|_| Sub2ApiError::InvalidConfig)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Sub2ApiError::InvalidConfig);
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/api/v1").unwrap_or(path).to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url)
}

fn classify_status(status: StatusCode) -> Sub2ApiError {
    match status {
        StatusCode::UNAUTHORIZED => Sub2ApiError::Authentication,
        StatusCode::FORBIDDEN => Sub2ApiError::Permission,
        StatusCode::TOO_MANY_REQUESTS => Sub2ApiError::RateLimited,
        _ => Sub2ApiError::RequestFailed(status.as_u16()),
    }
}

pub struct Sub2ApiProvider {
    provider_id: String,
    display_name: String,
    configured_marker: PathBuf,
    configured: AtomicBool,
    session: Mutex<Option<CachedSession>>,
}

impl Sub2ApiProvider {
    fn new(
        provider_id: String,
        display_name: String,
        configured_marker: PathBuf,
        configured: bool,
    ) -> Self {
        Self {
            provider_id,
            display_name,
            configured_marker,
            configured: AtomicBool::new(configured),
            session: Mutex::new(None),
        }
    }

    pub fn config_state(&self) -> Result<Sub2ApiConfigState, ProviderError> {
        Ok(self
            .load_config()?
            .map_or_else(Sub2ApiConfigState::default, |config| config.state()))
    }

    pub fn save_config(
        &self,
        input: Sub2ApiConfigInput,
    ) -> Result<Sub2ApiConfigState, ProviderError> {
        let previous = self.load_config()?;
        let password = if input.password.trim().is_empty() {
            previous
                .as_ref()
                .map(|config| config.password.clone())
                .ok_or(Sub2ApiError::InvalidConfig)?
        } else {
            input.password.trim().to_owned()
        };
        let config = StoredConfig {
            base_url: normalized_base_url_text(&input.base_url)?,
            email: input.email.trim().to_owned(),
            password,
        };
        if config.email.is_empty() || config.password.is_empty() {
            return Err(Sub2ApiError::InvalidConfig.into());
        }

        let session = self.validate_config(&config)?;
        let bytes = Zeroizing::new(
            serde_json::to_vec(&config).map_err(|_| Sub2ApiError::CredentialStorage)?,
        );
        write_owned_password(
            CREDENTIAL_SERVICE,
            self.credential_account(),
            bytes.as_slice(),
        )
        .map_err(|_| Sub2ApiError::CredentialStorage)?;
        self.set_configured(true)?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = session;
        }
        Ok(config.state())
    }

    pub fn delete_config(&self) -> Result<Sub2ApiConfigState, ProviderError> {
        delete_owned_password(CREDENTIAL_SERVICE, self.credential_account())
            .map_err(|_| Sub2ApiError::CredentialStorage)?;
        self.set_configured(false)?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = None;
        }
        Ok(Sub2ApiConfigState::default())
    }

    fn connect(&self, config: &StoredConfig) -> Result<CachedSession, Sub2ApiError> {
        let client = Sub2ApiClient::new(&config.base_url)?;
        let login = client.login(&config.email, &config.password)?;
        let (account, account_count) = client.first_codex_account(login.value.as_str())?;
        Ok(CachedSession {
            scope: session_scope(config),
            token: login.value,
            expires_at: token_expiry(login.expires_in),
            account,
            account_count,
        })
    }

    fn validate_config(
        &self,
        config: &StoredConfig,
    ) -> Result<Option<CachedSession>, Sub2ApiError> {
        let client = Sub2ApiClient::new(&config.base_url)?;
        let login = client.login(&config.email, &config.password)?;
        let (account, account_count) = match client.first_codex_account(login.value.as_str()) {
            Ok(account) => account,
            Err(Sub2ApiError::NoCodexAccount) => return Ok(None),
            Err(error) => return Err(error),
        };
        client.quota(login.value.as_str(), account.id)?;
        Ok(Some(CachedSession {
            scope: session_scope(config),
            token: login.value,
            expires_at: token_expiry(login.expires_in),
            account,
            account_count,
        }))
    }

    fn session(&self, config: &StoredConfig) -> Result<CachedSession, Sub2ApiError> {
        let scope = session_scope(config);
        if let Some(session) = self.session.lock().ok().and_then(|cached| {
            cached
                .as_ref()
                .filter(|cached| cached.scope == scope && cached.expires_at > Instant::now())
                .map(clone_session)
        }) {
            return Ok(session);
        }
        let session = self.connect(config)?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = Some(clone_session(&session));
        }
        Ok(session)
    }

    fn refresh_snapshot(&self, config: &StoredConfig) -> Result<ProviderSnapshot, Sub2ApiError> {
        let client = Sub2ApiClient::new(&config.base_url)?;
        let mut session = self.session(config)?;
        let body = match client.quota(session.token.as_str(), session.account.id) {
            Err(Sub2ApiError::Authentication) => {
                if let Ok(mut cached) = self.session.lock() {
                    *cached = None;
                }
                session = self.connect(config)?;
                client.quota(session.token.as_str(), session.account.id)?
            }
            result => result?,
        };
        let response = UsageResponse {
            status: StatusCode::OK,
            headers: HashMap::new(),
            body,
        };
        let mut mapped =
            map_usage(&response, None, Utc::now()).map_err(|_| Sub2ApiError::InvalidResponse)?;
        mapped.value_metrics.retain_mut(|metric| {
            if metric.id != "rateLimitResets" {
                return false;
            }
            metric.id = "sub2apiRateLimitResets".into();
            metric.expiries_at.clear();
            true
        });
        let warnings = (session.account_count > 1)
            .then(|| {
                format!(
                    "Showing Codex upstream {}. {} active Codex accounts were found.",
                    session.account.name, session.account_count
                )
            })
            .into_iter()
            .collect();
        Ok(ProviderSnapshot {
            provider_id: self.provider_id.clone(),
            plan: mapped.plan,
            quotas: mapped.quotas,
            value_metrics: mapped.value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage: UsageHistory::default(),
            warnings,
            refreshed_at: Utc::now(),
        })
    }

    fn credential_account(&self) -> &str {
        if self.provider_id == PROVIDER_ID {
            "connection"
        } else {
            &self.provider_id
        }
    }

    fn set_configured(&self, configured: bool) -> Result<(), Sub2ApiError> {
        if configured {
            let parent = self
                .configured_marker
                .parent()
                .ok_or(Sub2ApiError::CredentialStorage)?;
            fs::create_dir_all(parent).map_err(|_| Sub2ApiError::CredentialStorage)?;
            fs::write(&self.configured_marker, b"configured")
                .map_err(|_| Sub2ApiError::CredentialStorage)?;
        } else {
            match fs::remove_file(&self.configured_marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(Sub2ApiError::CredentialStorage),
            }
        }
        self.configured.store(configured, Ordering::SeqCst);
        Ok(())
    }

    fn load_config(&self) -> Result<Option<StoredConfig>, Sub2ApiError> {
        let Some(bytes) = read_owned_password(CREDENTIAL_SERVICE, self.credential_account())
            .map_err(|_| Sub2ApiError::CredentialStorage)?
        else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Sub2ApiError::CredentialStorage)
    }
}

pub struct Sub2ApiProviders {
    providers: HashMap<String, Arc<Sub2ApiProvider>>,
}

impl Sub2ApiProviders {
    pub fn new(marker_directory: PathBuf, storage: Arc<Storage>) -> Self {
        let migration_marker = marker_directory.join(".initialized");
        let migrate_snapshots = !migration_marker.is_file();
        let providers = (0..PROVIDER_SLOTS)
            .map(|index| {
                let number = index + 1;
                let provider_id = if index == 0 {
                    PROVIDER_ID.to_owned()
                } else {
                    format!("{PROVIDER_ID}@{number}")
                };
                let display_name = if index == 0 {
                    "Sub2API".to_owned()
                } else {
                    format!("Sub2API {number}")
                };
                let configured_marker = marker_directory.join(format!("{provider_id}.configured"));
                let configured = configured_marker.is_file()
                    || (migrate_snapshots
                        && storage
                            .load_snapshot_for_identity(
                                &provider_id,
                                crate::providers::CacheIdentity::Unscoped,
                            )
                            .is_ok_and(|snapshot| snapshot.is_some()));
                if configured && !configured_marker.is_file() {
                    let _ = fs::create_dir_all(&marker_directory);
                    let _ = fs::write(&configured_marker, b"configured");
                }
                let provider = Arc::new(Sub2ApiProvider::new(
                    provider_id.clone(),
                    display_name,
                    configured_marker,
                    configured,
                ));
                (provider_id, provider)
            })
            .collect();
        if migrate_snapshots {
            let _ = fs::create_dir_all(&marker_directory);
            let _ = fs::write(migration_marker, b"initialized");
        }
        Self { providers }
    }

    pub fn runtimes(&self) -> Vec<Arc<dyn UsageProvider>> {
        let mut providers = self.providers.values().cloned().collect::<Vec<_>>();
        providers.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
        providers
            .into_iter()
            .map(|provider| provider as Arc<dyn UsageProvider>)
            .collect()
    }

    pub fn provider(&self, provider_id: &str) -> Option<Arc<Sub2ApiProvider>> {
        self.providers.get(provider_id).cloned()
    }
}

impl UsageProvider for Sub2ApiProvider {
    fn definition(&self) -> ProviderDefinition {
        definition_for(&self.provider_id, &self.display_name)
    }

    fn has_local_credentials(&self) -> bool {
        self.configured.load(Ordering::SeqCst)
    }

    fn cache_identity(&self) -> CacheIdentity<'_> {
        if self.configured.load(Ordering::SeqCst) {
            CacheIdentity::Unscoped
        } else {
            CacheIdentity::Resolved("sub2api-unconfigured")
        }
    }

    fn supports_connection_configuration(&self) -> bool {
        true
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        let config = self.load_config()?.ok_or(Sub2ApiError::MissingConfig)?;
        self.refresh_snapshot(&config).map_err(ProviderError::from)
    }
}

fn normalized_base_url_text(value: &str) -> Result<String, Sub2ApiError> {
    let url = normalize_base_url(value)?;
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn session_scope(config: &StoredConfig) -> String {
    crate::hashing::sha256_hex(format!("{}\0{}", config.base_url, config.email).as_bytes())
}

fn token_expiry(expires_in: u64) -> Instant {
    Instant::now() + Duration::from_secs(expires_in.saturating_sub(60).max(1))
}

fn clone_session(session: &CachedSession) -> CachedSession {
    CachedSession {
        scope: session.scope.clone(),
        token: Zeroizing::new(session.token.to_string()),
        expires_at: session.expires_at,
        account: session.account.clone(),
        account_count: session.account_count,
    }
}

#[cfg(test)]
mod tests {
    use std::{
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
        definition_for, normalize_base_url, StoredConfig, Sub2ApiClient, Sub2ApiError,
        Sub2ApiProvider, Sub2ApiProviders,
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

    fn serve_sequence(
        responses: Vec<String>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            for body in responses {
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
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}"), receiver, worker)
    }

    #[test]
    fn definition_exposes_one_fixed_sub2api_provider() {
        let definition = definition_for("sub2api", "Sub2API");
        assert_eq!(definition.id, "sub2api");
        assert_eq!(definition.display_name, "Sub2API");
        assert_eq!(definition.metrics[0].id, "sub2api.session");
        assert!(!definition.fallback_enabled);
    }

    #[test]
    fn provider_slots_have_independent_ids_metrics_and_credentials() {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let providers = Sub2ApiProviders::new(directory.path().join("markers"), storage);
        let first = providers.provider("sub2api").unwrap();
        let second = providers.provider("sub2api@2").unwrap();

        assert_eq!(first.definition().metrics[0].id, "sub2api.session");
        assert_eq!(second.definition().display_name, "Sub2API 2");
        assert_eq!(second.definition().metrics[0].id, "sub2api@2.session");
        assert_eq!(first.credential_account(), "connection");
        assert_eq!(second.credential_account(), "sub2api@2");
    }

    #[test]
    fn existing_snapshots_migrate_once_without_resurrecting_deleted_markers() {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        storage
            .save_snapshot(&ProviderSnapshot {
                provider_id: "sub2api".into(),
                plan: None,
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: Vec::new(),
                usage: Default::default(),
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
    fn unconfigured_slots_never_load_old_quota_snapshots() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        storage
            .save_snapshot(&ProviderSnapshot {
                provider_id: "sub2api@2".into(),
                plan: Some("Old plan".into()),
                quotas: Vec::new(),
                value_metrics: Vec::new(),
                status_metrics: Vec::new(),
                notices: Vec::new(),
                usage: Default::default(),
                warnings: Vec::new(),
                refreshed_at: Utc::now(),
            })
            .unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api@2".into(),
            "Sub2API 2".into(),
            directory.path().join("sub2api@2.configured"),
            false,
        );

        assert!(storage
            .load_snapshot_for_identity("sub2api@2", provider.cache_identity())
            .unwrap()
            .is_none());
        provider.set_configured(true).unwrap();
        assert_eq!(
            storage
                .load_snapshot_for_identity("sub2api@2", provider.cache_identity())
                .unwrap()
                .and_then(|snapshot| snapshot.plan),
            Some("Old plan".into())
        );
    }

    #[test]
    fn valid_admin_login_can_be_saved_without_an_active_codex_account() {
        let responses = vec![
            r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600}}"#.into(),
            r#"{"code":0,"message":"success","data":{"items":[],"total":0,"page":1,"page_size":1,"pages":0}}"#.into(),
        ];
        let (base_url, _requests, worker) = serve_sequence(responses);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api@2".into(),
            "Sub2API 2".into(),
            directory.path().join("sub2api@2.configured"),
            false,
        );
        let config = StoredConfig {
            base_url,
            email: "admin@example.com".into(),
            password: "secret-password".into(),
        };

        assert!(provider.validate_config(&config).unwrap().is_none());
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
        let (account, count) = client.first_codex_account(login.value.as_str()).unwrap();
        let body = client.quota(login.value.as_str(), account.id).unwrap();
        worker.join().unwrap();

        assert_eq!(account.name, "Codex upstream");
        assert_eq!(count, 1);
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
    fn connection_errors_never_include_credentials() {
        let error = match Sub2ApiClient::new("not-a-url") {
            Ok(_) => panic!("invalid URL should be rejected"),
            Err(error) => error,
        };
        assert_eq!(error, Sub2ApiError::InvalidConfig);
        assert!(!error.to_string().contains("secret"));
    }
}
