use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tm_auth::{AuthEndpoints, AuthSession, TwitchAuthClient};

fn unique_temp_dir() -> PathBuf {
    env::temp_dir().join(format!(
        "tm-app-device-flow-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if header_end.is_none() {
            header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
            if let Some(position) = header_end {
                let header_bytes = &buffer[..position + 4];
                let headers = String::from_utf8_lossy(header_bytes);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("Content-Length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or_default();
                if buffer.len() >= position + 4 + content_length {
                    break;
                }
            }
        } else if let Some(position) = header_end {
            if buffer.len() >= position + 4 + content_length {
                break;
            }
        }
    }

    String::from_utf8(buffer).unwrap()
}

fn http_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn spawn_login_flow_server() -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            requests.push(request);
            let response = match index {
                0 => http_response(
                    "200 OK",
                    r#"{"device_code":"device-code","user_code":"ABCD","interval":0,"expires_in":60}"#,
                ),
                1 => http_response(
                    "400 Bad Request",
                    r#"{"status":400,"message":"authorization_pending"}"#,
                ),
                2 => http_response("200 OK", r#"{"access_token":"token-123"}"#),
                3 => http_response("200 OK", r#"{"data":{"user":{"id":"user-123"}}}"#),
                _ => unreachable!(),
            };
            stream.write_all(&response).unwrap();
        }
        requests
    });
    (format!("http://{address}"), handle)
}

#[tokio::test]
async fn mocked_device_flow_login_saves_session_and_uses_overridden_endpoints() {
    let temp_dir = unique_temp_dir();
    fs::create_dir_all(&temp_dir).unwrap();

    let (base_url, server) = spawn_login_flow_server();
    let client = TwitchAuthClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        AuthEndpoints {
            device_code_url: format!("{base_url}/oauth2/device"),
            token_url: format!("{base_url}/oauth2/token"),
            gql_url: format!("{base_url}/gql"),
        },
    );

    let result = client
        .login_with_device_flow("tester", "device-1", "ua", &temp_dir)
        .await
        .unwrap();
    assert_eq!(result.prompt.user_code, "ABCD");
    assert_eq!(
        result.prompt.verification_uri,
        "https://www.twitch.tv/activate"
    );

    let session = AuthSession::load_from_dir(&temp_dir, "tester").unwrap();
    assert_eq!(session.auth_token(), Some("token-123"));
    assert_eq!(session.user_id(), Some("user-123"));

    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("POST /oauth2/device "));
    assert!(requests[1].starts_with("POST /oauth2/token "));
    assert!(requests[2].starts_with("POST /oauth2/token "));
    assert!(requests[3].starts_with("POST /gql "));
    assert!(requests[3].contains(r#""operationName":"GetIDFromLogin""#));

    fs::remove_dir_all(&temp_dir).unwrap();
}
