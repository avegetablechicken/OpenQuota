use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use super::OpenCodeError;

const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct OpenCodePaths {
    data_directory: PathBuf,
    #[cfg(test)]
    test_major_version: Option<u64>,
}

impl OpenCodePaths {
    pub(crate) fn new() -> Self {
        Self {
            data_directory: data_directory(|name| std::env::var(name).ok(), &home_directory()),
            #[cfg(test)]
            test_major_version: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_data_directory(data_directory: PathBuf) -> Self {
        Self {
            data_directory,
            test_major_version: Some(1),
        }
    }

    pub(crate) fn database_files(&self) -> Result<Vec<PathBuf>, OpenCodeError> {
        database_files(&self.data_directory)
    }

    pub(crate) fn go_api_key(&self) -> Result<Option<Zeroizing<String>>, OpenCodeError> {
        #[cfg(test)]
        let major = self
            .test_major_version
            .map(Ok)
            .unwrap_or_else(installed_major_version)?;
        #[cfg(not(test))]
        let major = installed_major_version()?;
        if major >= 2 {
            self.database_go_api_key()
        } else {
            self.legacy_go_api_key()
        }
    }

    fn database_go_api_key(&self) -> Result<Option<Zeroizing<String>>, OpenCodeError> {
        let database_path = self.data_directory.join("opencode.db");
        if !database_path
            .try_exists()
            .map_err(|_| OpenCodeError::CredentialsUnreadable)?
        {
            return Ok(None);
        }
        let connection =
            Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|_| OpenCodeError::CredentialsUnreadable)?;
        // OpenCode >= 2 uses only the database, even if its table/key is missing.
        let content = connection
            .query_row(
                "SELECT value FROM credential WHERE integration_id = 'opencode-go' \
             ORDER BY active DESC, time_created DESC, id DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0).map(Zeroizing::new),
            )
            .optional()
            .map_err(|_| OpenCodeError::CredentialsUnreadable)?;
        let Some(content) = content else {
            return Ok(None);
        };
        if content.len() as u64 > MAX_AUTH_FILE_BYTES {
            return Err(OpenCodeError::CredentialsUnreadable);
        }
        let mut value = serde_json::from_str::<Value>(&content)
            .map_err(|_| OpenCodeError::CredentialsUnreadable)?;
        if value.get("type").and_then(Value::as_str) != Some("key") {
            return Ok(None);
        }
        Ok(take_key(&mut value))
    }

    fn legacy_go_api_key(&self) -> Result<Option<Zeroizing<String>>, OpenCodeError> {
        let path = self.data_directory.join("auth.json");
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(OpenCodeError::CredentialsUnreadable),
        };
        let metadata = file
            .metadata()
            .map_err(|_| OpenCodeError::CredentialsUnreadable)?;
        if !metadata.is_file() || metadata.len() > MAX_AUTH_FILE_BYTES {
            return Err(OpenCodeError::CredentialsUnreadable);
        }
        let mut content = Zeroizing::new(String::with_capacity(metadata.len() as usize));
        file.take(MAX_AUTH_FILE_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|_| OpenCodeError::CredentialsUnreadable)?;
        if content.len() as u64 > MAX_AUTH_FILE_BYTES {
            return Err(OpenCodeError::CredentialsUnreadable);
        }
        let mut value = serde_json::from_str::<Value>(&content)
            .map_err(|_| OpenCodeError::CredentialsUnreadable)?;
        let credentials = value
            .as_object_mut()
            .ok_or(OpenCodeError::CredentialsUnreadable)?;
        let Some(entry) = credentials.get_mut("opencode-go") else {
            return Ok(None);
        };
        Ok(take_key(entry))
    }
}

fn installed_major_version() -> Result<u64, OpenCodeError> {
    let executable = if cfg!(windows) {
        "opencode.exe"
    } else {
        "opencode"
    };
    let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    // GUI launches may not inherit the terminal's PATH.
    let home = home_directory();
    directories.extend([home.join(".opencode/bin"), home.join(".local/bin")]);
    #[cfg(unix)]
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ]);
    let binary = directories
        .into_iter()
        .map(|dir| dir.join(executable))
        .find(|path| path.is_file())
        .ok_or(OpenCodeError::CredentialsUnreadable)?;
    let output = crate::child_process::output_with_timeout(
        crate::child_process::background_command(
            binary
                .to_str()
                .ok_or(OpenCodeError::CredentialsUnreadable)?,
        )
        .arg("--version"),
        Duration::from_secs(5),
    )
    .map_err(|_| OpenCodeError::CredentialsUnreadable)?;
    if !output.status.success() {
        return Err(OpenCodeError::CredentialsUnreadable);
    }
    parse_major_version(
        std::str::from_utf8(&output.stdout).map_err(|_| OpenCodeError::CredentialsUnreadable)?,
    )
    .ok_or(OpenCodeError::CredentialsUnreadable)
}

fn parse_major_version(version: &str) -> Option<u64> {
    let version = version.trim();
    let version = version.strip_prefix("opencode ").unwrap_or(version).trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    let mut parts = version.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.split(['-', '+']).next()?;
    patch.parse::<u64>().ok()?;
    Some(major)
}

fn take_key(entry: &mut Value) -> Option<Zeroizing<String>> {
    let key_value = entry.as_object_mut()?.get_mut("key")?;
    let key = key_value
        .as_str()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(|key| Zeroizing::new(key.to_owned()));
    if let Value::String(value) = key_value {
        value.zeroize();
    }
    key
}

fn data_directory(environment: impl Fn(&str) -> Option<String>, home_directory: &Path) -> PathBuf {
    if let Some(configured) = environment("OPENCODE_DATA_DIR").and_then(non_empty) {
        return expand_home(&configured, home_directory);
    }
    if let Some(xdg_data_home) = environment("XDG_DATA_HOME").and_then(non_empty) {
        return expand_home(&xdg_data_home, home_directory).join("opencode");
    }
    home_directory.join(".local").join("share").join("opencode")
}

fn database_files(data_directory: &Path) -> Result<Vec<PathBuf>, OpenCodeError> {
    let entries = match fs::read_dir(data_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(OpenCodeError::DataDirectoryUnreadable),
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| OpenCodeError::DataDirectoryUnreadable)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("opencode") && name.ends_with(".db") {
            paths.push(entry.path());
        }
    }
    paths.sort();

    let mut identities = BTreeSet::new();
    paths.retain(|path| {
        let identity = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        identities.insert(identity)
    });
    Ok(paths)
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn expand_home(value: &str, home_directory: &Path) -> PathBuf {
    if value == "~" {
        return home_directory.to_path_buf();
    }
    if let Some(relative) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return home_directory.join(relative);
    }
    PathBuf::from(value)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs};

    use tempfile::tempdir;

    use super::{data_directory, database_files, OpenCodePaths, MAX_AUTH_FILE_BYTES};
    use crate::providers::opencode::OpenCodeError;

    #[test]
    fn data_directory_precedence_and_home_expansion_are_cross_platform() {
        let home = std::path::Path::new("/users/me");
        let environment = HashMap::from([
            ("OPENCODE_DATA_DIR", " ~/custom/opencode "),
            ("XDG_DATA_HOME", "/xdg/data"),
        ]);
        assert_eq!(
            data_directory(|name| environment.get(name).map(ToString::to_string), home),
            home.join("custom/opencode")
        );

        let xdg = HashMap::from([("XDG_DATA_HOME", "~/xdg")]);
        assert_eq!(
            data_directory(|name| xdg.get(name).map(ToString::to_string), home),
            home.join("xdg/opencode")
        );
        assert_eq!(
            data_directory(|_| None, home),
            home.join(".local/share/opencode")
        );
    }

    #[test]
    fn database_discovery_is_sorted_deduplicated_and_channel_aware() {
        let directory = tempdir().unwrap();
        for name in [
            "opencode-next.db",
            "opencode.db",
            "opencode.db-wal",
            "other.db",
        ] {
            fs::write(directory.path().join(name), "").unwrap();
        }

        let paths = database_files(directory.path()).unwrap();
        assert_eq!(
            paths,
            [
                directory.path().join("opencode-next.db"),
                directory.path().join("opencode.db"),
            ]
        );
    }

    #[test]
    fn missing_directory_is_absence_but_unreadable_path_is_typed() {
        let directory = tempdir().unwrap();
        assert!(database_files(&directory.path().join("missing"))
            .unwrap()
            .is_empty());

        let file = directory.path().join("not-a-directory");
        fs::write(&file, "").unwrap();
        assert_eq!(
            database_files(&file).unwrap_err(),
            OpenCodeError::DataDirectoryUnreadable
        );
    }

    #[test]
    fn parses_opencode_cli_version_output() {
        for (output, expected) in [
            ("opencode v2.0.30\n", 2),
            ("opencode v2.0.0\r\n", 2),
            ("opencode v0.9.0\n", 0),
            ("opencode v1.9.0\n", 1),
            ("2.0.30\n", 2),
            ("v2.0.30\n", 2),
        ] {
            assert_eq!(super::parse_major_version(output), Some(expected));
        }
    }

    #[test]
    fn credential_storage_is_selected_only_by_major_version() {
        let directory = tempdir().unwrap();
        let mut paths = OpenCodePaths::for_data_directory(directory.path().to_path_buf());
        fs::write(
            directory.path().join("auth.json"),
            r#"{"opencode-go":{"type":"api","key":"legacy"}}"#,
        )
        .unwrap();
        for version in [
            "0.9.0",
            "1.99.0",
            "2.0.0",
            "2.0.30",
            "3.0.0",
            "v2.0.0-beta.1",
        ] {
            let major = super::parse_major_version(version).unwrap();
            paths.test_major_version = Some(major);
            if major >= 2 {
                assert!(paths.go_api_key().unwrap().is_none());
            } else {
                assert_eq!(paths.go_api_key().unwrap().unwrap().as_str(), "legacy");
            }
        }
        for version in ["", "unknown", "2", "2.x.0"] {
            assert!(super::parse_major_version(version).is_none());
        }
        // Old versions do not inspect the database, even when it is unusable.
        fs::write(directory.path().join("opencode.db"), "corrupt").unwrap();
        paths.test_major_version = Some(0);
        assert_eq!(paths.go_api_key().unwrap().unwrap().as_str(), "legacy");
    }

    #[test]
    fn database_credentials_override_legacy_and_follow_opencode_selection() {
        let directory = tempdir().unwrap();
        let mut paths = OpenCodePaths::for_data_directory(directory.path().to_path_buf());
        paths.test_major_version = Some(2);
        fs::write(
            directory.path().join("auth.json"),
            r#"{"opencode-go":{"type":"api","key":"stale"}}"#,
        )
        .unwrap();
        let connection = rusqlite::Connection::open(directory.path().join("opencode.db")).unwrap();
        // Version 2 must not fall back even if migration has not created the table.
        assert_eq!(
            paths.go_api_key().unwrap_err(),
            OpenCodeError::CredentialsUnreadable
        );
        connection
            .execute_batch(
                "CREATE TABLE credential (
            id TEXT PRIMARY KEY, integration_id TEXT, value TEXT NOT NULL,
            active INTEGER, time_created INTEGER NOT NULL
        );",
            )
            .unwrap();
        assert!(paths.go_api_key().unwrap().is_none());
        for (id, integration, key, active, created) in [
            ("a", "opencode-go", " selected ", Some(1), 1),
            ("b", "opencode-go", "newer", Some(0), 2),
            ("c", "opencode-go", "tie-break", None, 2),
            ("d", "other", "unrelated", Some(1), 3),
        ] {
            connection
                .execute(
                    "INSERT INTO credential VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        id,
                        integration,
                        serde_json::json!({"type":"key", "key":key}).to_string(),
                        active,
                        created
                    ],
                )
                .unwrap();
        }
        assert_eq!(paths.go_api_key().unwrap().unwrap().as_str(), "selected");
        connection
            .execute("UPDATE credential SET active = NULL", [])
            .unwrap();
        assert_eq!(paths.go_api_key().unwrap().unwrap().as_str(), "tie-break");
        // Updates must be picked up on the next refresh, without caching a key.
        connection
            .execute(
                "UPDATE credential SET value = ?1 WHERE id = 'c'",
                [r#"{"type":"key","key":"rotated"}"#],
            )
            .unwrap();
        assert_eq!(paths.go_api_key().unwrap().unwrap().as_str(), "rotated");
        connection
            .execute(
                "DELETE FROM credential WHERE integration_id = 'opencode-go'",
                [],
            )
            .unwrap();
        assert!(paths.go_api_key().unwrap().is_none());
    }

    #[test]
    fn invalid_database_credentials_never_fall_back_to_stale_keys() {
        let directory = tempdir().unwrap();
        let mut paths = OpenCodePaths::for_data_directory(directory.path().to_path_buf());
        paths.test_major_version = Some(2);
        fs::write(
            directory.path().join("auth.json"),
            r#"{"opencode-go":{"type":"api","key":"stale"}}"#,
        )
        .unwrap();
        let database = directory.path().join("opencode.db");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE credential (
            id TEXT PRIMARY KEY, integration_id TEXT, value TEXT NOT NULL,
            active INTEGER, time_created INTEGER NOT NULL
        ); INSERT INTO credential VALUES ('a', 'opencode-go', 'invalid', 1, 1);",
            )
            .unwrap();
        assert_eq!(
            paths.go_api_key().unwrap_err(),
            OpenCodeError::CredentialsUnreadable
        );
        for value in [
            r#"{"type":"oauth","key":"wrong-type"}"#,
            r#"{"type":"key","key":" "}"#,
        ] {
            connection
                .execute("UPDATE credential SET value = ?1", [value])
                .unwrap();
            assert!(paths.go_api_key().unwrap().is_none());
        }
        drop(connection);
        fs::write(database, "corrupt").unwrap();
        assert_eq!(
            paths.go_api_key().unwrap_err(),
            OpenCodeError::CredentialsUnreadable
        );
    }

    #[test]
    fn go_key_loader_tolerates_unrelated_entries_and_rejects_bad_storage() {
        let directory = tempdir().unwrap();
        let paths = OpenCodePaths::for_data_directory(directory.path().to_path_buf());
        assert_eq!(paths.go_api_key().unwrap(), None);

        fs::write(
            directory.path().join("auth.json"),
            r#"{"$schema":"v1","other":[],"opencode-go":{"type":"api","key":"  key-1  "}}"#,
        )
        .unwrap();
        let key = paths.go_api_key().unwrap().unwrap();
        assert_eq!(key.as_str(), "key-1");

        fs::write(directory.path().join("auth.json"), "not json").unwrap();
        assert_eq!(
            paths.go_api_key().unwrap_err(),
            OpenCodeError::CredentialsUnreadable
        );

        fs::write(directory.path().join("auth.json"), "[]").unwrap();
        assert_eq!(
            paths.go_api_key().unwrap_err(),
            OpenCodeError::CredentialsUnreadable
        );

        fs::write(
            directory.path().join("auth.json"),
            vec![b' '; MAX_AUTH_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            paths.go_api_key().unwrap_err(),
            OpenCodeError::CredentialsUnreadable
        );
    }
}
