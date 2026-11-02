//! Opt-in encryption of the saved session file with AES-256-GCM.
//!
//! The key comes from a file named by [`KEY_FILE_ENV`], so it can live in a
//! Docker secret instead of the data directory. An encrypted file is
//! `MAGIC | version | nonce | ciphertext | tag`. The version byte and the
//! normalized login are authenticated, so a file cannot be moved to another
//! account's path or read as a different format without the check failing.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;

use crate::session::AuthSessionError;

/// Environment variable naming the file that holds the session key.
pub const KEY_FILE_ENV: &str = "TCPM_COOKIE_KEY_FILE";

const MAGIC: &[u8; 8] = b"TMR-SESS";
const FORMAT_VERSION: u8 = 1;
const TAG_LEN: usize = 16;
const KEY_HEX_LEN: usize = 64;
const MAX_KEY_FILE_BYTES: u64 = 1024;

#[derive(Debug, Error)]
pub enum SessionKeyError {
    #[error("TCPM_COOKIE_KEY_FILE: the key file cannot be read ({0})")]
    Unreadable(io::ErrorKind),
    #[error(
        "TCPM_COOKIE_KEY_FILE must name a file holding exactly 64 hexadecimal characters \
         (a 256-bit key), optionally surrounded by whitespace"
    )]
    Malformed,
}

/// A 256-bit session key. It never appears in `Debug` output or errors.
pub struct SessionKey {
    key: LessSafeKey,
    readable_by_others: bool,
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionKey(<redacted>)")
    }
}

impl SessionKey {
    /// Reads the key named by [`KEY_FILE_ENV`]. An unset or blank variable
    /// means the session file is not encrypted.
    pub fn from_env() -> Result<Option<Self>, SessionKeyError> {
        Self::from_env_value(std::env::var_os(KEY_FILE_ENV).as_deref())
    }

    fn from_env_value(value: Option<&OsStr>) -> Result<Option<Self>, SessionKeyError> {
        match value {
            Some(path) if !path.to_string_lossy().trim().is_empty() => {
                Self::from_file(Path::new(path)).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self, SessionKeyError> {
        let file =
            fs::File::open(path).map_err(|error| SessionKeyError::Unreadable(error.kind()))?;
        let metadata = file
            .metadata()
            .map_err(|error| SessionKeyError::Unreadable(error.kind()))?;
        let mut contents = Vec::new();
        (&file)
            .take(MAX_KEY_FILE_BYTES + 1)
            .read_to_end(&mut contents)
            .map_err(|error| SessionKeyError::Unreadable(error.kind()))?;
        if contents.len() as u64 > MAX_KEY_FILE_BYTES {
            return Err(SessionKeyError::Malformed);
        }
        let mut key = Self::from_hex(&contents)?;
        key.readable_by_others = readable_by_others(&metadata);
        Ok(key)
    }

    fn from_hex(contents: &[u8]) -> Result<Self, SessionKeyError> {
        let text = std::str::from_utf8(contents)
            .map_err(|_| SessionKeyError::Malformed)?
            // Windows editors and shells may save a byte order mark.
            .trim_start_matches('\u{feff}')
            .trim();
        if text.len() != KEY_HEX_LEN || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SessionKeyError::Malformed);
        }
        let bytes = text
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| (hex_value(pair[0]) << 4) | hex_value(pair[1]))
            .collect::<Vec<_>>();
        let key = UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| SessionKeyError::Malformed)?;
        Ok(Self {
            key: LessSafeKey::new(key),
            readable_by_others: false,
        })
    }

    /// Whether the key file was readable by group or others when it was read.
    /// Always false off Unix.
    #[must_use]
    pub fn file_readable_by_others(&self) -> bool {
        self.readable_by_others
    }

    pub(crate) fn seal(
        &self,
        username: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, AuthSessionError> {
        let mut nonce = [0_u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| AuthSessionError::Encrypt)?;
        let mut sealed = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(associated_data(username)),
                &mut sealed,
            )
            .map_err(|_| AuthSessionError::Encrypt)?;
        let mut output = Vec::with_capacity(MAGIC.len() + 1 + NONCE_LEN + sealed.len());
        output.extend_from_slice(MAGIC);
        output.push(FORMAT_VERSION);
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&sealed);
        Ok(output)
    }

    pub(crate) fn open(&self, username: &str, bytes: &[u8]) -> Result<Vec<u8>, AuthSessionError> {
        let rest = bytes
            .strip_prefix(MAGIC.as_slice())
            .ok_or(AuthSessionError::Decrypt)?;
        let (&version, rest) = rest.split_first().ok_or(AuthSessionError::Decrypt)?;
        if version != FORMAT_VERSION {
            return Err(AuthSessionError::UnsupportedEncryptionVersion(version));
        }
        if rest.len() < NONCE_LEN + TAG_LEN {
            return Err(AuthSessionError::Decrypt);
        }
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        let nonce =
            Nonce::try_assume_unique_for_key(nonce).map_err(|_| AuthSessionError::Decrypt)?;
        let mut buffer = sealed.to_vec();
        self.key
            .open_in_place(nonce, Aad::from(associated_data(username)), &mut buffer)
            .map(|plaintext| plaintext.to_vec())
            .map_err(|_| AuthSessionError::Decrypt)
    }
}

/// Whether `bytes` start with the encrypted-session header. Plaintext sessions
/// are JSON and can never start with it.
#[must_use]
pub fn is_encrypted_session(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

fn associated_data(username: &str) -> Vec<u8> {
    let mut data = vec![FORMAT_VERSION];
    data.extend_from_slice(username.as_bytes());
    data
}

// Callers pass only validated hexadecimal digits.
fn hex_value(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'a'..=b'f' => digit - b'a' + 10,
        _ => digit - b'A' + 10,
    }
}

#[cfg(unix)]
fn readable_by_others(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o077 != 0
}

#[cfg(not(unix))]
fn readable_by_others(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
#[path = "../tests/unit/encryption_tests.rs"]
mod tests;
