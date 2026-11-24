use std::{
    collections::HashMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

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
    env_key: Option<String>,
    experimental_bearer_token: Option<String>,
    #[serde(default)]
    http_headers: HashMap<String, String>,
    #[serde(default)]
    env_http_headers: HashMap<String, String>,
}

pub fn resolve_provider_base_url(name: &str) -> Result<String, CodexConfigError> {
    resolve_provider_base_url_in_directory(name, codex_config_directory())
}

fn codex_config_directory() -> PathBuf {
    crate::provider_environment::value("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
}

fn resolve_provider_base_url_in_directory(
    name: &str,
    directory: PathBuf,
) -> Result<String, CodexConfigError> {
    resolve_model_provider(name, &directory).map(|(base, _)| base)
}

pub(crate) fn subscription_credentials(
    name: &str,
) -> Result<(String, Option<String>), CodexConfigError> {
    let directory = codex_config_directory();
    let (base, provider) = resolve_model_provider(name, &directory)?;
    Ok((base, provider.api_key(&directory)))
}

impl ModelProvider {
    fn api_key(&self, directory: &Path) -> Option<String> {
        let lookup = |name: &str| crate::codex_environment::provider_key(directory, name.trim());
        if let Some(key) = self.env_key.as_deref().and_then(lookup).or_else(|| {
            self.experimental_bearer_token
                .as_ref()
                .filter(|v| !v.trim().is_empty())
                .cloned()
        }) {
            return Some(key.trim().to_owned());
        }
        for name in ["Authorization", "x-api-key"] {
            let header = |headers: &HashMap<String, String>| {
                headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.clone())
            };
            let value = header(&self.env_http_headers)
                .and_then(|var| lookup(&var))
                .or_else(|| header(&self.http_headers));
            if let Some(value) = value {
                let value = value.trim();
                let key = if name == "Authorization" {
                    value
                        .split_once(' ')
                        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
                        .map(|(_, key)| key.trim())
                } else {
                    Some(value)
                };
                if let Some(key) = key.filter(|key| !key.is_empty()) {
                    return Some(key.to_owned());
                }
            }
        }
        None
    }
}

fn resolve_model_provider(
    name: &str,
    directory: &Path,
) -> Result<(String, ModelProvider), CodexConfigError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CodexConfigError::MissingName);
    }
    let main_error = match read_config(directory.join("config.toml")) {
        Ok(config) => {
            if config.model_provider.as_deref() == Some(name)
                || config.model_providers.contains_key(name)
            {
                return provider_entry(config, name);
            }
            None
        }
        Err(error) => Some(error),
    };
    let Some(profile_path) = exact_profile_path(directory, name)? else {
        return Err(main_error.unwrap_or_else(|| CodexConfigError::NotFound(name.to_owned())));
    };
    let text = fs::read_to_string(profile_path)
        .map_err(|_| CodexConfigError::ProfileUnreadable(name.to_owned()))?;
    let profile = toml::from_str::<CodexConfig>(&text)
        .map_err(|_| CodexConfigError::ProfileInvalid(name.to_owned()))?;
    let provider = profile
        .model_provider
        .as_deref()
        .filter(|provider| !provider.is_empty())
        .ok_or_else(|| CodexConfigError::ProfileMissingProvider(name.to_owned()))?
        .to_owned();
    provider_entry(profile, &provider)
}

fn read_config(path: PathBuf) -> Result<CodexConfig, CodexConfigError> {
    let text = fs::read_to_string(path).map_err(|_| CodexConfigError::Unreadable)?;
    toml::from_str(&text).map_err(|_| CodexConfigError::Invalid)
}

fn exact_profile_path(directory: &Path, name: &str) -> Result<Option<PathBuf>, CodexConfigError> {
    let expected = format!("{name}.config.toml");
    let entries = fs::read_dir(directory).map_err(|_| CodexConfigError::Unreadable)?;
    Ok(entries
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() == OsStr::new(&expected) && entry.path().is_file())
        .map(|entry| entry.path()))
}

fn provider_entry(
    mut config: CodexConfig,
    name: &str,
) -> Result<(String, ModelProvider), CodexConfigError> {
    let missing = || CodexConfigError::ProviderMissingBaseUrl(name.to_owned());
    let provider = config.model_providers.remove(name).ok_or_else(missing)?;
    let base = provider
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(missing)?
        .to_owned();
    Ok((base, provider))
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
    fn selected_provider_resolves_its_key_and_follows_dotenv_rotation() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("work.config.toml"),
            r#"
model_provider = "relay"
[model_providers.relay]
base_url = "https://relay.example/v1"
env_key = "OPENQUOTA_TEST_RELAY_KEY"
experimental_bearer_token = "fixture-fallback"
"#,
        )
        .unwrap();
        let (_, provider) = super::resolve_model_provider("work", dir.path()).unwrap();
        for value in ["first", "second"] {
            fs::write(
                dir.path().join(".env"),
                format!("SOURCE={value}\nOPENQUOTA_TEST_RELAY_KEY=${{SOURCE}}\n"),
            )
            .unwrap();
            assert_eq!(provider.api_key(dir.path()).as_deref(), Some(value));
        }
        fs::write(dir.path().join(".env"), "DIFFERENT_PROVIDER_KEY=unrelated").unwrap();
        assert_eq!(
            provider.api_key(dir.path()).as_deref(),
            Some("fixture-fallback")
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
