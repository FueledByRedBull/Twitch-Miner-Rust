use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::cookies::{
    cookie_file_path, decode_cookie_store, encode_cookie_store, ensure_session_cookies,
    loaded_cookie_store, normalize_username, normalized_cookie_domain, CookieStore,
    CookieStoreError,
};
use crate::encryption::{is_encrypted_session, SessionKey};

const SPECIAL_COOKIE_DOMAINS: [&str; 2] = ["twitch.tv", "id.twitch.tv"];
const REFRESH_TOKEN_COOKIE_DOMAIN: &str = "refresh-token.invalid";

#[derive(Debug, Error)]
pub enum AuthSessionError {
    #[error("cookie io error: {0}")]
    Io(#[from] io::Error),
    #[error("cookie store error: {0}")]
    CookieStore(#[from] CookieStoreError),
    #[error("cookie encode error: {0}")]
    CookieEncode(#[from] serde_json::Error),
    #[error(
        "the saved session is encrypted; set TCPM_COOKIE_KEY_FILE to the file holding its key"
    )]
    EncryptedWithoutKey,
    #[error(
        "the saved session could not be decrypted: the key in TCPM_COOKIE_KEY_FILE is wrong, \
         the file belongs to another account, or it was modified; the file was left unchanged"
    )]
    Decrypt,
    #[error("the saved session uses encrypted format version {0}, which this release cannot read")]
    UnsupportedEncryptionVersion(u8),
    #[error("the session could not be encrypted")]
    Encrypt,
    #[error("the saved session is not encrypted")]
    NotEncrypted,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AuthSession {
    username: String,
    store: CookieStore,
    scopes: BTreeSet<String>,
}

impl fmt::Debug for AuthSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthSession")
            .field("username", &self.username)
            .field("store", &self.store)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl AuthSession {
    #[must_use]
    pub fn new(username: impl Into<String>, store: CookieStore) -> Self {
        Self {
            // Preserve non-ASCII input so the path validator can reject it;
            // Unicode case folding can otherwise turn invalid input such as
            // U+212A into a valid ASCII cookie filename.
            username: username.into().trim().to_ascii_lowercase(),
            store,
            scopes: BTreeSet::new(),
        }
    }

    /// Decodes session file contents. Plaintext is always accepted; encrypted
    /// contents need `key` and the login they were written for.
    pub fn from_bytes(
        username: impl Into<String>,
        bytes: &[u8],
        key: Option<&SessionKey>,
    ) -> Result<Self, AuthSessionError> {
        let username = username.into().trim().to_ascii_lowercase();
        let plaintext = open_session_bytes(&username, bytes, key)?;
        Ok(Self::new(username, decode_cookie_store(&plaintext)?))
    }

    pub fn load_from_dir(
        base_dir: impl AsRef<Path>,
        username: impl Into<String>,
        key: Option<&SessionKey>,
    ) -> Result<Self, AuthSessionError> {
        let username = normalize_username(&username.into())?;
        let bytes = fs::read(cookie_file_path(base_dir, &username)?)?;
        Self::from_bytes(username, &bytes, key)
    }

    /// Loads the session, falling back to the backup only when the file is
    /// missing or is not a valid session. An encrypted file that cannot be
    /// decrypted is never treated as invalid, so it is never replaced.
    pub fn load_from_dir_with_backup(
        base_dir: impl AsRef<Path>,
        username: impl Into<String>,
        key: Option<&SessionKey>,
    ) -> Result<(Self, bool), AuthSessionError> {
        let base_dir = base_dir.as_ref();
        let username = normalize_username(&username.into())?;
        match Self::load_from_dir(base_dir, &username, key) {
            Ok(session) => Ok((session, false)),
            Err(primary_error) if session_file_is_missing_or_invalid(&primary_error) => {
                let backup = backup_file_path(&cookie_file_path(base_dir, &username)?);
                match fs::read(backup)
                    .map_err(AuthSessionError::Io)
                    .and_then(|bytes| Self::from_bytes(&username, &bytes, key))
                {
                    Ok(session) => Ok((session, true)),
                    Err(backup_error) if session_file_is_missing_or_invalid(&backup_error) => {
                        Err(primary_error)
                    }
                    Err(backup_error) => Err(backup_error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Rewrites the session file without touching the backup, which is the
    /// source of a session recovered from it. With a key, the file is written
    /// encrypted and a plaintext backup is encrypted in place.
    pub fn restore_to_dir(
        &self,
        base_dir: impl AsRef<Path>,
        key: Option<&SessionKey>,
    ) -> Result<(), AuthSessionError> {
        let path = cookie_file_path(base_dir, &self.username)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(current) = read_if_exists(&path)? {
            open_session_bytes(&self.username, &current, key)?;
        }
        let payload = encode_cookie_store(&self.store)?;
        atomic_write_cookie_file(
            &path,
            &seal_for_disk(&self.username, payload.as_bytes(), key)?,
        )?;
        encrypt_plaintext_backup(&path, &self.username, key)
    }

    /// Saves the session, keeping the previous version as the backup. A file
    /// this process cannot read (encrypted without a key, or under a different
    /// key) is never overwritten. With a key, a plaintext session file and its
    /// plaintext backup are both replaced by encrypted ones.
    pub fn save_to_dir(
        &self,
        base_dir: impl AsRef<Path>,
        key: Option<&SessionKey>,
    ) -> Result<(), AuthSessionError> {
        let path = cookie_file_path(base_dir, &self.username)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = encode_cookie_store(&self.store)?;
        let current = read_if_exists(&path)?;
        if let Some(current) = &current {
            if open_session_bytes(&self.username, current, key)?.as_ref() == payload.as_bytes() {
                if key.is_some() && !is_encrypted_session(current) {
                    // Only the format changes, so the backup stays the previous version.
                    atomic_write_cookie_file(
                        &path,
                        &seal_for_disk(&self.username, payload.as_bytes(), key)?,
                    )?;
                } else {
                    set_private_cookie_permissions(&path)?;
                }
                return encrypt_plaintext_backup(&path, &self.username, key);
            }
        }
        if path.is_file() {
            let backup = backup_file_path(&path);
            match (&current, key) {
                (Some(current), Some(key)) if !is_encrypted_session(current) => {
                    let sealed = key.seal(&self.username, current)?;
                    atomic_write_cookie_file(&backup, &sealed)?;
                }
                _ => {
                    fs::copy(&path, &backup)?;
                    set_private_cookie_permissions(&backup)?;
                }
            }
        }
        atomic_write_cookie_file(
            &path,
            &seal_for_disk(&self.username, payload.as_bytes(), key)?,
        )?;
        encrypt_plaintext_backup(&path, &self.username, key)
    }

    /// Rewrites the encrypted session file and its backup as plaintext, for a
    /// rollback to a release that cannot read encrypted files. Nothing is
    /// written unless every existing file decrypts.
    pub fn decrypt_session_files(
        base_dir: impl AsRef<Path>,
        username: impl Into<String>,
        key: &SessionKey,
    ) -> Result<(), AuthSessionError> {
        let username = normalize_username(&username.into())?;
        let path = cookie_file_path(base_dir, &username)?;
        let current = fs::read(&path)?;
        if !is_encrypted_session(&current) {
            return Err(AuthSessionError::NotEncrypted);
        }
        let plaintext = key.open(&username, &current)?;
        let backup_path = backup_file_path(&path);
        let backup_plaintext = match fs::read(&backup_path) {
            Ok(bytes) if is_encrypted_session(&bytes) => Some(key.open(&username, &bytes)?),
            Ok(_) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        // The backup goes first: a failure between the two writes leaves the
        // session file encrypted, so the command can simply be run again.
        if let Some(backup_plaintext) = backup_plaintext {
            atomic_write_cookie_file(&backup_path, &backup_plaintext)?;
        }
        atomic_write_cookie_file(&path, &plaintext)?;
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
    pub fn refresh_token(&self) -> Option<&str> {
        self.store
            .get("refresh-token")
            .map(|cookie| cookie.value.trim())
            .filter(|value| !value.is_empty())
    }

    /// Stored beside the session cookies but never sent as one. Releases before
    /// 0.3.7 send any stored cookie whose domain matches the request, so a domain
    /// that matches no host keeps the token out of their requests after a rollback.
    pub fn set_refresh_token(&mut self, refresh_token: impl Into<String>) {
        self.store.insert(
            String::from("refresh-token"),
            crate::cookies::PersistedCookie {
                value: refresh_token.into(),
                path: None,
                domain: Some(String::from(REFRESH_TOKEN_COOKIE_DOMAIN)),
            },
        );
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

    pub fn set_scopes<I, S>(&mut self, scopes: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.scopes = scopes
            .into_iter()
            .map(|scope| scope.as_ref().trim().to_lowercase())
            .filter(|scope| !scope.is_empty())
            .collect();
    }

    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(&scope.trim().to_lowercase())
    }

    #[must_use]
    pub fn has_any_scope(&self, scopes: &[&str]) -> bool {
        scopes.iter().any(|scope| self.has_scope(scope))
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
            if matches!(name.as_str(), "auth-token" | "persistent" | "refresh-token") {
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

fn session_file_is_missing_or_invalid(error: &AuthSessionError) -> bool {
    matches!(
        error,
        AuthSessionError::Io(error) if error.kind() == io::ErrorKind::NotFound
    ) || matches!(error, AuthSessionError::CookieStore(_))
}

/// The file's contents, or `None` when it does not exist. Any other failure is an
/// error, so an unreadable file is never mistaken for a missing one.
fn read_if_exists(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn backup_file_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

/// Plaintext passes through; encrypted contents need the key.
fn open_session_bytes<'a>(
    username: &str,
    bytes: &'a [u8],
    key: Option<&SessionKey>,
) -> Result<Cow<'a, [u8]>, AuthSessionError> {
    if !is_encrypted_session(bytes) {
        return Ok(Cow::Borrowed(bytes));
    }
    let key = key.ok_or(AuthSessionError::EncryptedWithoutKey)?;
    key.open(username, bytes).map(Cow::Owned)
}

fn seal_for_disk(
    username: &str,
    plaintext: &[u8],
    key: Option<&SessionKey>,
) -> Result<Vec<u8>, AuthSessionError> {
    match key {
        Some(key) => key.seal(username, plaintext),
        None => Ok(plaintext.to_vec()),
    }
}

/// With a key, no plaintext copy of the session may stay behind in the backup.
fn encrypt_plaintext_backup(
    path: &Path,
    username: &str,
    key: Option<&SessionKey>,
) -> Result<(), AuthSessionError> {
    let Some(key) = key else {
        return Ok(());
    };
    let backup = backup_file_path(path);
    match fs::read(&backup) {
        Ok(bytes) if !is_encrypted_session(&bytes) => {
            atomic_write_cookie_file(&backup, &key.seal(username, &bytes)?)?;
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
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

    #[test]
    fn stored_refresh_token_matches_no_host_for_older_releases() {
        let mut session = sample_session();
        session.set_refresh_token("refresh-secret");
        assert_eq!(session.refresh_token(), Some("refresh-secret"));
        let domain = session.store["refresh-token"].domain.as_deref().unwrap();
        for host in [
            "www.twitch.tv",
            "gql.twitch.tv",
            "id.twitch.tv",
            "twitch.tv",
        ] {
            assert!(!host_matches_cookie_domain(host, domain));
        }
    }

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
        session.save_to_dir(dir.path(), None).unwrap();

        let loaded = AuthSession::load_from_dir(dir.path(), "ALICE", None).unwrap();
        assert_eq!(loaded.username(), "alice");
        assert_eq!(loaded.auth_token(), Some("token"));
        assert_eq!(loaded.user_id(), Some("user-1"));
        assert_eq!(loaded.store()["session"].value, "abc");
    }

    #[test]
    fn debug_redacts_session_credentials() {
        let session = AuthSession::new(
            "tester",
            CookieStore::from([(
                "auth-token".into(),
                PersistedCookie {
                    value: "secret-auth-token".into(),
                    path: None,
                    domain: None,
                },
            )]),
        );
        let output = format!("{session:?}");
        assert!(!output.contains("secret-auth-token"));
        assert!(output.contains("<redacted>"));
    }

    #[test]
    fn unsafe_username_is_rejected_before_auth_session_path_io() {
        let dir = tempfile::tempdir().unwrap();
        let session = AuthSession::new("../escape", CookieStore::new());

        assert!(matches!(
            session.save_to_dir(dir.path(), None),
            Err(AuthSessionError::CookieStore(
                CookieStoreError::InvalidUsername
            ))
        ));
        assert!(!dir.path().join("cookies").exists());
        assert!(matches!(
            AuthSession::load_from_dir(dir.path(), "C:\\escape", None),
            Err(AuthSessionError::CookieStore(
                CookieStoreError::InvalidUsername
            ))
        ));

        let kelvin_sign = char::from_u32(0x212a).unwrap().to_string();
        let unicode_session = AuthSession::new(kelvin_sign, CookieStore::new());
        assert!(matches!(
            unicode_session.save_to_dir(dir.path(), None),
            Err(AuthSessionError::CookieStore(
                CookieStoreError::InvalidUsername
            ))
        ));
        assert!(!dir.path().join("cookies").exists());
    }

    #[test]
    fn validated_scopes_are_normalized_and_remain_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = sample_session();
        session.set_scopes([" CHAT:READ ", "channel:read:predictions"]);

        assert!(session.has_scope("chat:read"));
        assert!(session.has_any_scope(&["channel:manage:predictions", "channel:read:predictions",]));
        session.save_to_dir(dir.path(), None).unwrap();

        let loaded = AuthSession::load_from_dir(dir.path(), "alice", None).unwrap();
        assert!(!loaded.has_scope("channel:read:predictions"));
    }

    #[test]
    fn cookie_atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = cookie_file_path(dir.path(), "tester").unwrap();
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
        session.save_to_dir(dir.path(), None).unwrap();
        session.set_auth_token("replacement-token");
        session.save_to_dir(dir.path(), None).unwrap();

        let path = cookie_file_path(dir.path(), session.username()).unwrap();
        let backup = AuthSession::from_bytes(
            "alice",
            &fs::read(path.with_extension("json.bak")).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(backup.auth_token(), Some("token"));
        assert_eq!(
            AuthSession::load_from_dir(dir.path(), "alice", None)
                .unwrap()
                .auth_token(),
            Some("replacement-token")
        );
    }

    #[test]
    fn backup_load_is_limited_to_missing_or_invalid_primary_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = sample_session();
        session.save_to_dir(dir.path(), None).unwrap();
        session.set_auth_token("replacement-token");
        session.save_to_dir(dir.path(), None).unwrap();
        let primary = cookie_file_path(dir.path(), "alice").unwrap();

        fs::write(&primary, b"{not-json").unwrap();
        let (backup, recovered) =
            AuthSession::load_from_dir_with_backup(dir.path(), "alice", None).unwrap();
        assert!(recovered);
        assert_eq!(backup.auth_token(), Some("token"));

        fs::remove_file(&primary).unwrap();
        fs::create_dir(&primary).unwrap();
        assert!(matches!(
            AuthSession::load_from_dir_with_backup(dir.path(), "alice", None),
            Err(AuthSessionError::Io(_))
        ));
    }

    #[test]
    fn invalid_primary_and_backup_report_the_primary_error() {
        let dir = tempfile::tempdir().unwrap();
        let primary = cookie_file_path(dir.path(), "tester").unwrap();
        fs::create_dir_all(primary.parent().unwrap()).unwrap();
        fs::write(&primary, b"{invalid-primary").unwrap();
        fs::write(primary.with_extension("json.bak"), b"{invalid-backup").unwrap();

        assert!(matches!(
            AuthSession::load_from_dir_with_backup(dir.path(), "tester", None),
            Err(AuthSessionError::CookieStore(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn session_cookie_file_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let session = sample_session();
        session.save_to_dir(dir.path(), None).unwrap();

        let path = cookie_file_path(dir.path(), session.username()).unwrap();
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_cookie_file_tightens_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let session = sample_session();
        session.save_to_dir(dir.path(), None).unwrap();

        let path = cookie_file_path(dir.path(), session.username()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        session.save_to_dir(dir.path(), None).unwrap();

        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn cookie_backup_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut session = sample_session();
        session.save_to_dir(dir.path(), None).unwrap();
        session.set_auth_token("replacement-token");
        session.save_to_dir(dir.path(), None).unwrap();

        let path = cookie_file_path(dir.path(), session.username())
            .unwrap()
            .with_extension("json.bak");
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
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
            None,
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
        let path = cookie_file_path(dir.path(), "tester").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"auth-token\":").unwrap();
        assert!(matches!(
            AuthSession::load_from_dir(dir.path(), "tester", None),
            Err(AuthSessionError::CookieStore(_))
        ));
    }
}
