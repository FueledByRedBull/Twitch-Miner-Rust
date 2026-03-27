use std::fs;
use std::path::{Path, PathBuf};

use tm_auth::{decode_cookie_store, AuthSession};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

#[test]
fn current_cookie_fixture_decodes_and_builds_headers() {
    let bytes = fs::read(fixture_path("cookies.current.json")).unwrap();
    let store = decode_cookie_store(&bytes).unwrap();
    assert!(store.contains_key("auth-token"));

    let session = AuthSession::from_bytes("fixture-user", &bytes).unwrap();
    assert_eq!(session.auth_token(), Some("token-123"));
    assert_eq!(session.user_id(), Some("user-123"));
    assert!(session
        .cookie_header_for_host("twitch.tv")
        .unwrap()
        .contains("auth-token=token-123"));
}

#[test]
fn legacy_cookie_fixture_decodes_and_roundtrips_to_session() {
    let bytes = fs::read(fixture_path("cookies.legacy.json")).unwrap();
    let session = AuthSession::from_bytes("fixture-user", &bytes).unwrap();
    assert_eq!(session.auth_token(), Some("token-123"));
    assert_eq!(session.user_id(), Some("user-123"));
    assert!(session
        .cookie_header_for_host("twitch.tv")
        .unwrap()
        .contains("auth-token=token-123"));
}

#[test]
fn missing_auth_token_fixture_loads_without_session_token() {
    let bytes = fs::read(fixture_path("cookies.missing_auth_token.json")).unwrap();
    let session = AuthSession::from_bytes("fixture-user", &bytes).unwrap();
    assert_eq!(session.auth_token(), None);
    assert_eq!(session.user_id(), Some("user-123"));
}
