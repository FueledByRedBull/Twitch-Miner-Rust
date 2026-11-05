//! The only test binary that sets proxy environment variables, so no other
//! test can observe them.

#[path = "support/mod.rs"]
mod support;

use std::time::Duration;

use tm_net::{NetworkPolicy, ProxyConfig};

use support::{spawn_http_target, spawn_socks, SocksOptions};

const PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

#[tokio::test]
async fn environment_proxies_are_ignored_in_every_case() {
    let environment_proxy = spawn_http_target("via-environment-proxy").await;
    let destination = spawn_http_target("direct").await;
    for name in PROXY_VARIABLES {
        std::env::set_var(name, format!("http://{}", environment_proxy.addr));
    }
    for name in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(name);
    }
    let url = format!("http://127.0.0.1:{}/", destination.addr.port());

    // Premise: an ordinary reqwest client does follow these variables.
    let ordinary = reqwest::Client::builder().build().unwrap();
    let body = ordinary
        .get(&url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "via-environment-proxy");
    let environment_proxy_requests = environment_proxy.connections();
    assert_eq!(destination.connections(), 0);

    let direct = NetworkPolicy::direct()
        .http_client_builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let body = direct.get(&url).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "direct");
    assert_eq!(environment_proxy.connections(), environment_proxy_requests);

    // A configured proxy wins over the environment, which stays unused.
    let proxy = spawn_socks(SocksOptions::forwarding_to(destination.addr)).await;
    let configured =
        NetworkPolicy::through(ProxyConfig::parse(&format!("socks5h://{}", proxy.addr)).unwrap())
            .http_client_builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
    let body = configured
        .get(&url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "direct");
    assert_eq!(proxy.requests().len(), 1);
    assert_eq!(environment_proxy.connections(), environment_proxy_requests);

    for name in PROXY_VARIABLES {
        std::env::remove_var(name);
    }
}
