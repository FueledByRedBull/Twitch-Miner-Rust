use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_COOKIE_DOMAIN: &str = ".twitch.tv";
pub const DEFAULT_COOKIE_PATH: &str = "/";
const MAX_TWITCH_USERNAME_LENGTH: usize = 25;

pub type CookieStore = BTreeMap<String, PersistedCookie>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PersistedCookie {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LegacyCookieRecord {
    name: String,
    value: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCookie {
    pub name: String,
    pub value: String,
    pub path: String,
    pub domain: String,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedCookieStore {
    pub auth_token: Option<String>,
    pub persistent: Option<String>,
    pub session_cookies: BTreeMap<String, Vec<SessionCookie>>,
}

#[derive(Debug, Error)]
pub enum CookieStoreError {
    #[error("invalid cookie store: {0}")]
    InvalidStore(#[from] serde_json::Error),
    #[error("invalid Twitch username for auth-session cookie path")]
    InvalidUsername,
}

/// Normalize and validate the account login used by auth-session persistence.
///
/// The login contract trims surrounding whitespace, lowercases the login,
/// accepts ASCII letters, digits, and underscores up to Twitch's 25-character
/// maximum, and rejects the built-in placeholder.  Cookie paths add a
/// platform-independent filename containment check so the normalized login
/// remains one ordinary path component on both Unix and Windows.
pub fn normalize_username(username: &str) -> Result<String, CookieStoreError> {
    let trimmed = username.trim();
    if !trimmed.is_ascii() {
        return Err(CookieStoreError::InvalidUsername);
    }
    let normalized = trimmed.to_ascii_lowercase();
    if normalized.is_empty() || normalized == "your-twitch-username" {
        return Err(CookieStoreError::InvalidUsername);
    }
    if normalized.len() > MAX_TWITCH_USERNAME_LENGTH
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(CookieStoreError::InvalidUsername);
    }

    let file_name = format!("{normalized}.json");
    if !is_safe_cookie_file_name(&file_name, &normalized) {
        return Err(CookieStoreError::InvalidUsername);
    }

    Ok(normalized)
}

pub fn cookie_file_path(
    base_dir: impl AsRef<Path>,
    username: &str,
) -> Result<PathBuf, CookieStoreError> {
    let username = normalize_username(username)?;
    Ok(base_dir
        .as_ref()
        .join("cookies")
        .join(format!("{username}.json")))
}

fn is_safe_cookie_file_name(file_name: &str, username: &str) -> bool {
    // Rust's path parser is host-platform-specific.  Explicitly reject both
    // separator spellings so a value accepted by a Unix test cannot become a
    // nested path when the same config is used on Windows (or vice versa).
    if file_name.is_empty()
        || file_name == "."
        || file_name == ".."
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains(':')
        || file_name.chars().any(char::is_control)
        || username.ends_with('.')
    {
        return false;
    }

    let mut components = Path::new(file_name).components();
    if !matches!(components.next(), Some(Component::Normal(component))
        if component == OsStr::new(file_name))
        || components.next().is_some()
    {
        return false;
    }

    !is_windows_reserved_component(username)
}

fn is_windows_reserved_component(username: &str) -> bool {
    // Windows strips trailing dots/spaces and treats device names as reserved
    // even when an extension is present (for example, CON.json).  Reject the
    // collision forms on every platform so a config can move safely between
    // Unix and Windows without changing its cookie identity.
    let candidate = username.trim_end_matches(['.', ' ']);
    let stem = candidate.split('.').next().unwrap_or_default();
    let stem = stem.to_ascii_lowercase();
    if matches!(stem.as_str(), "con" | "prn" | "aux" | "nul") {
        return true;
    }

    for prefix in ["com", "lpt"] {
        let Some(suffix) = stem.strip_prefix(prefix) else {
            continue;
        };
        if suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_digit() {
            return suffix.as_bytes()[0] != b'0';
        }
    }
    false
}

#[must_use]
pub fn cookies_dir(base_dir: impl AsRef<Path>) -> PathBuf {
    base_dir.as_ref().join("cookies")
}

pub fn decode_cookie_store(data: &[u8]) -> Result<CookieStore, CookieStoreError> {
    match serde_json::from_slice::<CookieStore>(data) {
        Ok(store) => Ok(store),
        Err(map_error) => {
            let legacy = serde_json::from_slice::<Vec<LegacyCookieRecord>>(data)?;
            let mut store = CookieStore::new();
            for record in legacy {
                if record.name.is_empty() {
                    continue;
                }
                store.insert(
                    record.name,
                    PersistedCookie {
                        value: record.value,
                        path: record.path,
                        domain: record.domain,
                    },
                );
            }
            if store.is_empty() {
                Err(map_error.into())
            } else {
                Ok(store)
            }
        }
    }
}

pub fn encode_cookie_store(store: &CookieStore) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(store)
}

pub fn ensure_session_cookies(
    store: &mut CookieStore,
    auth_token: Option<&str>,
    persistent: Option<&str>,
) {
    if !store.contains_key("auth-token") && auth_token.is_some_and(|token| !token.is_empty()) {
        store.insert(
            String::from("auth-token"),
            PersistedCookie {
                value: auth_token.unwrap_or_default().to_string(),
                path: None,
                domain: None,
            },
        );
    }
    if !store.contains_key("persistent") && persistent.is_some_and(|value| !value.is_empty()) {
        store.insert(
            String::from("persistent"),
            PersistedCookie {
                value: persistent.unwrap_or_default().to_string(),
                path: None,
                domain: None,
            },
        );
    }
}

#[must_use]
pub fn normalized_cookie_domain(domain: Option<&str>) -> String {
    let trimmed = domain.unwrap_or_default().trim();
    if trimmed.is_empty() {
        DEFAULT_COOKIE_DOMAIN.to_string()
    } else {
        trimmed.to_string()
    }
}

#[must_use]
pub fn normalized_cookie_path(path: Option<&str>) -> String {
    let trimmed = path.unwrap_or_default().trim();
    if trimmed.is_empty() {
        DEFAULT_COOKIE_PATH.to_string()
    } else {
        trimmed.to_string()
    }
}

#[must_use]
pub fn session_cookies_by_host(store: &CookieStore) -> BTreeMap<String, Vec<SessionCookie>> {
    let mut by_host: BTreeMap<String, Vec<SessionCookie>> = BTreeMap::new();
    for (name, cookie) in store {
        let value = cookie.value.trim();
        if value.is_empty() || name == "auth-token" || name == "persistent" {
            continue;
        }
        let domain = normalized_cookie_domain(cookie.domain.as_deref());
        let path = normalized_cookie_path(cookie.path.as_deref());
        let host = domain.trim_start_matches('.').to_string();
        by_host
            .entry(host.clone())
            .or_default()
            .push(SessionCookie {
                name: name.clone(),
                value: value.to_string(),
                path,
                domain,
                host,
            });
    }
    by_host
}

#[must_use]
pub fn loaded_cookie_store(store: &CookieStore) -> LoadedCookieStore {
    let auth_token = store
        .get("auth-token")
        .and_then(|cookie| (!cookie.value.is_empty()).then(|| cookie.value.clone()));
    let persistent = store
        .get("persistent")
        .and_then(|cookie| (!cookie.value.is_empty()).then(|| cookie.value.clone()));
    LoadedCookieStore {
        auth_token,
        persistent,
        session_cookies: session_cookies_by_host(store),
    }
}

#[must_use]
pub fn special_cookie_names(store: &CookieStore) -> BTreeSet<&str> {
    store
        .keys()
        .filter_map(|name| match name.as_str() {
            "auth-token" | "persistent" => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn sample_store() -> CookieStore {
        BTreeMap::from([
            (
                String::from("auth-token"),
                PersistedCookie {
                    value: String::from("token"),
                    path: None,
                    domain: None,
                },
            ),
            (
                String::from("persistent"),
                PersistedCookie {
                    value: String::from("user-id"),
                    path: None,
                    domain: None,
                },
            ),
            (
                String::from("session"),
                PersistedCookie {
                    value: String::from("abc"),
                    path: Some(String::from("/custom")),
                    domain: Some(String::from(".example.com")),
                },
            ),
        ])
    }

    #[test]
    fn decodes_current_map_format() {
        let json = br#"{
            "auth-token": {"value":"token"},
            "session": {"value":"abc","path":"/","domain":".example.com"}
        }"#;
        let store = decode_cookie_store(json).unwrap();
        assert_eq!(store["auth-token"].value, "token");
        assert_eq!(store["session"].path.as_deref(), Some("/"));
        assert_eq!(store["session"].domain.as_deref(), Some(".example.com"));
    }

    #[test]
    fn decodes_legacy_array_format() {
        let json = br#"[
            {"name":"auth-token","value":"token"},
            {"name":"session","value":"abc","path":"/p","domain":".example.com"},
            {"name":"","value":"skip"}
        ]"#;
        let store = decode_cookie_store(json).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store["auth-token"].value, "token");
        assert_eq!(store["session"].path.as_deref(), Some("/p"));
    }

    #[test]
    fn ensures_special_session_cookies_only_when_missing() {
        let mut store = CookieStore::from([(
            String::from("auth-token"),
            PersistedCookie {
                value: String::from("existing"),
                path: None,
                domain: None,
            },
        )]);
        ensure_session_cookies(&mut store, Some("new-token"), Some("user-id"));
        assert_eq!(store["auth-token"].value, "existing");
        assert_eq!(store["persistent"].value, "user-id");
    }

    #[test]
    fn keeps_existing_empty_special_entries() {
        let mut store = CookieStore::from([
            (
                String::from("auth-token"),
                PersistedCookie {
                    value: String::new(),
                    path: None,
                    domain: None,
                },
            ),
            (
                String::from("persistent"),
                PersistedCookie {
                    value: String::new(),
                    path: None,
                    domain: None,
                },
            ),
        ]);
        ensure_session_cookies(&mut store, Some("new-token"), Some("user-id"));
        assert_eq!(store["auth-token"].value, "");
        assert_eq!(store["persistent"].value, "");
    }

    #[test]
    fn normalizes_default_domain_and_path() {
        let cookie = PersistedCookie {
            value: String::from("abc"),
            path: None,
            domain: None,
        };
        let by_host = session_cookies_by_host(&BTreeMap::from([(String::from("session"), cookie)]));
        let cookie = &by_host["twitch.tv"][0];
        assert_eq!(cookie.domain, ".twitch.tv");
        assert_eq!(cookie.path, "/");
        assert_eq!(cookie.host, "twitch.tv");
    }

    #[test]
    fn loads_session_cookie_values() {
        let loaded = loaded_cookie_store(&sample_store());
        assert_eq!(loaded.auth_token.as_deref(), Some("token"));
        assert_eq!(loaded.persistent.as_deref(), Some("user-id"));
        let session = &loaded.session_cookies["example.com"][0];
        assert_eq!(session.name, "session");
        assert_eq!(session.domain, ".example.com");
        assert_eq!(session.path, "/custom");
    }

    #[test]
    fn cookie_paths_are_scoped_to_username() {
        let base = Path::new("C:/work");
        assert_eq!(
            cookie_file_path(base, "alice").unwrap(),
            PathBuf::from("C:/work/cookies/alice.json")
        );
        assert_eq!(cookies_dir(base), PathBuf::from("C:/work/cookies"));
    }

    #[test]
    fn cookie_paths_keep_the_existing_username_normalization() {
        let base = Path::new("C:/work");
        assert_eq!(normalize_username(" Alice ").unwrap(), "alice");
        assert_eq!(
            cookie_file_path(base, " Alice ").unwrap(),
            PathBuf::from("C:/work/cookies/alice.json")
        );
        assert!(matches!(
            normalize_username("your-twitch-username"),
            Err(CookieStoreError::InvalidUsername)
        ));
    }

    #[test]
    fn username_contract_accepts_short_logins_and_twenty_five_characters() {
        assert_eq!(normalize_username("A").unwrap(), "a");
        let longest = "a".repeat(MAX_TWITCH_USERNAME_LENGTH);
        assert_eq!(normalize_username(&longest).unwrap(), longest);
        assert!(matches!(
            normalize_username(&"a".repeat(MAX_TWITCH_USERNAME_LENGTH + 1)),
            Err(CookieStoreError::InvalidUsername)
        ));
    }

    #[test]
    fn username_contract_rejects_non_ascii_and_punctuation() {
        for username in [
            "alice bob",
            "alice-bob",
            "alice.name",
            "élise",
            "alice!",
            "a/b",
        ] {
            assert!(
                matches!(
                    normalize_username(username),
                    Err(CookieStoreError::InvalidUsername)
                ),
                "accepted invalid login {username:?}"
            );
        }
        let kelvin = char::from_u32(0x212A).map_or_else(String::new, |value| value.to_string());
        assert!(matches!(
            normalize_username(&kelvin),
            Err(CookieStoreError::InvalidUsername)
        ));
    }

    #[test]
    fn cookie_paths_reject_unix_and_windows_escape_forms() {
        for username in [
            "../alice",
            "..\\alice",
            "/tmp",
            "\\\\server\\share",
            "C:alice",
            "C:\\alice",
            "alice\0",
            "alice.",
        ] {
            assert!(
                matches!(
                    cookie_file_path("data", username),
                    Err(CookieStoreError::InvalidUsername)
                ),
                "accepted unsafe username {username:?}"
            );
        }
    }

    #[test]
    fn cookie_paths_reject_windows_device_name_collisions() {
        for username in [
            "con", "CON.txt", "prn", "aux", "nul", "com1", "com9", "lpt1", "LPT9",
        ] {
            assert!(
                matches!(
                    cookie_file_path("data", username),
                    Err(CookieStoreError::InvalidUsername)
                ),
                "accepted reserved username {username:?}"
            );
        }

        assert!(cookie_file_path("data", "com0").is_ok());
        assert!(cookie_file_path("data", "com10").is_ok());
    }

    #[test]
    fn encodes_pretty_json() {
        let json = encode_cookie_store(&sample_store()).unwrap();
        assert!(json.contains("\n  \"auth-token\""));
        assert!(json.contains("\"persistent\""));
    }

    #[test]
    fn can_roundtrip_through_file() {
        let mut tmp = NamedTempFile::new().unwrap();
        let encoded = encode_cookie_store(&sample_store()).unwrap();
        tmp.write_all(encoded.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let bytes = fs::read(tmp.path()).unwrap();
        let decoded = decode_cookie_store(&bytes).unwrap();
        assert_eq!(decoded["session"].domain.as_deref(), Some(".example.com"));
    }

    #[test]
    fn rejects_truncated_and_non_object_cookie_payloads() {
        assert!(matches!(
            decode_cookie_store(br#"{"auth-token":{"value":"unterminated"}"#),
            Err(CookieStoreError::InvalidStore(_))
        ));
        assert!(decode_cookie_store(br"[]").is_err());
    }
}
