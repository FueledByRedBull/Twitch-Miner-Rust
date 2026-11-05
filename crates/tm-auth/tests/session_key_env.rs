//! The only test that touches the process environment; it has its own test
//! binary, so no other test can observe the variable.

use tm_auth::{SessionKey, KEY_FILE_ENV};

#[test]
fn the_key_is_read_from_the_file_named_by_the_environment_variable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.key");
    std::fs::write(&path, format!("{}\n", "ab".repeat(32))).unwrap();

    std::env::remove_var(KEY_FILE_ENV);
    assert!(SessionKey::from_env().unwrap().is_none());

    std::env::set_var(KEY_FILE_ENV, "   ");
    assert!(SessionKey::from_env().unwrap().is_none());

    std::env::set_var(KEY_FILE_ENV, &path);
    assert!(SessionKey::from_env().unwrap().is_some());

    std::fs::write(&path, "too short").unwrap();
    let error = SessionKey::from_env().unwrap_err().to_string();
    assert!(error.contains(KEY_FILE_ENV), "{error}");
    assert!(!error.contains("too short"), "{error}");

    std::env::remove_var(KEY_FILE_ENV);
}
