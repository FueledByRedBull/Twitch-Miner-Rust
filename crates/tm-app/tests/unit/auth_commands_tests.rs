#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use super::*;
use tm_auth::{AuthEndpoints, CookieStore};

const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const OTHER_KEY_HEX: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
const ACCESS_TOKEN: &str = "status-access-token-marker";
const REFRESH_TOKEN: &str = "status-refresh-token-marker";

fn response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn valid_response() -> String {
    response(
        "200 OK",
        r#"{"login":"tester","user_id":"user-1","client_id":"c","scopes":["chat:read","channel_read"],"expires_in":5400}"#,
    )
}

/// Answers one request per canned response, then stops. Returns the requests seen.
fn serve(
    responses: Vec<String>,
) -> (
    TwitchAuthClient,
    Arc<Mutex<Vec<String>>>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            recorded
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    let validate_url = format!("http://{address}/oauth2/validate");
    let client = TwitchAuthClient::with_client_and_endpoints(
        reqwest::Client::new(),
        AuthEndpoints {
            device_code_url: validate_url.clone(),
            token_url: validate_url.clone(),
            validate_url,
        },
    );
    (client, requests, handle)
}

fn unreachable_client() -> TwitchAuthClient {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let validate_url = format!("http://{address}/oauth2/validate");
    TwitchAuthClient::with_client_and_endpoints(
        reqwest::Client::new(),
        AuthEndpoints {
            device_code_url: validate_url.clone(),
            token_url: validate_url.clone(),
            validate_url,
        },
    )
}

fn key(hex: &str) -> SessionKey {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.key");
    fs::write(&path, hex).unwrap();
    SessionKey::from_file(&path).unwrap()
}

fn saved_session(dir: &Path, key: Option<&SessionKey>) {
    let mut session = AuthSession::new("tester", CookieStore::new());
    session.set_auth_token(ACCESS_TOKEN);
    session.set_refresh_token(REFRESH_TOKEN);
    session.save_to_dir(dir, key).unwrap();
}

fn session_path(dir: &Path) -> std::path::PathBuf {
    cookie_file_path(dir, "tester").unwrap()
}

fn assert_no_secrets(output: &str) {
    for secret in [ACCESS_TOKEN, REFRESH_TOKEN, KEY_HEX, OTHER_KEY_HEX] {
        assert!(!output.contains(secret), "{output}");
    }
}

fn outputs(report: &AuthStatusReport) -> (String, String) {
    (report.render(), serde_json::to_string(report).unwrap())
}

#[tokio::test]
async fn a_valid_session_reports_scopes_validity_age_and_refresh_token() {
    let dir = tempfile::tempdir().unwrap();
    saved_session(dir.path(), None);
    let before = fs::read(session_path(dir.path())).unwrap();
    let (client, requests, server) = serve(vec![valid_response()]);

    let report = collect_auth_status(dir.path(), "tester", None, &client)
        .await
        .unwrap();

    server.join().unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(requests.lock().unwrap()[0].contains(ACCESS_TOKEN));
    assert!(report.outcome().is_ok());
    let (text, json) = outputs(&report);
    let age = duration_text(report.file_age_seconds.unwrap());
    let expected = [
        "session file: present, plaintext",
        "login: tester",
        "token: valid",
        "scopes: chat:read, channel_read",
        "expires in: 1h 30m 0s",
        &format!("session file age: {age}"),
        "refresh token: present",
    ];
    assert_eq!(text, expected.join("\n"));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["session_file"], true);
    assert_eq!(value["encrypted"], false);
    assert_eq!(value["login"], "tester");
    assert_eq!(value["validation"], "valid");
    assert_eq!(
        value["scopes"],
        serde_json::json!(["chat:read", "channel_read"])
    );
    assert_eq!(value["expires_in_seconds"], 5400);
    assert!(value["file_age_seconds"].is_u64());
    assert_eq!(value["refresh_token"], true);
    // The token goes to Twitch in the request, but never into either output.
    assert_no_secrets(&text);
    assert_no_secrets(&json);
    assert_eq!(fs::read(session_path(dir.path())).unwrap(), before);
}

#[tokio::test]
async fn a_rejected_session_exits_with_failure() {
    for status in ["401 Unauthorized", "400 Bad Request", "403 Forbidden"] {
        let dir = tempfile::tempdir().unwrap();
        saved_session(dir.path(), None);
        let (client, requests, server) = serve(vec![response(status, r#"{"status":401}"#)]);

        let report = collect_auth_status(dir.path(), "tester", None, &client)
            .await
            .unwrap();

        server.join().unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(report.validation, Validation::Rejected);
        assert!(report.outcome().is_err());
        let (text, json) = outputs(&report);
        assert!(text.contains("token: rejected"), "{text}");
        assert!(json.contains(r#""validation":"rejected""#), "{json}");
        assert!(report.scopes.is_empty());
        assert_no_secrets(&text);
        assert_no_secrets(&json);
    }
}

#[tokio::test]
async fn a_token_for_another_login_counts_as_rejected() {
    let dir = tempfile::tempdir().unwrap();
    saved_session(dir.path(), None);
    let (client, _, server) = serve(vec![response(
        "200 OK",
        r#"{"login":"someone_else","user_id":"user-2","scopes":[]}"#,
    )]);

    let report = collect_auth_status(dir.path(), "tester", None, &client)
        .await
        .unwrap();

    server.join().unwrap();
    assert_eq!(report.validation, Validation::Rejected);
    assert!(report.outcome().is_err());
    assert_no_secrets(&outputs(&report).0);
}

#[tokio::test]
async fn an_unreachable_twitch_is_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    saved_session(dir.path(), None);
    let (server_error, _, server) = serve(vec![response(
        "503 Service Unavailable",
        r#"{"status":503}"#,
    )]);
    let (rate_limited, _, limited_server) =
        serve(vec![response("429 Too Many Requests", r#"{"status":429}"#)]);

    for (client, class) in [
        (server_error, "server-error"),
        (rate_limited, "rate-limited"),
        (unreachable_client(), "connection-reset"),
    ] {
        let report = collect_auth_status(dir.path(), "tester", None, &client)
            .await
            .unwrap();
        assert_eq!(report.validation, Validation::Unreachable);
        assert_eq!(report.detail.as_deref(), Some(class));
        assert!(report.outcome().is_ok());
        let (text, json) = outputs(&report);
        assert!(text.contains("token: unreachable"), "{text}");
        assert!(json.contains(r#""validation":"unreachable""#), "{json}");
        assert_no_secrets(&text);
        assert_no_secrets(&json);
    }
    server.join().unwrap();
    limited_server.join().unwrap();
}

#[tokio::test]
async fn a_missing_session_fails_without_contacting_twitch_or_creating_files() {
    let dir = tempfile::tempdir().unwrap();
    let (client, requests, server) = serve(Vec::new());

    let report = collect_auth_status(dir.path(), "tester", None, &client)
        .await
        .unwrap();

    server.join().unwrap();
    assert!(requests.lock().unwrap().is_empty());
    assert!(!report.session_file);
    assert_eq!(report.validation, Validation::NotChecked);
    assert!(report.outcome().is_err());
    let (text, json) = outputs(&report);
    assert!(text.starts_with("session file: missing\nlogin: tester\ntoken: not checked"));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["session_file"], false);
    assert_eq!(value["validation"], "not_checked");
    assert!(value["file_age_seconds"].is_null());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn an_encrypted_session_is_checked_with_its_key_and_never_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let right = key(KEY_HEX);
    saved_session(dir.path(), Some(&right));
    let before = fs::read(session_path(dir.path())).unwrap();
    let (client, requests, server) = serve(vec![valid_response()]);

    let report = collect_auth_status(dir.path(), "tester", Some(&right), &client)
        .await
        .unwrap();

    server.join().unwrap();
    assert!(requests.lock().unwrap()[0].contains(ACCESS_TOKEN));
    assert!(report.encrypted);
    assert_eq!(report.validation, Validation::Valid);
    let (text, json) = outputs(&report);
    assert!(
        text.starts_with("session file: present, encrypted\n"),
        "{text}"
    );
    assert!(json.contains(r#""encrypted":true"#), "{json}");
    assert_no_secrets(&text);
    assert_no_secrets(&json);
    assert_eq!(fs::read(session_path(dir.path())).unwrap(), before);
}

#[tokio::test]
async fn an_encrypted_session_without_or_with_the_wrong_key_is_not_checked() {
    let dir = tempfile::tempdir().unwrap();
    saved_session(dir.path(), Some(&key(KEY_HEX)));
    let before = fs::read(session_path(dir.path())).unwrap();
    let wrong = key(OTHER_KEY_HEX);

    for (supplied, expected) in [
        (None, "TCPM_COOKIE_KEY_FILE"),
        (Some(&wrong), "could not be decrypted"),
    ] {
        let (client, requests, server) = serve(Vec::new());
        let report = collect_auth_status(dir.path(), "tester", supplied, &client)
            .await
            .unwrap();
        server.join().unwrap();

        assert!(requests.lock().unwrap().is_empty());
        assert!(report.encrypted && report.session_file);
        assert_eq!(report.validation, Validation::NotChecked);
        assert!(report.detail.as_deref().unwrap().contains(expected));
        assert!(report.outcome().is_err());
        assert_eq!(report.refresh_token, None);
        let (text, json) = outputs(&report);
        assert_no_secrets(&text);
        assert_no_secrets(&json);
    }
    assert_eq!(fs::read(session_path(dir.path())).unwrap(), before);
}

#[tokio::test]
async fn unusable_session_files_are_described_without_echoing_their_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_path(dir.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, br#"{"auth-token": "leaked-content-marker"}"#).unwrap();
    let (client, requests, server) = serve(Vec::new());

    let report = collect_auth_status(dir.path(), "tester", None, &client)
        .await
        .unwrap();
    assert_eq!(
        report.detail.as_deref(),
        Some("the session file is not a valid session")
    );
    assert!(report.outcome().is_err());
    assert!(!outputs(&report).0.contains("leaked-content-marker"));
    assert!(!outputs(&report).1.contains("leaked-content-marker"));

    AuthSession::new("tester", CookieStore::new())
        .save_to_dir(dir.path(), None)
        .unwrap();
    let report = collect_auth_status(dir.path(), "tester", None, &client)
        .await
        .unwrap();
    assert_eq!(
        report.detail.as_deref(),
        Some("the saved session has no auth token")
    );
    assert_eq!(report.refresh_token, Some(false));
    assert!(report.outcome().is_err());
    server.join().unwrap();
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn checking_a_plaintext_session_with_a_key_does_not_migrate_it() {
    let dir = tempfile::tempdir().unwrap();
    saved_session(dir.path(), None);
    let before = fs::read(session_path(dir.path())).unwrap();
    let (client, _, server) = serve(vec![valid_response()]);

    let report = collect_auth_status(dir.path(), "tester", Some(&key(KEY_HEX)), &client)
        .await
        .unwrap();

    server.join().unwrap();
    assert_eq!(report.validation, Validation::Valid);
    assert!(!report.encrypted);
    assert_eq!(fs::read(session_path(dir.path())).unwrap(), before);
    assert_eq!(
        fs::read_dir(session_path(dir.path()).parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn the_configured_login_is_read_without_writing_the_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths {
        work_dir: dir.path().to_path_buf(),
        config_path: dir.path().join("config.json"),
    };
    assert!(configured_username(&paths).is_err());
    assert!(!paths.config_path.exists());

    fs::write(&paths.config_path, br#"{"username":" Tester "}"#).unwrap();
    let before = fs::read(&paths.config_path).unwrap();
    assert_eq!(configured_username(&paths).unwrap(), "tester");
    assert_eq!(fs::read(&paths.config_path).unwrap(), before);
}

#[test]
fn decrypting_requires_an_encrypted_session_and_writes_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let right = key(KEY_HEX);
    assert!(decrypt_session_files(dir.path(), "tester", &right).is_err());

    saved_session(dir.path(), None);
    let plaintext = fs::read(session_path(dir.path())).unwrap();
    let error = decrypt_session_files(dir.path(), "tester", &right).unwrap_err();
    assert!(format!("{error:#}").contains("not encrypted"), "{error:#}");
    assert_eq!(fs::read(session_path(dir.path())).unwrap(), plaintext);

    fs::remove_file(session_path(dir.path())).unwrap();
    saved_session(dir.path(), Some(&right));
    let encrypted = fs::read(session_path(dir.path())).unwrap();
    let wrong = key(OTHER_KEY_HEX);
    let error = decrypt_session_files(dir.path(), "tester", &wrong).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("could not be decrypted"), "{text}");
    assert_no_secrets(&text);
    assert_eq!(fs::read(session_path(dir.path())).unwrap(), encrypted);

    decrypt_session_files(dir.path(), "tester", &right).unwrap();
    let restored = AuthSession::load_from_dir(dir.path(), "tester", None).unwrap();
    assert_eq!(restored.auth_token(), Some(ACCESS_TOKEN));
    assert_eq!(restored.refresh_token(), Some(REFRESH_TOKEN));
    assert!(!is_encrypted_session(
        &fs::read(session_path(dir.path())).unwrap()
    ));
}
