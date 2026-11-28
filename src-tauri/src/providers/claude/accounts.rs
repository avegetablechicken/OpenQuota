use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use super::auth::ClaudeCredentialScope;
use crate::hashing::sha256_hex;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeStateFile {
    oauth_account: Option<ClaudeOAuthAccount>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeOAuthAccount {
    account_uuid: Option<String>,
    organization_uuid: Option<String>,
}

/// Returns the cache identity of the login Claude Code currently uses, without exposing it.
pub(super) fn identity_for_scope(scope: &ClaudeCredentialScope) -> Option<String> {
    let identity = match scope {
        ClaudeCredentialScope::Standard => {
            default_identity(&home_directory(), env_text("CLAUDE_CONFIG_DIR").as_deref())
        }
        #[cfg(test)]
        ClaudeCredentialScope::ConfigDir { path, .. } => fs::read(path.join(".claude.json"))
            .ok()
            .and_then(|bytes| parse_identity(&bytes)),
    };
    identity.map(|identity| identity_stamp(&identity))
}

fn default_identity(home: &Path, config: Option<&str>) -> Option<String> {
    let configured = config.map(str::trim).filter(|value| !value.is_empty());
    if configured.is_some_and(|value| value.contains(',')) {
        return None;
    }
    let root = configured
        .map(|value| expand_home(value, home))
        .unwrap_or_else(|| home.join(".claude"));
    let identity_path = if canonical(&root) == canonical(&home.join(".claude")) {
        home.join(".claude.json")
    } else {
        root.join(".claude.json")
    };
    fs::read(identity_path)
        .ok()
        .and_then(|bytes| parse_identity(&bytes))
}

fn parse_identity(bytes: &[u8]) -> Option<String> {
    let state: ClaudeStateFile = serde_json::from_slice(bytes).ok()?;
    let account = state.oauth_account?;
    let account_uuid = account
        .account_uuid
        .and_then(nonempty)?
        .to_ascii_lowercase();
    let organization_uuid = account
        .organization_uuid
        .and_then(nonempty)
        .map(|value| value.to_ascii_lowercase());
    Some(
        organization_uuid
            .map(|organization| format!("{account_uuid}|{organization}"))
            .unwrap_or(account_uuid),
    )
}

/// Checks the server-authenticated login against the opaque identity attached to a snapshot.
/// Older Claude state files omit the organization, so accept the account-only form as well.
pub(super) fn profile_matches_identity(profile: &serde_json::Value, expected: &str) -> bool {
    let Some(account) = profile
        .pointer("/account/uuid")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let Some(organization) = profile
        .pointer("/organization/uuid")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    expected == identity_stamp(&format!("{account}|{organization}"))
        || expected == identity_stamp(account)
}

fn identity_stamp(identity: &str) -> String {
    sha256_hex(identity.to_ascii_lowercase().as_bytes())
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        .map(|rest| home.join(rest))
        .unwrap_or_else(|| PathBuf::from(value))
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
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

    use tempfile::tempdir;

    use super::{default_identity, identity_stamp, parse_identity};

    #[test]
    fn identity_combines_account_and_organization_case_insensitively() {
        assert_eq!(
            parse_identity(
                br#"{"oauthAccount":{"accountUuid":"Account-A","organizationUuid":"Org-A"}}"#
            )
            .as_deref(),
            Some("account-a|org-a")
        );
        assert_eq!(
            parse_identity(br#"{"oauthAccount":{"accountUuid":"account-a"}}"#).as_deref(),
            Some("account-a")
        );
        assert!(parse_identity(br#"{"oauthAccount":{"accountUuid":" "}}"#).is_none());
        assert_eq!(
            identity_stamp("Account-A|Org-A"),
            identity_stamp("account-a|org-a")
        );
    }

    #[test]
    fn profile_identity_checks_account_and_organization_across_token_generations() {
        let profile = serde_json::json!({
            "account": {"uuid": " Account-A "},
            "organization": {"uuid": "Org-A"}
        });
        for owner in ["account-a|org-a", "account-a"] {
            assert!(super::profile_matches_identity(
                &profile,
                &identity_stamp(owner)
            ));
        }
        for owner in ["account-b|org-a", "account-a|org-b", "account-b"] {
            assert!(!super::profile_matches_identity(
                &profile,
                &identity_stamp(owner)
            ));
        }
        for missing in [
            serde_json::json!({}),
            serde_json::json!({"organization":{"uuid":"Org-A"}}),
        ] {
            assert!(!super::profile_matches_identity(
                &missing,
                &identity_stamp("account-a")
            ));
        }
    }

    #[test]
    fn default_identity_follows_the_active_configuration_home() {
        let directory = tempdir().unwrap();
        let home = directory.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"account-a"}}"#,
        )
        .unwrap();
        let configured = home.join("work");
        fs::create_dir_all(&configured).unwrap();
        fs::write(
            configured.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"account-b"}}"#,
        )
        .unwrap();

        assert_eq!(default_identity(home, None).as_deref(), Some("account-a"));
        assert_eq!(
            default_identity(home, Some("~/.claude")).as_deref(),
            Some("account-a")
        );
        assert_eq!(
            default_identity(home, Some("~/work")).as_deref(),
            Some("account-b")
        );
        assert!(default_identity(home, Some("~/work,~/.claude")).is_none());
    }
}
