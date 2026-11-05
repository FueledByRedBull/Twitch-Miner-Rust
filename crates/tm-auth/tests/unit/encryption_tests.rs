use super::*;
use crate::cookies::{cookie_file_path, CookieStore};
use crate::session::{AuthSession, AuthSessionError};

const KEY_A: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const KEY_B: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
const TOKEN: &str = "plaintext-auth-token-marker";
const REFRESH: &str = "plaintext-refresh-token-marker";

fn session_key(hex: &str) -> SessionKey {
    SessionKey::from_hex(hex.as_bytes()).unwrap()
}

fn session(username: &str) -> AuthSession {
    let mut session = AuthSession::new(username, CookieStore::new());
    session.set_auth_token(TOKEN);
    session.set_refresh_token(REFRESH);
    session.set_user_id("user-1");
    session
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

fn session_paths(dir: &Path, username: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let primary = cookie_file_path(dir, username).unwrap();
    let backup = primary.with_extension("json.bak");
    (primary, backup)
}

fn error_texts(error: &AuthSessionError) -> String {
    format!("{error} {error:?}")
}

#[test]
fn key_file_accepts_exactly_64_hex_characters_with_surrounding_whitespace() {
    for text in [
        KEY_A.to_string(),
        KEY_A.to_uppercase(),
        format!("\n  {KEY_A}\r\n"),
        format!("\t{KEY_B} "),
        format!("\u{feff}{KEY_A}\r\n"),
    ] {
        assert!(SessionKey::from_hex(text.as_bytes()).is_ok(), "{text:?}");
    }
}

#[test]
fn malformed_key_files_name_the_variable_and_never_echo_the_content() {
    let secret = "s3cr3t-not-a-key-but-private";
    let mut cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"   \n".to_vec(),
        KEY_A.as_bytes()[..63].to_vec(),
        format!("{KEY_A}0").into_bytes(),
        format!("{}{KEY_A}", &KEY_A[..32]).into_bytes(),
        format!("{}g", &KEY_A[..63]).into_bytes(),
        format!("{KEY_A} {KEY_B}").into_bytes(),
        format!("0x{}", &KEY_A[..62]).into_bytes(),
        secret.as_bytes().to_vec(),
        vec![0xff, 0xfe, 0x00, 0x41],
        vec![b'a'; 100_000],
    ];
    cases.push(format!("{secret}{secret}{secret}{secret}").into_bytes());
    for content in cases {
        let error = SessionKey::from_hex(&content).unwrap_err();
        let text = format!("{error} {error:?}");
        assert!(matches!(error, SessionKeyError::Malformed));
        assert!(text.contains(KEY_FILE_ENV), "{text}");
        assert!(!text.contains(secret), "{text}");
        assert!(!text.contains(&KEY_A[..16]), "{text}");
    }
}

#[test]
fn key_files_are_read_from_disk_and_unreadable_files_name_the_variable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.key");
    fs::write(&path, format!("{KEY_A}\n")).unwrap();
    assert!(SessionKey::from_file(&path).is_ok());

    let oversized = dir.path().join("oversized.key");
    fs::write(&oversized, format!("{KEY_A}{}", " ".repeat(5_000))).unwrap();
    assert!(matches!(
        SessionKey::from_file(&oversized),
        Err(SessionKeyError::Malformed)
    ));

    let error = SessionKey::from_file(&dir.path().join("missing.key")).unwrap_err();
    assert!(matches!(error, SessionKeyError::Unreadable(_)));
    assert!(error.to_string().contains(KEY_FILE_ENV));
}

#[test]
fn an_unset_or_blank_variable_means_no_encryption() {
    assert!(SessionKey::from_env_value(None).unwrap().is_none());
    assert!(SessionKey::from_env_value(Some(OsStr::new("")))
        .unwrap()
        .is_none());
    assert!(SessionKey::from_env_value(Some(OsStr::new("  ")))
        .unwrap()
        .is_none());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.key");
    fs::write(&path, KEY_A).unwrap();
    assert!(SessionKey::from_env_value(Some(path.as_os_str()))
        .unwrap()
        .is_some());
    assert!(SessionKey::from_env_value(Some(OsStr::new("no-such-key-file"))).is_err());
}

#[cfg(unix)]
#[test]
fn key_files_readable_by_group_or_others_are_reported() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.key");
    fs::write(&path, KEY_A).unwrap();
    for (mode, shared) in [(0o600, false), (0o400, false), (0o640, true), (0o604, true)] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            SessionKey::from_file(&path)
                .unwrap()
                .file_readable_by_others(),
            shared,
            "{mode:o}"
        );
    }
}

#[test]
fn the_error_message_variable_name_matches_the_constant() {
    assert_eq!(KEY_FILE_ENV, "TCPM_COOKIE_KEY_FILE");
    for error in [
        AuthSessionError::EncryptedWithoutKey,
        AuthSessionError::Decrypt,
    ] {
        assert!(error.to_string().contains(KEY_FILE_ENV));
    }
}

#[test]
fn sealed_bytes_use_the_documented_layout_and_round_trip() {
    let key = session_key(KEY_A);
    let plaintext = br#"{"auth-token":{"value":"x"}}"#;
    let sealed = key.seal("alice", plaintext).unwrap();

    assert!(sealed.starts_with(b"TMR-SESS"));
    assert_eq!(sealed[8], 1);
    assert_eq!(sealed.len(), 8 + 1 + 12 + plaintext.len() + 16);
    assert!(is_encrypted_session(&sealed));
    assert!(!contains(&sealed, "auth-token"));
    assert_eq!(key.open("alice", &sealed).unwrap(), plaintext);
    assert!(!is_encrypted_session(plaintext));
}

#[test]
fn every_seal_uses_a_fresh_nonce() {
    let key = session_key(KEY_A);
    let first = key.seal("alice", b"same plaintext").unwrap();
    let second = key.seal("alice", b"same plaintext").unwrap();
    assert_ne!(first[9..21], second[9..21]);
    assert_ne!(first[21..], second[21..]);
}

#[test]
fn the_login_and_the_key_are_bound_to_the_ciphertext() {
    let sealed = session_key(KEY_A).seal("alice", b"payload").unwrap();
    assert!(matches!(
        session_key(KEY_A).open("bob", &sealed),
        Err(AuthSessionError::Decrypt)
    ));
    assert!(matches!(
        session_key(KEY_B).open("alice", &sealed),
        Err(AuthSessionError::Decrypt)
    ));
}

#[test]
fn any_modified_truncated_or_extended_file_fails_to_open() {
    let key = session_key(KEY_A);
    let sealed = key.seal("alice", b"payload").unwrap();
    for index in 0..sealed.len() {
        let mut tampered = sealed.clone();
        tampered[index] ^= 0x01;
        assert!(key.open("alice", &tampered).is_err(), "byte {index}");
    }
    for length in 0..sealed.len() {
        assert!(key.open("alice", &sealed[..length]).is_err(), "{length}");
    }
    let mut extended = sealed.clone();
    extended.push(0);
    assert!(matches!(
        key.open("alice", &extended),
        Err(AuthSessionError::Decrypt)
    ));

    let mut future = sealed;
    future[8] = 2;
    assert!(matches!(
        key.open("alice", &future),
        Err(AuthSessionError::UnsupportedEncryptionVersion(2))
    ));
}

#[test]
fn no_error_or_debug_text_carries_key_or_session_material() {
    let key = session_key(KEY_A);
    let sealed = key.seal("alice", TOKEN.as_bytes()).unwrap();
    let mut tampered = sealed.clone();
    tampered[30] ^= 1;
    let mut texts = vec![format!("{key:?}")];
    for error in [
        key.open("alice", &tampered).unwrap_err(),
        session_key(KEY_B).open("alice", &sealed).unwrap_err(),
        AuthSessionError::EncryptedWithoutKey,
        AuthSessionError::Encrypt,
        AuthSessionError::NotEncrypted,
    ] {
        texts.push(error_texts(&error));
    }
    for text in texts {
        assert!(!text.contains(KEY_A), "{text}");
        assert!(!text.contains(&KEY_A[..32]), "{text}");
        assert!(!text.contains(TOKEN), "{text}");
    }
}

#[test]
fn without_a_key_plaintext_sessions_behave_as_before() {
    let dir = tempfile::tempdir().unwrap();
    session("alice").save_to_dir(dir.path(), None).unwrap();

    let (primary, backup) = session_paths(dir.path(), "alice");
    let bytes = fs::read(&primary).unwrap();
    assert!(contains(&bytes, TOKEN));
    assert!(!backup.exists());
    let loaded = AuthSession::load_from_dir(dir.path(), "alice", None).unwrap();
    assert_eq!(loaded.auth_token(), Some(TOKEN));
}

#[test]
fn an_encrypted_session_round_trips_and_leaves_no_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let mut first = session("alice");
    first.save_to_dir(dir.path(), Some(&key)).unwrap();
    first.set_auth_token("replacement-token-marker");
    first.save_to_dir(dir.path(), Some(&key)).unwrap();

    let (primary, backup) = session_paths(dir.path(), "alice");
    for path in [&primary, &backup] {
        let bytes = fs::read(path).unwrap();
        assert!(is_encrypted_session(&bytes), "{}", path.display());
        assert!(!contains(&bytes, TOKEN));
        assert!(!contains(&bytes, REFRESH));
        assert!(!contains(&bytes, "replacement-token-marker"));
    }
    let (loaded, recovered) =
        AuthSession::load_from_dir_with_backup(dir.path(), "ALICE", Some(&key)).unwrap();
    assert!(!recovered);
    assert_eq!(loaded.auth_token(), Some("replacement-token-marker"));
    assert_eq!(loaded.refresh_token(), Some(REFRESH));
    let previous = AuthSession::from_bytes("alice", &fs::read(&backup).unwrap(), Some(&key));
    assert_eq!(previous.unwrap().auth_token(), Some(TOKEN));
}

#[test]
fn every_write_of_the_session_uses_a_fresh_nonce() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let (primary, backup) = session_paths(dir.path(), "alice");
    let mut nonces = Vec::new();
    let mut current = session("alice");
    for round in 0..4 {
        current.set_auth_token(format!("token-{round}"));
        current.save_to_dir(dir.path(), Some(&key)).unwrap();
        nonces.push(fs::read(&primary).unwrap()[9..21].to_vec());
    }
    let unique = nonces.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), nonces.len());
    // The backup is the previous file as it was written, not a second encryption.
    assert_eq!(fs::read(&backup).unwrap()[9..21], nonces[2][..]);
}

#[test]
fn saving_an_unchanged_encrypted_session_does_not_rewrite_it() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let session = session("alice");
    session.save_to_dir(dir.path(), Some(&key)).unwrap();
    let (primary, backup) = session_paths(dir.path(), "alice");
    let before = fs::read(&primary).unwrap();

    session.save_to_dir(dir.path(), Some(&key)).unwrap();

    assert_eq!(fs::read(&primary).unwrap(), before);
    assert!(!backup.exists());
}

#[test]
fn a_plaintext_session_is_read_with_a_key_and_encrypted_on_the_next_save() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let mut stored = session("alice");
    stored.save_to_dir(dir.path(), None).unwrap();
    stored.set_auth_token("second-token-marker");
    stored.save_to_dir(dir.path(), None).unwrap();
    let (primary, backup) = session_paths(dir.path(), "alice");
    assert!(contains(
        &fs::read(&primary).unwrap(),
        "second-token-marker"
    ));
    assert!(contains(&fs::read(&backup).unwrap(), TOKEN));

    let (loaded, recovered) =
        AuthSession::load_from_dir_with_backup(dir.path(), "alice", Some(&key)).unwrap();
    assert!(!recovered);
    assert_eq!(loaded.auth_token(), Some("second-token-marker"));
    assert!(contains(
        &fs::read(&primary).unwrap(),
        "second-token-marker"
    ));

    // Even an unchanged session is rewritten, so the migration needs no change.
    loaded.save_to_dir(dir.path(), Some(&key)).unwrap();
    for path in [&primary, &backup] {
        let bytes = fs::read(path).unwrap();
        assert!(is_encrypted_session(&bytes), "{}", path.display());
        assert!(!contains(&bytes, "token-marker"));
        assert!(!contains(&bytes, TOKEN));
    }
    let previous = AuthSession::load_from_dir(dir.path(), "alice", Some(&key)).unwrap();
    assert_eq!(previous.auth_token(), Some("second-token-marker"));
    let backup_session = AuthSession::from_bytes("alice", &fs::read(&backup).unwrap(), Some(&key));
    assert_eq!(backup_session.unwrap().auth_token(), Some(TOKEN));
    assert_ne!(
        fs::read(&primary).unwrap()[9..21],
        fs::read(&backup).unwrap()[9..21]
    );
}

#[test]
fn a_stale_plaintext_backup_is_encrypted_even_when_the_session_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let session = session("alice");
    session.save_to_dir(dir.path(), Some(&key)).unwrap();
    let (_, backup) = session_paths(dir.path(), "alice");
    fs::write(
        &backup,
        br#"{"auth-token":{"value":"old-plaintext-marker"}}"#,
    )
    .unwrap();

    session.save_to_dir(dir.path(), Some(&key)).unwrap();

    let bytes = fs::read(&backup).unwrap();
    assert!(is_encrypted_session(&bytes));
    assert!(!contains(&bytes, "old-plaintext-marker"));
}

#[test]
fn restoring_from_a_plaintext_backup_with_a_key_leaves_no_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let mut stored = session("alice");
    stored.save_to_dir(dir.path(), None).unwrap();
    stored.set_auth_token("second-token-marker");
    stored.save_to_dir(dir.path(), None).unwrap();
    let (primary, backup) = session_paths(dir.path(), "alice");
    fs::write(&primary, b"{not-json").unwrap();

    let (recovered, from_backup) =
        AuthSession::load_from_dir_with_backup(dir.path(), "alice", Some(&key)).unwrap();
    assert!(from_backup);
    recovered.restore_to_dir(dir.path(), Some(&key)).unwrap();

    for path in [&primary, &backup] {
        let bytes = fs::read(path).unwrap();
        assert!(is_encrypted_session(&bytes), "{}", path.display());
        assert!(!contains(&bytes, TOKEN));
    }
    let reloaded = AuthSession::load_from_dir(dir.path(), "alice", Some(&key)).unwrap();
    assert_eq!(reloaded.auth_token(), Some(TOKEN));
}

#[test]
fn a_wrong_key_or_modified_file_is_a_hard_error_and_never_uses_the_backup() {
    let dir = tempfile::tempdir().unwrap();
    let right = session_key(KEY_A);
    let wrong = session_key(KEY_B);
    let mut stored = session("alice");
    stored.save_to_dir(dir.path(), Some(&right)).unwrap();
    stored.set_auth_token("second-token");
    stored.save_to_dir(dir.path(), Some(&right)).unwrap();
    let (primary, backup) = session_paths(dir.path(), "alice");
    // A valid plaintext backup must not tempt the loader into recovering.
    fs::write(
        &backup,
        br#"{"auth-token":{"value":"plaintext-backup-token"}}"#,
    )
    .unwrap();
    let encrypted = fs::read(&primary).unwrap();
    let backup_bytes = fs::read(&backup).unwrap();

    let mut modified = encrypted.clone();
    let last = modified.len() - 1;
    modified[last] ^= 0x80;
    for (contents, key) in [(encrypted.clone(), &wrong), (modified, &right)] {
        fs::write(&primary, &contents).unwrap();
        assert!(matches!(
            AuthSession::load_from_dir(dir.path(), "alice", Some(key)),
            Err(AuthSessionError::Decrypt)
        ));
        assert!(matches!(
            AuthSession::load_from_dir_with_backup(dir.path(), "alice", Some(key)),
            Err(AuthSessionError::Decrypt)
        ));
        // Saving, restoring, and importing would overwrite it: they must refuse.
        assert!(matches!(
            session("alice").save_to_dir(dir.path(), Some(key)),
            Err(AuthSessionError::Decrypt)
        ));
        assert!(matches!(
            session("alice").restore_to_dir(dir.path(), Some(key)),
            Err(AuthSessionError::Decrypt)
        ));
        assert_eq!(fs::read(&primary).unwrap(), contents);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
    }
}

#[test]
fn an_encrypted_file_without_a_key_is_a_hard_error_and_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    session("alice")
        .save_to_dir(dir.path(), Some(&key))
        .unwrap();
    let (primary, backup) = session_paths(dir.path(), "alice");
    fs::write(
        &backup,
        br#"{"auth-token":{"value":"plaintext-backup-token"}}"#,
    )
    .unwrap();
    let encrypted = fs::read(&primary).unwrap();

    let error = AuthSession::load_from_dir(dir.path(), "alice", None).unwrap_err();
    assert!(matches!(error, AuthSessionError::EncryptedWithoutKey));
    assert!(error.to_string().contains(KEY_FILE_ENV));
    assert!(matches!(
        AuthSession::load_from_dir_with_backup(dir.path(), "alice", None),
        Err(AuthSessionError::EncryptedWithoutKey)
    ));
    assert!(matches!(
        session("alice").save_to_dir(dir.path(), None),
        Err(AuthSessionError::EncryptedWithoutKey)
    ));
    assert!(matches!(
        session("alice").restore_to_dir(dir.path(), None),
        Err(AuthSessionError::EncryptedWithoutKey)
    ));
    assert_eq!(fs::read(&primary).unwrap(), encrypted);
    assert!(contains(
        &fs::read(&backup).unwrap(),
        "plaintext-backup-token"
    ));
}

#[test]
fn a_missing_session_file_with_an_unreadable_backup_is_also_a_hard_error() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let mut stored = session("alice");
    stored.save_to_dir(dir.path(), Some(&key)).unwrap();
    stored.set_auth_token("second-token");
    stored.save_to_dir(dir.path(), Some(&key)).unwrap();
    let (primary, _) = session_paths(dir.path(), "alice");
    fs::remove_file(&primary).unwrap();

    assert!(matches!(
        AuthSession::load_from_dir_with_backup(dir.path(), "alice", Some(&session_key(KEY_B))),
        Err(AuthSessionError::Decrypt)
    ));
    assert!(matches!(
        AuthSession::load_from_dir_with_backup(dir.path(), "alice", None),
        Err(AuthSessionError::EncryptedWithoutKey)
    ));
    let (recovered, from_backup) =
        AuthSession::load_from_dir_with_backup(dir.path(), "alice", Some(&key)).unwrap();
    assert!(from_backup);
    assert_eq!(recovered.auth_token(), Some(TOKEN));
}

#[test]
fn an_encrypted_file_cannot_be_moved_to_another_account() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    session("alice")
        .save_to_dir(dir.path(), Some(&key))
        .unwrap();
    let (alice, _) = session_paths(dir.path(), "alice");
    let (bob, _) = session_paths(dir.path(), "bob");
    fs::copy(&alice, &bob).unwrap();

    assert!(matches!(
        AuthSession::load_from_dir(dir.path(), "bob", Some(&key)),
        Err(AuthSessionError::Decrypt)
    ));
    assert!(AuthSession::load_from_dir(dir.path(), "alice", Some(&key)).is_ok());
}

#[cfg(unix)]
#[test]
fn encrypted_session_files_use_private_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let mut stored = session("alice");
    stored.save_to_dir(dir.path(), None).unwrap();
    stored.set_auth_token("second-token");
    stored.save_to_dir(dir.path(), None).unwrap();
    let (primary, backup) = session_paths(dir.path(), "alice");
    for path in [&primary, &backup] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    stored.save_to_dir(dir.path(), Some(&key)).unwrap();
    for path in [&primary, &backup] {
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
    }
    stored.set_auth_token("third-token");
    stored.save_to_dir(dir.path(), Some(&key)).unwrap();
    for path in [&primary, &backup] {
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
    }
}

#[test]
fn decrypting_rewrites_the_file_and_its_backup_as_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    let mut stored = session("alice");
    stored.save_to_dir(dir.path(), Some(&key)).unwrap();
    stored.set_auth_token("second-token-marker");
    stored.save_to_dir(dir.path(), Some(&key)).unwrap();

    AuthSession::decrypt_session_files(dir.path(), "ALICE", &key).unwrap();

    let (primary, backup) = session_paths(dir.path(), "alice");
    assert!(contains(
        &fs::read(&primary).unwrap(),
        "second-token-marker"
    ));
    assert!(contains(&fs::read(&backup).unwrap(), TOKEN));
    let loaded = AuthSession::load_from_dir(dir.path(), "alice", None).unwrap();
    assert_eq!(loaded.auth_token(), Some("second-token-marker"));
    assert_eq!(loaded.refresh_token(), Some(REFRESH));
    let previous = AuthSession::from_bytes("alice", &fs::read(&backup).unwrap(), None);
    assert_eq!(previous.unwrap().auth_token(), Some(TOKEN));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&primary, &backup] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}

#[test]
fn decrypting_works_without_a_backup_and_leaves_no_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    session("alice")
        .save_to_dir(dir.path(), Some(&key))
        .unwrap();

    AuthSession::decrypt_session_files(dir.path(), "alice", &key).unwrap();

    let (primary, backup) = session_paths(dir.path(), "alice");
    assert!(!backup.exists());
    assert!(contains(&fs::read(&primary).unwrap(), TOKEN));
    let names = fs::read_dir(primary.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, ["alice.json"]);
}

#[test]
fn decrypting_refuses_plaintext_missing_and_unreadable_files_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let key = session_key(KEY_A);
    assert!(matches!(
        AuthSession::decrypt_session_files(dir.path(), "alice", &key),
        Err(AuthSessionError::Io(error)) if error.kind() == io::ErrorKind::NotFound
    ));

    session("alice").save_to_dir(dir.path(), None).unwrap();
    let (primary, backup) = session_paths(dir.path(), "alice");
    let plaintext = fs::read(&primary).unwrap();
    assert!(matches!(
        AuthSession::decrypt_session_files(dir.path(), "alice", &key),
        Err(AuthSessionError::NotEncrypted)
    ));
    assert_eq!(fs::read(&primary).unwrap(), plaintext);

    // The file decrypts but the backup does not: neither may change.
    let other = session_key(KEY_B);
    let mut stored = session("alice");
    stored.save_to_dir(dir.path(), Some(&key)).unwrap();
    stored.set_auth_token("second-token");
    stored.save_to_dir(dir.path(), Some(&key)).unwrap();
    let foreign = other.seal("alice", b"{}").unwrap();
    fs::write(&backup, &foreign).unwrap();
    let encrypted = fs::read(&primary).unwrap();
    assert!(matches!(
        AuthSession::decrypt_session_files(dir.path(), "alice", &key),
        Err(AuthSessionError::Decrypt)
    ));
    assert!(matches!(
        AuthSession::decrypt_session_files(dir.path(), "alice", &other),
        Err(AuthSessionError::Decrypt)
    ));
    assert_eq!(fs::read(&primary).unwrap(), encrypted);
    assert_eq!(fs::read(&backup).unwrap(), foreign);
}
