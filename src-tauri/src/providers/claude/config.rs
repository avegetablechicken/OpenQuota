use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

const BASE_URL_KEY: &str = "ANTHROPIC_BASE_URL";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClaudeConfigError {
    #[error("ANTHROPIC_BASE_URL was not found in ~/.claude/settings.json or the environment.")]
    MissingBaseUrl,
}

pub fn resolve_anthropic_base_url() -> Result<String, ClaudeConfigError> {
    let settings = fs::read_to_string(settings_path()).ok();
    resolve_anthropic_base_url_from(
        settings.as_deref(),
        crate::provider_environment::value(BASE_URL_KEY).as_deref(),
    )
}

pub(crate) fn settings_path() -> PathBuf {
    crate::provider_environment::value("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
        .join("settings.json")
}

pub(super) fn settings_path_for_scope(scope: &super::auth::ClaudeCredentialScope) -> PathBuf {
    match scope {
        super::auth::ClaudeCredentialScope::Standard => settings_path(),
        super::auth::ClaudeCredentialScope::ConfigDir { path, .. } => path.join("settings.json"),
    }
}

/// Resolve only this account's settings, without exporting values into the process.
pub(crate) fn environment_value(path: &Path, names: &[&str]) -> Option<String> {
    let text = fs::read_to_string(path).ok();
    environment_value_from(text.as_deref(), names, crate::provider_environment::value)
}

fn environment_value_from(
    settings: Option<&str>,
    names: &[&str],
    fallback: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let document = settings.and_then(|text| serde_json::from_str::<Value>(text).ok());
    names
        .iter()
        .find_map(|name| {
            document
                .as_ref()?
                .get("env")?
                .get(*name)?
                .as_str()
                .map(str::to_owned)
        })
        .or_else(|| names.iter().find_map(|name| fallback(name)))
}

fn resolve_anthropic_base_url_from(
    settings: Option<&str>,
    environment: Option<&str>,
) -> Result<String, ClaudeConfigError> {
    settings
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|document| {
            document
                .get(BASE_URL_KEY)
                .and_then(Value::as_str)
                .and_then(nonempty)
                .or_else(|| {
                    document
                        .pointer(&format!("/env/{BASE_URL_KEY}"))
                        .and_then(Value::as_str)
                        .and_then(nonempty)
                })
        })
        .or_else(|| environment.and_then(nonempty))
        .ok_or(ClaudeConfigError::MissingBaseUrl)
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{resolve_anthropic_base_url_from, ClaudeConfigError};

    #[test]
    fn environment_overrides_aliases_and_empty_values_without_exporting() {
        let before = std::env::var_os("HTTPS_PROXY");
        let settings = r#"{"env":{"https_proxy":"http://file:8080","NO_PROXY":"","INVALID":42}}"#;
        let process = |_: &str| Some("process".to_owned());
        assert_eq!(
            super::environment_value_from(Some(settings), &["HTTPS_PROXY", "https_proxy"], process)
                .as_deref(),
            Some("http://file:8080")
        );
        assert_eq!(
            super::environment_value_from(Some(settings), &["NO_PROXY"], process).as_deref(),
            Some("")
        );
        for input in [None, Some("broken"), Some(settings)] {
            assert_eq!(
                super::environment_value_from(input, &["INVALID"], process).as_deref(),
                Some("process")
            );
        }
        assert_eq!(std::env::var_os("HTTPS_PROXY"), before);
    }

    #[test]
    fn settings_env_value_precedes_the_environment() {
        assert_eq!(
            resolve_anthropic_base_url_from(
                Some(r#"{"env":{"ANTHROPIC_BASE_URL":"https://settings.example.com"}}"#),
                Some("https://environment.example.com")
            )
            .unwrap(),
            "https://settings.example.com"
        );
        assert_eq!(
            resolve_anthropic_base_url_from(
                Some(r#"{"ANTHROPIC_BASE_URL":"https://top.example.com"}"#),
                Some("https://environment.example.com")
            )
            .unwrap(),
            "https://top.example.com"
        );
    }

    #[test]
    fn missing_or_invalid_settings_fall_back_to_the_environment() {
        for settings in [None, Some("{broken"), Some(r#"{"env":{}}"#)] {
            assert_eq!(
                resolve_anthropic_base_url_from(
                    settings,
                    Some(" https://environment.example.com ")
                )
                .unwrap(),
                "https://environment.example.com"
            );
        }
        assert_eq!(
            resolve_anthropic_base_url_from(Some(r#"{"env":{}}"#), None),
            Err(ClaudeConfigError::MissingBaseUrl)
        );
    }
}
