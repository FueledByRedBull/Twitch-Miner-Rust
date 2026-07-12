use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use thiserror::Error;

use crate::cookies::{
    cookie_file_path, decode_cookie_store, encode_cookie_store, ensure_session_cookies,
    loaded_cookie_store, normalized_cookie_domain, CookieStore, CookieStoreError,
};

const SPECIAL_COOKIE_DOMAINS: [&str; 2] = ["twitch.tv", "id.twitch.tv"];

#[derive(Debug, Error)]
pub enum AuthSessionError {
    #[error("cookie io error: {0}")]
    Io(#[from] io::Error),
    #[error("cookie store error: {0}")]
    CookieStore(#[from] CookieStoreError),
    #[error("cookie encode error: {0}")]
    CookieEncode(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSession {
    username: String,
    store: CookieStore,
}

impl AuthSession {
    #[must_use]
    pub fn new(username: impl Into<String>, store: CookieStore) -> Self {
        Self {
            username: username.into().trim().to_lowercase(),
            store,
        }
    }

    pub fn from_bytes(username: impl Into<String>, bytes: &[u8]) -> Result<Self, AuthSessionError> {
        Ok(Self::new(username, decode_cookie_store(bytes)?))
    }

    pub fn load_from_dir(
        base_dir: impl AsRef<Path>,
        username: impl Into<String>,
    ) -> Result<Self, AuthSessionError> {
        let username = username.into().trim().to_lowercase();
        let bytes = fs::read(cookie_file_path(base_dir, &username))?;
        Self::from_bytes(username, &bytes)
    }

    pub fn save_to_dir(&self, base_dir: impl AsRef<Path>) -> Result<(), AuthSessionError> {
        let path = cookie_file_path(base_dir, &self.username);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = encode_cookie_store(&self.store)?;
        if fs::read(&path).is_ok_and(|current| current == payload.as_bytes()) {
            set_private_cookie_permissions(&path)?;
            return Ok(());
        }
        if path.is_file() {
            let backup = path.with_extension("json.bak");
            fs::copy(&path, &backup)?;
            set_private_cookie_permissions(&backup)?;
        }
        atomic_write_cookie_file(&path, payload.as_bytes())?;
        Ok(())
    }

    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    #[must_use]
    pub fn store(&self) -> &CookieStore {
        &self.store
    }

    #[must_use]
    pub fn auth_token(&self) -> Option<&str> {
        self.store
            .get("auth-token")
            .map(|cookie| cookie.value.trim())
            .filter(|value| !value.is_empty())
    }

    #[must_use]
    pub fn user_id(&self) -> Option<&str> {
        self.store
            .get("persistent")
            .map(|cookie| cookie.value.trim())
            .filter(|value| !value.is_empty())
    }

    pub fn set_auth_token(&mut self, auth_token: impl Into<String>) {
        let auth_token = auth_token.into();
        let user_id = self.user_id().map(str::to_string);
        ensure_session_cookies(&mut self.store, Some(&auth_token), user_id.as_deref());
        if let Some(cookie) = self.store.get_mut("auth-token") {
            cookie.value = auth_token;
        }
    }

    pub fn set_user_id(&mut self, user_id: impl Into<String>) {
        let user_id = user_id.into();
        let auth_token = self.auth_token().map(str::to_string);
        ensure_session_cookies(&mut self.store, auth_token.as_deref(), Some(&user_id));
        if let Some(cookie) = self.store.get_mut("persistent") {
            cookie.value = user_id;
        }
    }

    pub fn ensure_tokens(&mut self, auth_token: Option<&str>, user_id: Option<&str>) {
        ensure_session_cookies(&mut self.store, auth_token, user_id);
    }

    #[must_use]
    pub fn cookie_header_for_host(&self, host: &str) -> Option<String> {
        let request_host = host.trim().trim_start_matches('.').to_lowercase();
        if request_host.is_empty() {
            return None;
        }

        let loaded = loaded_cookie_store(&self.store);
        let mut pairs = Vec::new();

        for (name, cookie) in &self.store {
            let value = cookie.value.trim();
            if value.is_empty() {
                continue;
            }
            if matches!(name.as_str(), "auth-token" | "persistent") {
                continue;
            }

            let domain = normalized_cookie_domain(cookie.domain.as_deref());
            if host_matches_cookie_domain(&request_host, &domain) {
                pairs.push(format!("{name}={value}"));
            }
        }

        if SPECIAL_COOKIE_DOMAINS
            .iter()
            .any(|candidate| host_matches_cookie_domain(&request_host, candidate))
        {
            if let Some(auth_token) = loaded.auth_token {
                pairs.push(format!("auth-token={auth_token}"));
            }
            if let Some(user_id) = loaded.persistent {
                pairs.push(format!("persistent={user_id}"));
            }
        }

        (!pairs.is_empty()).then(|| pairs.join("; "))
    }
}

fn atomic_write_cookie_file(path: &Path, payload: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cookies.json");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = open_private_cookie_file(&temporary)?;
        file.write_all(payload)?;
        file.sync_all()?;
        match fs::rename(&temporary, path) {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(_) if path.is_file() => replace_windows_cookie_file(&temporary, path),
            Err(error) => Err(error),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_windows_cookie_file(temporary: &Path, path: &Path) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cookies.json");
    let replacement_backup =
        path.with_file_name(format!(".{file_name}.{}.replace.tmp", std::process::id()));

    fs::rename(path, &replacement_backup)?;
    if let Err(error) = fs::rename(temporary, path) {
        let _ = fs::rename(&replacement_backup, path);
        return Err(error);
    }
    let _ = fs::remove_file(replacement_backup);
    Ok(())
}

#[cfg(unix)]
fn open_private_cookie_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(unix)]
fn set_private_cookie_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_cookie_permissions(path: &Path) -> io::Result<()> {
    fs::metadata(path).map(|_| ())
}

#[cfg(not(unix))]
fn open_private_cookie_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
}

fn host_matches_cookie_domain(host: &str, domain: &str) -> bool {
    let normalized = domain.trim().trim_start_matches('.').to_lowercase();
    host == normalized || host.ends_with(&format!(".{normalized}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::{CookieStore, PersistedCookie};

    fn sample_session() -> AuthSession {
        AuthSession::new(
            "Alice",
            CookieStore::from([
                (
                    "auth-token".into(),
                    PersistedCookie {
                        value: "token".into(),
                        path: None,
                        domain: None,
                    },
                ),
                (
                    "persistent".into(),
                    PersistedCookie {
                        value: "user-1".into(),
                        path: None,
                        domain: None,
                    },
                ),
                (
                    "session".into(),
                    PersistedCookie {
                        value: "abc".into(),
                        path: Some("/".into()),
                        domain: Some(".twitch.tv".into()),
                    },
                ),
                (
                    "id-session".into(),
                    PersistedCookie {
                        value: "xyz".into(),
                        path: Some("/".into()),
                        domain: Some(".id.twitch.tv".into()),
                    },
                ),
            ]),
        )
    }

    #[test]
    fn host_cookie_header_matches_twitch_domains() {
        let session = sample_session();
        assert_eq!(
            session.cookie_header_for_host("twitch.tv").unwrap(),
            "session=abc; auth-token=token; persistent=user-1"
        );
        assert_eq!(
            session.cookie_header_for_host("gql.twitch.tv").unwrap(),
            "session=abc; auth-token=token; persistent=user-1"
        );
        assert_eq!(
            session.cookie_header_for_host("id.twitch.tv").unwrap(),
            "id-session=xyz; session=abc; auth-token=token; persistent=user-1"
        );
        assert_eq!(session.cookie_header_for_host("example.com"), None);
    }

    #[test]
    fn session_roundtrips_through_cookie_file() {
        let dir = tempfile::tempdir().unwrap();
        let session = sample_session();
        session.save_to_dir(dir.path()).unwrap();

        let loaded = AuthSession::load_from_dir(dir.path(), "ALICE").unwrap();
        assert_eq!(loaded.username(), "alice");
        assert_eq!(loaded.auth_token(), Some("token"));
        assert_eq!(loaded.user_id(), Some("user-1"));
        assert_eq!(loaded.store()["session"].value, "abc");
    }

    #[test]
    fn cookie_atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = cookie_file_path(dir.path(), "tester");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"old").unwrap();

        atomic_write_cookie_file(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        let temporary =
            path.with_file_name(format!(".{}.{}.tmp", "tester.json", std::process::id()));
        assert!(!temporary.exists());
    }

    #[test]
    fn changed_cookie_file_keeps_previous_version_as_backup() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = sample_session();
        session.save_to_dir(dir.path()).unwrap();
        session.set_auth_token("replacement-token");
        session.save_to_dir(dir.path()).unwrap();

        let path = cookie_file_path(dir.path(), session.username());
        let backup =
            AuthSession::from_bytes("alice", &fs::read(path.with_extension("json.bak")).unwrap())
                .unwrap();
        assert_eq!(backup.auth_token(), Some("token"));
        assert_eq!(
            AuthSession::load_from_dir(dir.path(), "alice")
                .unwrap()
                .auth_token(),
            Some("replacement-token")
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_cookie_file_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let session = sample_session();
        session.save_to_dir(dir.path()).unwrap();

        let path = cookie_file_path(dir.path(), session.username());
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_cookie_file_tightens_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let session = sample_session();
        session.save_to_dir(dir.path()).unwrap();

        let path = cookie_file_path(dir.path(), session.username());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        session.save_to_dir(dir.path()).unwrap();

        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn cookie_backup_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut session = sample_session();
        session.save_to_dir(dir.path()).unwrap();
        session.set_auth_token("replacement-token");
        session.save_to_dir(dir.path()).unwrap();

        let path = cookie_file_path(dir.path(), session.username()).with_extension("json.bak");
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn session_can_fill_missing_special_tokens() {
        let mut session = AuthSession::new(
            "tester",
            CookieStore::from([(
                "session".into(),
                PersistedCookie {
                    value: "abc".into(),
                    path: None,
                    domain: None,
                },
            )]),
        );

        session.ensure_tokens(Some("token"), Some("user-1"));
        assert_eq!(session.auth_token(), Some("token"));
        assert_eq!(session.user_id(), Some("user-1"));
    }

    #[test]
    fn session_token_setters_upsert_special_entries() {
        let mut session = AuthSession::new("tester", CookieStore::new());
        session.set_auth_token("token");
        session.set_user_id("user-1");
        assert_eq!(session.auth_token(), Some("token"));
        assert_eq!(session.user_id(), Some("user-1"));
    }

    #[test]
    fn from_bytes_supports_legacy_cookie_arrays() {
        let session = AuthSession::from_bytes(
            "tester",
            br#"[{"name":"auth-token","value":"token"},{"name":"session","value":"abc","domain":".twitch.tv","path":"/"}]"#,
        )
        .unwrap();
        assert_eq!(session.auth_token(), Some("token"));
        assert_eq!(
            session.cookie_header_for_host("twitch.tv").unwrap(),
            "session=abc; auth-token=token"
        );
    }

    #[test]
    fn corrupted_cookie_file_is_reported_without_fallback_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = cookie_file_path(dir.path(), "tester");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"auth-token\":").unwrap();
        assert!(matches!(
            AuthSession::load_from_dir(dir.path(), "tester"),
            Err(AuthSessionError::CookieStore(_))
        ));
    }
}
