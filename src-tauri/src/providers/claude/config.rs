use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClaudeConfigError {
    #[error("Enter a Claude settings name.")]
    MissingName,
    #[error("The Claude configuration directory could not be read.")]
    Unreadable,
    #[error(
        "No JSON configuration file matching \"{0}\" was found in the Claude configuration directory."
    )]
    NotFound(String),
    #[error("Multiple JSON configuration files match \"{0}\". Enter a more specific name or the full filename.")]
    Ambiguous(String),
    #[error("The selected Claude settings file could not be read.")]
    FileUnreadable,
    #[error("The selected Claude settings file is not valid JSON.")]
    Invalid,
    #[error("The selected Claude settings file does not define a nonempty ANTHROPIC_BASE_URL.")]
    MissingBaseUrl,
}

pub fn resolve_provider_base_url(name: &str) -> Result<String, ClaudeConfigError> {
    let settings = settings_path();
    resolve_provider_base_url_in_directory(name, settings.parent().unwrap_or(Path::new(".")))
}

fn resolve_provider_base_url_in_directory(
    name: &str,
    directory: &Path,
) -> Result<String, ClaudeConfigError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ClaudeConfigError::MissingName);
    }
    let mut settings_matches = Vec::new();
    let mut exact_matches = Vec::new();
    let mut directories = vec![directory.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).map_err(|_| ClaudeConfigError::Unreadable)? {
            let entry = entry.map_err(|_| ClaudeConfigError::Unreadable)?;
            let kind = entry
                .file_type()
                .map_err(|_| ClaudeConfigError::Unreadable)?;
            // Do not traverse symlink directories: configuration trees can contain cycles.
            if kind.is_dir() {
                directories.push(entry.path());
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            let Some(stem) = filename.strip_suffix(".json") else {
                continue;
            };
            if filename.contains("settings") && filename.contains(name) {
                settings_matches.push(entry.path());
            }
            if stem == name || filename == name {
                exact_matches.push(entry.path());
            }
        }
    }
    let mut matches = if settings_matches.is_empty() {
        exact_matches
    } else {
        settings_matches
    };
    let path = match matches.len() {
        0 => return Err(ClaudeConfigError::NotFound(name.into())),
        1 => matches.remove(0),
        _ => return Err(ClaudeConfigError::Ambiguous(name.into())),
    };
    let text = fs::read_to_string(path).map_err(|_| ClaudeConfigError::FileUnreadable)?;
    let document: Value = serde_json::from_str(&text).map_err(|_| ClaudeConfigError::Invalid)?;
    // Only this explicitly selected file may supply the endpoint; never use ambient values.
    document
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
        .or_else(|| document.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(ClaudeConfigError::MissingBaseUrl)
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

#[cfg(test)]
mod tests {

    #[test]
    fn provider_search_requires_a_unique_settings_json() {
        use super::{resolve_provider_base_url_in_directory as resolve, ClaudeConfigError};
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str| {
            std::fs::write(
                dir.path().join(name),
                r#"{"env":{"ANTHROPIC_BASE_URL":" https://relay.example.com/api/v1 "}}"#,
            )
            .unwrap()
        };
        write("work.settings.json");
        write("personal.json");
        write("settings-work.json.bak");
        assert_eq!(
            resolve(" work ", dir.path()).unwrap(),
            "https://relay.example.com/api/v1"
        );
        for name in ["Work", "../work", "absent"] {
            assert_eq!(
                resolve(name, dir.path()),
                Err(ClaudeConfigError::NotFound(name.into()))
            );
        }
        assert_eq!(
            resolve(" ", dir.path()),
            Err(ClaudeConfigError::MissingName)
        );
        write("settings-work.json");
        assert_eq!(
            resolve("work", dir.path()),
            Err(ClaudeConfigError::Ambiguous("work".into()))
        );
        assert!(resolve("work.settings.json", dir.path()).is_ok());
        write("copy-work.settings.json");
        assert_eq!(
            resolve("work.settings.json", dir.path()),
            Err(ClaudeConfigError::Ambiguous("work.settings.json".into()))
        );
    }

    #[test]
    fn settings_matches_precede_recursive_exact_stems_and_ambiguity_never_falls_back() {
        use super::{resolve_provider_base_url_in_directory as resolve, ClaudeConfigError};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("profiles")).unwrap();
        let exact = dir.path().join("profiles/api.json");
        std::fs::write(
            &exact,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://exact.example.com"}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve("api", dir.path()).unwrap(),
            "https://exact.example.com"
        );
        assert_eq!(
            resolve("api.json", dir.path()).unwrap(),
            "https://exact.example.com"
        );
        let settings = dir.path().join("settings-api.json");
        std::fs::write(
            &settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://settings.example.com"}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve("api", dir.path()).unwrap(),
            "https://settings.example.com"
        );
        std::fs::write(&settings, "{}").unwrap();
        assert_eq!(
            resolve("api", dir.path()),
            Err(ClaudeConfigError::MissingBaseUrl)
        );
        std::fs::write(dir.path().join("profiles/api-settings.json"), "{}").unwrap();
        assert_eq!(
            resolve("api", dir.path()),
            Err(ClaudeConfigError::Ambiguous("api".into()))
        );
        std::fs::remove_file(settings).unwrap();
        std::fs::remove_file(dir.path().join("profiles/api-settings.json")).unwrap();
        std::fs::write(dir.path().join("api.json"), "{}").unwrap();
        assert_eq!(
            resolve("api", dir.path()),
            Err(ClaudeConfigError::Ambiguous("api".into()))
        );
        assert_eq!(
            resolve("ap", dir.path()),
            Err(ClaudeConfigError::NotFound("ap".into()))
        );
        assert_eq!(
            resolve("profiles/api.json", dir.path()),
            Err(ClaudeConfigError::NotFound("profiles/api.json".into()))
        );
    }

    #[test]
    fn provider_reads_only_selected_file_and_preserves_explicit_empty_overrides() {
        use super::{resolve_provider_base_url_in_directory as resolve, ClaudeConfigError};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for text in [
            "{}",
            r#"{"env":{"ANTHROPIC_BASE_URL":" "},"ANTHROPIC_BASE_URL":"https://ignored.example.com"}"#,
            r#"{"env":{"ANTHROPIC_BASE_URL":false}}"#,
        ] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(
                resolve("settings", dir.path()),
                Err(ClaudeConfigError::MissingBaseUrl)
            );
        }
        std::fs::write(&path, "broken").unwrap();
        assert_eq!(
            resolve("settings", dir.path()),
            Err(ClaudeConfigError::Invalid)
        );
        std::fs::write(
            &path,
            r#"{"ANTHROPIC_BASE_URL":"https://legacy.example.com"}"#,
        )
        .unwrap();
        assert_eq!(
            resolve("settings", dir.path()).unwrap(),
            "https://legacy.example.com"
        );
    }

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
}
