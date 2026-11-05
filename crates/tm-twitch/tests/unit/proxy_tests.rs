//! Through a proxy the playback client installs no local resolver: its lookup
//! would run on this host, leaking the name and vetting the proxy's own address.

use std::sync::{Arc, Mutex};

use tm_net::{NetworkPolicy, ProxyConfig, ProxyScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{TwitchClient, TwitchClientError, TwitchEndpoints};

type Destinations = Arc<Mutex<Vec<(String, u16)>>>;

/// A SOCKS5 proxy that records each destination it is asked for, then hangs up.
async fn recording_socks() -> (std::net::SocketAddr, Destinations) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let destinations = Destinations::default();
    tokio::spawn({
        let destinations = Arc::clone(&destinations);
        async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let destinations = Arc::clone(&destinations);
                tokio::spawn(async move {
                    let mut greeting = [0_u8; 3];
                    stream.read_exact(&mut greeting).await.ok()?;
                    stream.write_all(&[5, 0]).await.ok()?;
                    let mut head = [0_u8; 4];
                    stream.read_exact(&mut head).await.ok()?;
                    if head[3] != 3 {
                        return None;
                    }
                    let len = usize::from(stream.read_u8().await.ok()?);
                    let mut name = vec![0_u8; len];
                    stream.read_exact(&mut name).await.ok()?;
                    let port = stream.read_u16().await.ok()?;
                    destinations
                        .lock()
                        .unwrap()
                        .push((String::from_utf8(name).unwrap(), port));
                    Some(())
                });
            }
        }
    });
    (address, destinations)
}

fn client(network: &NetworkPolicy) -> TwitchClient {
    TwitchClient::with_network(
        reqwest::Client::new(),
        "token",
        "agent",
        None,
        TwitchEndpoints::default(),
        network,
    )
}

#[tokio::test]
async fn a_proxied_playback_client_sends_the_name_to_the_proxy_unresolved() {
    let (address, destinations) = recording_socks().await;
    let network =
        NetworkPolicy::through(ProxyConfig::parse(&format!("socks5h://{address}")).unwrap());

    // The request itself fails, because the fake proxy hangs up. What matters is
    // that the proxy received the name, not an address looked up on this host.
    let result = client(&network)
        .fetch_settings_script_url("https://playback-host.test/page")
        .await;

    assert!(result.is_err());
    let destinations = destinations.lock().unwrap();
    assert_eq!(
        destinations.first(),
        Some(&(String::from("playback-host.test"), 443))
    );
}

/// An HTTP proxy that records the `CONNECT` target of each request, then hangs up.
async fn recording_http_proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let targets = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn({
        let targets = Arc::clone(&targets);
        async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let targets = Arc::clone(&targets);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        head.push(stream.read_u8().await.ok()?);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let target = head.split_whitespace().nth(1)?.to_string();
                    targets.lock().unwrap().push(target);
                    Some(())
                });
            }
        }
    });
    (port, targets)
}

#[tokio::test]
async fn a_proxy_named_by_host_is_not_checked_against_public_addresses() {
    // The playback resolver rejects loopback answers. Were it installed, it would
    // also vet the proxy's own address and this proxy could never be reached.
    let (port, targets) = recording_http_proxy().await;
    let network =
        NetworkPolicy::through(ProxyConfig::parse(&format!("http://localhost:{port}")).unwrap());

    let result = client(&network)
        .fetch_settings_script_url("https://playback-host.test/page")
        .await;

    assert!(result.is_err());
    assert_eq!(
        targets.lock().unwrap().first().map(String::as_str),
        Some("playback-host.test:443")
    );
}

#[tokio::test]
async fn endpoint_syntax_checks_still_apply_through_a_proxy() {
    let (address, destinations) = recording_socks().await;
    let network = NetworkPolicy::through(ProxyConfig::parse(&format!("http://{address}")).unwrap());
    let client = client(&network);

    for url in [
        "http://playback-host.test/page",
        "https://10.0.0.1/page",
        "https://localhost/page",
        "https://user:secret@playback-host.test/page",
        "https://printer.local/page",
    ] {
        let error = client.fetch_settings_script_url(url).await.unwrap_err();
        assert!(
            matches!(error, TwitchClientError::InvalidField(_)),
            "{url}: {error:?}"
        );
    }
    assert!(destinations.lock().unwrap().is_empty());
}

#[test]
fn the_client_exposes_the_policy_it_was_built_with() {
    let network = NetworkPolicy::through(ProxyConfig::parse("socks5h://127.0.0.1:1080").unwrap());
    assert_eq!(
        client(&network).network().proxy_scheme(),
        Some(ProxyScheme::Socks5h)
    );
    assert_eq!(
        TwitchClient::with_client(reqwest::Client::new(), "token", "agent")
            .network()
            .proxy_scheme(),
        None
    );
}
