//! With the proxy unavailable, nothing is sent to the destination directly.

#[path = "support/mod.rs"]
mod support;

use std::time::Duration;

use tm_net::{NetworkPolicy, ProxyConfig};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use support::{
    refused_address, spawn_http_proxy, spawn_http_target, spawn_socks, spawn_ws_target,
    HttpProxyOptions, SocksOptions, Target,
};

/// Proxy URLs that cannot carry a connection: nothing listens on the address,
/// or the proxy answers but refuses the destination.
async fn unavailable_proxies(destination: &Target) -> Vec<NetworkPolicy> {
    let dead = refused_address().await;
    let mut socks_refusal = SocksOptions::forwarding_to(destination.addr);
    socks_refusal.reply = 0x02;
    let mut http_refusal = HttpProxyOptions::forwarding_to(destination.addr);
    http_refusal.status = 403;
    let refusing_socks = spawn_socks(socks_refusal).await;
    let refusing_http = spawn_http_proxy(http_refusal).await;
    [
        format!("socks5h://{dead}"),
        format!("http://{dead}"),
        format!("socks5h://{}", refusing_socks.addr),
        format!("http://{}", refusing_http.addr),
    ]
    .iter()
    .map(|url| NetworkPolicy::through(ProxyConfig::parse(url).unwrap()))
    .collect()
}

async fn assert_never_reached(destination: &Target) {
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(destination.connections(), 0, "the destination was reached");
}

#[tokio::test]
async fn an_http_client_fails_instead_of_connecting_directly() {
    let destination = spawn_http_target("direct").await;
    for policy in unavailable_proxies(&destination).await {
        let client = policy
            .http_client_builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let result = client
            .get(format!("http://127.0.0.1:{}/", destination.addr.port()))
            .send()
            .await;
        // A refusing HTTP proxy answers the request itself; that is not the destination.
        assert!(!result.is_ok_and(|response| response.status().is_success()));
    }
    assert_never_reached(&destination).await;
}

#[tokio::test]
async fn a_websocket_fails_instead_of_connecting_directly() {
    let destination = spawn_ws_target("direct").await;
    for policy in unavailable_proxies(&destination).await {
        let result = policy
            .connect_websocket(
                &format!("ws://127.0.0.1:{}/", destination.addr.port()),
                WebSocketConfig::default(),
            )
            .await;
        assert!(result.is_err());
    }
    assert_never_reached(&destination).await;
}

#[tokio::test]
async fn a_tcp_connection_fails_instead_of_connecting_directly() {
    let destination = spawn_http_target("direct").await;
    for policy in unavailable_proxies(&destination).await {
        let result = policy
            .connect_tcp("127.0.0.1", destination.addr.port())
            .await;
        assert!(result.is_err());
    }
    assert_never_reached(&destination).await;
}

#[tokio::test]
async fn the_destination_is_reachable_when_the_policy_is_direct() {
    let destination = spawn_http_target("direct").await;
    let client = NetworkPolicy::direct()
        .http_client_builder()
        .build()
        .unwrap();
    let body = client
        .get(format!("http://127.0.0.1:{}/", destination.addr.port()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "direct");
    assert_eq!(destination.connections(), 1);
}
