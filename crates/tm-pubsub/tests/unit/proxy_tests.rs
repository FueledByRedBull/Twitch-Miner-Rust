//! Each transport carries its WebSocket (and `EventSub` its HTTP calls) through the
//! configured proxy, and fails instead of connecting directly when the proxy is
//! unavailable. Everything runs against loopback fakes.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tm_domain::{IrcMode, Streamer, StreamerSettings};
use tm_net::{NetworkPolicy, ProxyConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::{
    connect_hermes_topics_and_listen, EventSubClient, EventSubClientSettings,
    EventSubConnectionEvent, PubSubClient, PubSubClientSettings, PubSubConnectionEvent,
    PubSubError,
};

#[path = "../../../tm-net/tests/support/mod.rs"]
mod support;

use support::{
    refused_address, spawn_http_proxy, spawn_socks, spawn_ws_target, HttpProxyOptions,
    SocksOptions, SocksTarget,
};

const HERMES_WELCOME: &str = r#"{"id":"w","type":"welcome","timestamp":"2030-01-01T00:00:00Z","welcome":{"keepaliveSec":30,"recoveryUrl":"wss://r","sessionId":"s"}}"#;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Socks,
    Http,
}

const KINDS: [Kind; 2] = [Kind::Socks, Kind::Http];

/// A fake proxy and a view of what it was asked to reach.
struct Proxy {
    policy: NetworkPolicy,
    destinations: Box<dyn Fn() -> Vec<String>>,
}

async fn proxy(kind: Kind, forward_to: SocketAddr, routes: Vec<(u16, SocketAddr)>) -> Proxy {
    match kind {
        Kind::Socks => {
            let mut options = SocksOptions::forwarding_to(forward_to);
            options.routes = routes;
            let fake = spawn_socks(options).await;
            let policy = NetworkPolicy::through(
                ProxyConfig::parse(&format!("socks5h://{}", fake.addr)).unwrap(),
            );
            Proxy {
                policy,
                destinations: Box::new(move || {
                    fake.requests()
                        .into_iter()
                        .map(|request| match request.target {
                            SocksTarget::Domain(host, port) => format!("{host}:{port}"),
                            SocksTarget::Ip(address, port) => format!("ip {address}:{port}"),
                        })
                        .collect()
                }),
            }
        }
        Kind::Http => {
            let mut options = HttpProxyOptions::forwarding_to(forward_to);
            options.routes = routes;
            let fake = spawn_http_proxy(options).await;
            let policy = NetworkPolicy::through(
                ProxyConfig::parse(&format!("http://{}", fake.addr)).unwrap(),
            );
            Proxy {
                policy,
                destinations: Box::new(move || {
                    fake.heads()
                        .iter()
                        .filter_map(|head| head.split_whitespace().nth(1).map(str::to_string))
                        .collect()
                }),
            }
        }
    }
}

async fn unavailable_proxies() -> [NetworkPolicy; 2] {
    let dead = refused_address().await;
    [
        NetworkPolicy::through(ProxyConfig::parse(&format!("socks5h://{dead}")).unwrap()),
        NetworkPolicy::through(ProxyConfig::parse(&format!("http://{dead}")).unwrap()),
    ]
}

fn streamer() -> Streamer {
    Streamer {
        channel_id: String::from("100"),
        username: String::from("tester"),
        settings: StreamerSettings {
            irc_mode: IrcMode::Never,
            ..StreamerSettings::default()
        },
        ..Streamer::default()
    }
}

fn pubsub_client(url: String, network: NetworkPolicy) -> PubSubClient {
    PubSubClient::new(PubSubClientSettings {
        websocket_url: url,
        network,
        ..PubSubClientSettings::default()
    })
}

fn reached(proxy: &Proxy, destination: &str) -> bool {
    (proxy.destinations)()
        .iter()
        .any(|seen| seen.contains(destination))
}

async fn serve_pubsub_listen(listener: TcpListener) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    let Some(Ok(Message::Text(raw))) = socket.next().await else {
        panic!("expected a LISTEN frame");
    };
    let payload: Value = serde_json::from_str(raw.as_ref()).unwrap();
    let nonce = payload["nonce"].as_str().unwrap();
    socket
        .send(Message::Text(
            json!({"type":"RESPONSE","error":"","nonce":nonce})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket.close(None).await.unwrap();
}

#[tokio::test]
async fn pubsub_session_completes_through_each_proxy_type() {
    for kind in KINDS {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_pubsub_listen(listener));
        let proxy = proxy(kind, address, Vec::new()).await;
        let client = pubsub_client(
            format!("ws://destination.test:{}", address.port()),
            proxy.policy.clone(),
        );
        let (sender, mut receiver) = mpsc::channel(4);

        client
            .connect_topics_and_listen(
                &[String::from("community-points-user-v1.viewer")],
                "token",
                None,
                &[],
                sender,
            )
            .await
            .unwrap();

        assert_eq!(
            receiver.recv().await,
            Some(PubSubConnectionEvent::ListenAcknowledged {
                topic_class: String::from("points-user")
            }),
            "{kind:?}"
        );
        server.await.unwrap();
        assert!(
            reached(&proxy, &format!("destination.test:{}", address.port())),
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn hermes_session_completes_through_each_proxy_type() {
    for kind in KINDS {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(HERMES_WELCOME.into()))
                .await
                .unwrap();
            socket.next().await.unwrap().unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "type": "authenticateResponse",
                        "authenticateResponse": { "result": "ok" }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let subscribe = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let subscribe: Value = serde_json::from_str(subscribe.as_ref()).unwrap();
            let id = subscribe["subscribe"]["id"].as_str().unwrap().to_string();
            for frame in [
                json!({
                    "type": "subscribeResponse",
                    "subscribeResponse": { "result": "ok", "subscription": { "id": id } }
                }),
                json!({ "type": "reconnect" }),
            ] {
                socket
                    .send(Message::Text(frame.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let proxy = proxy(kind, address, Vec::new()).await;
        let (sender, mut receiver) = mpsc::channel(4);

        let result = connect_hermes_topics_and_listen(
            &proxy.policy,
            &format!("ws://destination.test:{}", address.port()),
            &[String::from("user-drop-events.viewer")],
            "token",
            &[],
            sender,
        )
        .await;

        assert!(
            matches!(result, Err(PubSubError::ReconnectRequested)),
            "{kind:?}"
        );
        assert_eq!(
            receiver.recv().await,
            Some(PubSubConnectionEvent::ListenAcknowledged {
                topic_class: String::from("drops")
            })
        );
        server.await.unwrap();
        assert!(
            reached(&proxy, &format!("destination.test:{}", address.port())),
            "{kind:?}"
        );
    }
}

/// A minimal subscriptions endpoint: lists nothing, accepts every creation.
async fn serve_subscription_requests(listener: TcpListener) {
    let mut created = 0;
    while let Ok((mut stream, _)) = listener.accept().await {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let content_length = head
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or_default();
        let mut body = vec![0_u8; content_length];
        stream.read_exact(&mut body).await.unwrap();
        let (status, response) = if head.starts_with("GET ") {
            (
                "200 OK",
                json!({
                    "data": [], "total": 0, "total_cost": 0,
                    "max_total_cost": 10, "pagination": {}
                }),
            )
        } else {
            created += 1;
            let request: Value = serde_json::from_slice(&body).unwrap();
            (
                "202 Accepted",
                json!({
                    "data": [{
                        "id": format!("subscription-{created}"),
                        "status": "enabled",
                        "type": request["type"],
                        "version": "1",
                        "cost": 1,
                        "condition": request["condition"],
                        "transport": request["transport"],
                        "created_at": "2026-07-13T10:00:00Z"
                    }],
                    "total": created,
                    "total_cost": created,
                    "max_total_cost": 10
                }),
            )
        };
        let response = response.to_string();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{response}",
            response.len()
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
    }
}

#[tokio::test]
async fn eventsub_session_and_subscriptions_complete_through_each_proxy_type() {
    for kind in KINDS {
        let websocket_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let websocket_address = websocket_listener.local_addr().unwrap();
        let subscriptions_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let subscriptions_address = subscriptions_listener.local_addr().unwrap();
        let subscriptions_server =
            tokio::spawn(serve_subscription_requests(subscriptions_listener));
        let websocket_server = tokio::spawn(async move {
            let (stream, _) = websocket_listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "metadata": {"message_id": "welcome-1", "message_type": "session_welcome"},
                        "payload": {"session": {
                            "id": "session-1", "keepalive_timeout_seconds": 30, "reconnect_url": null
                        }}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while socket.next().await.is_some() {}
        });
        let proxy = proxy(
            kind,
            websocket_address,
            vec![
                (websocket_address.port(), websocket_address),
                (subscriptions_address.port(), subscriptions_address),
            ],
        )
        .await;
        let mut settings =
            EventSubClientSettings::with_network("client", "token", &proxy.policy).unwrap();
        settings.websocket_url = format!("ws://destination.test:{}", websocket_address.port());
        settings.subscriptions_url = format!(
            "http://destination.test:{}/eventsub",
            subscriptions_address.port()
        );
        let client = EventSubClient::new(settings);
        let (sender, mut receiver) = mpsc::channel(8);
        let session =
            tokio::spawn(async move { client.connect_and_listen(&[streamer()], sender).await });

        let first = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await
            .unwrap();

        let Some(EventSubConnectionEvent::Setup(report)) = first else {
            panic!("{kind:?}: no setup report: {first:?}");
        };
        assert_eq!(report.active_subscriptions, 2, "{kind:?}");
        assert!(
            reached(
                &proxy,
                &format!("destination.test:{}", websocket_address.port())
            ),
            "{kind:?}: {:?}",
            (proxy.destinations)()
        );
        assert!(
            reached(
                &proxy,
                &format!("destination.test:{}", subscriptions_address.port())
            ),
            "{kind:?}: {:?}",
            (proxy.destinations)()
        );
        session.abort();
        websocket_server.abort();
        subscriptions_server.abort();
    }
}

#[tokio::test]
async fn every_transport_fails_instead_of_connecting_directly() {
    let target = spawn_ws_target("direct").await;
    let url = format!("ws://127.0.0.1:{}", target.addr.port());
    for network in unavailable_proxies().await {
        let (sender, _receiver) = mpsc::channel(4);
        let pubsub = pubsub_client(url.clone(), network.clone())
            .connect_topics_and_listen(
                &[String::from("community-points-user-v1.viewer")],
                "token",
                None,
                &[],
                sender.clone(),
            )
            .await;
        assert!(pubsub.is_err());

        let hermes =
            connect_hermes_topics_and_listen(&network, &url, &[], "token", &[], sender).await;
        assert!(hermes.is_err());

        let (sender, _receiver) = mpsc::channel(4);
        let mut settings =
            EventSubClientSettings::with_network("client", "token", &network).unwrap();
        settings.websocket_url = url.clone();
        settings.subscriptions_url = format!("http://127.0.0.1:{}/eventsub", target.addr.port());
        let eventsub = EventSubClient::new(settings)
            .connect_and_listen(&[streamer()], sender)
            .await;
        assert!(eventsub.is_err());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(target.connections(), 0, "a transport connected directly");
}
