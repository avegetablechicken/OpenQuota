use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::providers::{
    claude::config::resolve_provider_base_url as resolve_claude_provider,
    codex::config::resolve_provider_base_url, ProviderError,
};

use super::client::{normalize_base_url, Sub2ApiError};
use super::template::{track_for_id, track_provider_id};

pub(super) const PROVIDER_ID: &str = "sub2api";
pub(super) const METRIC_TEMPLATE_VERSION: u8 = 2;
// Keep server-standard summaries separate from previous local price estimates.
pub(super) const ADMIN_CACHE_KEY: &str = "sub2api-standard-summary-v1";
pub(super) const SUBSCRIPTION_CACHE_KEY: &str = "sub2api-subscription-v1";

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

impl Sub2ApiConfigInput {
    /// Whether the save enters its address rather than reading it from a named provider, as
    /// `connection_target` decides.
    fn custom_base_url_in_effect(&self) -> bool {
        self.custom_base_url
            || !self
                .provider_source
                .unwrap_or(self.upstream)
                .resolves_provider_names()
    }
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
    /// Present when the connection keeps a secret a new save can reuse instead of asking for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reusable_credential: Option<ReusableCredential>,
}

/// What a save must match to keep a saved connection's secret: its address, and for a
/// subscription where the key was read. A field left out matches anything.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReusableCredential {
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_base_url: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_source: Option<Sub2ApiUpstream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_provider: Option<String>,
}

impl ReusableCredential {
    /// Whether a save of `input` at `base_url` may keep the secret.
    fn admits(&self, input: &Sub2ApiConfigInput, base_url: &str) -> bool {
        self.base_url == base_url
            && self
                .custom_base_url
                .is_none_or(|custom| custom == input.custom_base_url_in_effect())
            && self
                .provider_source
                .is_none_or(|source| source == input.provider_source.unwrap_or(input.upstream))
            && self
                .codex_provider
                .as_deref()
                .is_none_or(|name| name == input.codex_provider.trim())
    }
}

/// A saved, cleared or removed connection, and a warning when OpenQuota could not finish
/// bringing provider status in line with it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sub2ApiConfigOutcome {
    #[serde(flatten)]
    pub state: Sub2ApiConfigState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
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
    pub(super) fn platform(self) -> &'static str {
        match self {
            Self::Codex => "openai",
            Self::Claude => "anthropic",
            // Sub2API keeps Antigravity's OAuth accounts apart from Gemini CLI's (`gemini`).
            Self::Antigravity => "antigravity",
            Self::Grok => "grok",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::Grok => "Grok",
        }
    }

    pub(super) fn provider_id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::Grok => "grok",
        }
    }

    /// Whether the Base URL can be read from a named local client configuration.
    pub(super) fn resolves_provider_names(self) -> bool {
        matches!(self, Self::Codex | Self::Claude)
    }
}

pub(super) fn default_display_name_for_slot(provider_id: &str) -> String {
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
pub(super) struct StoredConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) provider_source: Option<Sub2ApiUpstream>,
    #[serde(default = "legacy_admin_mode")]
    pub(super) mode: Sub2ApiMode,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) api_key: String,
    pub(super) base_url: String,
    #[serde(default)]
    pub(super) codex_provider: String,
    #[serde(default = "default_true")]
    pub(super) custom_base_url: bool,
    pub(super) email: String,
    pub(super) password: String,
    #[serde(default)]
    pub(super) upstream: Sub2ApiUpstream,
    #[serde(default)]
    pub(super) metric_template_version: u8,
}

/// Keep the active fields at the top level so existing single-mode keychain records
/// deserialize directly. Both modes are replaced together in a single secure write.
#[derive(Serialize, Deserialize)]
pub(super) struct StoredConnections {
    #[serde(flatten)]
    pub(super) active: StoredConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) other: Option<StoredConfig>,
}

impl StoredConnections {
    pub(super) fn for_mode(&self, mode: Sub2ApiMode) -> Option<&StoredConfig> {
        if self.active.mode == mode {
            Some(&self.active)
        } else {
            self.other.as_ref().filter(|config| config.mode == mode)
        }
    }

    pub(super) fn with_active(active: StoredConfig, previous: Option<Self>) -> Self {
        let other = previous.and_then(|old| {
            if old.active.mode == active.mode {
                old.other
            } else {
                Some(old.active)
            }
        });
        Self { active, other }
    }

    pub(super) fn state(&self) -> Sub2ApiConfigState {
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
    pub(super) fn subscription_key(&self) -> Result<Zeroizing<String>, Sub2ApiError> {
        if !self.api_key.is_empty() {
            return Ok(Zeroizing::new(self.api_key.clone()));
        }
        if self.custom_base_url || self.codex_provider.is_empty() {
            return Err(Sub2ApiError::MissingApiKey);
        }
        let (url, key) = match self.provider_source.unwrap_or(self.upstream) {
            Sub2ApiUpstream::Codex => {
                let (url, key) = crate::providers::codex::config::subscription_credentials(
                    &self.codex_provider,
                )?;
                (normalized_codex_provider_base_url_text(&url)?, key)
            }
            Sub2ApiUpstream::Claude => {
                let (url, key) = crate::providers::claude::config::subscription_credentials(
                    &self.codex_provider,
                )?;
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

    pub(super) fn cache_key(&self) -> &'static str {
        match self.mode {
            Sub2ApiMode::Subscription => SUBSCRIPTION_CACHE_KEY,
            Sub2ApiMode::Admin => ADMIN_CACHE_KEY,
        }
    }

    /// Whether the address was entered rather than read from a named provider. Older Claude
    /// connections without a provider name were saved as named ones.
    fn custom_base_url_in_effect(&self) -> bool {
        self.custom_base_url
            || !self
                .provider_source
                .unwrap_or(self.upstream)
                .resolves_provider_names()
            || (self.upstream == Sub2ApiUpstream::Claude && self.codex_provider.is_empty())
    }

    /// What a later save must match to keep this connection's secret, if it has one.
    fn reusable_credential(&self) -> Option<ReusableCredential> {
        let base_url = self.base_url.clone();
        match self.mode {
            Sub2ApiMode::Admin => (!self.password.is_empty()).then_some(ReusableCredential {
                base_url,
                ..ReusableCredential::default()
            }),
            Sub2ApiMode::Subscription if self.api_key.is_empty() => None,
            // An entered address keeps its key whichever upstream the connection is marked as.
            Sub2ApiMode::Subscription if self.custom_base_url_in_effect() => {
                Some(ReusableCredential {
                    base_url,
                    custom_base_url: Some(true),
                    provider_source: None,
                    codex_provider: Some(self.codex_provider.clone()),
                })
            }
            Sub2ApiMode::Subscription => Some(ReusableCredential {
                base_url,
                custom_base_url: Some(false),
                provider_source: Some(self.provider_source.unwrap_or(self.upstream)),
                codex_provider: Some(self.codex_provider.clone()),
            }),
        }
    }

    pub(super) fn state(&self) -> Sub2ApiConfigState {
        Sub2ApiConfigState {
            reusable_credential: self.reusable_credential(),
            provider_source: self.provider_source,
            use_provider_key: self.mode == Sub2ApiMode::Subscription
                && !self.custom_base_url
                && self.api_key.is_empty(),
            configured: true,
            mode: self.mode,
            base_url: self.base_url.clone(),
            codex_provider: self.codex_provider.clone(),
            custom_base_url: self.custom_base_url_in_effect(),
            email: self.email.clone(),
            upstream: self.upstream,

            ..Sub2ApiConfigState::default()
        }
    }
}

/// Reads a configuration marker as its cache key and, when recorded, its track id.
///
/// Markers written before tracks were recorded hold only the cache key (or `configured`).
pub(super) fn parse_configuration_marker(contents: &str) -> (&str, Option<&'static str>) {
    let mut lines = contents.lines();
    let cache_key = lines.next().unwrap_or_default();
    let track = match lines.next() {
        Some(recorded) => track_for_id(recorded),
        // Only subscriptions use this cache key, and their track does not depend on the upstream.
        None if cache_key == SUBSCRIPTION_CACHE_KEY => {
            Some((Sub2ApiMode::Subscription, Sub2ApiUpstream::Codex))
        }
        None => None,
    }
    .map(|(mode, upstream)| track_provider_id(mode, upstream));
    (cache_key, track)
}

fn normalized_base_url_text(value: &str) -> Result<String, Sub2ApiError> {
    let url = normalize_base_url(value)?;
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub(super) fn resolve_codex_provider_base_url(value: &str) -> Result<String, Sub2ApiError> {
    normalized_codex_provider_base_url_text(&resolve_provider_base_url(value)?)
}

pub fn codex_provider_base_url(value: &str) -> Result<String, ProviderError> {
    resolve_codex_provider_base_url(value).map_err(ProviderError::from)
}

pub(super) fn resolve_claude_provider_base_url(name: &str) -> Result<String, Sub2ApiError> {
    normalized_base_url_text(&resolve_claude_provider(name)?)
}

pub fn claude_provider_base_url(name: &str) -> Result<String, ProviderError> {
    resolve_claude_provider_base_url(name).map_err(ProviderError::from)
}

fn default_true() -> bool {
    true
}

pub(super) fn normalized_codex_provider_base_url_text(value: &str) -> Result<String, Sub2ApiError> {
    let mut url = normalize_base_url(value)?;
    let path = url.path().trim_end_matches('/');
    if let Some(path) = path.strip_suffix("/v1") {
        let path = if path.is_empty() { "/" } else { path }.to_owned();
        url.set_path(&path);
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub(super) fn connection_target(
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

pub(super) fn credential_for_save(
    input: &Sub2ApiConfigInput,
    base_url: &str,
    previous: Option<&StoredConfig>,
) -> Result<String, Sub2ApiError> {
    // The scope is the one the connection state shows, so the editor promises only what a save
    // keeps.
    let previous = previous.filter(|config| {
        config.mode == input.mode
            && config
                .reusable_credential()
                .is_some_and(|scope| scope.admits(input, base_url))
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
            if let Some(saved) = previous.filter(|_| !input.use_provider_key) {
                return Ok(saved.api_key.clone());
            }
            if input.custom_base_url {
                return Err(Sub2ApiError::MissingApiKey);
            }
            Ok(String::new())
        }
    }
}

pub(super) fn session_scope(config: &StoredConfig) -> String {
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
