use std::path::{Path, PathBuf};

/// Read file overrides without modifying the process environment.
/// File aliases take precedence over all process aliases, including empty values.
pub fn first_value(names: &[&str]) -> Option<String> {
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
    let mut values = std::collections::HashMap::new();
    if let Some(path) = path {
        load_from(path, |key, value| {
            if names.contains(&key) {
                values.insert(key.to_owned(), value.to_owned());
            }
        });
    }
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

fn load_from(path: &Path, mut set: impl FnMut(&str, &str)) {
    let Ok(iter) = dotenvy::from_path_iter(path) else {
        return;
    };
    for (key, value) in iter.flatten() {
        if !key.to_ascii_uppercase().starts_with("CODEX_") {
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
    fn dotenv_overrides_existing_values_and_filters_codex_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".env");
        fs::write(
            &path,
            "KEY=first\nexport KEY=last # comment\nEMPTY=\nCODEX_HOME=/ignored\ncodex_key=ignored\nINVALID LINE\nAFTER='quoted # value'\n",
        )
        .unwrap();
        let mut values = HashMap::from([("KEY".to_owned(), "process-value".to_owned())]);
        load_from(&path, |key, value| {
            values.insert(key.to_owned(), value.to_owned());
        });
        assert_eq!(values.get("KEY").unwrap(), "last");
        assert_eq!(values.get("EMPTY").unwrap(), "");
        assert_eq!(values.get("AFTER").unwrap(), "quoted # value");
        assert_eq!(values.len(), 3);
    }

    #[test]
    fn dotenv_uses_codex_parser_for_substitution_and_multiline_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".env");
        fs::write(
            &path,
            "OPENQUOTA_DOTENV_BASE=hello\nEXPANDED=${OPENQUOTA_DOTENV_BASE}-world\nLITERAL='${OPENQUOTA_DOTENV_BASE}'\nMULTILINE=\"first\nsecond\"\n",
        )
        .unwrap();
        let mut values = HashMap::new();
        load_from(&path, |key, value| {
            values.insert(key.to_owned(), value.to_owned());
        });
        assert_eq!(values.get("EXPANDED").unwrap(), "hello-world");
        assert_eq!(values.get("LITERAL").unwrap(), "${OPENQUOTA_DOTENV_BASE}");
        assert_eq!(values.get("MULTILINE").unwrap(), "first\nsecond");
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
