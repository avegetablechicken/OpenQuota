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

use chrono::{Days, Local, NaiveDate, Utc};
use reqwest::{blocking::Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use crate::models::{
    DailyUsage, MetricDefinition, MetricLayout, MetricSection, ModelUsageBreakdown,
    ModelUsageEntry, ProviderDefinition, ProviderSnapshot, UsageHistories, UsageHistory,
    UsagePeriod, UsagePeriodSelection,
};
use crate::storage::Storage;

use super::{
    claude::map_sub2api_usage,
    codex::{client::UsageResponse, mapper::map_usage},
    credential_store::{delete_owned_password, read_owned_password, write_owned_password},
    CacheIdentity, ProviderError, UsageProvider,
};

const PROVIDER_ID: &str = "sub2api";
const PROVIDER_SLOTS: usize = 8;
const CREDENTIAL_SERVICE: &str = "io.github.deviffyy.openquota.sub2api";
const METRIC_TEMPLATE_VERSION: u8 = 1;

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
                &metric_id("sonnet"),
                "Sonnet",
                "sonnet",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "Sn",
            ),
            MetricDefinition::quota(
                &metric_id("fable"),
                "Fable",
                "fable",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "F",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("extra"),
                "Extra Usage",
                "extra",
                false,
                MetricSection::OnDemand,
                false,
                "E",
            ),
            MetricDefinition::trend(&metric_id("trend")),
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
            MetricDefinition::usage(
                &metric_id("today"),
                "Today",
                UsagePeriodSelection::Today,
                MetricSection::OnDemand,
                "T",
            ),
            MetricDefinition::usage(
                &metric_id("yesterday"),
                "Yesterday",
                UsagePeriodSelection::Yesterday,
                MetricSection::OnDemand,
                "Y",
            ),
            MetricDefinition::usage(
                &metric_id("last30"),
                "Last 30 Days",
                UsagePeriodSelection::Last30Days,
                MetricSection::OnDemand,
                "M",
            ),
        ],
    }
}

pub fn metric_template(provider_id: &str, upstream: Sub2ApiUpstream) -> Vec<MetricLayout> {
    let metric = |suffix: &str, enabled: bool, section: MetricSection, pinned: bool| MetricLayout {
        id: format!("{provider_id}.{suffix}"),
        enabled,
        section,
        pinned,
    };
    let always = MetricSection::AlwaysVisible;
    let demand = MetricSection::OnDemand;
    match upstream {
        Sub2ApiUpstream::Claude => vec![
            metric("session", true, always, true),
            metric("weekly", true, always, true),
            metric("trend", true, always, false),
            metric("sonnet", false, demand, false),
            metric("fable", false, demand, false),
            metric("extra", false, demand, false),
            metric("today", true, demand, false),
            metric("yesterday", true, demand, false),
            metric("last30", true, demand, false),
            metric("spark", false, demand, false),
            metric("sparkWeekly", false, demand, false),
            metric("rateLimitResets", false, demand, false),
        ],
        Sub2ApiUpstream::Codex => vec![
            metric("session", true, always, true),
            metric("weekly", true, always, true),
            metric("trend", true, always, false),
            metric("spark", true, demand, false),
            metric("sparkWeekly", true, demand, false),
            metric("rateLimitResets", true, demand, false),
            metric("today", true, demand, false),
            metric("yesterday", true, demand, false),
            metric("last30", true, demand, false),
            metric("sonnet", false, demand, false),
            metric("fable", false, demand, false),
            metric("extra", false, demand, false),
        ],
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sub2ApiConfigInput {
    pub base_url: String,
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub upstream: Sub2ApiUpstream,
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
    pub upstream: Sub2ApiUpstream,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Sub2ApiUpstream {
    #[default]
    Codex,
    Claude,
}

impl Sub2ApiUpstream {
    fn platform(self) -> &'static str {
        match self {
            Self::Codex => "openai",
            Self::Claude => "anthropic",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }
}

impl std::fmt::Display for Sub2ApiUpstream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Serialize, Deserialize)]
struct StoredConfig {
    base_url: String,
    email: String,
    password: String,
    #[serde(default)]
    upstream: Sub2ApiUpstream,
    #[serde(default)]
    metric_template_version: u8,
}

impl Clone for StoredConfig {
    fn clone(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            email: self.email.clone(),
            password: self.password.clone(),
            upstream: self.upstream,
            metric_template_version: self.metric_template_version,
        }
    }
}

pub struct Sub2ApiConfigSaveOutcome {
    pub state: Sub2ApiConfigState,
    pub apply_metric_template: bool,
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
            upstream: self.upstream,
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
    #[error("No active {0} upstream account was found in Sub2API.")]
    NoUpstreamAccount(Sub2ApiUpstream),
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
            Sub2ApiError::NoUpstreamAccount(_) | Sub2ApiError::InvalidResponse => {
                Kind::InvalidResponse
            }
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

#[derive(Deserialize)]
struct AccountStats {
    #[serde(default)]
    history: Vec<AccountStatsDay>,
    #[serde(default)]
    models: Vec<AccountStatsModel>,
}

#[derive(Deserialize)]
struct AccountStatsDay {
    date: String,
    #[serde(default)]
    tokens: u64,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    actual_cost: Option<f64>,
}

impl AccountStatsDay {
    fn measured_cost(&self) -> Option<f64> {
        measured_cost(self.actual_cost, self.cost)
    }
}

#[derive(Deserialize)]
struct AccountStatsModel {
    model: String,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    actual_cost: Option<f64>,
}

impl AccountStatsModel {
    fn measured_cost(&self) -> Option<f64> {
        measured_cost(self.actual_cost, self.cost)
    }
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
        let client = crate::http_client::blocking_client_builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("OpenQuota/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| transport_error("client setup", &error))?;
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
            .map_err(|error| transport_error("login", &error))?;
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

    fn first_account(
        &self,
        token: &str,
        upstream: Sub2ApiUpstream,
    ) -> Result<(Sub2ApiAccount, usize), Sub2ApiError> {
        let mut url = self.endpoint(&["api", "v1", "admin", "accounts"])?;
        url.query_pairs_mut()
            .append_pair("page", "1")
            .append_pair("page_size", "1")
            .append_pair("platform", upstream.platform())
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
            .map_err(|error| transport_error("account discovery", &error))?;
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
            .ok_or(Sub2ApiError::NoUpstreamAccount(upstream))?;
        Ok((account, page.total))
    }

    fn usage(
        &self,
        token: &str,
        account_id: i64,
        upstream: Sub2ApiUpstream,
    ) -> Result<Value, Sub2ApiError> {
        let account_id = account_id.to_string();
        let segments: &[&str] = match upstream {
            Sub2ApiUpstream::Codex => &[
                "api",
                "v1",
                "admin",
                "openai",
                "accounts",
                &account_id,
                "quota",
            ],
            Sub2ApiUpstream::Claude => &["api", "v1", "admin", "accounts", &account_id, "usage"],
        };
        let response = self
            .client
            .get(self.endpoint(segments)?)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .map_err(|error| transport_error("upstream usage", &error))?;
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

    fn stats(&self, token: &str, account_id: i64) -> Result<AccountStats, Sub2ApiError> {
        let account_id = account_id.to_string();
        let mut url = self.endpoint(&["api", "v1", "admin", "accounts", &account_id, "stats"])?;
        url.query_pairs_mut().append_pair("days", "30");
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .map_err(|error| transport_error("usage statistics", &error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_status(status));
        }
        let envelope = response
            .json::<Envelope<AccountStats>>()
            .map_err(|_| Sub2ApiError::InvalidResponse)?;
        if envelope.code != 0 {
            return Err(Sub2ApiError::InvalidResponse);
        }
        envelope.data.ok_or(Sub2ApiError::InvalidResponse)
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

fn transport_error(stage: &str, error: &reqwest::Error) -> Sub2ApiError {
    let reason = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else {
        "transport"
    };
    crate::app_warn!("http", "Sub2API {stage} request failed ({reason})");
    Sub2ApiError::ConnectionFailed
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
    ) -> Result<Sub2ApiConfigSaveOutcome, ProviderError> {
        let previous = self.load_config()?;
        let password = if input.password.trim().is_empty() {
            previous
                .as_ref()
                .map(|config| config.password.clone())
                .ok_or(Sub2ApiError::InvalidConfig)?
        } else {
            input.password.trim().to_owned()
        };
        let apply_metric_template = should_apply_metric_template(previous.as_ref(), input.upstream);
        let config = StoredConfig {
            base_url: normalized_base_url_text(&input.base_url)?,
            email: input.email.trim().to_owned(),
            password,
            upstream: input.upstream,
            metric_template_version: if apply_metric_template {
                0
            } else {
                METRIC_TEMPLATE_VERSION
            },
        };
        if config.email.is_empty() || config.password.is_empty() {
            return Err(Sub2ApiError::InvalidConfig.into());
        }

        let session = self.validate_config(&config)?;
        self.write_config(&config)?;
        self.set_configured(true)?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = session;
        }
        Ok(Sub2ApiConfigSaveOutcome {
            state: config.state(),
            apply_metric_template,
        })
    }

    pub fn mark_metric_template_applied(&self) -> Result<(), ProviderError> {
        let mut config = self.load_config()?.ok_or(Sub2ApiError::MissingConfig)?;
        config.metric_template_version = METRIC_TEMPLATE_VERSION;
        self.write_config(&config).map_err(ProviderError::from)
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
        let (account, account_count) =
            client.first_account(login.value.as_str(), config.upstream)?;
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
        let (account, account_count) =
            match client.first_account(login.value.as_str(), config.upstream) {
                Ok(account) => account,
                Err(Sub2ApiError::NoUpstreamAccount(_)) => return Ok(None),
                Err(error) => return Err(error),
            };
        client.usage(login.value.as_str(), account.id, config.upstream)?;
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
        let body = match client.usage(session.token.as_str(), session.account.id, config.upstream) {
            Err(Sub2ApiError::Authentication) => {
                if let Ok(mut cached) = self.session.lock() {
                    *cached = None;
                }
                session = self.connect(config)?;
                client.usage(session.token.as_str(), session.account.id, config.upstream)?
            }
            result => result?,
        };
        let (plan, quotas, value_metrics) = match config.upstream {
            Sub2ApiUpstream::Codex => {
                let response = UsageResponse {
                    status: StatusCode::OK,
                    headers: HashMap::new(),
                    body,
                };
                let mut mapped = map_usage(&response, None, Utc::now())
                    .map_err(|_| Sub2ApiError::InvalidResponse)?;
                mapped.value_metrics.retain_mut(|metric| {
                    if metric.id != "rateLimitResets" {
                        return false;
                    }
                    metric.id = "sub2apiRateLimitResets".into();
                    metric.expiries_at.clear();
                    true
                });
                (mapped.plan, mapped.quotas, mapped.value_metrics)
            }
            Sub2ApiUpstream::Claude => {
                let mapped = map_sub2api_usage(&body).map_err(|_| Sub2ApiError::InvalidResponse)?;
                (mapped.plan, mapped.quotas, mapped.value_metrics)
            }
        };
        let mut warnings = (session.account_count > 1)
            .then(|| {
                format!(
                    "Showing {} upstream {}. {} active {} accounts were found.",
                    config.upstream, session.account.name, session.account_count, config.upstream
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        let stats = match client.stats(session.token.as_str(), session.account.id) {
            Err(Sub2ApiError::Authentication) => {
                if let Ok(mut cached) = self.session.lock() {
                    *cached = None;
                }
                session = self.connect(config)?;
                client.stats(session.token.as_str(), session.account.id)
            }
            result => result,
        };
        let usage = match stats {
            Ok(stats) => map_stats(stats, Utc::now()),
            Err(error) => {
                warnings.push(format!(
                    "Sub2API server usage statistics are unavailable: {error}"
                ));
                UsageHistory::default()
            }
        };
        Ok(ProviderSnapshot {
            provider_id: self.provider_id.clone(),
            plan,
            quotas,
            value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories::account(usage),
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

    fn write_config(&self, config: &StoredConfig) -> Result<(), Sub2ApiError> {
        let bytes = Zeroizing::new(
            serde_json::to_vec(config).map_err(|_| Sub2ApiError::CredentialStorage)?,
        );
        write_owned_password(
            CREDENTIAL_SERVICE,
            self.credential_account(),
            bytes.as_slice(),
        )
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
    crate::hashing::sha256_hex(
        format!(
            "{}\0{}\0{}",
            config.base_url,
            config.email,
            config.upstream.label()
        )
        .as_bytes(),
    )
}

fn should_apply_metric_template(
    previous: Option<&StoredConfig>,
    upstream: Sub2ApiUpstream,
) -> bool {
    previous.is_none_or(|previous| {
        previous.upstream != upstream || previous.metric_template_version < METRIC_TEMPLATE_VERSION
    })
}

fn map_stats(mut stats: AccountStats, now: chrono::DateTime<Utc>) -> UsageHistory {
    stats.history.retain(|day| {
        NaiveDate::parse_from_str(day.date.trim(), "%Y-%m-%d").is_ok()
            && (day.tokens > 0 || day.measured_cost().is_some_and(|cost| cost > 0.0))
    });
    stats
        .history
        .sort_by(|left, right| left.date.cmp(&right.date));
    let today = now.with_timezone(&Local).date_naive();
    let yesterday = today.checked_sub_days(Days::new(1));
    let source_note = "From Sub2API server usage logs";
    let today_period = stats
        .history
        .iter()
        .find(|day| day.date == today.to_string())
        .and_then(stats_day_period);
    let yesterday_period = yesterday.and_then(|date| {
        stats
            .history
            .iter()
            .find(|day| day.date == date.to_string())
            .and_then(stats_day_period)
    });
    let daily = stats
        .history
        .iter()
        .map(|day| DailyUsage {
            date: day.date.clone(),
            tokens: day.tokens,
            estimated_cost_usd: day.measured_cost(),
            estimate_complete: day.measured_cost().is_some(),
        })
        .collect::<Vec<_>>();
    let total_tokens = stats
        .history
        .iter()
        .fold(0_u64, |total, day| total.saturating_add(day.tokens));
    let costs = stats
        .history
        .iter()
        .map(AccountStatsDay::measured_cost)
        .collect::<Option<Vec<_>>>();
    let model_breakdown = stats_model_breakdown(stats.models, source_note);
    let last_30_days = (!stats.history.is_empty()).then(|| UsagePeriod {
        tokens: total_tokens,
        estimated_cost_usd: costs
            .as_ref()
            .map(|values| values.iter().copied().sum::<f64>()),
        cost_estimated: false,
        estimate_complete: costs.is_some(),
        model_breakdown,
        unknown_models: Vec::new(),
    });

    UsageHistory {
        today: today_period,
        yesterday: yesterday_period,
        last_30_days,
        daily,
        unknown_models: Vec::new(),
    }
}

fn stats_day_period(day: &AccountStatsDay) -> Option<UsagePeriod> {
    (day.tokens > 0 || day.measured_cost().is_some_and(|cost| cost > 0.0)).then(|| UsagePeriod {
        tokens: day.tokens,
        estimated_cost_usd: day.measured_cost(),
        cost_estimated: false,
        estimate_complete: day.measured_cost().is_some(),
        model_breakdown: None,
        unknown_models: Vec::new(),
    })
}

fn stats_model_breakdown(
    models: Vec<AccountStatsModel>,
    source_note: &str,
) -> Option<ModelUsageBreakdown> {
    let mut models = models
        .into_iter()
        .filter_map(|model| {
            let name = model.model.trim().to_owned();
            if name.is_empty()
                || (model.total_tokens == 0
                    && !model.measured_cost().is_some_and(|cost| cost > 0.0))
            {
                return None;
            }
            Some(ModelUsageEntry {
                model: name,
                total_tokens: model.total_tokens,
                cost_usd: model.measured_cost(),
                variants: None,
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .cost_usd
            .partial_cmp(&left.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.total_tokens.cmp(&left.total_tokens))
            .then_with(|| left.model.cmp(&right.model))
    });
    (!models.is_empty()).then(|| ModelUsageBreakdown {
        models,
        source_note: source_note.to_owned(),
    })
}

fn measured_cost(actual: Option<f64>, standard: Option<f64>) -> Option<f64> {
    actual
        .filter(|value| value.is_finite())
        .or_else(|| standard.filter(|value| value.is_finite()))
        .map(|value| value.max(0.0))
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
        definition_for, map_stats, metric_template, normalize_base_url,
        should_apply_metric_template, AccountStats, AccountStatsDay, StoredConfig, Sub2ApiClient,
        Sub2ApiError, Sub2ApiProvider, Sub2ApiProviders,
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

    const STATS_DATA: &str = r#"{
      "history":[
        {"date":"2026-08-22","requests":2,"tokens":1000,"cost":0.5,"actual_cost":0.4,"user_cost":0.6},
        {"date":"2026-08-23","requests":3,"tokens":2500,"cost":1.5,"actual_cost":1.25,"user_cost":1.7}
      ],
      "summary":{"days":31,"actual_days_used":2,"total_cost":1.65,"total_requests":5,"total_tokens":3500},
      "models":[
        {"model":"claude-sonnet-5","requests":3,"total_tokens":2500,"cost":1.5,"actual_cost":1.25},
        {"model":"claude-haiku-4-5","requests":2,"total_tokens":1000,"cost":0.5,"actual_cost":0.4}
      ],
      "endpoints":[],
      "upstream_endpoints":[]
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
        ] {
            let template = metric_template("sub2api@2", upstream);
            for definition in native.metrics {
                let suffix = definition.id.split_once('.').unwrap().1;
                if suffix == unsupported {
                    continue;
                }
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

        let claude = metric_template("sub2api@2", super::Sub2ApiUpstream::Claude);
        assert!(claude
            .iter()
            .filter(|metric| metric.id.ends_with("spark")
                || metric.id.ends_with("sparkWeekly")
                || metric.id.ends_with("rateLimitResets"))
            .all(|metric| !metric.enabled));
        let codex = metric_template("sub2api@2", super::Sub2ApiUpstream::Codex);
        assert!(codex
            .iter()
            .filter(|metric| metric.id.ends_with("sonnet") || metric.id.ends_with("fable"))
            .all(|metric| !metric.enabled));
        assert!(claude
            .iter()
            .chain(codex.iter())
            .filter(|metric| metric.id.ends_with("extra"))
            .all(|metric| !metric.enabled));
    }

    #[test]
    fn metric_template_runs_once_per_version_or_upstream_change() {
        let mut config = StoredConfig {
            base_url: "https://sub2api.example.com".into(),
            email: "admin@example.com".into(),
            password: "secret".into(),
            upstream: super::Sub2ApiUpstream::Codex,
            metric_template_version: 0,
        };

        assert!(should_apply_metric_template(
            Some(&config),
            super::Sub2ApiUpstream::Codex
        ));
        config.metric_template_version = super::METRIC_TEMPLATE_VERSION;
        assert!(!should_apply_metric_template(
            Some(&config),
            super::Sub2ApiUpstream::Codex
        ));
        assert!(should_apply_metric_template(
            Some(&config),
            super::Sub2ApiUpstream::Claude
        ));
        assert!(should_apply_metric_template(
            None,
            super::Sub2ApiUpstream::Claude
        ));
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
                usage_histories: Default::default(),
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
            upstream: super::Sub2ApiUpstream::Codex,
            metric_template_version: super::METRIC_TEMPLATE_VERSION,
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
        let (account, count) = client
            .first_account(login.value.as_str(), super::Sub2ApiUpstream::Codex)
            .unwrap();
        let body = client
            .usage(
                login.value.as_str(),
                account.id,
                super::Sub2ApiUpstream::Codex,
            )
            .unwrap();
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
        let (account, count) = client
            .first_account(login.value.as_str(), super::Sub2ApiUpstream::Claude)
            .unwrap();
        let body = client
            .usage(
                login.value.as_str(),
                account.id,
                super::Sub2ApiUpstream::Claude,
            )
            .unwrap();
        worker.join().unwrap();

        assert_eq!(account.name, "Claude upstream");
        assert_eq!(count, 1);
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
    fn stats_endpoint_maps_server_history_and_model_breakdown() {
        let responses = vec![format!(
            r#"{{"code":0,"message":"success","data":{STATS_DATA}}}"#
        )];
        let (base_url, requests, worker) = serve_sequence(responses);
        let client = Sub2ApiClient::new(&base_url).unwrap();
        let stats = client.stats("admin-jwt", 42).unwrap();
        worker.join().unwrap();

        let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request.starts_with("GET /api/v1/admin/accounts/42/stats?days=30 HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer admin-jwt"));

        let history = map_stats(stats, Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap());
        assert_eq!(history.daily.len(), 2);
        assert_eq!(history.today.as_ref().unwrap().tokens, 2500);
        assert_eq!(
            history.today.as_ref().unwrap().estimated_cost_usd,
            Some(1.25)
        );
        assert!(!history.today.as_ref().unwrap().cost_estimated);
        assert_eq!(history.yesterday.as_ref().unwrap().tokens, 1000);
        let last_30 = history.last_30_days.unwrap();
        assert_eq!(last_30.tokens, 3500);
        assert_eq!(last_30.estimated_cost_usd, Some(1.65));
        assert_eq!(
            last_30
                .model_breakdown
                .as_ref()
                .unwrap()
                .models
                .iter()
                .map(|model| (model.model.as_str(), model.total_tokens))
                .collect::<Vec<_>>(),
            [("claude-sonnet-5", 2500), ("claude-haiku-4-5", 1000)]
        );
        assert_eq!(
            last_30.model_breakdown.unwrap().source_note,
            "From Sub2API server usage logs"
        );
    }

    #[test]
    fn successful_empty_and_zero_stats_stay_unbacked() {
        let cases = [
            AccountStats {
                history: Vec::new(),
                models: Vec::new(),
            },
            AccountStats {
                history: vec![AccountStatsDay {
                    date: "2026-08-24".into(),
                    tokens: 0,
                    cost: Some(0.0),
                    actual_cost: Some(0.0),
                }],
                models: Vec::new(),
            },
        ];

        for stats in cases {
            let history = map_stats(stats, Utc.with_ymd_and_hms(2026, 8, 24, 12, 0, 0).unwrap());
            assert!(history.today.is_none());
            assert!(history.yesterday.is_none());
            assert!(history.last_30_days.is_none());
            assert!(history.daily.is_empty());
        }
    }

    #[test]
    fn claude_refresh_combines_quota_with_server_usage_history() {
        let responses = vec![
            r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600}}"#.into(),
            r#"{"code":0,"message":"success","data":{"items":[{"id":7,"name":"Claude upstream"}],"total":1,"page":1,"page_size":1,"pages":1}}"#.into(),
            format!(r#"{{"code":0,"message":"success","data":{CLAUDE_USAGE_DATA}}}"#),
            format!(r#"{{"code":0,"message":"success","data":{STATS_DATA}}}"#),
        ];
        let (base_url, requests, worker) = serve_sequence(responses);
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
        assert!(request_paths[3].starts_with("GET /api/v1/admin/accounts/7/stats?days=30 HTTP/1.1"));
    }

    #[test]
    fn stored_connections_without_an_upstream_remain_codex_connections() {
        let config: StoredConfig = serde_json::from_str(
            r#"{"base_url":"https://sub2api.example.com","email":"admin@example.com","password":"secret"}"#,
        )
        .unwrap();

        assert_eq!(config.upstream, super::Sub2ApiUpstream::Codex);
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
}
        other_usage: None,
