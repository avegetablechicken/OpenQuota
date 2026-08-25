use std::{collections::HashMap, ffi::OsStr, fs, path::PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodexConfigError {
    #[error("Enter a Codex model provider or profile name.")]
    MissingName,
    #[error("Codex config could not be read from ~/.codex/config.toml.")]
    Unreadable,
    #[error("Codex config.toml could not be parsed.")]
    Invalid,
    #[error("No exact Codex model provider or profile named \"{0}\" was found in ~/.codex.")]
    NotFound(String),
    #[error("Codex profile \"{0}\" could not be read.")]
    ProfileUnreadable(String),
    #[error("Codex profile \"{0}\" could not be parsed.")]
    ProfileInvalid(String),
    #[error("Codex profile \"{0}\" does not select a model provider.")]
    ProfileMissingProvider(String),
    #[error("Codex model provider \"{0}\" does not define a Base URL in its configuration.")]
    ProviderMissingBaseUrl(String),
}

#[derive(Deserialize)]
struct CodexConfig {
    model_provider: Option<String>,
    #[serde(default)]
    model_providers: HashMap<String, ModelProvider>,
}

#[derive(Deserialize)]
struct ModelProvider {
    base_url: Option<String>,
}

pub fn resolve_provider_base_url(name: &str) -> Result<String, CodexConfigError> {
    resolve_provider_base_url_in_directory(name, codex_config_directory())
}

fn codex_config_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".codex")
}

fn resolve_provider_base_url_in_directory(
    name: &str,
    directory: PathBuf,
) -> Result<String, CodexConfigError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CodexConfigError::MissingName);
    }
    let main = read_config(directory.join("config.toml"));
    if let Ok(config) = &main {
        if config.model_provider.as_deref() == Some(name)
            || config.model_providers.contains_key(name)
        {
            return provider_base_url(config, name);
        }
    }
    let Some(profile_path) = exact_profile_path(&directory, name)? else {
        return Err(match main {
            Err(error @ (CodexConfigError::Unreadable | CodexConfigError::Invalid)) => error,
            _ => CodexConfigError::NotFound(name.to_owned()),
        });
    };
    let text = fs::read_to_string(profile_path)
        .map_err(|_| CodexConfigError::ProfileUnreadable(name.to_owned()))?;
    let profile = toml::from_str::<CodexConfig>(&text)
        .map_err(|_| CodexConfigError::ProfileInvalid(name.to_owned()))?;
    let provider = profile
        .model_provider
        .as_deref()
        .filter(|provider| !provider.is_empty())
        .ok_or_else(|| CodexConfigError::ProfileMissingProvider(name.to_owned()))?;
    provider_base_url(&profile, provider)
}

fn read_config(path: PathBuf) -> Result<CodexConfig, CodexConfigError> {
    let text = fs::read_to_string(path).map_err(|_| CodexConfigError::Unreadable)?;
    toml::from_str(&text).map_err(|_| CodexConfigError::Invalid)
}

fn exact_profile_path(
    directory: &PathBuf,
    name: &str,
) -> Result<Option<PathBuf>, CodexConfigError> {
    let expected = format!("{name}.config.toml");
    let entries = fs::read_dir(directory).map_err(|_| CodexConfigError::Unreadable)?;
    Ok(entries
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() == OsStr::new(&expected) && entry.path().is_file())
        .map(|entry| entry.path()))
}

fn provider_base_url(config: &CodexConfig, provider: &str) -> Result<String, CodexConfigError> {
    config
        .model_providers
        .get(provider)
        .and_then(|provider| provider.base_url.as_deref())
        .map(str::trim)
        .filter(|base_url| !base_url.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CodexConfigError::ProviderMissingBaseUrl(provider.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{resolve_provider_base_url_in_directory, CodexConfigError};

    const MAIN_CONFIG: &str = r#"
model_provider = "primary"

[model_providers.primary]
base_url = "https://primary.example.com/api/v1"

[model_providers.secondary]
base_url = "https://secondary.example.com"

[model_providers.my]
base_url = "https://my.example.com"
"#;

    #[test]
    fn resolves_model_provider_tables_and_standalone_profile_files() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("config.toml"), MAIN_CONFIG).unwrap();
        fs::write(
            directory.path().join("work.config.toml"),
            r#"
model_provider = "secondary"

[model_providers.secondary]
base_url = "https://profile.example.com"
"#,
        )
        .unwrap();

        assert_eq!(
            resolve_provider_base_url_in_directory("primary", directory.path().into()).unwrap(),
            "https://primary.example.com/api/v1"
        );
        assert_eq!(
            resolve_provider_base_url_in_directory("secondary", directory.path().into()).unwrap(),
            "https://secondary.example.com"
        );
        assert_eq!(
            resolve_provider_base_url_in_directory("my", directory.path().into()).unwrap(),
            "https://my.example.com"
        );
        assert_eq!(
            resolve_provider_base_url_in_directory("work", directory.path().into()).unwrap(),
            "https://profile.example.com"
        );
    }

    #[test]
    fn requires_an_exact_selected_provider_or_profile_filename() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("config.toml"), MAIN_CONFIG).unwrap();
        fs::write(
            directory.path().join("work.config.toml"),
            "model_provider = \"secondary\"",
        )
        .unwrap();

        for name in ["Primary", "Secondary", "Work", "../work"] {
            assert_eq!(
                resolve_provider_base_url_in_directory(name, directory.path().into()),
                Err(CodexConfigError::NotFound(name.into()))
            );
        }
        assert_eq!(
            resolve_provider_base_url_in_directory("work", directory.path().into()),
            Err(CodexConfigError::ProviderMissingBaseUrl("secondary".into()))
        );
    }
}
