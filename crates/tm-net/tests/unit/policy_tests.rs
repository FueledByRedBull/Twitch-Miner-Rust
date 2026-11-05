use std::io;
use std::time::Duration;

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use super::{NetworkPolicy, ProxyConfig, ProxyFailure};

#[path = "../support/mod.rs"]
mod support;

use support::{
    refused_address, spawn_http_proxy, spawn_http_target, spawn_socks, spawn_ws_target,
    HttpProxyOptions, SocksOptions, SocksTarget,
};

fn policy(url: &str) -> NetworkPolicy {
    NetworkPolicy::through(ProxyConfig::parse(url).unwrap())
}

fn failure(error: &io::Error) -> ProxyFailure {
    *error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ProxyFailure>())
        .unwrap_or_else(|| panic!("not a proxy failure: {error}"))
}

#[tokio::test]
async fn direct_policy_connects_without_a_proxy() {
    let target = spawn_http_target("direct").await;
    let stream = NetworkPolicy::direct()
        .connect_tcp("127.0.0.1", target.addr.port())
        .await;
    assert!(stream.is_ok());
}

#[tokio::test]
async fn socks5_receives_the_destination_as_a_domain_name() {
    let target = spawn_http_target("socks-body").await;
    let proxy = spawn_socks(SocksOptions::forwarding_to(target.addr)).await;
    let policy = policy(&format!("socks5h://{}", proxy.addr));

    let stream = policy.connect_tcp("destination.test", 4443).await;
    assert!(stream.is_ok());

    let requests = proxy.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].target,
        SocksTarget::Domain(String::from("destination.test"), 4443)
    );
    assert_eq!(requests[0].offered_methods, vec![0x00]);
    assert_eq!(requests[0].credentials, None);
}

#[tokio::test]
async fn socks5_sends_an_ip_literal_as_an_address() {
    let target = spawn_http_target("socks-body").await;
    let proxy = spawn_socks(SocksOptions::forwarding_to(target.addr)).await;
    let policy = policy(&format!("socks5h://{}", proxy.addr));

    policy.connect_tcp("203.0.113.9", 80).await.unwrap();

    assert!(matches!(
        proxy.requests()[0].target,
        SocksTarget::Ip(address, 80) if address.to_string() == "203.0.113.9"
    ));
}

#[tokio::test]
async fn socks5_authenticates_with_username_and_password() {
    let target = spawn_http_target("socks-body").await;
    let mut options = SocksOptions::forwarding_to(target.addr);
    options.required_credentials = Some((String::from("alice"), String::from("p@ss")));
    let proxy = spawn_socks(options).await;

    let accepted = policy(&format!("socks5h://alice:p%40ss@{}", proxy.addr));
    accepted.connect_tcp("destination.test", 443).await.unwrap();
    let request = proxy.requests().remove(0);
    assert_eq!(
        request.credentials,
        Some((String::from("alice"), String::from("p@ss")))
    );
    assert!(request.offered_methods.contains(&0x02));

    let wrong = policy(&format!("socks5h://alice:nope@{}", proxy.addr));
    let error = wrong
        .connect_tcp("destination.test", 443)
        .await
        .unwrap_err();
    assert_eq!(failure(&error), ProxyFailure::Authentication);
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

    let missing = policy(&format!("socks5h://{}", proxy.addr));
    let error = missing
        .connect_tcp("destination.test", 443)
        .await
        .unwrap_err();
    assert_eq!(failure(&error), ProxyFailure::Authentication);
    assert_eq!(proxy.requests().len(), 1);
}

#[tokio::test]
async fn socks5_refusal_and_a_dead_proxy_are_errors() {
    let target = spawn_http_target("socks-body").await;
    let mut options = SocksOptions::forwarding_to(target.addr);
    options.reply = 0x05;
    let refusing = spawn_socks(options).await;
    let error = policy(&format!("socks5h://{}", refusing.addr))
        .connect_tcp("destination.test", 443)
        .await
        .unwrap_err();
    assert_eq!(failure(&error), ProxyFailure::Rejected);

    let dead = refused_address().await;
    let error = policy(&format!("socks5h://{dead}"))
        .connect_tcp("destination.test", 443)
        .await
        .unwrap_err();
    assert_eq!(failure(&error), ProxyFailure::Unreachable);
    assert_eq!(target.connections(), 0);
}

#[tokio::test]
async fn http_connect_sends_the_destination_name_and_no_credentials_by_default() {
    let target = spawn_http_target("tunnel").await;
    let proxy = spawn_http_proxy(HttpProxyOptions::forwarding_to(target.addr)).await;

    let stream = policy(&format!("http://{}", proxy.addr))
        .connect_tcp("destination.test", 4443)
        .await;
    assert!(stream.is_ok());

    assert_eq!(
        proxy.heads(),
        vec![String::from(
            "CONNECT destination.test:4443 HTTP/1.1\r\nHost: destination.test:4443\r\n\r\n"
        )]
    );
}

#[tokio::test]
async fn http_connect_brackets_ipv6_destinations() {
    let target = spawn_http_target("tunnel").await;
    let proxy = spawn_http_proxy(HttpProxyOptions::forwarding_to(target.addr)).await;

    policy(&format!("http://{}", proxy.addr))
        .connect_tcp("2001:db8::1", 443)
        .await
        .unwrap();

    assert!(proxy.heads()[0].starts_with("CONNECT [2001:db8::1]:443 HTTP/1.1\r\n"));
}

#[tokio::test]
async fn http_connect_sends_basic_credentials() {
    let target = spawn_http_target("tunnel").await;
    let mut options = HttpProxyOptions::forwarding_to(target.addr);
    options.required_authorization = Some(String::from("Basic YWxpY2U6cEBzcw=="));
    let proxy = spawn_http_proxy(options).await;

    policy(&format!("http://alice:p%40ss@{}", proxy.addr))
        .connect_tcp("destination.test", 443)
        .await
        .unwrap();
    assert!(proxy.heads()[0].contains("Proxy-Authorization: Basic YWxpY2U6cEBzcw==\r\n"));

    let error = policy(&format!("http://alice:wrong@{}", proxy.addr))
        .connect_tcp("destination.test", 443)
        .await
        .unwrap_err();
    assert_eq!(failure(&error), ProxyFailure::Authentication);
}

#[tokio::test]
async fn http_connect_rejects_a_non_success_status() {
    let target = spawn_http_target("tunnel").await;
    for status in [403_u16, 502, 301, 199] {
        let mut options = HttpProxyOptions::forwarding_to(target.addr);
        options.status = status;
        let proxy = spawn_http_proxy(options).await;
        let error = policy(&format!("http://{}", proxy.addr))
            .connect_tcp("destination.test", 443)
            .await
            .unwrap_err();
        assert_eq!(failure(&error), ProxyFailure::Rejected, "{status}");
    }
    assert_eq!(target.connections(), 0);
}

#[tokio::test]
async fn http_connect_rejects_malformed_and_oversized_heads() {
    let target = spawn_http_target("tunnel").await;
    let oversized = format!(
        "HTTP/1.1 200 OK\r\nX-Padding: {}\r\n\r\n",
        "a".repeat(64 * 1024)
    );
    let responses: [(&str, Vec<u8>); 5] = [
        ("not http", b"SSH-2.0-OpenSSH_9.6\r\n\r\n".to_vec()),
        ("bad status", b"HTTP/1.1 2x0 OK\r\n\r\n".to_vec()),
        ("bad version", b"HTTP/2 200 OK\r\n\r\n".to_vec()),
        ("oversized", oversized.into_bytes()),
        ("closed mid-head", b"HTTP/1.1 200 OK\r\nX-Part".to_vec()),
    ];
    for (name, raw) in responses {
        let mut options = HttpProxyOptions::forwarding_to(target.addr);
        options.raw_response = Some(raw);
        let proxy = spawn_http_proxy(options).await;
        let error = policy(&format!("http://{}", proxy.addr))
            .connect_tcp("destination.test", 443)
            .await
            .unwrap_err();
        assert_eq!(failure(&error), ProxyFailure::Protocol, "{name}");
    }
}

#[tokio::test]
async fn http_connect_does_not_read_past_the_head() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\n\r\nserver-first")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });

    let mut stream = policy(&format!("http://{addr}"))
        .connect_tcp("destination.test", 443)
        .await
        .unwrap();
    let mut greeting = [0_u8; 12];
    tokio::io::AsyncReadExt::read_exact(&mut stream, &mut greeting)
        .await
        .unwrap();
    assert_eq!(&greeting, b"server-first");
}

#[tokio::test]
async fn the_handshake_has_a_deadline() {
    let target = spawn_http_target("tunnel").await;
    let mut options = HttpProxyOptions::forwarding_to(target.addr);
    options.hang = true;
    let proxy = spawn_http_proxy(options).await;

    let error = policy(&format!("http://{}", proxy.addr))
        .connect_tcp_within("destination.test", 443, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert_eq!(failure(&error), ProxyFailure::Timeout);
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn an_http_client_reaches_the_destination_through_a_socks5_proxy() {
    let target = spawn_http_target("through-socks").await;
    let proxy = spawn_socks(SocksOptions::forwarding_to(target.addr)).await;
    let client = policy(&format!("socks5h://{}", proxy.addr))
        .http_client_builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let body = client
        .get(format!("http://destination.test:{}/", target.addr.port()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(body, "through-socks");
    assert_eq!(
        proxy.requests()[0].target,
        SocksTarget::Domain(String::from("destination.test"), target.addr.port())
    );
}

#[tokio::test]
async fn an_http_client_reaches_the_destination_through_an_http_proxy() {
    let target = spawn_http_target("through-http").await;
    let proxy = spawn_http_proxy(HttpProxyOptions::forwarding_to(target.addr)).await;
    let client = policy(&format!("http://{}", proxy.addr))
        .http_client_builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let port = target.addr.port();

    let body = client
        .get(format!("http://destination.test:{port}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "through-http");
    assert!(
        proxy.heads()[0].starts_with(&format!("GET http://destination.test:{port}/ HTTP/1.1\r\n"))
    );

    // A TLS destination is tunnelled with CONNECT and a name, not an address.
    let tls_request = client.get("https://destination.test/").send();
    let _ = tokio::time::timeout(Duration::from_millis(500), tls_request).await;
    assert!(proxy
        .heads()
        .iter()
        .any(|head| head.starts_with("CONNECT destination.test:443 HTTP/1.1\r\n")));
}

#[tokio::test]
async fn a_websocket_session_completes_through_a_socks5_proxy() {
    let target = spawn_ws_target("hello-socks").await;
    let proxy = spawn_socks(SocksOptions::forwarding_to(target.addr)).await;
    let policy = policy(&format!("socks5h://{}", proxy.addr));

    let (mut socket, _) = policy
        .connect_websocket(
            &format!("ws://destination.test:{}/", target.addr.port()),
            WebSocketConfig::default(),
        )
        .await
        .unwrap();

    let Some(Ok(Message::Text(text))) = socket.next().await else {
        panic!("no greeting");
    };
    assert_eq!(text.as_str(), "hello-socks");
    assert_eq!(
        proxy.requests()[0].target,
        SocksTarget::Domain(String::from("destination.test"), target.addr.port())
    );
}

#[tokio::test]
async fn a_websocket_session_completes_through_an_http_proxy() {
    let target = spawn_ws_target("hello-http").await;
    let proxy = spawn_http_proxy(HttpProxyOptions::forwarding_to(target.addr)).await;
    let policy = policy(&format!("http://{}", proxy.addr));
    let port = target.addr.port();

    let (mut socket, _) = policy
        .connect_websocket(
            &format!("ws://destination.test:{port}/"),
            WebSocketConfig::default(),
        )
        .await
        .unwrap();

    let Some(Ok(Message::Text(text))) = socket.next().await else {
        panic!("no greeting");
    };
    assert_eq!(text.as_str(), "hello-http");
    assert!(proxy.heads()[0].starts_with(&format!("CONNECT destination.test:{port} HTTP/1.1\r\n")));
}

#[tokio::test]
async fn a_direct_websocket_does_not_touch_any_proxy() {
    let target = spawn_ws_target("hello-direct").await;

    let (mut socket, _) = NetworkPolicy::direct()
        .connect_websocket(
            &format!("ws://127.0.0.1:{}/", target.addr.port()),
            WebSocketConfig::default(),
        )
        .await
        .unwrap();

    let Some(Ok(Message::Text(text))) = socket.next().await else {
        panic!("no greeting");
    };
    assert_eq!(text.as_str(), "hello-direct");
}

#[test]
fn environment_proxies_are_reported_only_when_no_proxy_is_configured() {
    let is_set = |name: &str| matches!(name, "HTTP_PROXY" | "all_proxy");

    assert_eq!(
        NetworkPolicy::direct().ignored_env_proxies(is_set),
        ["HTTP_PROXY", "all_proxy"]
    );
    assert!(NetworkPolicy::direct()
        .ignored_env_proxies(|_| false)
        .is_empty());
    assert!(policy("socks5h://127.0.0.1:1080")
        .ignored_env_proxies(is_set)
        .is_empty());
}

#[tokio::test]
async fn websocket_urls_without_a_usable_host_or_scheme_are_errors() {
    let target = spawn_ws_target("unused").await;
    let proxy = spawn_socks(SocksOptions::forwarding_to(target.addr)).await;
    let policy = policy(&format!("socks5h://{}", proxy.addr));
    for url in ["not a url", "http://destination.test/", "/relative"] {
        assert!(
            policy
                .connect_websocket(url, WebSocketConfig::default())
                .await
                .is_err(),
            "{url}"
        );
    }
    assert!(proxy.requests().is_empty());
}
