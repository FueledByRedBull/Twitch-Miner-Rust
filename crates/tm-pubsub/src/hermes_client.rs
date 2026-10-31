use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use tm_domain::Streamer;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::client::PubSubConnectionEvent;
use crate::errors::PubSubError;
use crate::hermes::{HermesObservation, HermesObserver, HermesObserverError};
use crate::topics::pubsub_topic_class;

pub const HERMES_WEBSOCKET_URL: &str = "wss://hermes.twitch.tv/v1";

/// Used until the server's welcome frame states its keepalive interval.
const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(60);

/// Carries the same topics and events as a `PubSub` connection over Hermes, for
/// when `PubSub` itself is unavailable.
pub async fn connect_hermes_topics_and_listen(
    url: &str,
    topics: &[String],
    auth_token: &str,
    tracked_streamers: &[Streamer],
    sender: mpsc::Sender<PubSubConnectionEvent>,
) -> Result<(), PubSubError> {
    let (mut socket, _) = connect_async(url).await?;
    let subscriptions: HashMap<String, String> = topics
        .iter()
        .enumerate()
        .map(|(index, topic)| (format!("subscription-{index}"), topic.clone()))
        .collect();
    let mut observer = HermesObserver::new(subscriptions.clone()).map_err(protocol_error)?;
    let mut keepalive = DEFAULT_KEEPALIVE;

    loop {
        // Hermes promises a frame each keepalive interval; twice that means it died.
        let next = tokio::time::timeout(keepalive.saturating_mul(2), socket.next()).await;
        let Ok(message) = next else {
            return Err(PubSubError::PongTimeout);
        };
        let Some(message) = message else {
            return Ok(());
        };
        let raw = match message? {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8(bytes.to_vec())?,
            Message::Close(_) => return Ok(()),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        };
        let frame: Value = serde_json::from_str(&raw)?;
        match frame
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "welcome" => {
                observer
                    .observe_frame(&raw, tracked_streamers)
                    .map_err(protocol_error)?;
                if let Some(seconds) = frame
                    .pointer("/welcome/keepaliveSec")
                    .and_then(Value::as_u64)
                {
                    keepalive = Duration::from_secs(seconds);
                }
                let request = request("authenticate", json!({ "token": auth_token }), "auth");
                socket
                    .send(Message::Text(request.to_string().into()))
                    .await?;
            }
            "authenticateResponse" => {
                if frame
                    .pointer("/authenticateResponse/result")
                    .and_then(Value::as_str)
                    != Some("ok")
                {
                    // Not `BadAuth`: this token may only fail on this transport, and
                    // the session check decides whether the session itself is dead.
                    return Err(PubSubError::Protocol("Hermes rejected authentication"));
                }
                for (id, topic) in &subscriptions {
                    let body = json!({ "id": id, "type": "pubsub", "pubsub": { "topic": topic } });
                    let request = request("subscribe", body, id);
                    socket
                        .send(Message::Text(request.to_string().into()))
                        .await?;
                }
            }
            "subscribeResponse" => {
                let id = frame
                    .pointer("/subscribeResponse/subscription/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(topic) = subscriptions.get(id) else {
                    continue;
                };
                let topic_class = pubsub_topic_class(topic).to_string();
                let event = if frame
                    .pointer("/subscribeResponse/result")
                    .and_then(Value::as_str)
                    == Some("ok")
                {
                    PubSubConnectionEvent::ListenAcknowledged { topic_class }
                } else {
                    PubSubConnectionEvent::ResponseError {
                        nonce: None,
                        topic_class: Some(topic_class),
                    }
                };
                sender
                    .send(event)
                    .await
                    .map_err(|_| PubSubError::EventChannelClosed)?;
            }
            "keepalive" => {
                sender
                    .send(PubSubConnectionEvent::Heartbeat)
                    .await
                    .map_err(|_| PubSubError::EventChannelClosed)?;
            }
            "notification" => {
                if let HermesObservation::Event(event) = observer
                    .observe_frame(&raw, tracked_streamers)
                    .map_err(protocol_error)?
                {
                    sender
                        .send(PubSubConnectionEvent::Event(event))
                        .await
                        .map_err(|_| PubSubError::EventChannelClosed)?;
                }
            }
            "reconnect" => return Err(PubSubError::ReconnectRequested),
            _ => {}
        }
    }
}

fn request(kind: &str, body: Value, id: &str) -> Value {
    let timestamp = time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let mut request =
        json!({ "id": format!("request-{id}"), "type": kind, "timestamp": timestamp });
    request[kind] = body;
    request
}

fn protocol_error(error: HermesObserverError) -> PubSubError {
    match error {
        HermesObserverError::InvalidJson(error) => PubSubError::InvalidPayload(error),
        HermesObserverError::PubSub(error) => error,
        HermesObserverError::FrameTooLarge => PubSubError::Protocol("Hermes frame is too large"),
        HermesObserverError::Protocol(message) => PubSubError::Protocol(message),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    const WELCOME: &str = r#"{"id":"w","type":"welcome","timestamp":"2030-01-01T00:00:00Z","welcome":{"keepaliveSec":30,"recoveryUrl":"wss://r","sessionId":"s"}}"#;

    async fn serve_one(
        authenticate_result: &'static str,
    ) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut received = Vec::new();
            socket.send(Message::Text(WELCOME.into())).await.unwrap();
            let auth = socket.next().await.unwrap().unwrap().into_text().unwrap();
            received.push(serde_json::from_str::<Value>(auth.as_ref()).unwrap());
            let response = json!({
                "type": "authenticateResponse",
                "authenticateResponse": { "result": authenticate_result }
            });
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
            if authenticate_result != "ok" {
                return received;
            }
            let subscribe = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let subscribe: Value = serde_json::from_str(subscribe.as_ref()).unwrap();
            let id = subscribe["subscribe"]["id"].as_str().unwrap().to_string();
            received.push(subscribe);
            let frames = [
                json!({
                    "type": "subscribeResponse",
                    "subscribeResponse": { "result": "ok", "subscription": { "id": id } }
                }),
                json!({ "type": "keepalive" }),
                json!({
                    "id": "n",
                    "type": "notification",
                    "timestamp": "2030-01-01T00:00:00Z",
                    "notification": {
                        "type": "pubsub",
                        "subscription": { "id": id },
                        "pubsub": r#"{"type":"drop-claim","data":{"drop_instance_id":"i"}}"#
                    }
                }),
                json!({ "type": "reconnect" }),
            ];
            for frame in frames {
                socket
                    .send(Message::Text(frame.to_string().into()))
                    .await
                    .unwrap();
            }
            received
        });
        (format!("ws://{address}"), server)
    }

    #[tokio::test]
    async fn hermes_connection_authenticates_subscribes_and_delivers_events() {
        let (url, server) = serve_one("ok").await;
        let (sender, mut receiver) = mpsc::channel(8);
        let result = connect_hermes_topics_and_listen(
            &url,
            &[String::from("user-drop-events.viewer")],
            "token",
            &[],
            sender,
        )
        .await;
        assert!(matches!(result, Err(PubSubError::ReconnectRequested)));

        let received = server.await.unwrap();
        assert_eq!(received[0]["type"], "authenticate");
        assert_eq!(received[0]["authenticate"]["token"], "token");
        assert_eq!(
            received[1]["subscribe"]["pubsub"]["topic"],
            "user-drop-events.viewer"
        );
        assert_eq!(
            receiver.recv().await,
            Some(PubSubConnectionEvent::ListenAcknowledged {
                topic_class: String::from("drops")
            })
        );
        assert_eq!(
            receiver.recv().await,
            Some(PubSubConnectionEvent::Heartbeat)
        );
        assert!(matches!(
            receiver.recv().await,
            Some(PubSubConnectionEvent::Event(_))
        ));
    }

    #[tokio::test]
    async fn rejected_hermes_authentication_is_not_bad_auth() {
        let (url, server) = serve_one("error").await;
        let (sender, _receiver) = mpsc::channel(8);
        let result = connect_hermes_topics_and_listen(&url, &[], "token", &[], sender).await;
        assert!(matches!(result, Err(PubSubError::Protocol(_))));
        server.await.unwrap();
    }
}
