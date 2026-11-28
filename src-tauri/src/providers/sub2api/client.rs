use std::time::{Duration, Instant};

use reqwest::{blocking::Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::providers::{
    claude::config::ClaudeConfigError, codex::config::CodexConfigError, ProviderError,
};

use super::config::Sub2ApiUpstream;
use super::reported::AccountStats;

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum Sub2ApiError {
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
pub(super) struct Envelope<T> {
    code: i64,
    pub(super) data: Option<T>,
}

#[derive(Deserialize)]
struct LoginData {
    access_token: Option<String>,
    expires_in: Option<u64>,
    #[serde(default)]
    requires_2fa: bool,
}

#[derive(Deserialize)]
pub(super) struct AccountPage {
    items: Vec<Sub2ApiAccount>,
    pub(super) total: usize,
}

#[derive(Clone, Deserialize)]
pub(super) struct Sub2ApiAccount {
    pub(super) id: i64,
    pub(super) name: String,
}

pub(super) struct LoginToken {
    pub(super) value: Zeroizing<String>,
    pub(super) expires_in: u64,
}

pub(super) struct CachedSession {
    pub(super) scope: String,
    pub(super) token: Zeroizing<String>,
    pub(super) expires_at: Instant,
}

pub(super) struct Sub2ApiClient {
    pub(super) client: Client,
    pub(super) base_url: Url,
}

impl Sub2ApiClient {
    #[cfg(test)]
    pub(super) fn new(base_url: &str) -> Result<Self, Sub2ApiError> {
        Self::for_provider(base_url, "sub2api")
    }

    pub(super) fn for_provider(base_url: &str, provider_id: &str) -> Result<Self, Sub2ApiError> {
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

    pub(super) fn login(&self, email: &str, password: &str) -> Result<LoginToken, Sub2ApiError> {
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

    pub(super) fn accounts(
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

    pub(super) fn usage(
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

    pub(super) fn stats(&self, token: &str, account_id: i64) -> Result<AccountStats, Sub2ApiError> {
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

    pub(super) fn endpoint(&self, segments: &[&str]) -> Result<Url, Sub2ApiError> {
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

pub(super) fn normalize_base_url(value: &str) -> Result<Url, Sub2ApiError> {
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

pub(super) fn classify_status(status: StatusCode) -> Sub2ApiError {
    match status {
        StatusCode::UNAUTHORIZED => Sub2ApiError::Authentication,
        StatusCode::FORBIDDEN => Sub2ApiError::Permission,
        StatusCode::TOO_MANY_REQUESTS => Sub2ApiError::RateLimited,
        _ => Sub2ApiError::RequestFailed(status.as_u16()),
    }
}

pub(super) fn transport_error(stage: &str, error: &reqwest::Error) -> Sub2ApiError {
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
pub(super) fn decode_response<T: serde::de::DeserializeOwned>(
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

pub(super) fn token_expiry(expires_in: u64) -> Instant {
    Instant::now() + Duration::from_secs(expires_in.saturating_sub(60).max(1))
}

pub(super) fn clone_session(session: &CachedSession) -> CachedSession {
    CachedSession {
        scope: session.scope.clone(),
        token: Zeroizing::new(session.token.to_string()),
        expires_at: session.expires_at,
    }
}
