mod dotenv;

use std::path::{Path, PathBuf};

/// Read file overrides without modifying the process environment.
/// File aliases take precedence over all process aliases, including empty values.
pub fn proxy_value(names: &[&str]) -> Option<String> {
    let configured_home = std::env::var("CODEX_HOME").ok();
    let directory = codex_home(configured_home.as_deref(), dirs::home_dir());
    resolve_value(
        directory
            .as_deref()
            .map(|path| path.join(".env"))
            .as_deref(),
        names,
        |name| std::env::var(name).ok(),
    )
}

fn resolve_value(
    path: Option<&Path>,
    names: &[&str],
    process: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let names: Vec<_> = names
        .iter()
        .copied()
        .filter(|name| crate::provider_environment::is_proxy_variable(name))
        .collect();
    let mut values = path
        .and_then(|path| std::fs::File::open(path).ok())
        .map(|file| dotenv::load_proxy_values(file, &process))
        .unwrap_or_default();
    names
        .iter()
        .find_map(|name| values.remove(*name))
        .or_else(|| names.iter().find_map(|name| process(name)))
}

fn codex_home(configured: Option<&str>, home: Option<PathBuf>) -> Option<PathBuf> {
    match configured.filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = Path::new(value);
            path.is_dir().then(|| path.canonicalize().ok()).flatten()
        }
        None => home.map(|path| path.join(".codex")),
    }
}

#[cfg(test)]
fn load_from(path: &Path, mut set: impl FnMut(&str, &str)) {
    if let Ok(file) = std::fs::File::open(path) {
        for (key, value) in dotenv::load_proxy_values(file, |name| std::env::var(name).ok()) {
            set(&key, &value);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs};

    use super::{codex_home, load_from};

    #[test]
    fn file_values_precede_process_aliases_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".env");
        let before = std::env::var_os("HTTPS_PROXY");
        fs::write(&path, "https_proxy=http://file:8080\nNO_PROXY=\n").unwrap();
        let process = |_: &str| Some("process-value".to_owned());
        assert_eq!(
            super::resolve_value(Some(&path), &["HTTPS_PROXY", "https_proxy"], process).as_deref(),
            Some("http://file:8080")
        );
        assert_eq!(
            super::resolve_value(Some(&path), &["NO_PROXY"], process).as_deref(),
            Some("")
        );
        assert_eq!(
            super::resolve_value(Some(&path), &["ALL_PROXY"], process).as_deref(),
            Some("process-value")
        );
        assert_eq!(
            super::resolve_value(None, &["HTTPS_PROXY"], process).as_deref(),
            Some("process-value")
        );
        assert_eq!(std::env::var_os("HTTPS_PROXY"), before);
    }

    #[test]
    fn retains_only_proxies_after_expanding_helper_variables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".env");
        fs::write(&path, "PORT=7890\nPORT=7892\nHTTPS_PROXY=http://127.0.0.1:${PORT}\nNO_PROXY=\nAPI_KEY=secret\nCODEX_HOME=/ignored\n").unwrap();
        let mut values = HashMap::new();
        load_from(&path, |key, value| {
            values.insert(key.to_owned(), value.to_owned());
        });
        assert_eq!(values.len(), 2);
        assert_eq!(values["HTTPS_PROXY"], "http://127.0.0.1:7892");
        assert_eq!(values["NO_PROXY"], "");
        for name in ["PORT", "API_KEY", "CODEX_HOME", "PATH", "Http_Proxy"] {
            assert_eq!(
                super::resolve_value(Some(&path), &[name], |_| Some("inherited".into())),
                None
            );
        }
    }

    #[test]
    fn codex_home_override_requires_an_existing_directory() {
        let directory = tempfile::tempdir().unwrap();
        let home = Some(directory.path().to_path_buf());
        assert_eq!(
            codex_home(None, home.clone()),
            Some(directory.path().join(".codex"))
        );
        assert_eq!(
            codex_home(Some(""), home.clone()),
            codex_home(None, home.clone())
        );
        assert_eq!(
            codex_home(directory.path().to_str(), home.clone()),
            Some(directory.path().canonicalize().unwrap())
        );
        assert!(codex_home(directory.path().join("missing").to_str(), home.clone()).is_none());
        let file = directory.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(codex_home(file.to_str(), home).is_none());
        load_from(&directory.path().join("missing.env"), |_, _| {
            panic!("missing file must be ignored")
        });
    }
}
