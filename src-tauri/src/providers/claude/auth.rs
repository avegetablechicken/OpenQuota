use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{
    hashing::sha256_hex,
    providers::credential_store::{read_generic_password, write_generic_password},
};

use serde_json::Value;

use super::{client::ClaudeRefreshResponse, ClaudeError};

const DEFAULT_API_BASE: &str = "https://api.anthropic.com";
const DEFAULT_REFRESH_URL: &str = "https://platform.claude.com/v1/oauth/token";
const DEFAULT_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const NON_PROD_CLIENT_ID: &str = "22422756-60c9-4084-8eb7-27705fd5cf9a";
/// The only `CLAUDE_CODE_CUSTOM_OAUTH_URL` values Claude Code accepts; it refuses to start with
/// any other, so a login it saved was never sent anywhere else.
const APPROVED_CUSTOM_OAUTH_URLS: &[&str] = &[
    "https://beacon.claude-ai.staging.ant.dev",
    "https://claude.fedstart.com",
    "https://claude-staging.fedstart.com",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeOAuth {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<f64>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub scopes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ClaudeCredentialsFile {
    claude_ai_oauth: Option<ClaudeOAuth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CredentialSource {
    File(PathBuf),
    Keychain { service: String, account: String },
    Environment,
}

#[derive(Debug, Clone)]
pub struct ClaudeCredential {
    pub oauth: ClaudeOAuth,
    source: CredentialSource,
    document: ClaudeCredentialsFile,
    pub inference_only: bool,
    /// The refresh token the saved login still holds while `oauth` carries a renewal that could
    /// not be saved; `None` while `oauth` is what the saved login holds.
    unsaved_from: Option<String>,
}

/// A renewal that could not be saved, kept for the rest of the session while the saved login it
/// was made from still holds the refresh token it spent.
#[derive(Debug, Clone)]
pub(super) struct UnsavedRenewal {
    source: CredentialSource,
    saved_refresh_token: String,
    oauth: ClaudeOAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCredentialGeneration {
    entries: Vec<ClaudeCredentialGenerationEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub(super) enum ClaudeCredentialScope {
    Standard,
    /// Lets tests point the provider at an isolated Claude configuration directory.
    #[cfg(test)]
    ConfigDir {
        path: PathBuf,
        keychain_literal: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClaudeCredentialGenerationEntry {
    source: CredentialSource,
    oauth_fingerprint: [u8; 32],
}

impl ClaudeCredentialGeneration {
    pub fn from_candidates(candidates: &[ClaudeCredential]) -> Self {
        Self {
            entries: candidates
                .iter()
                .filter(|candidate| !candidate.inference_only)
                .map(|candidate| ClaudeCredentialGenerationEntry {
                    source: candidate.source.clone(),
                    oauth_fingerprint: oauth_generation_fingerprint(&candidate.oauth),
                })
                .collect(),
        }
    }

    pub fn replacing(&self, credential: &ClaudeCredential) -> Option<Self> {
        let mut updated = self.clone();
        let entry = updated
            .entries
            .iter_mut()
            .find(|entry| entry.source == credential.source)?;
        entry.oauth_fingerprint = oauth_generation_fingerprint(&credential.oauth);
        Some(updated)
    }
}

impl ClaudeCredential {
    pub fn access_token(&self) -> Option<&str> {
        self.oauth
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    pub fn has_profile_scope(&self) -> bool {
        if self.inference_only {
            return false;
        }
        self.oauth.scopes.as_ref().is_none_or(|scopes| {
            scopes.is_empty() || scopes.iter().any(|scope| scope == "user:profile")
        })
    }

    pub fn needs_refresh(&self, now_millis: i64) -> bool {
        self.oauth
            .expires_at
            .is_some_and(|expires| expires - now_millis as f64 <= 5.0 * 60.0 * 1000.0)
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        let access = Sha256::digest(
            self.oauth
                .access_token
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        let refresh = Sha256::digest(
            self.oauth
                .refresh_token
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        let mut pair = Sha256::new();
        pair.update(access);
        pair.update(refresh);
        pair.finalize().into()
    }

    /// The renewal this login carries that its saved login does not hold.
    pub(super) fn unsaved_renewal(&self) -> Option<UnsavedRenewal> {
        if self.inference_only || self.source == CredentialSource::Environment {
            return None;
        }
        Some(UnsavedRenewal {
            source: self.source.clone(),
            saved_refresh_token: self.unsaved_from.clone()?,
            oauth: self.oauth.clone(),
        })
    }

    /// Whether `renewal` was made from the saved login `self` was read from.
    pub(super) fn is_for_source(&self, renewal: &UnsavedRenewal) -> bool {
        self.source == renewal.source
    }

    /// Uses `renewal` in place of this login when it was made from this saved login, which still
    /// holds the refresh token the renewal spent.
    pub(super) fn resume_unsaved(&mut self, renewal: &UnsavedRenewal) -> bool {
        if self.inference_only
            || self.unsaved_from.is_some()
            || self.source != renewal.source
            || self.oauth.refresh_token.as_deref() != Some(&renewal.saved_refresh_token)
        {
            return false;
        }
        self.unsaved_from = Some(renewal.saved_refresh_token.clone());
        self.oauth = renewal.oauth.clone();
        true
    }

    /// Applies a renewal of the refresh token `spent` to this login and to the saved login it was
    /// read from, while that still holds `spent` (or, for a login carrying an unsaved renewal, the
    /// refresh token that renewal spent). Only the members a renewal changes are
    /// written: everything else the saved login keeps (Claude Code's client, its MCP logins,
    /// members unknown here) stays exactly as it is. A saved login holding another refresh token
    /// was renewed or switched by someone else and is never written over.
    pub(super) fn save_renewal(
        &mut self,
        renewal: &ClaudeRefreshResponse,
        spent: &str,
        now_millis: i64,
    ) -> Result<bool, ClaudeError> {
        let expected = self
            .unsaved_from
            .clone()
            .unwrap_or_else(|| spent.to_owned());
        let saved = match &self.source {
            CredentialSource::Environment => {
                self.apply_in_memory(renewal, spent, now_millis);
                return Ok(false);
            }
            source => read_saved_document(source),
        };
        let mut document = match saved {
            Ok(Some(document)) => document,
            Ok(None) => return Err(ClaudeError::CredentialsChanged),
            Err(error) => {
                self.apply_in_memory(renewal, spent, now_millis);
                return Err(error);
            }
        };
        let oauth = document
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .filter(|oauth| {
                oauth.get("refreshToken").and_then(Value::as_str) == Some(expected.as_str())
            })
            .ok_or(ClaudeError::CredentialsChanged)?;
        apply_renewal(oauth, renewal, now_millis);
        let saved: ClaudeCredentialsFile = match serde_json::from_value(document.clone()) {
            Ok(saved) => saved,
            Err(_) => {
                self.apply_in_memory(renewal, spent, now_millis);
                return Err(ClaudeError::AuthWrite);
            }
        };
        let written = serde_json::to_vec(&document)
            .map_err(|_| ClaudeError::AuthWrite)
            .and_then(|bytes| match &self.source {
                CredentialSource::File(path) => {
                    // Replace a symlink's target (a dotfiles checkout, say), not the link.
                    let path = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                    write_private_file_atomic(&path, &bytes)
                }
                CredentialSource::Keychain { service, account } => {
                    write_generic_password(service, account, &bytes)
                        .map_err(|_| ClaudeError::AuthWrite)
                }
                CredentialSource::Environment => Ok(()),
            });
        if let Err(error) = written {
            self.apply_in_memory(renewal, spent, now_millis);
            return Err(error);
        }
        if let Some(oauth) = saved.claude_ai_oauth.clone() {
            self.oauth = oauth;
        }
        self.document = saved;
        self.unsaved_from = None;
        Ok(true)
    }

    /// Keeps a renewal that could not be saved for this session.
    fn apply_in_memory(&mut self, renewal: &ClaudeRefreshResponse, spent: &str, now_millis: i64) {
        // A login already carrying an unsaved renewal keeps the token its saved login holds.
        self.unsaved_from.get_or_insert_with(|| spent.to_owned());
        let mut oauth = serde_json::to_value(&self.oauth)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        apply_renewal(&mut oauth, renewal, now_millis);
        if let Ok(updated) = serde_json::from_value(Value::Object(oauth)) {
            self.oauth = updated;
        }
    }
}

/// The members a token response changes, as Claude Code records them: times are integer
/// milliseconds since the epoch.
fn apply_renewal(
    oauth: &mut serde_json::Map<String, Value>,
    renewal: &ClaudeRefreshResponse,
    now_millis: i64,
) {
    oauth.insert("accessToken".into(), renewal.access_token.clone().into());
    if let Some(refresh) = renewal
        .refresh_token
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        oauth.insert("refreshToken".into(), refresh.into());
    }
    let at = |seconds: Option<f64>| {
        seconds
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(|seconds| now_millis.saturating_add((seconds * 1000.0).round() as i64))
    };
    if let Some(expires) = at(renewal.expires_in) {
        oauth.insert("expiresAt".into(), expires.into());
    }
    if let Some(expires) = at(renewal.refresh_token_expires_in) {
        oauth.insert("refreshTokenExpiresAt".into(), expires.into());
    }
    if let Some(scope) = renewal.scope.as_deref() {
        let scopes: Vec<&str> = scope.split_whitespace().collect();
        if !scopes.is_empty() {
            oauth.insert("scopes".into(), scopes.into());
        }
    }
}

/// The saved login as a document; `None` once it is gone.
fn read_saved_document(source: &CredentialSource) -> Result<Option<Value>, ClaudeError> {
    let bytes = match source {
        CredentialSource::File(path) => match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ClaudeError::AuthWrite),
        },
        CredentialSource::Keychain { service, account } => {
            match read_generic_password(service, account) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return Ok(None),
                Err(_) => return Err(ClaudeError::AuthWrite),
            }
        }
        CredentialSource::Environment => return Ok(None),
    };
    Ok(parse_document(&bytes))
}

/// How long a renewal waits while Claude Code renews its login.
const CLAUDE_CODE_LOCK_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(600)
} else {
    std::time::Duration::from_secs(10)
};

/// Claude Code's refresh lock over the login kept in `scope`'s configuration directory; held
/// until dropped. Claude Code creates the directory before locking it.
pub(super) fn claude_code_lock(
    scope: &ClaudeCredentialScope,
) -> Result<super::refresh_lock::RefreshLock, ClaudeError> {
    let path = credential_path(scope);
    let directory = path.parent().ok_or(ClaudeError::RefreshLockUnavailable)?;
    fs::create_dir_all(directory).map_err(|_| ClaudeError::RefreshLockUnavailable)?;
    match super::refresh_lock::RefreshLock::acquire(
        directory,
        super::refresh_lock::Wait::up_to(CLAUDE_CODE_LOCK_WAIT),
    ) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(ClaudeError::ClaudeCodeRenewing),
        Err(_) => Err(ClaudeError::RefreshLockUnavailable),
    }
}

pub(super) fn credential_generation(scope: &ClaudeCredentialScope) -> ClaudeCredentialGeneration {
    ClaudeCredentialGeneration::from_candidates(&load_candidates(scope))
}

fn oauth_generation_fingerprint(oauth: &ClaudeOAuth) -> [u8; 32] {
    let mut fingerprint = Sha256::new();
    update_optional_text(&mut fingerprint, oauth.access_token.as_deref());
    update_optional_text(&mut fingerprint, oauth.refresh_token.as_deref());
    match oauth.expires_at {
        Some(value) => {
            fingerprint.update([1]);
            fingerprint.update(value.to_bits().to_le_bytes());
        }
        None => fingerprint.update([0]),
    }
    update_optional_text(&mut fingerprint, oauth.subscription_type.as_deref());
    update_optional_text(&mut fingerprint, oauth.rate_limit_tier.as_deref());
    match oauth.scopes.as_ref() {
        Some(scopes) => {
            fingerprint.update([1]);
            fingerprint.update((scopes.len() as u64).to_le_bytes());
            for scope in scopes {
                update_text(&mut fingerprint, scope);
            }
        }
        None => fingerprint.update([0]),
    }
    fingerprint.finalize().into()
}

fn update_optional_text(fingerprint: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            fingerprint.update([1]);
            update_text(fingerprint, value);
        }
        None => fingerprint.update([0]),
    }
}

fn update_text(fingerprint: &mut Sha256, value: &str) {
    fingerprint.update((value.len() as u64).to_le_bytes());
    fingerprint.update(value.as_bytes());
}

fn write_private_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), ClaudeError> {
    let parent = path.parent().ok_or(ClaudeError::AuthWrite)?;
    fs::create_dir_all(parent).map_err(|_| ClaudeError::AuthWrite)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|_| ClaudeError::AuthWrite)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| ClaudeError::AuthWrite)?;
    }
    temporary
        .write_all(bytes)
        .map_err(|_| ClaudeError::AuthWrite)?;
    temporary.flush().map_err(|_| ClaudeError::AuthWrite)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| ClaudeError::AuthWrite)?;
    temporary
        .persist(path)
        .map_err(|_| ClaudeError::AuthWrite)?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ClaudeOAuthConfig {
    pub usage_url: String,
    pub refresh_url: String,
    pub client_id: String,
}

pub(super) fn has_local_credentials(scope: &ClaudeCredentialScope) -> bool {
    !load_candidates(scope).is_empty()
}

pub fn has_desktop_app_data() -> bool {
    #[cfg(target_os = "macos")]
    {
        has_desktop_app_material_at(&home_directory())
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(any(target_os = "macos", test))]
fn has_desktop_app_material_at(home: &Path) -> bool {
    let root = home
        .join("Library")
        .join("Application Support")
        .join("Claude");
    let Ok(document) = fs::read(root.join("config.json")) else {
        return false;
    };
    let Ok(config) = serde_json::from_slice::<serde_json::Value>(&document) else {
        return false;
    };
    let has_token_cache = ["oauth:tokenCacheV2", "oauth:tokenCache"]
        .into_iter()
        .any(|key| {
            config
                .get(key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
        });
    has_token_cache
        && [root.join("Cookies"), root.join("Network").join("Cookies")]
            .into_iter()
            .any(|path| path.is_file())
}

pub(super) fn load_candidates(scope: &ClaudeCredentialScope) -> Vec<ClaudeCredential> {
    load_candidates_with_environment(scope, env_text("CLAUDE_CODE_OAUTH_TOKEN"))
}

fn load_candidates_with_environment(
    scope: &ClaudeCredentialScope,
    environment_token: Option<String>,
) -> Vec<ClaudeCredential> {
    let file = {
        let path = credential_path(scope);
        fs::read(&path)
            .ok()
            .and_then(|bytes| parse_candidate(&bytes, CredentialSource::File(path), false))
    };
    let load_keychain = || {
        let mut candidate = None;
        for (service, account) in keychain_candidates(scope) {
            let Ok(Some(bytes)) = read_generic_password(&service, &account) else {
                continue;
            };
            if let Some(credential) = parse_candidate(
                &bytes,
                CredentialSource::Keychain { service, account },
                false,
            ) {
                candidate = Some(credential);
                break;
            }
        }
        candidate
    };

    let mut stored = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some(credential) = load_keychain() {
        stored.push(credential);
    }
    #[cfg(not(target_os = "macos"))]
    let needs_keychain_fallback = file.is_none();
    if let Some(credential) = file {
        stored.push(credential);
    }
    #[cfg(not(target_os = "macos"))]
    if needs_keychain_fallback {
        if let Some(credential) = load_keychain() {
            stored.push(credential);
        }
    }

    #[cfg(test)]
    if !matches!(scope, ClaudeCredentialScope::Standard) {
        return stored;
    }
    let Some(environment_token) = environment_token else {
        return stored;
    };
    let base = stored.first().cloned().unwrap_or(ClaudeCredential {
        oauth: ClaudeOAuth::default(),
        source: CredentialSource::Environment,
        document: ClaudeCredentialsFile::default(),
        inference_only: true,
        unsaved_from: None,
    });
    let mut oauth = base.oauth;
    oauth.access_token = Some(environment_token);
    let environment = ClaudeCredential {
        oauth,
        source: CredentialSource::Environment,
        document: base.document,
        inference_only: true,
        unsaved_from: None,
    };
    let live = stored
        .into_iter()
        .filter(|candidate| candidate.has_profile_scope())
        .collect::<Vec<_>>();
    if live.is_empty() {
        vec![environment]
    } else {
        live.into_iter().chain(Some(environment)).collect()
    }
}

pub(super) fn oauth_config() -> Result<ClaudeOAuthConfig, ClaudeError> {
    oauth_config_from(env_text)
}

fn oauth_config_from(
    env: impl Fn(&str) -> Option<String>,
) -> Result<ClaudeOAuthConfig, ClaudeError> {
    // Claude Code's refresh token and access token must not be sent to an unapproved host.
    if env("CLAUDE_CODE_CUSTOM_OAUTH_URL")
        .is_some_and(|custom| approved_custom_oauth_url(&custom).is_none())
    {
        return Err(ClaudeError::InvalidOAuthUrl);
    }
    let (base, refresh_url, default_client_id, _) = resolved_oauth_settings_from(&env);
    let usage_url = format!("{base}/api/oauth/usage");
    validate_http_url(&usage_url)?;
    validate_http_url(&refresh_url)?;
    Ok(ClaudeOAuthConfig {
        usage_url,
        refresh_url,
        client_id: env("CLAUDE_CODE_OAUTH_CLIENT_ID").unwrap_or(default_client_id),
    })
}

fn resolved_oauth_settings() -> (String, String, String, &'static str) {
    resolved_oauth_settings_from(env_text)
}

fn resolved_oauth_settings_from(
    env: impl Fn(&str) -> Option<String>,
) -> (String, String, String, &'static str) {
    let flag = |name: &str| {
        env(name)
            .map(|value| {
                !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            })
            .unwrap_or(false)
    };
    let mut base = DEFAULT_API_BASE.to_owned();
    let mut refresh = DEFAULT_REFRESH_URL.to_owned();
    let mut client_id = DEFAULT_CLIENT_ID.to_owned();
    let mut suffix = "";
    if env("USER_TYPE").as_deref() == Some("ant") && flag("USE_LOCAL_OAUTH") {
        base = env("CLAUDE_LOCAL_OAUTH_API_BASE")
            .unwrap_or_else(|| "http://localhost:8000".into())
            .trim_end_matches('/')
            .to_owned();
        refresh = format!("{base}/v1/oauth/token");
        client_id = NON_PROD_CLIENT_ID.into();
        suffix = "-local-oauth";
    } else if env("USER_TYPE").as_deref() == Some("ant") && flag("USE_STAGING_OAUTH") {
        base = "https://api-staging.anthropic.com".into();
        refresh = "https://platform.staging.ant.dev/v1/oauth/token".into();
        client_id = NON_PROD_CLIENT_ID.into();
        suffix = "-staging-oauth";
    }
    if let Some(custom) = env("CLAUDE_CODE_CUSTOM_OAUTH_URL")
        .as_deref()
        .and_then(approved_custom_oauth_url)
    {
        base = custom.to_owned();
        refresh = format!("{base}/v1/oauth/token");
        suffix = "-custom-oauth";
    }
    (base, refresh, client_id, suffix)
}

fn approved_custom_oauth_url(value: &str) -> Option<&'static str> {
    let value = value.trim_end_matches('/');
    APPROVED_CUSTOM_OAUTH_URLS
        .iter()
        .copied()
        .find(|approved| *approved == value)
}

fn validate_http_url(value: &str) -> Result<(), ClaudeError> {
    let url = reqwest::Url::parse(value).map_err(|_| ClaudeError::InvalidOAuthUrl)?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(ClaudeError::InvalidOAuthUrl);
    }
    Ok(())
}

fn parse_candidate(
    bytes: &[u8],
    source: CredentialSource,
    inference_only: bool,
) -> Option<ClaudeCredential> {
    let document = parse_credentials(bytes)?;
    let oauth = document.claude_ai_oauth.clone()?;
    oauth
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(ClaudeCredential {
        oauth,
        source,
        document,
        inference_only,
        unsaved_from: None,
    })
}

fn parse_credentials(bytes: &[u8]) -> Option<ClaudeCredentialsFile> {
    serde_json::from_value(parse_document(bytes)?).ok()
}

/// A credentials document: JSON, or JSON hex-encoded as Claude Code writes the Keychain item.
fn parse_document(bytes: &[u8]) -> Option<Value> {
    serde_json::from_slice(bytes).ok().or_else(|| {
        let text = std::str::from_utf8(bytes).ok()?.trim();
        if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let decoded = (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
            .collect::<Option<Vec<_>>>()?;
        serde_json::from_slice(&decoded).ok()
    })
}

fn credential_path(scope: &ClaudeCredentialScope) -> PathBuf {
    match scope {
        ClaudeCredentialScope::Standard => claude_home(),
        #[cfg(test)]
        ClaudeCredentialScope::ConfigDir { path, .. } => path.clone(),
    }
    .join(".credentials.json")
}

pub fn claude_home() -> PathBuf {
    env_text("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_directory().join(".claude"))
}

fn keychain_candidates(scope: &ClaudeCredentialScope) -> Vec<(String, String)> {
    let suffix = resolved_oauth_settings().3;
    let service = format!("Claude Code{suffix}-credentials");
    let services = match scope {
        ClaudeCredentialScope::Standard => {
            if let Some(config_dir) = env_text("CLAUDE_CONFIG_DIR") {
                vec![scoped_keychain_service_name(&config_dir), service]
            } else {
                vec![service]
            }
        }
        #[cfg(test)]
        ClaudeCredentialScope::ConfigDir {
            keychain_literal, ..
        } => vec![scoped_keychain_service_name(keychain_literal)],
    };
    let accounts = keychain_accounts();
    services
        .into_iter()
        .flat_map(|service| {
            accounts
                .iter()
                .cloned()
                .map(move |account| (service.clone(), account))
        })
        .collect()
}

pub(super) fn scoped_keychain_service_name(config_dir_literal: &str) -> String {
    let suffix = resolved_oauth_settings().3;
    let service = format!("Claude Code{suffix}-credentials");
    let normalized = config_dir_literal.replace('\\', "/");
    let hash = sha256_hex(normalized.as_bytes());
    format!("{service}-{}", &hash[..8])
}

pub(super) fn keychain_accounts() -> Vec<String> {
    let user = env_text("USER")
        .or_else(|| env_text("USERNAME"))
        .unwrap_or_default();
    let mut accounts = vec![user, String::new()];
    accounts.dedup();
    accounts
}

fn env_text(name: &str) -> Option<String> {
    crate::provider_environment::value(name)
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::Utc;
    use tempfile::tempdir;

    use super::{
        has_desktop_app_material_at, load_candidates_with_environment, parse_candidate,
        parse_credentials, parse_document, write_private_file_atomic, ClaudeCredential,
        ClaudeCredentialGeneration, ClaudeCredentialScope, ClaudeCredentialsFile, ClaudeOAuth,
        ClaudeRefreshResponse, CredentialSource,
    };
    use crate::providers::claude::ClaudeError;

    #[test]
    fn account_settings_do_not_change_oauth_endpoints_or_credentials() {
        let directory = tempfile::tempdir().unwrap();
        for account in ["work", "personal"] {
            let path = directory.path().join(account);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(
                path.join("settings.json"),
                serde_json::json!({"env": {
                    "CLAUDE_CODE_CUSTOM_OAUTH_URL": format!("https://{account}.example.com"),
                    "CLAUDE_CODE_OAUTH_CLIENT_ID": account,
                    "CLAUDE_CODE_OAUTH_TOKEN": format!("{account}-token")
                }})
                .to_string(),
            )
            .unwrap();
            let scope = super::ClaudeCredentialScope::ConfigDir {
                keychain_literal: path.to_string_lossy().into_owned(),
                path,
            };
            let config = super::oauth_config_from(|_| None).unwrap();
            assert_eq!(
                config.usage_url,
                "https://api.anthropic.com/api/oauth/usage"
            );
            assert_eq!(
                config.refresh_url,
                "https://platform.claude.com/v1/oauth/token"
            );
            assert_eq!(config.client_id, super::DEFAULT_CLIENT_ID);
            fs::write(
                super::credential_path(&scope),
                br#"{"claudeAiOauth":{"accessToken":"stored-token"}}"#,
            )
            .unwrap();
            assert_eq!(
                super::load_candidates(&scope)[0].access_token(),
                Some("stored-token")
            );
        }
    }

    #[test]
    fn oauth_environment_selects_endpoints_client_and_keychain_namespace() {
        let resolve = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let custom = resolve(&[
            (
                "CLAUDE_CODE_CUSTOM_OAUTH_URL",
                "https://claude.fedstart.com/",
            ),
            ("CLAUDE_CODE_OAUTH_CLIENT_ID", "custom-client"),
        ]);
        let config = super::oauth_config_from(&custom).unwrap();
        assert_eq!(
            config.usage_url,
            "https://claude.fedstart.com/api/oauth/usage"
        );
        assert_eq!(
            config.refresh_url,
            "https://claude.fedstart.com/v1/oauth/token"
        );
        assert_eq!(config.client_id, "custom-client");
        assert_eq!(
            super::resolved_oauth_settings_from(&custom).3,
            "-custom-oauth"
        );

        let staging = resolve(&[("USER_TYPE", "ant"), ("USE_STAGING_OAUTH", "1")]);
        let config = super::oauth_config_from(&staging).unwrap();
        assert_eq!(
            config.usage_url,
            "https://api-staging.anthropic.com/api/oauth/usage"
        );
        assert_eq!(
            config.refresh_url,
            "https://platform.staging.ant.dev/v1/oauth/token"
        );
        assert_eq!(config.client_id, super::NON_PROD_CLIENT_ID);
        assert_eq!(
            super::resolved_oauth_settings_from(&staging).3,
            "-staging-oauth"
        );

        let local = resolve(&[("USER_TYPE", "ant"), ("USE_LOCAL_OAUTH", "true")]);
        let config = super::oauth_config_from(&local).unwrap();
        assert_eq!(config.refresh_url, "http://localhost:8000/v1/oauth/token");
        assert_eq!(
            super::resolved_oauth_settings_from(&local).3,
            "-local-oauth"
        );

        for disabled in [
            resolve(&[("USE_STAGING_OAUTH", "1")]),
            resolve(&[("USER_TYPE", "ant"), ("USE_STAGING_OAUTH", "off")]),
        ] {
            let config = super::oauth_config_from(&disabled).unwrap();
            assert_eq!(config.refresh_url, super::DEFAULT_REFRESH_URL);
            assert_eq!(super::resolved_oauth_settings_from(&disabled).3, "");
        }

        // Like Claude Code, only approved custom endpoints receive its tokens.
        for value in [
            "not a url",
            "http://example.test",
            "https://oauth.example",
            "http://claude.fedstart.com",
            "https://claude.fedstart.com.example.test",
        ] {
            let invalid = resolve(&[("CLAUDE_CODE_CUSTOM_OAUTH_URL", value)]);
            assert!(
                matches!(
                    super::oauth_config_from(&invalid),
                    Err(ClaudeError::InvalidOAuthUrl)
                ),
                "{value}"
            );
            assert_eq!(super::resolved_oauth_settings_from(&invalid).3, "");
        }
    }

    #[test]
    fn parses_claude_credentials_and_hex_fallback() {
        let raw = br#"{"claudeAiOauth":{"accessToken":"placeholder","scopes":["user:profile"]}}"#;
        let parsed = parse_credentials(raw).unwrap();
        assert_eq!(
            parsed.claude_ai_oauth.unwrap().access_token.as_deref(),
            Some("placeholder")
        );
        let hex = raw
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let _: ClaudeCredentialsFile = parse_credentials(hex.as_bytes()).unwrap();
    }

    #[test]
    fn config_dir_scope_reads_only_its_own_credentials() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(
            first.join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"first-token"}}"#,
        )
        .unwrap();
        fs::write(
            second.join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"second-token"}}"#,
        )
        .unwrap();

        let candidates = load_candidates_with_environment(
            &ClaudeCredentialScope::ConfigDir {
                path: first.clone(),
                keychain_literal: first.to_string_lossy().into_owned(),
            },
            Some("ambient-token".into()),
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].access_token(), Some("first-token"));
    }

    fn renewal(access: &str, refresh: Option<&str>) -> ClaudeRefreshResponse {
        ClaudeRefreshResponse {
            access_token: access.into(),
            refresh_token: refresh.map(Into::into),
            expires_in: Some(3600.0),
            refresh_token_expires_in: Some(7200.0),
            scope: Some("user:profile user:inference".into()),
        }
    }

    fn file_credential(path: &std::path::Path) -> ClaudeCredential {
        parse_candidate(
            &fs::read(path).unwrap(),
            CredentialSource::File(path.to_path_buf()),
            false,
        )
        .unwrap()
    }

    #[test]
    fn credential_write_failures_are_typed_and_do_not_expose_tokens() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".credentials.json");
        fs::write(
            &path,
            br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"spent"}}"#,
        )
        .unwrap();
        let mut credential = file_credential(&path);
        // The saved login cannot be replaced: its directory is read-only.
        let blocked = directory.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        let blocked_path = blocked.join(".credentials.json");
        fs::copy(&path, &blocked_path).unwrap();
        credential.source = CredentialSource::File(blocked_path.clone());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
        }
        #[cfg(not(unix))]
        fs::remove_file(&blocked_path)
            .and_then(|()| fs::create_dir(&blocked_path))
            .unwrap();

        let error = credential
            .save_renewal(
                &renewal("secret-access", Some("secret-refresh")),
                "spent",
                Utc::now().timestamp_millis(),
            )
            .unwrap_err();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        }

        assert!(matches!(error, ClaudeError::AuthWrite));
        assert!(!error.to_string().contains("secret-access"));
        assert!(!error.to_string().contains("secret-refresh"));
        // The renewal stays usable for this session.
        assert_eq!(credential.access_token(), Some("secret-access"));
        assert_eq!(
            credential.oauth.refresh_token.as_deref(),
            Some("secret-refresh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unsaved_renewal_resumes_on_the_reloaded_login_and_is_saved_once_writable() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        let path = blocked.join(".credentials.json");
        fs::write(
            &path,
            br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"spent"}}"#,
        )
        .unwrap();
        let mut credential = file_credential(&path);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
        let saved = credential.save_renewal(&renewal("first", Some("next")), "spent", 0);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(saved, Err(ClaudeError::AuthWrite)));
        let unsaved = credential
            .unsaved_renewal()
            .expect("the failed save keeps the renewal");

        // The next poll reloads the saved login, which still holds the spent token.
        let mut reloaded = file_credential(&path);
        assert_eq!(reloaded.oauth.refresh_token.as_deref(), Some("spent"));
        assert!(reloaded.resume_unsaved(&unsaved));
        assert_eq!(reloaded.access_token(), Some("first"));
        assert_eq!(reloaded.oauth.refresh_token.as_deref(), Some("next"));

        // Renewing the resumed login spends "next" and replaces the saved "spent".
        assert!(reloaded
            .save_renewal(&renewal("second", Some("last")), "next", 0)
            .unwrap());
        assert!(reloaded.unsaved_renewal().is_none());
        let saved = file_credential(&path);
        assert_eq!(saved.access_token(), Some("second"));
        assert_eq!(saved.oauth.refresh_token.as_deref(), Some("last"));

        // A saved login renewed or switched elsewhere no longer takes the unsaved renewal.
        let mut changed = file_credential(&path);
        assert!(!changed.resume_unsaved(&unsaved));
        assert_eq!(changed.access_token(), Some("second"));
    }

    #[test]
    fn atomic_credential_write_replaces_an_existing_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        fs::write(&path, b"old credential").unwrap();

        write_private_file_atomic(&path, b"new credential").unwrap();

        assert_eq!(fs::read(path).unwrap(), b"new credential");
    }

    #[test]
    fn renewal_keeps_every_member_it_does_not_change() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".credentials.json");
        // Members Claude Code keeps beside the tokens, including ones unknown here.
        fs::write(
            &path,
            br#"{"mcpOAuth":{"server|1":{"accessToken":"mcp","expiresAt":5}},"claudeAiOauth":{"accessToken":"old","refreshToken":"spent","expiresAt":1,"clientId":"code-client","subscriptionType":"max","rateLimitTier":null,"scopes":["user:inference"],"tokenAccount":{"uuid":"acct"}},"other":[1,2]}"#,
        )
        .unwrap();
        let mut credential = file_credential(&path);
        let now = 1_700_000_000_123;

        assert!(credential
            .save_renewal(&renewal("new", Some("next")), "spent", now)
            .unwrap());

        let raw = fs::read_to_string(&path).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let oauth = &saved["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], "new");
        assert_eq!(oauth["refreshToken"], "next");
        // Integer milliseconds, as Claude Code writes them.
        assert_eq!(oauth["expiresAt"].as_i64(), Some(now + 3_600_000));
        assert_eq!(
            oauth["refreshTokenExpiresAt"].as_i64(),
            Some(now + 7_200_000)
        );
        assert!(!raw.contains(".0,") && !raw.contains(".0}"));
        assert_eq!(
            oauth["scopes"],
            serde_json::json!(["user:profile", "user:inference"])
        );
        assert_eq!(oauth["clientId"], "code-client");
        assert_eq!(oauth["subscriptionType"], "max");
        assert!(oauth["rateLimitTier"].is_null() && oauth.get("rateLimitTier").is_some());
        assert_eq!(oauth["tokenAccount"]["uuid"], "acct");
        assert_eq!(saved["mcpOAuth"]["server|1"]["accessToken"], "mcp");
        assert_eq!(saved["other"], serde_json::json!([1, 2]));
        // What was saved is what is used and what is read back.
        assert_eq!(credential.access_token(), Some("new"));
        assert_eq!(
            ClaudeCredentialGeneration::from_candidates(&[credential.clone()]),
            ClaudeCredentialGeneration::from_candidates(&[file_credential(&path)])
        );

        // A response without a new refresh token keeps the one in use.
        assert!(credential
            .save_renewal(&renewal("newer", None), "next", now)
            .unwrap());
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "newer");
        assert_eq!(saved["claudeAiOauth"]["refreshToken"], "next");
    }

    #[test]
    fn credential_rotation_refuses_to_overwrite_a_changed_login() {
        let directory = tempdir().unwrap();
        let path = directory.path().join(".credentials.json");
        fs::write(
            &path,
            br#"{"claudeAiOauth":{"accessToken":"account-a","refreshToken":"refresh-a"}}"#,
        )
        .unwrap();
        let mut credential = file_credential(&path);
        // Renewed or switched by another program while our refresh ran.
        let changed =
            br#"{"claudeAiOauth":{"accessToken":"account-b","refreshToken":"refresh-b"}}"#;
        fs::write(&path, changed).unwrap();

        let error = credential
            .save_renewal(
                &renewal("rotated-a", Some("rotated-refresh-a")),
                "refresh-a",
                Utc::now().timestamp_millis(),
            )
            .unwrap_err();

        assert!(matches!(error, ClaudeError::CredentialsChanged));
        assert_eq!(credential.oauth.access_token.as_deref(), Some("account-a"));
        assert_eq!(fs::read(&path).unwrap(), changed);
        // A removed login is not recreated either.
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            credential.save_renewal(&renewal("rotated-a", None), "refresh-a", 0),
            Err(ClaudeError::CredentialsChanged)
        ));
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn renewal_replaces_a_symlinked_logins_target_not_the_link() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("dotfiles-credentials.json");
        fs::write(
            &target,
            br#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"spent"}}"#,
        )
        .unwrap();
        let link = directory.path().join(".credentials.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut credential = file_credential(&link);

        credential
            .save_renewal(&renewal("new", Some("next")), "spent", 0)
            .unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "new");
    }

    #[test]
    fn hex_encoded_keychain_documents_are_read_whole() {
        let raw = br#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"},"mcpOAuth":{"k":1}}"#;
        let hex = raw
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let document = parse_document(hex.as_bytes()).unwrap();
        assert_eq!(document["mcpOAuth"]["k"], 1);
    }

    #[test]
    fn credential_generation_tracks_order_source_and_complete_oauth_state() {
        let credential = |source: CredentialSource, access: &str, refresh: &str| ClaudeCredential {
            oauth: ClaudeOAuth {
                access_token: Some(access.into()),
                refresh_token: Some(refresh.into()),
                expires_at: Some(1.0),
                subscription_type: Some("pro".into()),
                rate_limit_tier: Some("tier-a".into()),
                scopes: Some(vec!["user:profile".into()]),
            },
            source,
            document: ClaudeCredentialsFile::default(),
            inference_only: false,
            unsaved_from: None,
        };
        let file = credential(
            CredentialSource::File("credentials.json".into()),
            "access-a",
            "refresh-a",
        );
        let keychain = credential(
            CredentialSource::Keychain {
                service: "service".into(),
                account: "account".into(),
            },
            "access-b",
            "refresh-b",
        );
        let original =
            ClaudeCredentialGeneration::from_candidates(&[file.clone(), keychain.clone()]);

        assert_ne!(
            original,
            ClaudeCredentialGeneration::from_candidates(&[keychain.clone(), file.clone()])
        );
        let mut metadata_changed = file.clone();
        metadata_changed.oauth.rate_limit_tier = Some("tier-b".into());
        assert_ne!(
            original,
            ClaudeCredentialGeneration::from_candidates(&[metadata_changed, keychain.clone()])
        );
        let mut rotated = file;
        rotated.oauth.access_token = Some("rotated".into());
        let replaced = original.replacing(&rotated).unwrap();
        assert_eq!(
            replaced,
            ClaudeCredentialGeneration::from_candidates(&[rotated, keychain])
        );
    }

    #[test]
    fn credential_fingerprint_covers_the_complete_token_pair() {
        let credential = |access: &str, refresh: &str| ClaudeCredential {
            oauth: ClaudeOAuth {
                access_token: Some(access.into()),
                refresh_token: Some(refresh.into()),
                ..ClaudeOAuth::default()
            },
            source: CredentialSource::Environment,
            document: ClaudeCredentialsFile::default(),
            inference_only: false,
            unsaved_from: None,
        };

        let original = credential("shared-access", "refresh-a").fingerprint();
        assert_eq!(
            original,
            credential("shared-access", "refresh-a").fingerprint()
        );
        assert_ne!(
            original,
            credential("shared-access", "refresh-b").fingerprint()
        );
        assert_ne!(
            original,
            credential("different-access", "refresh-a").fingerprint()
        );
    }

    #[test]
    fn desktop_detection_requires_token_cache_and_cookie_database() {
        let directory = tempdir().unwrap();
        let root = directory
            .path()
            .join("Library")
            .join("Application Support")
            .join("Claude");
        fs::create_dir_all(root.join("Network")).unwrap();

        assert!(!has_desktop_app_material_at(directory.path()));
        fs::write(
            root.join("config.json"),
            br#"{"oauth:tokenCacheV2":"encrypted-cache"}"#,
        )
        .unwrap();
        assert!(!has_desktop_app_material_at(directory.path()));

        fs::write(root.join("Network").join("Cookies"), b"sqlite").unwrap();
        assert!(has_desktop_app_material_at(directory.path()));
    }

    #[test]
    fn desktop_detection_rejects_empty_or_invalid_cache_documents() {
        let directory = tempdir().unwrap();
        let root = directory
            .path()
            .join("Library")
            .join("Application Support")
            .join("Claude");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cookies"), b"sqlite").unwrap();

        fs::write(root.join("config.json"), br#"{"oauth:tokenCache":""}"#).unwrap();
        assert!(!has_desktop_app_material_at(directory.path()));
        fs::write(root.join("config.json"), b"not-json").unwrap();
        assert!(!has_desktop_app_material_at(directory.path()));
    }
}
