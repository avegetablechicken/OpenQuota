mod reported;
mod subscription;
mod upstream_quota;
pub(crate) use subscription::restore_subscription_windows;

use reported::AccountStats;

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
    MetricDefinition, MetricLayout, MetricSection, ProviderDefinition, ProviderSnapshot,
    QuotaWindow, UsageHistories, UsageHistory, UsagePeriodSelection,
};
use crate::storage::Storage;

use super::{
    claude::{
        config::{resolve_provider_base_url as resolve_claude_provider, ClaudeConfigError},
        map_sub2api_usage,
    },
    codex::{
        client::UsageResponse,
        config::{resolve_provider_base_url, CodexConfigError},
        mapper::map_usage,
    },
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
                true,
                true,
                MetricSection::AlwaysVisible,
                true,
                "S",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("weekly"),
                "Weekly",
                "weekly",
                true,
                MetricSection::AlwaysVisible,
                true,
                "W",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("daily"),
                "Daily",
                "daily",
                false,
                MetricSection::OnDemand,
                false,
                "D",
            ),
            MetricDefinition::quota_or_value(
                &metric_id("monthly"),
                "Monthly",
                "monthly",
                false,
                MetricSection::OnDemand,
                false,
                "M",
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
            MetricDefinition::quota(
                &metric_id("claude"),
                "Claude",
                "claude",
                true,
                false,
                MetricSection::OnDemand,
                false,
                "C",
            ),
            MetricDefinition::quota(
                &metric_id("claudeWeekly"),
                "Claude Weekly",
                "claudeWeekly",
                false,
                false,
                MetricSection::OnDemand,
                false,
                "CW",
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

/// Whether a connection's metric can report data for its upstream, matching the Customize page.
pub fn metric_supported(
    provider_id: &str,
    metric_id: &str,
    upstream_provider_id: Option<&str>,
) -> bool {
    let Some(suffix) = metric_id
        .strip_prefix(provider_id)
        .and_then(|rest| rest.strip_prefix('.'))
    else {
        return true;
    };
    if upstream_provider_id == Some("subscription") {
        return matches!(
            suffix,
            "daily" | "weekly" | "monthly" | "today" | "yesterday" | "last30" | "trend"
        );
    }
    let upstream_only: &[&str] = match upstream_provider_id {
        Some("codex") => &["session", "spark", "sparkWeekly", "rateLimitResets"],
        Some("claude") => &["session", "sonnet", "fable"],
        Some("antigravity") => &["session", "claude", "claudeWeekly"],
        Some("grok") => &["monthly"],
        _ => {
            return !matches!(
                suffix,
                "daily" | "monthly" | "extra" | "claude" | "claudeWeekly"
            )
        }
    };
    matches!(
        suffix,
        "weekly" | "trend" | "today" | "yesterday" | "last30"
    ) || upstream_only.contains(&suffix)
}

pub fn metric_template(
    provider_id: &str,
    upstream: Sub2ApiUpstream,
    mode: Sub2ApiMode,
) -> Vec<MetricLayout> {
    let metric = |suffix: &str, enabled: bool, section: MetricSection, pinned: bool| MetricLayout {
        id: format!("{provider_id}.{suffix}"),
        enabled,
        section,
        pinned,
    };
    let always = MetricSection::AlwaysVisible;
    let demand = MetricSection::OnDemand;
    if mode == Sub2ApiMode::Subscription {
        let mut template = vec![
            metric("daily", true, always, true),
            metric("weekly", true, always, true),
            metric("monthly", true, always, false),
            metric("trend", true, always, false),
            metric("today", true, demand, false),
            metric("yesterday", true, demand, false),
            metric("last30", true, demand, false),
        ];
        for suffix in [
            "session",
            "sonnet",
            "fable",
            "claude",
            "claudeWeekly",
            "extra",
            "spark",
            "sparkWeekly",
            "rateLimitResets",
        ] {
            template.push(metric(suffix, false, demand, false));
        }
        return template;
    }
    let (shown, hidden): (Vec<MetricLayout>, &[&str]) = match upstream {
        Sub2ApiUpstream::Claude => (
            vec![
                metric("session", true, always, true),
                metric("weekly", true, always, true),
                metric("trend", true, always, false),
                metric("sonnet", false, demand, false),
                metric("fable", false, demand, false),
                metric("extra", false, demand, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "monthly",
                "spark",
                "sparkWeekly",
                "rateLimitResets",
                "claude",
                "claudeWeekly",
            ],
        ),
        Sub2ApiUpstream::Codex => (
            vec![
                metric("session", true, always, true),
                metric("weekly", true, always, true),
                metric("trend", true, always, false),
                metric("spark", true, demand, false),
                metric("sparkWeekly", true, demand, false),
                metric("rateLimitResets", true, demand, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "monthly",
                "sonnet",
                "fable",
                "extra",
                "claude",
                "claudeWeekly",
            ],
        ),
        // Antigravity's Session and Weekly are its Gemini pool, as in the native provider.
        Sub2ApiUpstream::Antigravity => (
            vec![
                metric("session", true, always, true),
                metric("weekly", true, always, true),
                metric("trend", true, always, false),
                metric("claude", true, demand, false),
                metric("claudeWeekly", true, demand, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "monthly",
                "spark",
                "sparkWeekly",
                "rateLimitResets",
                "sonnet",
                "fable",
                "extra",
            ],
        ),
        // Grok's billing period is weekly or monthly; only the reported one has data.
        Sub2ApiUpstream::Grok => (
            vec![
                metric("weekly", true, always, false),
                metric("monthly", true, always, false),
                metric("trend", true, always, false),
                metric("today", true, demand, false),
                metric("yesterday", true, demand, false),
                metric("last30", true, demand, false),
            ],
            &[
                "daily",
                "session",
                "spark",
                "sparkWeekly",
                "rateLimitResets",
                "sonnet",
                "fable",
                "extra",
                "claude",
                "claudeWeekly",
            ],
        ),
    };
    let mut template = shown;
    template.extend(
        hidden
            .iter()
            .map(|suffix| metric(suffix, false, demand, false)),
    );
    template
}

// Keep server-standard summaries separate from previous local price estimates.
const ADMIN_CACHE_KEY: &str = "sub2api-standard-summary-v1";
const SUBSCRIPTION_CACHE_KEY: &str = "sub2api-subscription-v1";

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Sub2ApiMode {
    #[default]
    Subscription,
    Admin,
}

fn legacy_admin_mode() -> Sub2ApiMode {
    Sub2ApiMode::Admin
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sub2ApiConfigInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_source: Option<Sub2ApiUpstream>,
    #[serde(default)]
    pub use_provider_key: bool,
    #[serde(default = "legacy_admin_mode")]
    pub mode: Sub2ApiMode,
    #[serde(default)]
    pub api_key: String,
    pub base_url: String,
    #[serde(default)]
    // Legacy wire key shared by both upstreams for stored-configuration compatibility.
    pub codex_provider: String,
    #[serde(default)]
    pub custom_base_url: bool,
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub upstream: Sub2ApiUpstream,
}

impl Drop for Sub2ApiConfigInput {
    fn drop(&mut self) {
        self.password.zeroize();
        self.api_key.zeroize();
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sub2ApiConfigState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_source: Option<Sub2ApiUpstream>,
    pub use_provider_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_mode: Option<Box<Sub2ApiConfigState>>,
    pub mode: Sub2ApiMode,
    pub configured: bool,
    pub base_url: String,
    // Legacy wire key shared by both upstreams for stored-configuration compatibility.
    pub codex_provider: String,
    pub custom_base_url: bool,
    pub email: String,
    pub upstream: Sub2ApiUpstream,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Sub2ApiUpstream {
    #[default]
    Codex,
    Claude,
    Antigravity,
    Grok,
}

impl Sub2ApiUpstream {
    fn platform(self) -> &'static str {
        match self {
            Self::Codex => "openai",
            Self::Claude => "anthropic",
            // Sub2API keeps Antigravity's OAuth accounts apart from Gemini CLI's (`gemini`).
            Self::Antigravity => "antigravity",
            Self::Grok => "grok",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::Grok => "Grok",
        }
    }

    fn provider_id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::Grok => "grok",
        }
    }

    /// Whether the Base URL can be read from a named local client configuration.
    fn resolves_provider_names(self) -> bool {
        matches!(self, Self::Codex | Self::Claude)
    }
}

fn default_display_name_for_slot(provider_id: &str) -> String {
    provider_id
        .strip_prefix(PROVIDER_ID)
        .and_then(|suffix| suffix.strip_prefix('@'))
        .map_or_else(
            || "Sub2API".to_owned(),
            |number| format!("Sub2API {number}"),
        )
}

impl std::fmt::Display for Sub2ApiUpstream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_source: Option<Sub2ApiUpstream>,
    #[serde(default = "legacy_admin_mode")]
    mode: Sub2ApiMode,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    api_key: String,
    base_url: String,
    #[serde(default)]
    codex_provider: String,
    #[serde(default = "default_true")]
    custom_base_url: bool,
    email: String,
    password: String,
    #[serde(default)]
    upstream: Sub2ApiUpstream,
    #[serde(default)]
    metric_template_version: u8,
}

/// Keep the active fields at the top level so existing single-mode keychain records
/// deserialize directly. Both modes are replaced together in a single secure write.
#[derive(Serialize, Deserialize)]
struct StoredConnections {
    #[serde(flatten)]
    active: StoredConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    other: Option<StoredConfig>,
}

impl StoredConnections {
    fn for_mode(&self, mode: Sub2ApiMode) -> Option<&StoredConfig> {
        if self.active.mode == mode {
            Some(&self.active)
        } else {
            self.other.as_ref().filter(|config| config.mode == mode)
        }
    }

    fn with_active(active: StoredConfig, previous: Option<Self>) -> Self {
        let other = previous.and_then(|old| {
            if old.active.mode == active.mode {
                old.other
            } else {
                Some(old.active)
            }
        });
        Self { active, other }
    }

    fn state(&self) -> Sub2ApiConfigState {
        let mut state = self.active.state();
        state.other_mode = self.other.as_ref().map(|config| Box::new(config.state()));
        state
    }
}

pub struct Sub2ApiConfigSaveOutcome {
    pub state: Sub2ApiConfigState,
    pub apply_metric_template: bool,
    /// The track the connection used before this save, whose template the layout may still hold.
    pub previous_track: Option<(Sub2ApiMode, Sub2ApiUpstream)>,
}

impl Drop for StoredConfig {
    fn drop(&mut self) {
        self.password.zeroize();
        self.api_key.zeroize();
    }
}

impl StoredConfig {
    fn subscription_key(&self) -> Result<Zeroizing<String>, Sub2ApiError> {
        if !self.api_key.is_empty() {
            return Ok(Zeroizing::new(self.api_key.clone()));
        }
        if self.custom_base_url || self.codex_provider.is_empty() {
            return Err(Sub2ApiError::MissingApiKey);
        }
        let (url, key) = match self.provider_source.unwrap_or(self.upstream) {
            Sub2ApiUpstream::Codex => {
                let (url, key) =
                    super::codex::config::subscription_credentials(&self.codex_provider)?;
                (normalized_codex_provider_base_url_text(&url)?, key)
            }
            Sub2ApiUpstream::Claude => {
                let (url, key) =
                    super::claude::config::subscription_credentials(&self.codex_provider)?;
                (normalized_base_url_text(&url)?, key)
            }
            // Subscriptions read keys only from Codex and Claude configurations.
            Sub2ApiUpstream::Antigravity | Sub2ApiUpstream::Grok => {
                return Err(Sub2ApiError::MissingApiKey)
            }
        };
        if url != self.base_url {
            return Err(Sub2ApiError::ProviderEndpointChanged);
        }
        key.map(Zeroizing::new)
            .ok_or(Sub2ApiError::MissingProviderKey)
    }

    fn cache_key(&self) -> &'static str {
        match self.mode {
            Sub2ApiMode::Subscription => SUBSCRIPTION_CACHE_KEY,
            Sub2ApiMode::Admin => ADMIN_CACHE_KEY,
        }
    }

    fn state(&self) -> Sub2ApiConfigState {
        Sub2ApiConfigState {
            provider_source: self.provider_source,
            use_provider_key: self.mode == Sub2ApiMode::Subscription
                && !self.custom_base_url
                && self.api_key.is_empty(),
            configured: true,
            mode: self.mode,
            base_url: self.base_url.clone(),
            codex_provider: self.codex_provider.clone(),
            custom_base_url: self.custom_base_url
                || !self
                    .provider_source
                    .unwrap_or(self.upstream)
                    .resolves_provider_names()
                || (self.upstream == Sub2ApiUpstream::Claude && self.codex_provider.is_empty()),
            email: self.email.clone(),
            upstream: self.upstream,

            ..Sub2ApiConfigState::default()
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
enum Sub2ApiError {
    #[error("Configure the Sub2API connection in Customize.")]
    MissingConfig,
    #[error("Enter a valid Base URL, email, and password.")]
    InvalidConfig,
    #[error("Enter an API key for this Sub2API subscription.")]
    MissingApiKey,
    #[error("No API key was found for this provider. Check its credentials or enter an API key.")]
    MissingProviderKey,
    #[error(
        "The provider Base URL has changed. Save the connection again to use the new endpoint."
    )]
    ProviderEndpointChanged,
    #[error("Sub2API rejected the API key or denied access to its subscription.")]
    ApiKeyRejected,
    #[error("This API key does not report subscription details. Use a key linked to a subscription without key-level limits.")]
    NoSubscription,
    #[error("Clear Provider when using a custom Base URL.")]
    ProviderWithCustomBaseUrl,
    #[error(transparent)]
    CodexConfig(#[from] CodexConfigError),
    #[error(transparent)]
    ClaudeConfig(#[from] ClaudeConfigError),
    #[error("The Sub2API connection could not be read or updated securely.")]
    CredentialStorage,
    #[error(
        "Sub2API rejected the administrator login. Check the instance address and login details."
    )]
    LoginRejected,
    #[error("The relay requires a Bearer API token before it can forward a password login. In Custom Base URL, enter the Sub2API instance address.")]
    RelayRequiresKey,
    #[error("Sub2API rejected the session token for an administrator request.")]
    Authentication,
    #[error("This Sub2API login does not have administrator access.")]
    Permission,
    #[error("Sub2API requires two-factor authentication, which is not supported yet.")]
    TwoFactorRequired,
    #[error("Sub2API is rate limiting requests. Try again later.")]
    RateLimited,
    #[error("Sub2API request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("Could not reach Sub2API. Check the Base URL and connection.")]
    ConnectionFailed,
    #[error("The local Sub2API usage cache is unavailable.")]
    HistoryCache,
    #[error("Sub2API returned an invalid response.")]
    InvalidResponse,
}

impl From<Sub2ApiError> for ProviderError {
    fn from(error: Sub2ApiError) -> Self {
        use crate::models::ProviderErrorKind as Kind;

        let kind = match error {
            Sub2ApiError::MissingConfig
            | Sub2ApiError::LoginRejected
            | Sub2ApiError::RelayRequiresKey
            | Sub2ApiError::Authentication
            | Sub2ApiError::ApiKeyRejected
            | Sub2ApiError::MissingApiKey
            | Sub2ApiError::MissingProviderKey
            | Sub2ApiError::TwoFactorRequired => Kind::Authentication,
            Sub2ApiError::Permission => Kind::Permission,
            Sub2ApiError::CredentialStorage => Kind::CredentialStorage,
            Sub2ApiError::HistoryCache
            | Sub2ApiError::ProviderEndpointChanged
            | Sub2ApiError::CodexConfig(_)
            | Sub2ApiError::ClaudeConfig(_) => Kind::LocalData,
            Sub2ApiError::RateLimited => Kind::RateLimited,
            Sub2ApiError::RequestFailed(_) | Sub2ApiError::ConnectionFailed => Kind::Network,
            Sub2ApiError::InvalidConfig | Sub2ApiError::ProviderWithCustomBaseUrl => Kind::Internal,
            Sub2ApiError::InvalidResponse | Sub2ApiError::NoSubscription => Kind::InvalidResponse,
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
}

struct Sub2ApiClient {
    client: Client,
    base_url: Url,
}

impl Sub2ApiClient {
    #[cfg(test)]
    fn new(base_url: &str) -> Result<Self, Sub2ApiError> {
        Self::for_provider(base_url, "sub2api")
    }

    fn for_provider(base_url: &str, provider_id: &str) -> Result<Self, Sub2ApiError> {
        let base_url = normalize_base_url(base_url)?;
        let client = crate::http_client::ProviderClient::new(provider_id, |builder| {
            builder
                .gzip(true)
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("OpenQuota/", env!("CARGO_PKG_VERSION")))
        })
        .and_then(|client| client.current())
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
        if status == StatusCode::UNAUTHORIZED {
            let body = response.json::<Value>().ok();
            let message = body
                .as_ref()
                .and_then(|body| body.pointer("/error/message"))
                .and_then(Value::as_str);
            if matches!(
                message,
                Some("An API Bearer token is required." | "A configured Bearer token is required.")
            ) {
                return Err(Sub2ApiError::RelayRequiresKey);
            }
            return Err(Sub2ApiError::LoginRejected);
        }
        if status == StatusCode::FORBIDDEN {
            return Err(Sub2ApiError::LoginRejected);
        }
        if !status.is_success() {
            return Err(classify_status(status));
        }
        let envelope: Envelope<LoginData> = decode_response("login", response)?;
        if envelope.code != 0 {
            return Err(Sub2ApiError::LoginRejected);
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

    fn accounts(
        &self,
        token: &str,
        upstream: Sub2ApiUpstream,
    ) -> Result<Vec<Sub2ApiAccount>, Sub2ApiError> {
        let mut accounts = Vec::new();
        let mut page_number = 1;
        loop {
            let mut url = self.endpoint(&["api", "v1", "admin", "accounts"])?;
            url.query_pairs_mut()
                .append_pair("page", &page_number.to_string())
                .append_pair("page_size", "100")
                .append_pair("platform", upstream.platform())
                .append_pair("type", "oauth")
                .append_pair("status", "active")
                .append_pair("sort_by", "name")
                .append_pair("sort_order", "asc")
                .append_pair("lite", "true");
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
            let envelope: Envelope<AccountPage> = decode_response("account discovery", response)?;
            if envelope.code != 0 {
                return Err(Sub2ApiError::InvalidResponse);
            }
            let page = envelope.data.ok_or(Sub2ApiError::InvalidResponse)?;
            let previous_len = accounts.len();
            for account in page.items {
                if !accounts
                    .iter()
                    .any(|known: &Sub2ApiAccount| known.id == account.id)
                {
                    accounts.push(account);
                }
            }
            if accounts.len() >= page.total {
                return Ok(accounts);
            }
            if accounts.len() == previous_len {
                return Err(Sub2ApiError::InvalidResponse);
            }
            page_number += 1;
        }
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
            Sub2ApiUpstream::Claude | Sub2ApiUpstream::Antigravity | Sub2ApiUpstream::Grok => {
                &["api", "v1", "admin", "accounts", &account_id, "usage"]
            }
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
        let envelope: Envelope<Value> = decode_response("upstream usage", response)?;
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
            .map_err(|error| transport_error("account statistics", &error))?;
        if !response.status().is_success() {
            return Err(classify_status(response.status()));
        }
        let envelope: Envelope<AccountStats> = decode_response("account statistics", response)?;
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

/// Read the body separately: `Response::json` also reports body timeouts as decode
/// errors. Never log the response or error text, which can contain credentials.
fn decode_response<T: serde::de::DeserializeOwned>(
    stage: &str,
    response: reqwest::blocking::Response,
) -> Result<T, Sub2ApiError> {
    let status = response.status().as_u16();
    let started = Instant::now();
    let bytes = response.bytes().map_err(|error| {
        transport_error(&format!("{stage} response body (HTTP {status})"), &error)
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        crate::app_warn!(
            "http",
            "Sub2API {stage} JSON decode failed (HTTP {status}, bytes={}, body_read_ms={}, category={:?}, line={}, column={})",
            bytes.len(),
            started.elapsed().as_millis(),
            error.classify(),
            error.line(),
            error.column()
        );
        Sub2ApiError::InvalidResponse
    })
}

pub struct Sub2ApiProvider {
    provider_id: String,
    display_name: String,
    configured_marker: PathBuf,
    configured: AtomicBool,
    session: Mutex<Option<CachedSession>>,
    model_families: Mutex<Option<subscription::CachedModelFamilies>>,
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
            model_families: Mutex::new(None),
        }
    }

    pub fn config_state(&self) -> Result<Sub2ApiConfigState, ProviderError> {
        Ok(self
            .load_connections()?
            .map(|connections| connections.state())
            .unwrap_or_default())
    }

    fn cache_path(&self) -> PathBuf {
        self.configured_marker.with_extension("usage.sqlite3")
    }

    pub fn save_config(
        &self,
        input: Sub2ApiConfigInput,
    ) -> Result<Sub2ApiConfigSaveOutcome, ProviderError> {
        let previous = self.load_connections()?;
        let previous_track = previous
            .as_ref()
            .map(|saved| (saved.active.mode, saved.active.upstream));
        let apply_metric_template = should_apply_metric_template(
            previous.as_ref().map(|saved| &saved.active),
            input.mode,
            input.upstream,
        );
        let (base_url, codex_provider, custom_base_url) = connection_target(
            &input,
            resolve_codex_provider_base_url,
            resolve_claude_provider_base_url,
        )?;
        let secret = credential_for_save(
            &input,
            &base_url,
            previous
                .as_ref()
                .and_then(|saved| saved.for_mode(input.mode)),
        )?;
        let (email, password, api_key) = match input.mode {
            Sub2ApiMode::Admin => (input.email.trim().to_owned(), secret, String::new()),
            Sub2ApiMode::Subscription => (String::new(), String::new(), secret),
        };
        let config = StoredConfig {
            provider_source: input.provider_source,
            mode: input.mode,
            api_key,
            base_url,
            codex_provider,
            custom_base_url,
            email,
            password,
            upstream: input.upstream,
            metric_template_version: if apply_metric_template {
                0
            } else {
                METRIC_TEMPLATE_VERSION
            },
        };

        if config.mode == Sub2ApiMode::Subscription && config.api_key.is_empty() {
            // Validate locally, then resolve again on refresh to follow key rotations.
            config.subscription_key()?;
        }
        let connections = StoredConnections::with_active(config, previous);
        self.write_connections(&connections)?;
        self.set_configuration_marker(Some(connections.active.cache_key()))?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = None;
        }
        if let Ok(mut cached) = self.model_families.lock() {
            *cached = None;
        }
        Ok(Sub2ApiConfigSaveOutcome {
            state: connections.state(),
            apply_metric_template,
            previous_track,
        })
    }

    pub fn mark_metric_template_applied(&self) -> Result<(), ProviderError> {
        let mut config = self
            .load_connections()?
            .ok_or(Sub2ApiError::MissingConfig)?;
        config.active.metric_template_version = METRIC_TEMPLATE_VERSION;
        self.write_connections(&config).map_err(ProviderError::from)
    }

    pub fn delete_config(&self) -> Result<Sub2ApiConfigState, ProviderError> {
        match fs::remove_file(self.cache_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Sub2ApiError::HistoryCache.into()),
        }
        delete_owned_password(CREDENTIAL_SERVICE, self.credential_account())
            .map_err(|_| Sub2ApiError::CredentialStorage)?;
        self.set_configuration_marker(None)?;
        if let Ok(mut cached) = self.session.lock() {
            *cached = None;
        }
        if let Ok(mut cached) = self.model_families.lock() {
            *cached = None;
        }
        Ok(Sub2ApiConfigState::default())
    }

    fn connect(&self, config: &StoredConfig) -> Result<CachedSession, Sub2ApiError> {
        let client = Sub2ApiClient::for_provider(&config.base_url, &self.provider_id)?;
        let login = client.login(&config.email, &config.password)?;
        Ok(CachedSession {
            scope: session_scope(config),
            token: login.value,
            expires_at: token_expiry(login.expires_in),
        })
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

    #[cfg(test)]
    fn refresh_snapshot(&self, config: &StoredConfig) -> Result<ProviderSnapshot, Sub2ApiError> {
        self.refresh_snapshot_with_preview(config, &|_| {})
    }

    /// Fetches every account's quota first and hands that to `preview`, then syncs usage
    /// summaries. The quota preview stays visible while server statistics are loading.
    fn refresh_snapshot_with_preview(
        &self,
        config: &StoredConfig,
        preview: &dyn Fn(ProviderSnapshot),
    ) -> Result<ProviderSnapshot, Sub2ApiError> {
        let client = Sub2ApiClient::for_provider(&config.base_url, &self.provider_id)?;
        if config.mode == Sub2ApiMode::Subscription {
            let key = config.subscription_key()?;
            let mut snapshot =
                client.subscription_snapshot(&key, self.empty_snapshot(Vec::new()))?;
            // Codex and Claude double as the configuration an address is read from, so their
            // mark comes from the key's model catalog. Antigravity and Grok are chosen only to
            // name the upstream, whose catalogs can mix model families.
            snapshot.upstream_provider_id = if config.upstream.resolves_provider_names() {
                client
                    .subscription_icon(&key, &self.model_families)
                    .map(str::to_owned)
            } else {
                Some(config.upstream.provider_id().to_owned())
            };
            return Ok(snapshot);
        }
        let mut session = self.session(config)?;
        let accounts = self.with_session(config, &mut session, |token| {
            client.accounts(token, config.upstream)
        })?;
        let now = Utc::now();
        let mut snapshots = Vec::with_capacity(accounts.len());
        for account in &accounts {
            let snapshot = self.with_session(config, &mut session, |token| {
                self.account_quota(config, &client, token, account, now)
            })?;
            snapshots.push(crate::models::AccountSnapshot {
                id: account.id.to_string(),
                name: format!("Sub2API · {} · {}", config.upstream, account.name),
                snapshot,
            });
        }
        preview(self.connection_snapshot(snapshots.clone(), UsageHistory::default()));

        let mut total = AccountStats::default();
        for (account, snapshot) in accounts.iter().zip(&mut snapshots) {
            let stats = self.with_session(config, &mut session, |token| {
                client.stats(token, account.id)
            })?;
            total.history.extend(stats.history.iter().cloned());
            total.models.extend(stats.models.iter().cloned());
            snapshot.snapshot.usage_histories =
                UsageHistories::account(stats.into_standard_history(now));
        }
        Ok(self.connection_snapshot(snapshots, total.into_standard_history(now)))
    }

    /// Runs one administrator request, logging in again once if the cached token expired.
    fn with_session<T>(
        &self,
        config: &StoredConfig,
        session: &mut CachedSession,
        mut request: impl FnMut(&str) -> Result<T, Sub2ApiError>,
    ) -> Result<T, Sub2ApiError> {
        match request(session.token.as_str()) {
            Err(Sub2ApiError::Authentication) => {
                *session = self.connect(config)?;
                if let Ok(mut cached) = self.session.lock() {
                    *cached = Some(clone_session(session));
                }
                request(session.token.as_str())
            }
            result => result,
        }
    }

    /// The connection-level snapshot mirrors the first account and lists all of them.
    fn connection_snapshot(
        &self,
        accounts: Vec<crate::models::AccountSnapshot>,
        total: UsageHistory,
    ) -> ProviderSnapshot {
        let mut snapshot = accounts
            .first()
            .map(|account| account.snapshot.clone())
            .unwrap_or_else(|| self.empty_snapshot(Vec::new()));
        snapshot.usage_histories = UsageHistories::account(total);
        snapshot.accounts = Some(accounts);
        snapshot
    }

    fn empty_snapshot(&self, warnings: Vec<String>) -> ProviderSnapshot {
        ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: self.provider_id.clone(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories::account(UsageHistory::default()),
            warnings,
            refreshed_at: Utc::now(),
        }
    }

    fn account_quota(
        &self,
        config: &StoredConfig,
        client: &Sub2ApiClient,
        token: &str,
        account: &Sub2ApiAccount,
        now: chrono::DateTime<Utc>,
    ) -> Result<ProviderSnapshot, Sub2ApiError> {
        let body = client.usage(token, account.id, config.upstream)?;
        let mut warnings = Vec::new();
        let (plan, quotas, value_metrics) = match config.upstream {
            Sub2ApiUpstream::Codex => {
                let response = UsageResponse {
                    status: StatusCode::OK,
                    headers: HashMap::new(),
                    body,
                };
                let mut mapped =
                    map_usage(&response, None, now).map_err(|_| Sub2ApiError::InvalidResponse)?;
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
            Sub2ApiUpstream::Antigravity | Sub2ApiUpstream::Grok => {
                let mapped = if config.upstream == Sub2ApiUpstream::Antigravity {
                    upstream_quota::map_antigravity(&body, now)
                } else {
                    upstream_quota::map_grok(&body, now)
                };
                warnings = mapped.warnings;
                (mapped.plan, mapped.quotas, Vec::new())
            }
        };
        let quotas = normalize_refreshed_quotas(config.upstream, quotas, now);
        Ok(ProviderSnapshot {
            upstream_provider_id: Some(config.upstream.provider_id().into()),
            accounts: None,
            provider_id: self.provider_id.clone(),
            plan,
            quotas,
            value_metrics,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: UsageHistories::account(UsageHistory::default()),
            warnings,
            refreshed_at: now,
        })
    }

    fn credential_account(&self) -> &str {
        if self.provider_id == PROVIDER_ID {
            "connection"
        } else {
            &self.provider_id
        }
    }

    fn set_configuration_marker(&self, cache_key: Option<&str>) -> Result<(), Sub2ApiError> {
        if let Some(cache_key) = cache_key {
            let parent = self
                .configured_marker
                .parent()
                .ok_or(Sub2ApiError::CredentialStorage)?;
            fs::create_dir_all(parent).map_err(|_| Sub2ApiError::CredentialStorage)?;
            fs::write(&self.configured_marker, cache_key)
                .map_err(|_| Sub2ApiError::CredentialStorage)?;
        } else {
            match fs::remove_file(&self.configured_marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(Sub2ApiError::CredentialStorage),
            }
        }
        self.configured.store(cache_key.is_some(), Ordering::SeqCst);
        Ok(())
    }

    fn load_config(&self) -> Result<Option<StoredConfig>, Sub2ApiError> {
        Ok(self.load_connections()?.map(|saved| saved.active))
    }

    fn load_connections(&self) -> Result<Option<StoredConnections>, Sub2ApiError> {
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

    fn write_connections(&self, config: &StoredConnections) -> Result<(), Sub2ApiError> {
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
                let display_name = default_display_name_for_slot(&provider_id);
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

    fn supports_account_names(&self) -> bool {
        self.configured.load(Ordering::SeqCst)
    }

    fn account_identity(&self) -> Option<&str> {
        self.configured
            .load(Ordering::SeqCst)
            .then_some(self.provider_id.as_str())
    }

    fn display_name(&self) -> String {
        self.display_name.clone()
    }

    fn cache_identity(&self) -> CacheIdentity<'_> {
        if !self.configured.load(Ordering::SeqCst) {
            return CacheIdentity::Resolved("sub2api-unconfigured");
        }
        let key = match fs::read_to_string(&self.configured_marker).as_deref() {
            Ok(SUBSCRIPTION_CACHE_KEY) => SUBSCRIPTION_CACHE_KEY,
            _ => ADMIN_CACHE_KEY,
        };
        CacheIdentity::Resolved(key)
    }

    fn supports_connection_configuration(&self) -> bool {
        true
    }

    fn upstream_provider_id(&self) -> Option<&'static str> {
        self.load_config()
            .ok()
            .flatten()
            .map(|config| match config.mode {
                Sub2ApiMode::Subscription => "subscription",
                Sub2ApiMode::Admin => config.upstream.provider_id(),
            })
    }

    fn refresh_for_service(&self) -> Result<super::ProviderRefresh, ProviderError> {
        self.refresh_for_service_with_preview(&|_| {})
    }

    fn refresh_for_service_with_preview(
        &self,
        preview: &dyn Fn(super::ProviderRefresh),
    ) -> Result<super::ProviderRefresh, ProviderError> {
        let config = self.load_config()?.ok_or(Sub2ApiError::MissingConfig)?;
        let cache_identity = config.cache_key();
        let snapshot = self.refresh_snapshot_with_preview(&config, &|snapshot| {
            preview(super::ProviderRefresh {
                snapshot,
                cache_identity: Some(cache_identity.to_owned()),
                account: None,
            })
        })?;
        Ok(super::ProviderRefresh {
            snapshot,
            cache_identity: Some(cache_identity.to_owned()),
            account: None,
        })
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        Ok(self.refresh_for_service()?.snapshot)
    }
}

fn normalized_base_url_text(value: &str) -> Result<String, Sub2ApiError> {
    let url = normalize_base_url(value)?;
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn resolve_codex_provider_base_url(value: &str) -> Result<String, Sub2ApiError> {
    normalized_codex_provider_base_url_text(&resolve_provider_base_url(value)?)
}

pub fn codex_provider_base_url(value: &str) -> Result<String, ProviderError> {
    resolve_codex_provider_base_url(value).map_err(ProviderError::from)
}

fn resolve_claude_provider_base_url(name: &str) -> Result<String, Sub2ApiError> {
    normalized_base_url_text(&resolve_claude_provider(name)?)
}

pub fn claude_provider_base_url(name: &str) -> Result<String, ProviderError> {
    resolve_claude_provider_base_url(name).map_err(ProviderError::from)
}

fn default_true() -> bool {
    true
}

fn normalized_codex_provider_base_url_text(value: &str) -> Result<String, Sub2ApiError> {
    let mut url = normalize_base_url(value)?;
    let path = url.path().trim_end_matches('/');
    if let Some(path) = path.strip_suffix("/v1") {
        let path = if path.is_empty() { "/" } else { path }.to_owned();
        url.set_path(&path);
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn connection_target(
    input: &Sub2ApiConfigInput,
    resolve_codex: impl FnOnce(&str) -> Result<String, Sub2ApiError>,
    resolve_claude: impl FnOnce(&str) -> Result<String, Sub2ApiError>,
) -> Result<(String, String, bool), Sub2ApiError> {
    let provider_name = input.codex_provider.trim().to_owned();
    match input.provider_source.unwrap_or(input.upstream) {
        _ if input.custom_base_url => {
            if !provider_name.is_empty() {
                return Err(Sub2ApiError::ProviderWithCustomBaseUrl);
            }
            Ok((
                normalized_base_url_text(&input.base_url)?,
                String::new(),
                true,
            ))
        }
        Sub2ApiUpstream::Codex => Ok((resolve_codex(&provider_name)?, provider_name, false)),
        Sub2ApiUpstream::Claude => Ok((resolve_claude(&provider_name)?, provider_name, false)),
        // Neither client keeps its endpoint under a provider name, so the address is entered.
        Sub2ApiUpstream::Antigravity | Sub2ApiUpstream::Grok => {
            if !provider_name.is_empty() {
                return Err(Sub2ApiError::ProviderWithCustomBaseUrl);
            }
            Ok((
                normalized_base_url_text(&input.base_url)?,
                String::new(),
                true,
            ))
        }
    }
}

fn credential_for_save(
    input: &Sub2ApiConfigInput,
    base_url: &str,
    previous: Option<&StoredConfig>,
) -> Result<String, Sub2ApiError> {
    let previous = previous.filter(|config| {
        config.base_url == base_url
            && config.mode == input.mode
            && (input.mode == Sub2ApiMode::Admin
                || config.provider_source.unwrap_or(config.upstream)
                    == input.provider_source.unwrap_or(input.upstream)
                || (config.custom_base_url && input.custom_base_url))
    });
    match input.mode {
        Sub2ApiMode::Admin => {
            if input.email.trim().is_empty() {
                return Err(Sub2ApiError::InvalidConfig);
            }
            if !input.password.trim().is_empty() {
                return Ok(input.password.clone());
            }
            previous
                .map(|config| &config.password)
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or(Sub2ApiError::InvalidConfig)
        }
        Sub2ApiMode::Subscription => {
            let entered = input.api_key.trim();
            if !entered.is_empty() {
                return Ok(entered.to_owned());
            }
            if let Some(saved) = previous.filter(|config| {
                !input.use_provider_key
                    && config.custom_base_url == input.custom_base_url
                    && config.codex_provider == input.codex_provider.trim()
                    && !config.api_key.is_empty()
            }) {
                return Ok(saved.api_key.clone());
            }
            if input.custom_base_url {
                return Err(Sub2ApiError::MissingApiKey);
            }
            Ok(String::new())
        }
    }
}

fn session_scope(config: &StoredConfig) -> String {
    let password_fingerprint = crate::hashing::sha256_hex(config.password.as_bytes());
    crate::hashing::sha256_hex(
        format!(
            "{}\0{}\0{}\0{}",
            config.base_url,
            config.email,
            password_fingerprint,
            config.upstream.label()
        )
        .as_bytes(),
    )
}

fn should_apply_metric_template(
    previous: Option<&StoredConfig>,
    mode: Sub2ApiMode,
    upstream: Sub2ApiUpstream,
) -> bool {
    previous.is_none_or(|previous| {
        previous.metric_template_version < METRIC_TEMPLATE_VERSION
            || previous.mode != mode
            || (mode == Sub2ApiMode::Admin && previous.upstream != upstream)
    })
}

fn normalize_refreshed_quotas(
    upstream: Sub2ApiUpstream,
    mut quotas: Vec<QuotaWindow>,
    now: chrono::DateTime<Utc>,
) -> Vec<QuotaWindow> {
    let expected: &[&str] = match upstream {
        Sub2ApiUpstream::Codex => &["session", "weekly", "spark", "sparkWeekly"],
        Sub2ApiUpstream::Claude => &["session", "weekly", "sonnet", "fable"],
        Sub2ApiUpstream::Antigravity => &["session", "weekly", "claude", "claudeWeekly"],
        Sub2ApiUpstream::Grok => &["weekly", "monthly"],
    };
    let mut normalized = Vec::with_capacity(quotas.len().max(expected.len()));

    for &id in expected {
        let Some(index) = quotas.iter().position(|quota| quota.id == id) else {
            continue;
        };
        let mut quota = quotas.remove(index);
        if quota
            .resets_at
            .as_ref()
            .is_some_and(|resets_at| *resets_at <= now)
        {
            quota.used_percent = 0.0;
            quota.resets_at = None;
        }
        normalized.push(quota);
    }

    normalized.extend(quotas);
    normalized
}

fn token_expiry(expires_in: u64) -> Instant {
    Instant::now() + Duration::from_secs(expires_in.saturating_sub(60).max(1))
}

fn clone_session(session: &CachedSession) -> CachedSession {
    CachedSession {
        scope: session.scope.clone(),
        token: Zeroizing::new(session.token.to_string()),
        expires_at: session.expires_at,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
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
        connection_target, credential_for_save, default_display_name_for_slot, definition_for,
        metric_template, normalize_base_url, normalize_refreshed_quotas,
        normalized_codex_provider_base_url_text, session_scope, should_apply_metric_template,
        StoredConfig, Sub2ApiClient, Sub2ApiConfigInput, Sub2ApiError, Sub2ApiProvider,
        Sub2ApiProviders,
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

    fn usage_data(_account: i64, now: chrono::DateTime<Utc>) -> String {
        let today = now.with_timezone(&chrono::Local).date_naive();
        serde_json::json!({"code":0,"data":{
            "history":[
                {"date":(today-chrono::Days::new(1)).to_string(),"tokens":1000,"cost":1.0,"actual_cost":45.0},
                {"date":today.to_string(),"tokens":2500,"cost":2.0,"actual_cost":60.0}
            ],
            "models":[
                {"model":"claude-haiku-4-5","total_tokens":1000,"cost":1.0,"actual_cost":45.0},
                {"model":"claude-sonnet-5","total_tokens":2500,"cost":2.0,"actual_cost":60.0}
            ]
        }}).to_string()
    }

    pub(super) fn serve_sequence(
        responses: Vec<String>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        serve_sequence_with_status(responses.into_iter().map(|body| (200, body)).collect())
    }

    fn serve_sequence_with_status(
        responses: Vec<(u16, String)>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            for (status, body) in responses {
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
                    "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}"), receiver, worker)
    }

    #[test]
    fn response_body_timeout_and_truncation_are_network_errors() {
        for stall in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let (release, released) = mpsc::channel();
            let worker = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).unwrap() > 0);
                stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\n\r\n{",
                ).unwrap();
                if stall {
                    let _ = released.recv_timeout(Duration::from_secs(5));
                }
            });
            let client = reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(1))
                .build()
                .unwrap();
            let response = client.get(format!("http://{address}")).send().unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let result = super::decode_response::<serde_json::Value>("test history", response);
            let _ = release.send(());
            worker.join().unwrap();
            assert_eq!(result, Err(Sub2ApiError::ConnectionFailed), "stall={stall}");
        }
    }

    #[test]
    fn response_decoder_preserves_json_and_schema_failures() {
        let (base, _requests, worker) = serve_sequence(vec![
            "<html>gateway response</html>".into(),
            r#"{"code":0,"data":{"items":[],"total":"wrong-type"}}"#.into(),
            r#"{"code":0,"data":{"items":[],"total":0}}"#.into(),
        ]);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        for expected_valid in [false, false, true] {
            let response = client.get(&base).send().unwrap();
            let result = super::decode_response::<super::Envelope<super::AccountPage>>(
                "test accounts",
                response,
            );
            if expected_valid {
                assert_eq!(result.unwrap().data.unwrap().total, 0);
            } else {
                assert!(matches!(result, Err(Sub2ApiError::InvalidResponse)));
            }
        }
        worker.join().unwrap();
    }

    #[test]
    fn metric_support_follows_the_configured_upstream() {
        use super::metric_supported;

        assert!(!metric_supported("sub2api@2", "sub2api@2.extra", None));
        assert!(metric_supported("sub2api@2", "sub2api@2.spark", None));
        assert!(metric_supported(
            "sub2api@2",
            "sub2api@2.spark",
            Some("codex")
        ));
        assert!(!metric_supported(
            "sub2api@2",
            "sub2api@2.spark",
            Some("claude")
        ));
        assert!(!metric_supported(
            "sub2api@2",
            "sub2api@2.rateLimitResets",
            Some("claude")
        ));
        assert!(metric_supported(
            "sub2api@2",
            "sub2api@2.fable",
            Some("claude")
        ));
        assert!(!metric_supported(
            "sub2api@2",
            "sub2api@2.fable",
            Some("codex")
        ));
        assert!(metric_supported(
            "sub2api@2",
            "sub2api@2.session",
            Some("codex")
        ));
        assert!(metric_supported("sub2api", "sub2api@2.extra", None));
        assert!(!metric_supported("sub2api@2", "sub2api@2.claude", None));
        for suffix in ["session", "weekly", "claude", "claudeWeekly", "last30"] {
            assert!(metric_supported(
                "sub2api@2",
                &format!("sub2api@2.{suffix}"),
                Some("antigravity")
            ));
        }
        for suffix in ["sonnet", "spark", "monthly", "extra"] {
            assert!(!metric_supported(
                "sub2api@2",
                &format!("sub2api@2.{suffix}"),
                Some("antigravity")
            ));
        }
        for suffix in ["weekly", "monthly", "trend", "today"] {
            assert!(metric_supported(
                "sub2api@2",
                &format!("sub2api@2.{suffix}"),
                Some("grok")
            ));
        }
        for suffix in ["session", "claude", "daily", "expires", "extra"] {
            assert!(!metric_supported(
                "sub2api@2",
                &format!("sub2api@2.{suffix}"),
                Some("grok")
            ));
        }
        assert!(!metric_supported(
            "sub2api@2",
            "sub2api@2.claude",
            Some("claude")
        ));
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
            (
                super::Sub2ApiUpstream::Antigravity,
                crate::providers::antigravity::definition(),
                "",
            ),
            (
                super::Sub2ApiUpstream::Grok,
                crate::providers::grok::definition(),
                "payAsYouGo",
            ),
        ] {
            let template = metric_template("sub2api@2", upstream, super::Sub2ApiMode::Admin);
            for definition in native.metrics {
                let suffix = match definition.id.split_once('.').unwrap().1 {
                    "geminiPro" => "session",
                    "geminiWeekly" => "weekly",
                    suffix => suffix,
                };
                if suffix == unsupported {
                    continue;
                }
                assert!(
                    super::metric_supported(
                        "sub2api@2",
                        &format!("sub2api@2.{suffix}"),
                        Some(upstream.provider_id())
                    ),
                    "{upstream} {suffix}"
                );
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

        let claude = metric_template(
            "sub2api@2",
            super::Sub2ApiUpstream::Claude,
            super::Sub2ApiMode::Admin,
        );
        assert!(claude
            .iter()
            .filter(|metric| metric.id.ends_with("spark")
                || metric.id.ends_with("sparkWeekly")
                || metric.id.ends_with("rateLimitResets"))
            .all(|metric| !metric.enabled));
        let codex = metric_template(
            "sub2api@2",
            super::Sub2ApiUpstream::Codex,
            super::Sub2ApiMode::Admin,
        );
        assert!(codex
            .iter()
            .filter(|metric| metric.id.ends_with("sonnet") || metric.id.ends_with("fable"))
            .all(|metric| !metric.enabled));
        assert!(claude
            .iter()
            .chain(codex.iter())
            .filter(|metric| metric.id.ends_with("extra"))
            .all(|metric| !metric.enabled));
        // Every template lists every metric, and enables only what its upstream reports.
        let definition = definition_for("sub2api@2", "Sub2API 2");
        for upstream in [
            super::Sub2ApiUpstream::Codex,
            super::Sub2ApiUpstream::Claude,
            super::Sub2ApiUpstream::Antigravity,
            super::Sub2ApiUpstream::Grok,
        ] {
            let template = metric_template("sub2api@2", upstream, super::Sub2ApiMode::Admin);
            for known in &definition.metrics {
                let suffix = known.id.split_once('.').unwrap().1;
                assert!(
                    template.iter().any(|metric| metric.id == known.id),
                    "{upstream} {suffix}"
                );
            }
            for metric in template {
                assert!(
                    definition.metrics.iter().any(|known| known.id == metric.id),
                    "{upstream} {}",
                    metric.id
                );
                if metric.enabled {
                    assert!(
                        super::metric_supported(
                            "sub2api@2",
                            &metric.id,
                            Some(upstream.provider_id())
                        ),
                        "{upstream} {}",
                        metric.id
                    );
                }
            }
        }
    }

    #[test]
    fn claude_refresh_omits_missing_and_resets_expired_quotas() {
        let body = serde_json::json!({
            "limits": [{
                "kind": "weekly_scoped",
                "scope": {"model": {"display_name": "Fable"}},
                "percent": 58,
                "resets_at": "2026-08-29T06:00:00Z"
            }]
        });
        let mapped = crate::providers::claude::map_sub2api_usage(&body).unwrap();

        let quotas = normalize_refreshed_quotas(
            super::Sub2ApiUpstream::Claude,
            mapped.quotas,
            Utc.with_ymd_and_hms(2026, 9, 1, 18, 0, 0).unwrap(),
        );

        assert_eq!(
            quotas
                .iter()
                .map(|quota| (quota.id.as_str(), quota.used_percent))
                .collect::<Vec<_>>(),
            [("fable", 0.0)]
        );
        assert!(quotas.iter().all(|quota| quota.resets_at.is_none()));
    }

    #[test]
    fn codex_refresh_omits_missing_quotas() {
        let response = UsageResponse {
            status: StatusCode::OK,
            headers: Default::default(),
            body: serde_json::json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 38,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787810131
                    }
                }
            }),
        };
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 0, 0, 0).unwrap();
        let mapped = map_usage(&response, None, now).unwrap();

        let quotas = normalize_refreshed_quotas(super::Sub2ApiUpstream::Codex, mapped.quotas, now);

        assert_eq!(
            quotas
                .iter()
                .map(|quota| (quota.id.as_str(), quota.used_percent))
                .collect::<Vec<_>>(),
            [("weekly", 38.0)]
        );
    }

    #[test]
    fn metric_template_runs_once_per_version_or_upstream_change() {
        let mut config = StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://sub2api.example.com".into(),
            codex_provider: String::new(),
            custom_base_url: true,
            email: "admin@example.com".into(),
            password: "secret".into(),
            upstream: super::Sub2ApiUpstream::Codex,
            metric_template_version: 0,
        };

        assert!(should_apply_metric_template(
            Some(&config),
            super::Sub2ApiMode::Admin,
            super::Sub2ApiUpstream::Codex
        ));
        config.metric_template_version = super::METRIC_TEMPLATE_VERSION;
        assert!(!should_apply_metric_template(
            Some(&config),
            super::Sub2ApiMode::Admin,
            super::Sub2ApiUpstream::Codex
        ));
        assert!(should_apply_metric_template(
            Some(&config),
            super::Sub2ApiMode::Admin,
            super::Sub2ApiUpstream::Claude
        ));
        assert!(should_apply_metric_template(
            None,
            super::Sub2ApiMode::Admin,
            super::Sub2ApiUpstream::Claude
        ));
        assert!(should_apply_metric_template(
            Some(&config),
            super::Sub2ApiMode::Subscription,
            config.upstream
        ));
        config.mode = super::Sub2ApiMode::Subscription;
        assert!(!should_apply_metric_template(
            Some(&config),
            config.mode,
            super::Sub2ApiUpstream::Claude
        ));
    }

    #[test]
    fn cached_sessions_are_scoped_to_the_saved_password() {
        let mut config = StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://sub2api.example.com".into(),
            codex_provider: String::new(),
            custom_base_url: true,
            email: "admin@example.com".into(),
            password: "first-password".into(),
            upstream: super::Sub2ApiUpstream::Codex,
            metric_template_version: super::METRIC_TEMPLATE_VERSION,
        };
        let first = session_scope(&config);

        config.password = "second-password".into();

        assert_ne!(session_scope(&config), first);
    }

    #[test]
    fn provider_slots_have_independent_ids_metrics_and_credentials() {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        let providers = Sub2ApiProviders::new(directory.path().join("markers"), storage);
        let first = providers.provider("sub2api").unwrap();
        let second = providers.provider("sub2api@2").unwrap();

        assert_eq!(first.definition().metrics[0].id, "sub2api.session");
        assert_eq!(
            second.definition().display_name,
            default_display_name_for_slot("sub2api@2")
        );
        assert_eq!(second.definition().metrics[0].id, "sub2api@2.session");
        assert_eq!(first.credential_account(), "connection");
        assert_eq!(second.credential_account(), "sub2api@2");
    }

    #[test]
    fn unconfigured_slot_names_are_generated_from_the_slot_id() {
        assert_eq!(default_display_name_for_slot("sub2api"), "Sub2API");
        assert_eq!(
            default_display_name_for_slot("sub2api@2"),
            format!("Sub2API {}", 2)
        );
    }

    #[test]
    fn only_configured_slots_expose_account_names() {
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api@2".into(),
            default_display_name_for_slot("sub2api@2"),
            directory.path().join("sub2api@2.configured"),
            false,
        );

        assert!(!provider.has_local_credentials());
        assert!(provider.account_identity().is_none());

        provider
            .set_configuration_marker(Some(super::ADMIN_CACHE_KEY))
            .unwrap();
        assert!(provider.has_local_credentials());
        assert!(provider.supports_account_names());
        assert_eq!(provider.account_identity(), Some("sub2api@2"));

        provider.set_configuration_marker(None).unwrap();
        assert!(provider.account_identity().is_none());
    }

    #[test]
    fn existing_snapshots_migrate_once_without_resurrecting_deleted_markers() {
        let directory = tempdir().unwrap();
        let storage = Arc::new(Storage::open(&directory.path().join("openquota.db")).unwrap());
        storage
            .save_snapshot(&ProviderSnapshot {
                upstream_provider_id: None,
                accounts: None,
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
    fn old_admin_billing_markers_never_restore_mixed_price_snapshots() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        storage
            .save_snapshot(&ProviderSnapshot {
                upstream_provider_id: None,
                accounts: None,
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
            default_display_name_for_slot("sub2api@2"),
            directory.path().join("sub2api@2.configured"),
            false,
        );

        assert!(storage
            .load_snapshot_for_identity("sub2api@2", provider.cache_identity())
            .unwrap()
            .is_none());
        let old_snapshot = storage
            .load_snapshot_for_identity("sub2api@2", crate::providers::CacheIdentity::Unscoped)
            .unwrap()
            .unwrap();
        for key in [
            "sub2api-upstream-price-v1",
            "sub2api-upstream-price-v2",
            "sub2api-server-price-v1",
        ] {
            storage
                .save_snapshot_for_identity(&old_snapshot, Some(key))
                .unwrap();
            provider.set_configuration_marker(Some(key)).unwrap();
            assert_eq!(
                provider.cache_identity(),
                crate::providers::CacheIdentity::Resolved(super::ADMIN_CACHE_KEY)
            );
            assert!(storage
                .load_snapshot_for_identity("sub2api@2", provider.cache_identity())
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn switching_modes_preserves_both_profiles_in_one_credential_record() {
        use super::{StoredConnections, Sub2ApiMode};
        let legacy = r#"{"base_url":"https://admin.example","email":"admin@example.com","password":"test-password","upstream":"claude","billing_mode":"server"}"#;
        let admin: StoredConnections = serde_json::from_str(legacy).unwrap();
        let subscription: StoredConfig = serde_json::from_str(r#"{"mode":"subscription","base_url":"https://user.example","api_key":"test-user-key","email":"","password":""}"#).unwrap();
        let saved = StoredConnections::with_active(subscription, Some(admin));
        let saved: StoredConnections =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        let admin = saved.for_mode(Sub2ApiMode::Admin).unwrap().clone();
        assert_eq!(admin.password, "test-password");
        assert_eq!(admin.email, "admin@example.com");
        assert_eq!(admin.upstream, super::Sub2ApiUpstream::Claude);
        assert_eq!(admin.cache_key(), super::ADMIN_CACHE_KEY);
        let active = StoredConnections::with_active(admin, Some(saved));
        let saved = StoredConnections::with_active(active.active.clone(), Some(active));
        assert_eq!(
            saved.for_mode(Sub2ApiMode::Subscription).unwrap().api_key,
            "test-user-key"
        );
        let state = saved.state();
        assert_eq!(state.mode, Sub2ApiMode::Admin);
        assert_eq!(
            state.other_mode.as_ref().unwrap().base_url,
            "https://user.example"
        );
        let public = serde_json::to_string(&state).unwrap();
        assert!(!public.contains("test-user-key") && !public.contains("test-password"));
    }

    #[test]
    fn relay_authentication_error_is_distinct_from_rejected_admin_login() {
        let (url, _requests, worker) = serve_sequence_with_status(vec![
            (
                401,
                r#"{"error":{"message":"An API Bearer token is required."}}"#.into(),
            ),
            (401, r#"{"message":"Login failed"}"#.into()),
        ]);
        let client = Sub2ApiClient::new(&url).unwrap();
        assert!(matches!(
            client.login("admin@example.com", "secret"),
            Err(Sub2ApiError::RelayRequiresKey)
        ));
        assert!(matches!(
            client.login("admin@example.com", "secret"),
            Err(Sub2ApiError::LoginRejected)
        ));
        worker.join().unwrap();
    }

    #[test]
    fn subscription_reads_key_usage_and_caches_its_advertised_model_families() {
        let day = chrono::Local::now().date_naive();
        let body = serde_json::json!({
            "planName": "Example plan",
            "subscription": {
                "daily_usage_usd": 2.5, "daily_limit_usd": 10,
                "weekly_usage_usd": 0, "weekly_limit_usd": 50,
                "monthly_usage_usd": 11, "monthly_limit_usd": null,
                "expires_at": "2028-01-01T00:00:00Z"
            },
            "daily_usage": [{"date": day, "total_tokens": 120, "actual_cost": 0, "cost": 4}]
        });
        let (base, requests, worker) = serve_sequence(vec![
            body.to_string(),
            r#"{"data":[{"id":"gpt-test"},{"id":"o3-mini"}]}"#.into(),
            body.to_string(),
        ]);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api".into(),
            "Sub2API".into(),
            directory.path().join("marker"),
            false,
        );
        let config: StoredConfig = serde_json::from_value(serde_json::json!({
            "base_url": format!("{base}/relay/v1"), "mode": "subscription", "api_key": "user-test-key", "email": "", "password": ""
        })).unwrap();
        let snapshot = provider.refresh_snapshot(&config).unwrap();
        let again = provider.refresh_snapshot(&config).unwrap();
        assert_eq!(again.upstream_provider_id, snapshot.upstream_provider_id);
        worker.join().unwrap();
        let request = requests.recv().unwrap();
        assert!(request.starts_with("GET /relay/v1/usage?"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer user-test-key"));
        assert!(requests
            .recv()
            .unwrap()
            .starts_with("GET /relay/v1/models "));
        assert!(requests.recv().unwrap().starts_with("GET /relay/v1/usage?"));
        assert_eq!(snapshot.upstream_provider_id.as_deref(), Some("codex"));
        assert_eq!(snapshot.plan.as_deref(), Some("Example plan"));
        assert!(snapshot.accounts.is_none());
        assert_eq!(snapshot.quotas.len(), 3);
        assert!(snapshot.value_metrics.is_empty());
        assert_eq!(
            snapshot.quotas[2].format,
            crate::models::QuotaFormat::UnlimitedDollars
        );
        assert_eq!(snapshot.quotas[2].used_value, Some(11.0));
        assert_eq!(snapshot.quotas[2].limit_value, None);
        assert_eq!(
            snapshot.quotas[0].format,
            crate::models::QuotaFormat::Percent
        );
        assert_eq!(snapshot.quotas[0].used_percent, 25.0);
        assert_eq!(snapshot.quotas[1].used_value, Some(0.0));
        assert!(snapshot.status_metrics.is_empty());
        assert_eq!(snapshot.quotas[2].label, "Monthly");
        let mut cached = snapshot.clone();
        cached.quotas[2].label = "Monthly quota".into();
        cached.quotas[0].format = crate::models::QuotaFormat::Dollars;
        super::restore_subscription_windows(&mut cached);
        assert_eq!(cached.quotas, snapshot.quotas);
        let today = snapshot.usage_histories.account.unwrap().today.unwrap();
        assert_eq!(today.tokens, 120);
        assert_eq!(today.estimated_cost_usd, Some(0.0));
        assert!(!serde_json::to_string(&config.state())
            .unwrap()
            .contains("user-test-key"));
        assert!(!provider.cache_path().exists());
    }

    #[test]
    fn antigravity_and_grok_subscriptions_are_marked_by_their_chosen_upstream() {
        let body = serde_json::json!({
            "planName": "antigravity",
            "subscription": {"weekly_usage_usd": 0, "weekly_limit_usd": 0},
            "daily_usage": []
        });
        for upstream in ["antigravity", "grok"] {
            // Only the usage request is served: the mixed catalog is never requested.
            let (base, requests, worker) = serve_sequence(vec![body.to_string()]);
            let directory = tempdir().unwrap();
            let provider = Sub2ApiProvider::new(
                "sub2api".into(),
                "Sub2API".into(),
                directory.path().join("marker"),
                false,
            );
            let config: StoredConfig = serde_json::from_value(serde_json::json!({
                "base_url": base, "mode": "subscription", "api_key": "user-test-key",
                "email": "", "password": "", "upstream": upstream, "custom_base_url": true
            }))
            .unwrap();
            let snapshot = provider.refresh_snapshot(&config).unwrap();
            worker.join().unwrap();

            assert_eq!(snapshot.upstream_provider_id.as_deref(), Some(upstream));
            assert!(requests.recv().unwrap().starts_with("GET /v1/usage?"));
            assert!(requests.try_recv().is_err());
            let state = config.state();
            assert_eq!(state.upstream.provider_id(), upstream);
            assert!(state.custom_base_url);
        }
    }

    #[test]
    fn empty_upstream_list_returns_no_account_cards() {
        let responses = vec![
            r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600}}"#.into(),
            r#"{"code":0,"message":"success","data":{"items":[],"total":0,"page":1,"page_size":1,"pages":0}}"#.into(),
        ];
        let (base_url, _requests, worker) = serve_sequence(responses);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api@2".into(),
            default_display_name_for_slot("sub2api@2"),
            directory.path().join("sub2api@2.configured"),
            false,
        );
        let config = StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url,
            codex_provider: String::new(),
            custom_base_url: true,
            email: "admin@example.com".into(),
            password: "secret-password".into(),
            upstream: super::Sub2ApiUpstream::Codex,
            metric_template_version: super::METRIC_TEMPLATE_VERSION,
        };

        assert!(provider
            .refresh_snapshot(&config)
            .unwrap()
            .accounts
            .unwrap()
            .is_empty());
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
    fn codex_provider_urls_are_converted_from_openai_to_instance_roots() {
        assert_eq!(
            normalized_codex_provider_base_url_text("https://quota.example.test/v1/").unwrap(),
            "https://quota.example.test"
        );
        assert_eq!(
            normalized_codex_provider_base_url_text("https://quota.example.test/sub2/v1").unwrap(),
            "https://quota.example.test/sub2"
        );
    }

    #[test]
    fn codex_connection_targets_cannot_override_resolved_provider_urls() {
        let input = Sub2ApiConfigInput {
            provider_source: None,
            use_provider_key: false,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://ignored.example.com".into(),
            codex_provider: " exact-provider ".into(),
            custom_base_url: false,
            email: "admin@example.com".into(),
            password: "secret".into(),
            upstream: super::Sub2ApiUpstream::Codex,
        };

        let target = connection_target(
            &input,
            |provider| {
                assert_eq!(provider, "exact-provider");
                Ok("https://resolved.example.com".into())
            },
            |_| unreachable!(),
        )
        .unwrap();

        assert_eq!(
            target,
            (
                "https://resolved.example.com".into(),
                "exact-provider".into(),
                false
            )
        );
    }

    #[test]
    fn changing_the_tracked_platform_preserves_the_provider_used_for_the_address() {
        let input: Sub2ApiConfigInput = serde_json::from_value(serde_json::json!({
            "mode":"admin","upstream":"claude","providerSource":"codex","codexProvider":"work",
            "baseUrl":"https://relay.example","email":"admin@example.com","password":""
        }))
        .unwrap();
        assert_eq!(
            connection_target(
                &input,
                |name| {
                    assert_eq!(name, "work");
                    Ok("https://relay.example".to_owned())
                },
                |_| unreachable!()
            )
            .unwrap()
            .0,
            "https://relay.example"
        );
    }

    #[test]
    fn custom_codex_urls_reject_provider_names() {
        let input = Sub2ApiConfigInput {
            provider_source: None,
            use_provider_key: false,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://custom.example.com".into(),
            codex_provider: "provider-must-be-empty".into(),
            custom_base_url: true,
            email: "admin@example.com".into(),
            password: "secret".into(),
            upstream: super::Sub2ApiUpstream::Codex,
        };

        assert_eq!(
            connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
            Sub2ApiError::ProviderWithCustomBaseUrl
        );
    }

    #[test]
    fn antigravity_and_grok_connections_always_use_an_entered_base_url() {
        for upstream in [
            super::Sub2ApiUpstream::Antigravity,
            super::Sub2ApiUpstream::Grok,
        ] {
            let mut input = Sub2ApiConfigInput {
                provider_source: None,
                use_provider_key: false,
                mode: super::Sub2ApiMode::Admin,
                api_key: String::new(),
                base_url: "https://relay.example.com/api/v1/".into(),
                codex_provider: String::new(),
                custom_base_url: false,
                email: "admin@example.com".into(),
                password: "secret".into(),
                upstream,
            };
            assert_eq!(
                connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap(),
                ("https://relay.example.com".into(), String::new(), true)
            );
            input.codex_provider = "work".into();
            assert_eq!(
                connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
                Sub2ApiError::ProviderWithCustomBaseUrl
            );

            let config = admin_config("https://relay.example.com".into(), upstream);
            let stored: StoredConfig = serde_json::from_value(serde_json::json!({
                "base_url": config.base_url,
                "email": config.email,
                "password": config.password,
                "upstream": upstream.provider_id(),
                "custom_base_url": false
            }))
            .unwrap();
            assert_eq!(stored.upstream, upstream);
            assert!(stored.state().custom_base_url);

            // The instance address can still come from a Codex or Claude provider.
            input.provider_source = Some(super::Sub2ApiUpstream::Codex);
            assert_eq!(
                connection_target(
                    &input,
                    |name| Ok(format!("https://{name}.example.com")),
                    |_| unreachable!()
                )
                .unwrap(),
                ("https://work.example.com".into(), "work".into(), false)
            );
            let mut stored = admin_config("https://work.example.com".into(), upstream);
            stored.custom_base_url = false;
            stored.codex_provider = "work".into();
            stored.provider_source = Some(super::Sub2ApiUpstream::Codex);
            assert!(!stored.state().custom_base_url);
        }
    }

    #[test]
    fn legacy_claude_connections_expose_custom_mode_without_changing_named_providers() {
        let mut config: StoredConfig = serde_json::from_str(
            r#"{"base_url":"https://saved.example.com","email":"admin@example.com","password":"secret","upstream":"claude","custom_base_url":false}"#,
        ).unwrap();
        let state = config.state();
        assert!(state.custom_base_url);
        assert_eq!(state.base_url, "https://saved.example.com");
        config.codex_provider = "work".into();
        assert!(!config.state().custom_base_url);
        config.codex_provider.clear();
        config.upstream = super::Sub2ApiUpstream::Codex;
        assert!(!config.state().custom_base_url);
    }

    #[test]
    fn named_subscription_prefers_entered_then_saved_key_and_defaults_to_provider() {
        let previous: StoredConfig = serde_json::from_value(serde_json::json!({
            "mode":"subscription", "api_key":"saved", "codex_provider":"relay",
            "base_url":"https://relay.example", "custom_base_url":false, "email":"", "password":""
        }))
        .unwrap();
        let mut input: Sub2ApiConfigInput = serde_json::from_value(serde_json::json!({
            "mode":"subscription", "codexProvider":"relay", "baseUrl":"https://relay.example", "email":"", "password":""
        })).unwrap();
        assert_eq!(
            credential_for_save(&input, &input.base_url, None).unwrap(),
            ""
        );
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            "saved"
        );
        input.upstream = super::Sub2ApiUpstream::Grok;
        input.provider_source = Some(super::Sub2ApiUpstream::Codex);
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            "saved"
        );
        input.provider_source = Some(super::Sub2ApiUpstream::Claude);
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            ""
        );
        input.provider_source = Some(super::Sub2ApiUpstream::Codex);
        input.use_provider_key = true;
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            ""
        );
        input.api_key = "  override  ".into();
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            "override"
        );
        input.api_key.clear();
        input.use_provider_key = false;
        input.codex_provider = "another-provider".into();
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            ""
        );
    }

    #[test]
    fn an_entered_subscription_key_follows_a_changed_upstream_mark() {
        let previous: StoredConfig = serde_json::from_value(serde_json::json!({
            "mode":"subscription", "api_key":"saved", "upstream":"codex",
            "base_url":"https://relay.example", "custom_base_url":true, "email":"", "password":""
        }))
        .unwrap();
        let mut input: Sub2ApiConfigInput = serde_json::from_value(serde_json::json!({
            "mode":"subscription", "upstream":"antigravity", "baseUrl":"https://relay.example",
            "customBaseUrl":true, "codexProvider":"", "email":"", "password":""
        }))
        .unwrap();
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            "saved"
        );
        // A key read with a named provider stays scoped to that provider's upstream.
        input.custom_base_url = false;
        input.upstream = super::Sub2ApiUpstream::Claude;
        input.codex_provider = "relay".into();
        assert_eq!(
            credential_for_save(&input, &input.base_url, Some(&previous)).unwrap(),
            ""
        );
    }

    #[test]
    fn admin_password_follows_the_instance_while_subscription_keys_stay_scoped() {
        let previous = StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://claude.example.com".into(),
            codex_provider: String::new(),
            custom_base_url: true,
            email: "old@example.com".into(),
            password: "old-password".into(),
            upstream: super::Sub2ApiUpstream::Claude,
            metric_template_version: super::METRIC_TEMPLATE_VERSION,
        };
        let mut same_upstream = Sub2ApiConfigInput {
            provider_source: None,
            use_provider_key: false,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://claude.example.com".into(),
            codex_provider: String::new(),
            custom_base_url: true,
            email: "new@example.com".into(),
            password: String::new(),
            upstream: super::Sub2ApiUpstream::Claude,
        };
        assert_eq!(
            credential_for_save(&same_upstream, &same_upstream.base_url, Some(&previous)).unwrap(),
            "old-password"
        );

        assert_eq!(
            credential_for_save(&same_upstream, "https://other.example", Some(&previous)),
            Err(Sub2ApiError::InvalidConfig)
        );
        same_upstream.upstream = super::Sub2ApiUpstream::Codex;
        assert_eq!(
            credential_for_save(&same_upstream, &previous.base_url, Some(&previous)).unwrap(),
            "old-password"
        );
        same_upstream.mode = super::Sub2ApiMode::Subscription;
        assert_eq!(
            credential_for_save(&same_upstream, &previous.base_url, Some(&previous)),
            Err(Sub2ApiError::MissingApiKey)
        );

        let changed_upstream = Sub2ApiConfigInput {
            provider_source: None,
            use_provider_key: false,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://codex.example.com".into(),
            codex_provider: String::new(),
            custom_base_url: true,
            email: "new@example.com".into(),
            password: String::new(),
            upstream: super::Sub2ApiUpstream::Codex,
        };
        assert_eq!(
            credential_for_save(
                &changed_upstream,
                &changed_upstream.base_url,
                Some(&previous)
            ),
            Err(Sub2ApiError::InvalidConfig)
        );
    }

    #[test]
    fn claude_provider_is_required_and_resolved_again_on_save() {
        let mut input = Sub2ApiConfigInput {
            provider_source: None,
            use_provider_key: false,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url: "https://ignored.example.com".into(),
            codex_provider: " work ".into(),
            custom_base_url: false,
            email: "admin@example.com".into(),
            password: "secret".into(),
            upstream: super::Sub2ApiUpstream::Claude,
        };
        assert_eq!(
            connection_target(
                &input,
                |_| unreachable!(),
                |name| {
                    assert_eq!(name, "work");
                    Ok("https://selected.example.com".into())
                }
            )
            .unwrap(),
            ("https://selected.example.com".into(), "work".into(), false)
        );
        input.codex_provider.clear();
        assert_eq!(
            connection_target(
                &input,
                |_| unreachable!(),
                |_| Err(super::ClaudeConfigError::MissingName.into())
            )
            .unwrap_err(),
            Sub2ApiError::ClaudeConfig(super::ClaudeConfigError::MissingName)
        );
        input.custom_base_url = true;
        input.base_url = "   ".into();
        assert_eq!(
            connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
            Sub2ApiError::InvalidConfig
        );
        input.base_url = "https://manual.example.com/api/v1".into();
        assert_eq!(
            connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap(),
            ("https://manual.example.com".into(), String::new(), true)
        );
        input.codex_provider = "work".into();
        assert_eq!(
            connection_target(&input, |_| unreachable!(), |_| unreachable!()).unwrap_err(),
            Sub2ApiError::ProviderWithCustomBaseUrl
        );
    }

    #[test]
    fn discovery_reads_every_page_and_rejects_non_advancing_pages() {
        for repeated in [false, true] {
            let second = if repeated { 1 } else { 2 };
            let (base_url, requests, worker) = serve_sequence(vec![
                r#"{"code":0,"data":{"items":[{"id":1,"name":"First"}],"total":2}}"#.into(),
                format!(
                    r#"{{"code":0,"data":{{"items":[{{"id":{second},"name":"Second"}}],"total":2}}}}"#
                ),
            ]);
            let result = Sub2ApiClient::new(&base_url)
                .unwrap()
                .accounts("token", super::Sub2ApiUpstream::Codex);
            if repeated {
                assert!(matches!(result, Err(Sub2ApiError::InvalidResponse)));
            } else {
                assert_eq!(
                    result.unwrap().iter().map(|a| a.id).collect::<Vec<_>>(),
                    [1, 2]
                );
            }
            worker.join().unwrap();
            assert!(requests.recv().unwrap().contains("page=1&page_size=100"));
            assert!(requests.recv().unwrap().contains("page=2&page_size=100"));
        }
    }

    #[test]
    fn refresh_keeps_all_accounts_and_rediscovers_with_a_cached_token() {
        let now = Utc::now();
        let quota = format!(r#"{{"code":0,"data":{CLAUDE_USAGE_DATA}}}"#);
        let (base_url, _requests, worker) = serve_sequence(vec![
            r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
            r#"{"code":0,"data":{"items":[{"id":7,"name":"Same name"},{"id":8,"name":"Same name"}],"total":2}}"#.into(),
            // All quotas come first so they can be shown before usage history loads.
            quota.clone(), quota, usage_data(7, now), usage_data(8, now),
            r#"{"code":0,"data":{"items":[{"id":9,"name":"Unavailable"}],"total":1}}"#.into(),
            r#"{"code":1}"#.into(),
            r#"{"code":0,"data":{"items":[],"total":0}}"#.into(),
        ]);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api".into(),
            "Sub2API".into(),
            directory.path().join("configured"),
            true,
        );
        let config = StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url,
            codex_provider: String::new(),
            custom_base_url: true,
            email: "admin@example.com".into(),
            password: "password".into(),
            upstream: super::Sub2ApiUpstream::Claude,
            metric_template_version: super::METRIC_TEMPLATE_VERSION,
        };
        let snapshot = provider.refresh_snapshot(&config).unwrap();
        let accounts = snapshot.accounts.unwrap();
        assert_eq!(
            accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["7", "8"]
        );
        assert!(accounts
            .iter()
            .all(|a| a.name == "Sub2API · Claude · Same name" && !a.snapshot.quotas.is_empty()));
        let history = snapshot.usage_histories.account.unwrap();
        assert_eq!(history.daily.len(), 2);
        assert_eq!(history.last_30_days.as_ref().unwrap().tokens, 7000);
        assert_eq!(
            history
                .last_30_days
                .unwrap()
                .model_breakdown
                .unwrap()
                .models
                .len(),
            2
        );
        assert_eq!(
            provider.refresh_snapshot(&config).unwrap_err(),
            Sub2ApiError::InvalidResponse
        );
        assert!(provider
            .refresh_snapshot(&config)
            .unwrap()
            .accounts
            .unwrap()
            .is_empty());
        worker.join().unwrap();
    }

    #[test]
    fn quotas_of_every_account_are_previewed_before_any_usage_history_request() {
        let now = Utc::now();
        let quota = format!(r#"{{"code":0,"data":{CLAUDE_USAGE_DATA}}}"#);
        let (base_url, requests, worker) = serve_sequence(vec![
            r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
            r#"{"code":0,"data":{"items":[{"id":7,"name":"First"},{"id":8,"name":"Second"}],"total":2}}"#.into(),
            quota.clone(), quota, usage_data(7, now), usage_data(8, now),
        ]);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api".into(),
            "Sub2API".into(),
            directory.path().join("configured"),
            true,
        );
        let config = StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url,
            codex_provider: String::new(),
            custom_base_url: true,
            email: "admin@example.com".into(),
            password: "password".into(),
            upstream: super::Sub2ApiUpstream::Claude,
            metric_template_version: super::METRIC_TEMPLATE_VERSION,
        };
        let previews = std::sync::Mutex::new(Vec::new());
        let snapshot = provider
            .refresh_snapshot_with_preview(&config, &|preview| {
                let sent: Vec<String> = requests.try_iter().collect();
                assert!(sent.iter().all(|request| !request.contains("/admin/usage")));
                previews.lock().unwrap().push(preview);
            })
            .unwrap();
        worker.join().unwrap();

        let previews = previews.into_inner().unwrap();
        assert_eq!(previews.len(), 1);
        let accounts = previews[0].accounts.as_ref().unwrap();
        assert_eq!(
            accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["7", "8"]
        );
        assert!(accounts.iter().all(|a| !a.snapshot.quotas.is_empty()
            && a.snapshot.usage_histories.account == Some(super::UsageHistory::default())));
        assert!(accounts.iter().all(|a| a.snapshot.notices.is_empty()));
        assert!(snapshot.notices.is_empty());
        assert_eq!(
            snapshot
                .usage_histories
                .account
                .unwrap()
                .last_30_days
                .unwrap()
                .tokens,
            7000
        );
    }

    fn admin_config(base_url: String, upstream: super::Sub2ApiUpstream) -> StoredConfig {
        StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url,
            codex_provider: String::new(),
            custom_base_url: true,
            email: "admin@example.com".into(),
            password: "password".into(),
            upstream,
            metric_template_version: super::METRIC_TEMPLATE_VERSION,
        }
    }

    #[test]
    fn antigravity_accounts_show_both_pools_and_keep_unusable_accounts() {
        let now = Utc::now();
        let five_hours = (now + chrono::Duration::hours(3)).to_rfc3339();
        let week = (now + chrono::Duration::days(3)).to_rfc3339();
        let quota = serde_json::json!({"code":0,"data":{
            "updated_at": now.to_rfc3339(),
            "subscription_tier": "PRO",
            "antigravity_quota_groups": [
                {"key":"gemini","windows":[
                    {"window":"5h","utilization":20,"resets_at":five_hours},
                    {"window":"weekly","utilization":5,"resets_at":week}]},
                {"key":"3p","windows":[
                    {"window":"5h","utilization":40,"resets_at":five_hours},
                    {"window":"weekly","utilization":52,"resets_at":week}]}
            ]
        }})
        .to_string();
        let (base_url, requests, worker) = serve_sequence(vec![
            r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
            r#"{"code":0,"data":{"items":[{"id":6,"name":"agy"},{"id":9,"name":"banned"}],"total":2}}"#.into(),
            quota,
            r#"{"code":0,"data":{"is_forbidden":true,"forbidden_reason":"Google disabled the account"}}"#.into(),
            usage_data(6, now), usage_data(9, now),
        ]);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api".into(),
            "Sub2API".into(),
            directory.path().join("configured"),
            true,
        );
        let config = admin_config(base_url, super::Sub2ApiUpstream::Antigravity);
        let snapshot = provider.refresh_snapshot(&config).unwrap();
        worker.join().unwrap();

        let sent: Vec<String> = requests.try_iter().collect();
        assert!(sent[1].contains("platform=antigravity"));
        assert!(sent[2].starts_with("GET /api/v1/admin/accounts/6/usage "));
        assert!(sent[3].starts_with("GET /api/v1/admin/accounts/9/usage "));
        let accounts = snapshot.accounts.unwrap();
        assert_eq!(accounts[0].name, "Sub2API · Antigravity · agy");
        assert_eq!(accounts[0].snapshot.plan.as_deref(), Some("Pro"));
        assert_eq!(
            accounts[0]
                .snapshot
                .quotas
                .iter()
                .map(|quota| (quota.id.as_str(), quota.used_percent))
                .collect::<Vec<_>>(),
            [
                ("session", 20.0),
                ("weekly", 5.0),
                ("claude", 40.0),
                ("claudeWeekly", 52.0)
            ]
        );
        assert!(accounts[1].snapshot.quotas.is_empty());
        assert_eq!(
            accounts[1].snapshot.warnings,
            ["Google disabled the account"]
        );
        assert!(accounts[1].snapshot.usage_histories.account.is_some());
    }

    #[test]
    fn grok_accounts_show_their_billing_period() {
        let now = Utc::now();
        let quota = serde_json::json!({"code":0,"data":{
            "seven_day": {"utilization": 88, "resets_at": (now + chrono::Duration::days(4)).to_rfc3339()},
            "grok_billing": {"period_type":"weekly","usage_percent":88}
        }})
        .to_string();
        let (base_url, requests, worker) = serve_sequence(vec![
            r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
            r#"{"code":0,"data":{"items":[{"id":7,"name":"grok"}],"total":1}}"#.into(),
            quota,
            usage_data(7, now),
        ]);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api".into(),
            "Sub2API".into(),
            directory.path().join("configured"),
            true,
        );
        let config = admin_config(base_url, super::Sub2ApiUpstream::Grok);
        let snapshot = provider.refresh_snapshot(&config).unwrap();
        worker.join().unwrap();

        let sent: Vec<String> = requests.try_iter().collect();
        assert!(sent[1].contains("platform=grok"));
        assert!(sent[2].starts_with("GET /api/v1/admin/accounts/7/usage "));
        let accounts = snapshot.accounts.unwrap();
        assert_eq!(accounts[0].name, "Sub2API · Grok · grok");
        let weekly = &accounts[0].snapshot.quotas;
        assert_eq!(weekly.len(), 1);
        assert_eq!(
            (weekly[0].id.as_str(), weekly[0].used_percent),
            ("weekly", 88.0)
        );
        assert!(weekly[0].resets_at.is_some());
    }

    #[tokio::test]
    async fn failed_refresh_preserves_all_cached_accounts_on_disk_and_recovers() {
        use crate::{
            models::{ProviderDefinition, SnapshotSource},
            providers::{ProviderError, ProviderRegistry},
            service::ProviderService,
        };

        struct ConfiguredProvider {
            provider: Sub2ApiProvider,
            config: StoredConfig,
        }
        impl UsageProvider for ConfiguredProvider {
            fn definition(&self) -> ProviderDefinition {
                let mut definition = self.provider.definition();
                definition.fallback_enabled = true;
                definition
            }
            fn has_local_credentials(&self) -> bool {
                true
            }
            fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
                self.provider
                    .refresh_snapshot(&self.config)
                    .map_err(ProviderError::from)
            }
        }

        // Discovery, a later account's quota, and history must all preserve the same cache.
        for failed_stage in 0..3 {
            let page = r#"{"code":0,"data":{"items":[{"id":7,"name":"First"},{"id":8,"name":"Second"}],"total":2}}"#.to_owned();
            let quota = format!(r#"{{"code":0,"data":{CLAUDE_USAGE_DATA}}}"#);
            let now = Utc::now();
            let mut responses = vec![
                (
                    200,
                    r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
                ),
                (200, page.clone()),
                (200, quota.clone()),
                (200, quota.clone()),
                (200, usage_data(7, now)),
            ];

            responses.push((200, usage_data(8, now)));

            if failed_stage > 0 {
                responses.extend([(200, page), (200, quota.clone())]);
                if failed_stage == 2 {
                    responses.extend([(200, quota.clone()), (200, usage_data(7, now))]);
                }
            }
            responses.push((503, "temporary outage".into()));
            responses.extend([
                (
                    200,
                    r#"{"code":0,"data":{"items":[{"id":8,"name":"Recovered"}],"total":1}}"#.into(),
                ),
                (200, quota),
                (200, usage_data(8, now)),
                (200, r#"{"code":0,"data":{"items":[],"total":0}}"#.into()),
            ]);
            let (base_url, _requests, worker) = serve_sequence_with_status(responses);
            let directory = tempdir().unwrap();
            let storage = Arc::new(Storage::open(&directory.path().join("cache.db")).unwrap());
            let registry = Arc::new(
                ProviderRegistry::new(vec![Arc::new(ConfiguredProvider {
                    provider: Sub2ApiProvider::new(
                        "sub2api".into(),
                        "Sub2API · Claude".into(),
                        directory.path().join("configured"),
                        true,
                    ),
                    config: StoredConfig {
                        provider_source: None,
                        mode: super::Sub2ApiMode::Admin,
                        api_key: String::new(),
                        base_url,
                        codex_provider: String::new(),
                        custom_base_url: true,
                        email: "admin@example.com".into(),
                        password: "password".into(),
                        upstream: super::Sub2ApiUpstream::Claude,
                        metric_template_version: super::METRIC_TEMPLATE_VERSION,
                    },
                })])
                .unwrap(),
            );
            let service = Arc::new(ProviderService::new(registry.clone(), storage.clone()));
            let successful = service.refresh("sub2api", true).await;
            assert!(successful.error.is_none());
            let cached = successful.snapshot.unwrap();
            assert_eq!(cached.accounts.as_ref().unwrap().len(), 2);
            assert_eq!(
                cached
                    .usage_histories
                    .account
                    .as_ref()
                    .unwrap()
                    .last_30_days
                    .as_ref()
                    .unwrap()
                    .tokens,
                7000
            );

            let failed = service.refresh("sub2api", true).await;
            assert_eq!(
                failed.error_kind,
                Some(crate::models::ProviderErrorKind::Network)
            );
            assert_eq!(failed.snapshot.as_ref(), Some(&cached));
            drop(service);
            let restarted = Arc::new(ProviderService::new(registry, storage));
            let restored = restarted.state().providers.remove("sub2api").unwrap();
            assert_eq!(restored.source, SnapshotSource::Cache);
            assert_eq!(restored.snapshot.as_ref(), Some(&cached));

            let recovered = restarted.refresh("sub2api", true).await;
            assert!(recovered.error.is_none());
            assert_eq!(recovered.snapshot.unwrap().accounts.unwrap().len(), 1);
            let empty = restarted.refresh("sub2api", true).await;
            assert!(empty.error.is_none());
            assert!(empty.snapshot.unwrap().accounts.unwrap().is_empty());
            worker.join().unwrap();
        }
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
        let accounts = client
            .accounts(login.value.as_str(), super::Sub2ApiUpstream::Codex)
            .unwrap();
        let body = client
            .usage(
                login.value.as_str(),
                accounts[0].id,
                super::Sub2ApiUpstream::Codex,
            )
            .unwrap();
        worker.join().unwrap();

        assert_eq!(accounts[0].name, "Codex upstream");
        assert_eq!(accounts.len(), 1);
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
        let accounts = client
            .accounts(login.value.as_str(), super::Sub2ApiUpstream::Claude)
            .unwrap();
        let body = client
            .usage(
                login.value.as_str(),
                accounts[0].id,
                super::Sub2ApiUpstream::Claude,
            )
            .unwrap();
        worker.join().unwrap();

        assert_eq!(accounts[0].name, "Claude upstream");
        assert_eq!(accounts.len(), 1);
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
    fn stats_endpoint_reads_only_the_summary_and_uses_standard_cost() {
        let now = Utc::now();
        let (base_url, requests, worker) = serve_sequence(vec![usage_data(42, now)]);
        let client = Sub2ApiClient::new(&base_url).unwrap();
        let history = client
            .stats("admin-jwt", 42)
            .unwrap()
            .into_standard_history(now);
        worker.join().unwrap();
        let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(request.starts_with("GET /api/v1/admin/accounts/42/stats?days=30 HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer admin-jwt"));
        assert_eq!(history.today.unwrap().estimated_cost_usd, Some(2.0));
        let monthly = history.last_30_days.unwrap();
        assert_eq!(monthly.tokens, 3500);
        assert_eq!(monthly.estimated_cost_usd, Some(3.0));
        assert!(!monthly.cost_estimated);
        assert_eq!(
            monthly.model_breakdown.unwrap().models[0].model,
            "claude-sonnet-5"
        );
    }

    #[test]
    fn repeated_refreshes_use_summaries_without_opening_the_old_detail_cache() {
        let now = Utc::now();
        let accounts =
            r#"{"code":0,"data":{"items":[{"id":10,"name":"upstream"}],"total":1}}"#.to_owned();
        let quota = format!(r#"{{"code":0,"data":{QUOTA_DATA}}}"#);
        let (base_url, requests, worker) = serve_sequence(vec![
            r#"{"code":0,"data":{"access_token":"token","expires_in":3600}}"#.into(),
            accounts.clone(),
            quota.clone(),
            usage_data(10, now),
            accounts,
            quota,
            usage_data(10, now),
        ]);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api".into(),
            "Codex".into(),
            directory.path().join("configured"),
            true,
        );
        let legacy = b"legacy detail cache must not be opened or changed";
        fs::write(provider.cache_path(), legacy).unwrap();
        let config = admin_config(base_url, super::Sub2ApiUpstream::Codex);
        for _ in 0..2 {
            let snapshot = provider.refresh_snapshot(&config).unwrap();
            assert_eq!(
                snapshot
                    .usage_histories
                    .account
                    .unwrap()
                    .last_30_days
                    .unwrap()
                    .estimated_cost_usd,
                Some(3.0)
            );
        }
        worker.join().unwrap();
        assert_eq!(fs::read(provider.cache_path()).unwrap(), legacy);
        let requests: Vec<_> = requests.try_iter().collect();
        assert_eq!(requests.len(), 7);
        assert_eq!(
            requests
                .iter()
                .filter(
                    |request| request.starts_with("GET /api/v1/admin/accounts/10/stats?days=30 ")
                )
                .count(),
            2
        );
        assert!(requests
            .iter()
            .all(|request| !request.contains("/admin/usage")));
    }

    #[test]
    fn stats_negotiate_gzip_and_accept_uncompressed_servers() {
        let body = usage_data(10, Utc::now());
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            for (encoding, bytes) in [
                ("Content-Encoding: gzip\r\n", compressed),
                ("", body.into_bytes()),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                    let mut chunk = [0; 2048];
                    let read = stream.read(&mut chunk).unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                assert!(String::from_utf8(request)
                    .unwrap()
                    .to_ascii_lowercase()
                    .lines()
                    .any(|line| line.starts_with("accept-encoding:") && line.contains("gzip")));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{encoding}Connection: close\r\n\r\n", bytes.len()).unwrap();
                stream.write_all(&bytes).unwrap();
            }
        });
        let client = Sub2ApiClient::new(&base).unwrap();
        let compressed = client.stats("test-token", 10).unwrap();
        let plain = client.stats("test-token", 10).unwrap();
        assert!(compressed == plain);
        worker.join().unwrap();
    }

    #[test]
    fn claude_refresh_combines_quota_with_standard_cost_summary() {
        let now = Utc::now();
        let responses = vec![
            r#"{"code":0,"message":"success","data":{"access_token":"admin-jwt","expires_in":3600}}"#.into(),
            r#"{"code":0,"message":"success","data":{"items":[{"id":7,"name":"Claude upstream"}],"total":1,"page":1,"page_size":1,"pages":1}}"#.into(),
            format!(r#"{{"code":0,"message":"success","data":{CLAUDE_USAGE_DATA}}}"#),
            usage_data(7, now),
        ];
        let (base_url, requests, worker) = serve_sequence(responses);
        let directory = tempdir().unwrap();
        let provider = Sub2ApiProvider::new(
            "sub2api@2".into(),
            default_display_name_for_slot("sub2api@2"),
            directory.path().join("sub2api@2.configured"),
            false,
        );
        let config = StoredConfig {
            provider_source: None,
            mode: super::Sub2ApiMode::Admin,
            api_key: String::new(),
            base_url,
            codex_provider: String::new(),
            custom_base_url: true,
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
        assert!(request_paths[3].starts_with("GET /api/v1/admin/accounts/7/stats?days=30 "));
    }

    #[test]
    fn stored_connections_without_an_upstream_remain_codex_connections() {
        let config: StoredConfig = serde_json::from_str(
            r#"{"base_url":"https://sub2api.example.com","email":"admin@example.com","password":"secret"}"#,
        )
        .unwrap();

        assert_eq!(config.mode, super::Sub2ApiMode::Admin);
        assert_eq!(
            super::Sub2ApiConfigState::default().mode,
            super::Sub2ApiMode::Subscription
        );
        assert_eq!(config.upstream, super::Sub2ApiUpstream::Codex);
        assert_eq!(config.cache_key(), super::ADMIN_CACHE_KEY);
        assert!(config.codex_provider.is_empty());
        assert!(config.custom_base_url);
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
