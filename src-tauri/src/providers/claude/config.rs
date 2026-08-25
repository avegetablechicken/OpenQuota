use std::{fs, path::PathBuf};

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

fn settings_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".claude")
        .join("settings.json")
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
