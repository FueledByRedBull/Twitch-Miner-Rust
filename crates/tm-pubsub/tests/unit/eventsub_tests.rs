use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use super::{
    event_from_notification, listen_socket, parse_eventsub_message,
    subscription_plan_with_capacity, subscription_requests, subscription_requests_with_policy,
    EventSubClient, EventSubClientSettings, EventSubConnectionEvent, EventSubMessage,
    MessageDeduper, EVENTSUB_WEBSOCKET_URL,
};
use futures_util::SinkExt;
use serde_json::json;
use tm_domain::{IrcMode, Streamer, StreamerSettings};
use tm_events::MinerEvent;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async};

fn streamer() -> Streamer {
    Streamer {
        channel_id: String::from("100"),
        username: String::from("tester"),
        settings: StreamerSettings {
            make_predictions: true,
            irc_mode: IrcMode::Never,
            ..StreamerSettings::default()
        },
        ..Streamer::default()
    }
}

#[test]
fn default_websocket_url_requests_setup_safe_keepalive_window() {
    assert_eq!(
        EVENTSUB_WEBSOCKET_URL,
        "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30"
    );
}

#[tokio::test]
async fn keepalive_wait_allows_a_small_delivery_grace() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        socket
            .send(Message::Text(
                json!({
                    "metadata": {
                        "message_id": "keepalive-1",
                        "message_type": "session_keepalive"
                    },
                    "payload": {}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
    });

    let (mut socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
    let mut deduper = MessageDeduper::default();
    let result = listen_socket(
        &mut socket,
        &[streamer()],
        &sender,
        &mut deduper,
        std::time::Duration::from_millis(50),
    )
    .await;

    assert!(result.is_ok());
    assert!(matches!(
        receiver.recv().await,
        Some(EventSubConnectionEvent::Heartbeat)
    ));
    server.await.unwrap();
}

async fn read_http_json(stream: &mut tokio::net::TcpStream) -> serde_json::Value {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut expected_length = None;
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "HTTP request ended before its JSON body");
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let body_start = header_end + 4;
            let content_length = *expected_length.get_or_insert_with(|| {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or_default()
            });
            if request.len() >= body_start + content_length {
                if content_length == 0 {
                    return serde_json::Value::Null;
                }
                return serde_json::from_slice(&request[body_start..body_start + content_length])
                    .unwrap();
            }
        }
        assert!(request.len() < 16 * 1024);
    }
}

fn accepted_subscription_response(request: &serde_json::Value, id: usize) -> String {
    json!({
        "data": [{
            "id": format!("subscription-{id}"),
            "status": "enabled",
            "type": request["type"],
            "version": "1",
            "cost": 1,
            "condition": request["condition"],
            "transport": request["transport"],
            "created_at": "2026-07-13T10:00:00Z"
        }],
        "total": id,
        "total_cost": id,
        "max_total_cost": 10
    })
    .to_string()
}

fn inherited_list_response(session_id: &str, count: usize) -> String {
    let data = (1..=count)
        .map(|id| {
            json!({
                "id": format!("subscription-{id}"),
                "status": "enabled",
                "type": "stream.online",
                "version": "1",
                "cost": 1,
                "condition": {"broadcaster_user_id": "100"},
                "transport": {"method": "websocket", "session_id": session_id},
                "created_at": "2026-07-13T10:00:00Z"
            })
        })
        .collect::<Vec<_>>();
    json!({
        "data": data,
        "total": count,
        "total_cost": count,
        "max_total_cost": 10,
        "pagination": {}
    })
    .to_string()
}

fn capacity_response(total_cost: u32, max_total_cost: u32) -> String {
    json!({
        "data": [],
        "total": 0,
        "total_cost": total_cost,
        "max_total_cost": max_total_cost,
        "pagination": {}
    })
    .to_string()
}

async fn write_json_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    stream.write_all(response.as_bytes()).await.unwrap();
}

#[test]
fn parses_welcome_keepalive_reconnect_and_revocation() {
    let tracked = [streamer()];
    let welcome = parse_eventsub_message(
            &json!({
                "metadata": {"message_id":"1","message_type":"session_welcome"},
                "payload": {"session": {"id":"session-1","status":"connected","keepalive_timeout_seconds":10,"reconnect_url":null}}
            })
            .to_string(),
            &tracked,
        )
        .unwrap();
    assert!(matches!(welcome, EventSubMessage::Welcome { .. }));

    let keepalive = parse_eventsub_message(
        &json!({
            "metadata": {"message_id":"2","message_type":"session_keepalive"},
            "payload": {}
        })
        .to_string(),
        &tracked,
    )
    .unwrap();
    assert_eq!(keepalive, EventSubMessage::Keepalive);

    let reconnect = parse_eventsub_message(
        &json!({
            "metadata": {"message_id":"3","message_type":"session_reconnect"},
            "payload": {"session": {"reconnect_url":"wss://example.test/ws"}}
        })
        .to_string(),
        &tracked,
    )
    .unwrap();
    assert!(matches!(reconnect, EventSubMessage::Reconnect { .. }));

    let revoked = parse_eventsub_message(
        &json!({
            "metadata": {"message_id":"4","message_type":"revocation"},
            "payload": {"subscription": {"status":"authorization_revoked"}}
        })
        .to_string(),
        &tracked,
    )
    .unwrap();
    assert!(matches!(revoked, EventSubMessage::Revocation { .. }));
}

#[test]
fn parses_stream_and_prediction_notifications_strictly() {
    let tracked = [streamer()];
    let stream = parse_eventsub_message(
        &json!({
            "metadata": {"message_id":"stream-1","message_type":"notification"},
            "payload": {
                "subscription": {"type":"stream.online"},
                "event": {"broadcaster_user_id":"100"}
            }
        })
        .to_string(),
        &tracked,
    )
    .unwrap();
    assert!(matches!(stream, EventSubMessage::Notification { .. }));

    let prediction = parse_eventsub_message(
            &json!({
                "metadata": {"message_id":"prediction-1","message_type":"notification"},
                "payload": {
                    "subscription": {"type":"channel.prediction.begin"},
                    "event": {
                        "id":"event-1","broadcaster_user_id":"100","title":"Question",
                        "outcomes":[
                            {"id":"outcome-1","title":"Yes","color":"BLUE","users":0,"channel_points":0},
                            {"id":"outcome-2","title":"No","color":"PINK","users":0,"channel_points":0}
                        ],
                        "started_at":"2026-07-12T10:00:00Z","locks_at":"2026-07-12T10:01:00Z"
                    }
                }
            })
            .to_string(),
            &tracked,
        )
        .unwrap();
    assert!(matches!(prediction, EventSubMessage::Notification { .. }));

    let prediction_lock = parse_eventsub_message(
            &json!({
                "metadata": {"message_id":"prediction-lock-1","message_type":"notification"},
                "payload": {
                    "subscription": {"type":"channel.prediction.lock"},
                    "event": {
                        "id":"event-1","broadcaster_user_id":"100","title":"Question",
                        "outcomes":[
                            {"id":"outcome-1","title":"Yes","color":"BLUE","users":3,"channel_points":30},
                            {"id":"outcome-2","title":"No","color":"PINK","users":2,"channel_points":20}
                        ],
                        "started_at":"2026-07-12T10:00:00Z","locked_at":"2026-07-12T10:01:00Z"
                    }
                }
            })
            .to_string(),
            &tracked,
        )
        .unwrap();
    assert!(matches!(
        prediction_lock,
        EventSubMessage::Notification { .. }
    ));
}

#[test]
fn parses_prediction_end_notifications_and_rejects_incomplete_events() {
    let tracked = [streamer()];
    let prediction_end = parse_eventsub_message(
            &json!({
                "metadata": {"message_id":"prediction-end-1","message_type":"notification"},
                "payload": {
                    "subscription": {"type":"channel.prediction.end"},
                    "event": {
                        "id":"event-1","broadcaster_user_id":"100","title":"Question",
                        "winning_outcome_id":"outcome-1","status":"resolved",
                        "outcomes":[
                            {"id":"outcome-1","title":"Yes","color":"BLUE","users":3,"channel_points":30},
                            {"id":"outcome-2","title":"No","color":"PINK","users":2,"channel_points":20}
                        ],
                        "started_at":"2026-07-12T10:00:00Z","ended_at":"2026-07-12T10:02:00Z"
                    }
                }
            })
            .to_string(),
            &tracked,
        )
        .unwrap();
    assert!(matches!(
        prediction_end,
        EventSubMessage::Notification { .. }
    ));

    let canceled_prediction_end = parse_eventsub_message(
            &json!({
                "metadata": {"message_id":"prediction-end-2","message_type":"notification"},
                "payload": {
                    "subscription": {"type":"channel.prediction.end"},
                    "event": {
                        "id":"event-1","broadcaster_user_id":"100","title":"Question",
                        "winning_outcome_id":"","status":"canceled",
                        "outcomes":[
                            {"id":"outcome-1","title":"Yes","color":"BLUE","users":3,"channel_points":30},
                            {"id":"outcome-2","title":"No","color":"PINK","users":2,"channel_points":20}
                        ],
                        "started_at":"2026-07-12T10:00:00Z","ended_at":"2026-07-12T10:02:00Z"
                    }
                }
            })
            .to_string(),
            &tracked,
        )
        .unwrap();
    assert!(matches!(
        canceled_prediction_end,
        EventSubMessage::Notification { .. }
    ));

    let incomplete_prediction = json!({
        "metadata": {"message_id":"prediction-invalid","message_type":"notification"},
        "payload": {
            "subscription": {"type":"channel.prediction.begin"},
            "event": {
                "id":"event-1","broadcaster_user_id":"100","title":"Question",
                "outcomes":[{"id":"outcome-1","title":"Yes","color":"BLUE"}],
                "started_at":"2026-07-12T10:00:00Z","locks_at":"2026-07-12T10:01:00Z"
            }
        }
    })
    .to_string();
    assert!(parse_eventsub_message(&incomplete_prediction, &tracked).is_err());
}

#[test]
fn deduper_is_bounded_and_rejects_duplicates() {
    let mut deduper = MessageDeduper::default();
    assert!(deduper.insert(String::from("one")));
    assert!(!deduper.insert(String::from("one")));
    for index in 0..5000 {
        assert!(deduper.insert(format!("id-{index}")));
    }
    assert!(deduper.ids.len() <= super::MAX_SEEN_MESSAGE_IDS);
}

#[test]
fn raid_notifications_are_observed_without_fabricating_a_mutation_id() {
    let tracked = [streamer()];
    let event = event_from_notification(
        "channel.raid",
        &json!({
            "from_broadcaster_user_id": "100",
            "to_broadcaster_user_login": "target"
        }),
        &tracked,
    )
    .unwrap();
    assert_eq!(
        event,
        MinerEvent::Raid {
            channel_id: String::from("100"),
            raid_id: String::new(),
            target_login: String::from("target"),
        }
    );
}

#[test]
fn stream_offline_2026_fixture_accepts_additive_stream_id() {
    let tracked = [streamer()];
    let message = parse_eventsub_message(
        include_str!("../../../../tests/fixtures/eventsub.stream_offline.2026.json"),
        &tracked,
    )
    .unwrap();
    let EventSubMessage::Notification { event, .. } = message else {
        panic!("expected notification");
    };

    assert_eq!(
        *event,
        MinerEvent::Playback {
            channel_id: String::from("100"),
            kind: crate::PlaybackType::StreamDown,
        }
    );
}

#[test]
fn unmodelled_message_and_subscription_types_are_ignored_not_fatal() {
    let tracked = [streamer()];

    let unknown_message_type = parse_eventsub_message(
        &json!({
            "metadata": {"message_id":"future-1","message_type":"session_future_thing"},
            "payload": {"anything": true}
        })
        .to_string(),
        &tracked,
    )
    .unwrap();
    assert!(matches!(unknown_message_type, EventSubMessage::Unsupported));

    let unknown_subscription = parse_eventsub_message(
        &json!({
            "metadata": {"message_id":"future-2","message_type":"notification"},
            "payload": {
                "subscription": {"type":"channel.future.thing"},
                "event": {"unmodelled": true}
            }
        })
        .to_string(),
        &tracked,
    )
    .unwrap();
    assert!(matches!(unknown_subscription, EventSubMessage::Unsupported));

    // A payload for a type the miner does act on must still fail closed.
    assert!(parse_eventsub_message(
        &json!({
            "metadata": {"message_id":"bad-1","message_type":"notification"},
            "payload": {
                "subscription": {"type":"stream.online"},
                "event": {"wrong_field": "100"}
            }
        })
        .to_string(),
        &tracked,
    )
    .is_err());
}

#[test]
fn raid_subscription_is_only_requested_when_follow_raid_is_enabled() {
    let without_raid = subscription_requests("session", &[streamer()]);
    assert!(!without_raid.iter().any(|(kind, _)| kind == "channel.raid"));

    let mut raid_streamer = streamer();
    raid_streamer.settings.follow_raid = true;
    let with_raid = subscription_requests("session", &[raid_streamer]);
    assert!(with_raid.iter().any(|(kind, _)| kind == "channel.raid"));
}

#[test]
fn viewer_policy_does_not_request_broadcaster_prediction_subscriptions() {
    let requests = subscription_requests_with_policy(
        "session",
        &[streamer()],
        crate::TransportSourcePolicy::viewer_compatibility(),
    );

    assert!(requests
        .iter()
        .all(|(kind, _)| !kind.starts_with("channel.prediction.")));
    assert!(requests.iter().any(|(kind, _)| kind == "stream.online"));
    assert!(requests.iter().any(|(kind, _)| kind == "stream.offline"));
}

#[test]
fn viewer_policy_prefers_eventsub_predictions_only_for_authenticated_broadcaster() {
    let mut own_channel = streamer();
    own_channel.channel_id = String::from("viewer-100");
    let mut other_channel = streamer();
    other_channel.channel_id = String::from("other-200");

    let (requests, report) = super::subscription_plan(
        &[own_channel, other_channel],
        crate::TransportSourcePolicy::viewer_compatibility(),
        Some("viewer-100"),
    );

    assert_eq!(
        report.capabilities[0].prediction_source,
        "eventsub-broadcaster"
    );
    assert_eq!(
        report.capabilities[1].prediction_source,
        "pubsub-compatibility"
    );
    let prediction_requests = requests
        .iter()
        .filter(|request| request.subscription_type.starts_with("channel.prediction."))
        .collect::<Vec<_>>();
    assert_eq!(prediction_requests.len(), 4);
    assert!(prediction_requests
        .iter()
        .all(|request| request.streamer_index == 0));
}

#[test]
fn capacity_plan_is_deterministic_and_uses_polling_for_presence_overflow() {
    let tracked = (0..8)
        .map(|index| Streamer {
            channel_id: format!("channel-{index}"),
            ..Streamer::default()
        })
        .collect::<Vec<_>>();
    let report = super::plan_eventsub_capacity(
        &tracked,
        crate::TransportSourcePolicy::viewer_compatibility(),
    );

    assert_eq!(report.planned_subscriptions, 10);
    assert_eq!(report.overflow_streamers, 3);
    assert_eq!(
        report.capabilities[4].presence_source,
        "eventsub+gql-polling"
    );
    assert_eq!(report.capabilities[5].presence_source, "gql-polling");
    assert_eq!(
        report.capabilities[5].failure_class.as_deref(),
        Some("capacity-overflow")
    );
}

#[test]
fn capacity_plan_prioritizes_presence_then_raid_before_predictions() {
    let tracked = (0..2)
        .map(|index| Streamer {
            channel_id: format!("channel-{index}"),
            settings: StreamerSettings {
                follow_raid: true,
                make_predictions: true,
                ..StreamerSettings::default()
            },
            ..Streamer::default()
        })
        .collect::<Vec<_>>();
    let report = super::plan_eventsub_capacity(
        &tracked,
        crate::TransportSourcePolicy::broadcaster_eventsub(),
    );

    assert_eq!(report.planned_subscriptions, 10);
    assert!(report.capabilities[0]
        .planned_subscription_types
        .contains(&String::from("channel.prediction.end")));
    assert!(report.capabilities[1]
        .skipped_subscription_types
        .contains(&String::from("channel.prediction.begin")));
}

#[test]
fn capacity_plan_uses_current_cost_and_zero_cost_authenticated_broadcaster() {
    let tracked = vec![
        streamer(),
        Streamer {
            channel_id: String::from("200"),
            ..streamer()
        },
    ];
    let (requests, report) = subscription_plan_with_capacity(
        &tracked,
        crate::TransportSourcePolicy::viewer_compatibility(),
        None,
        2,
        8,
        10,
    );
    assert_eq!(requests.len(), 2);
    assert_eq!(report.total_cost, 8);
    assert_eq!(report.max_total_cost, 10);
    assert_eq!(report.overflow_streamers, 1);

    let (requests, report) = subscription_plan_with_capacity(
        std::slice::from_ref(&tracked[0]),
        crate::TransportSourcePolicy::viewer_compatibility(),
        Some("100"),
        0,
        10,
        10,
    );
    assert_eq!(requests.len(), 6);
    assert_eq!(report.overflow_streamers, 0);
    assert_eq!(
        report.capabilities[0].prediction_source,
        "eventsub-broadcaster"
    );
}

#[tokio::test]
async fn partial_subscription_failure_retains_successful_presence_subscription() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(read_http_json(&mut stream).await.is_null());
        write_json_response(&mut stream, "200 OK", &capacity_response(0, 10)).await;
        for (id, status) in ["500 Internal Server Error", "202 Accepted"]
            .into_iter()
            .enumerate()
        {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_json(&mut stream).await;
            let body = if status.starts_with("202") {
                accepted_subscription_response(&request, id + 1)
            } else {
                String::from("{}")
            };
            write_json_response(&mut stream, status, &body).await;
        }
    });

    let mut settings = EventSubClientSettings::new("client", "token");
    settings.subscriptions_url = format!("http://{address}/eventsub");
    let client = EventSubClient::new(settings);
    let report = client
        .create_subscriptions("session", &[streamer()])
        .await
        .unwrap();

    assert_eq!(report.planned_subscriptions, 2);
    assert_eq!(report.active_subscriptions, 1);
    assert_eq!(report.failed_subscriptions, 1);
    assert_eq!(report.capabilities[0].presence_source, "gql-polling");
    assert_eq!(
        report.capabilities[0].failure_class.as_deref(),
        Some("server-error")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn diagnostic_setup_lists_and_verifies_created_subscriptions_with_bounded_retry() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(read_http_json(&mut stream).await.is_null());
        write_json_response(&mut stream, "200 OK", &capacity_response(0, 10)).await;
        let mut created = Vec::new();
        for id in 1..=2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_json(&mut stream).await;
            created.push(request.clone());
            let body = accepted_subscription_response(&request, id);
            write_json_response(&mut stream, "202 Accepted", &body).await;
        }

        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(read_http_json(&mut stream).await.is_null());
        write_json_response(&mut stream, "429 Too Many Requests", "{}").await;

        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(read_http_json(&mut stream).await.is_null());
        let body = json!({
            "data": created.iter().enumerate().map(|(index, request)| json!({
                "id": format!("subscription-{}", index + 1),
                "status": "enabled",
                "type": request["type"],
                "version": "1",
                "cost": 1,
                "condition": request["condition"],
                "transport": request["transport"],
                "created_at": "2026-07-13T10:00:00Z"
            })).collect::<Vec<_>>(),
            "total": 2,
            "total_cost": 2,
            "max_total_cost": 10,
            "pagination": {}
        })
        .to_string();
        let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        stream.write_all(response.as_bytes()).await.unwrap();
    });

    let mut settings = EventSubClientSettings::new("client", "token");
    settings.subscriptions_url = format!("http://{address}/eventsub");
    settings.verify_subscriptions = true;
    let client = EventSubClient::new(settings);
    let report = client
        .create_subscriptions("session", &[streamer()])
        .await
        .unwrap();

    assert!(report.verified);
    assert_eq!(report.active_subscriptions, 2);
    server.await.unwrap();
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn websocket_reconnect_during_subscription_creation_does_not_duplicate_subscriptions() {
    let websocket_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let websocket_address = websocket_listener.local_addr().unwrap();
    let subscriptions_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let subscriptions_address = subscriptions_listener.local_addr().unwrap();
    let subscription_count = Arc::new(AtomicUsize::new(0));
    let subscription_count_for_server = Arc::clone(&subscription_count);

    let subscriptions_server = tokio::spawn(async move {
        let (mut stream, _) = subscriptions_listener.accept().await.unwrap();
        assert!(read_http_json(&mut stream).await.is_null());
        write_json_response(&mut stream, "200 OK", &capacity_response(0, 10)).await;
        for id in 1..=6 {
            let (mut stream, _) = subscriptions_listener.accept().await.unwrap();
            let request = read_http_json(&mut stream).await;
            if id == 1 {
                // Keep the first creation in flight while the WebSocket peer sends its
                // reconnect instruction. The client must finish setup once, then inherit
                // those subscriptions on the reconnect URL instead of recreating them.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let body = accepted_subscription_response(&request, id);
            write_json_response(&mut stream, "202 Accepted", &body).await;
            subscription_count_for_server.fetch_add(1, Ordering::SeqCst);
        }
        // The reconnected session re-derives its active count from Twitch
        // instead of reporting the previous session's numbers.
        let (mut stream, _) = subscriptions_listener.accept().await.unwrap();
        assert!(read_http_json(&mut stream).await.is_null());
        write_json_response(
            &mut stream,
            "200 OK",
            &inherited_list_response("session-2", 6),
        )
        .await;
    });

    let websocket_server = tokio::spawn(async move {
        let (stream, _) = websocket_listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
                .send(Message::Text(
                    json!({
                        "metadata": {"message_id":"welcome-1","message_type":"session_welcome"},
                        "payload": {"session": {"id":"session-1","keepalive_timeout_seconds":30,"reconnect_url":null}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        socket
            .send(Message::Text(
                json!({
                    "metadata": {"message_id":"reconnect-1","message_type":"session_reconnect"},
                    "payload": {"session": {"reconnect_url":format!("ws://{websocket_address}")}}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        drop(socket);

        let (stream, _) = websocket_listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
                .send(Message::Text(
                    json!({
                        "metadata": {"message_id":"welcome-2","message_type":"session_welcome"},
                        "payload": {"session": {"id":"session-2","keepalive_timeout_seconds":30,"reconnect_url":null}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        socket
            .send(Message::Text(
                json!({
                    "metadata": {"message_id":"event-1","message_type":"notification"},
                    "payload": {
                        "subscription": {"type":"stream.online"},
                        "event": {"broadcaster_user_id":"100"}
                    }
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        socket.close(None).await.unwrap();
    });

    let mut settings = EventSubClientSettings::new("client", "token");
    settings.source_policy = crate::TransportSourcePolicy::broadcaster_eventsub();
    settings.websocket_url = format!("ws://{websocket_address}");
    settings.subscriptions_url = format!("http://{subscriptions_address}/eventsub");
    let client = EventSubClient::new(settings);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.connect_and_listen(&[streamer()], sender),
    )
    .await
    .unwrap();
    assert!(result.is_ok());

    let mut messages = Vec::new();
    while let Ok(message) = receiver.try_recv() {
        messages.push(message);
    }
    assert_eq!(subscription_count.load(Ordering::SeqCst), 6);
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, super::EventSubConnectionEvent::Heartbeat))
            .count(),
        2
    );

    // The inherited session reports a count verified against Twitch rather than
    // the pre-reconnect number.
    let setups = messages
        .iter()
        .filter_map(|message| match message {
            super::EventSubConnectionEvent::Setup(report) => Some(report.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(setups.len(), 2);
    assert!(!setups[0].verified);
    assert!(setups[1].verified);
    assert_eq!(setups[1].active_subscriptions, 6);
    assert!(messages.iter().any(|message| {
            matches!(
                message,
                super::EventSubConnectionEvent::Event(event)
                    if matches!(event.as_ref(), MinerEvent::Playback { channel_id, .. } if channel_id == "100")
            )
        }));

    websocket_server.await.unwrap();
    subscriptions_server.await.unwrap();
}

#[tokio::test]
async fn strict_canary_mode_rejects_missing_prediction_scope() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(read_http_json(&mut stream).await.is_null());
        write_json_response(&mut stream, "200 OK", &capacity_response(0, 10)).await;
        for (id, status) in [202_u16, 202, 401].into_iter().enumerate() {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_json(&mut stream).await;
            let reason = if status == 401 {
                "Unauthorized"
            } else {
                "Accepted"
            };
            let body = if status == 202 {
                accepted_subscription_response(&request, id + 1)
            } else {
                String::from("{}")
            };
            write_json_response(&mut stream, &format!("{status} {reason}"), &body).await;
        }
    });

    let mut settings = EventSubClientSettings::new("client", "token");
    settings.source_policy = crate::TransportSourcePolicy::broadcaster_eventsub();
    settings.subscriptions_url = format!("http://{address}/eventsub");
    settings.allow_prediction_scope_fallback = false;
    let client = EventSubClient::new(settings);
    let error = client
        .create_subscriptions("session", &[streamer()])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        super::EventSubError::HttpStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            ..
        }
    ));
    server.await.unwrap();
}

#[test]
fn malformed_eventsub_frames_are_rejected_without_panicking() {
    let tracked = [streamer()];
    for frame in [
        "",
        "{",
        "{\"metadata\":{}}",
        "{\"metadata\":{\"message_id\":\"1\",\"message_type\":\"session_welcome\"},\"payload\":{}}",
        "{\"metadata\":{\"message_id\":\"1\",\"message_type\":\"notification\"},\"payload\":{}}",
    ] {
        assert!(parse_eventsub_message(frame, &tracked).is_err());
    }
    let mut state = 0x9e37_79b9_u64;
    for length in 0..1024 {
        let mut bytes = Vec::with_capacity(length);
        for _ in 0..length {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push(u8::try_from(state & u64::from(u8::MAX)).unwrap());
        }
        let text = String::from_utf8_lossy(&bytes);
        let _ = parse_eventsub_message(&text, &tracked);
    }
}
