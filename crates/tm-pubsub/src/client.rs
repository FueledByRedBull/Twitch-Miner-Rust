use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tm_domain::Streamer;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::errors::PubSubError;
use crate::parse::{bad_auth_cookie_file, parse_transport_message};
use crate::topics::{build_topics, listen_payloads, ping_payload, pubsub_topic_class};
use crate::types::{IncomingTransportMessage, PubSubEvent};
use crate::WEBSOCKET_URL;

const PING_JITTER_SECONDS: u64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubSubClientSettings {
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
    pub websocket_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PubSubConnectionEvent {
    Heartbeat,
    ListenAcknowledged {
        topic_class: String,
    },
    Event(Box<PubSubEvent>),
    ResponseError {
        nonce: Option<String>,
        topic_class: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubSubClient {
    settings: PubSubClientSettings,
}

impl Default for PubSubClientSettings {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(25),
            pong_timeout: Duration::from_secs(5 * 60),
            websocket_url: WEBSOCKET_URL.to_string(),
        }
    }
}

impl Default for PubSubClient {
    fn default() -> Self {
        Self::new(PubSubClientSettings::default())
    }
}

impl PubSubClient {
    #[must_use]
    pub fn new(settings: PubSubClientSettings) -> Self {
        Self { settings }
    }

    pub async fn connect_and_listen(
        &self,
        user_id: &str,
        auth_token: &str,
        username: Option<&str>,
        tracked_streamers: &[Streamer],
        sender: mpsc::Sender<PubSubConnectionEvent>,
    ) -> Result<(), PubSubError> {
        let topics = build_topics(user_id, tracked_streamers)?;
        self.connect_topics_and_listen(&topics, auth_token, username, tracked_streamers, sender)
            .await
    }

    pub async fn connect_topics_and_listen(
        &self,
        topics: &[String],
        auth_token: &str,
        username: Option<&str>,
        tracked_streamers: &[Streamer],
        sender: mpsc::Sender<PubSubConnectionEvent>,
    ) -> Result<(), PubSubError> {
        let (mut socket, _) = connect_async(&self.settings.websocket_url).await?;
        let mut pending_listens = HashMap::new();
        for (topic, payload) in topics.iter().zip(listen_payloads(topics, auth_token)) {
            if let Some(nonce) = payload.get("nonce").and_then(serde_json::Value::as_str) {
                pending_listens.insert(nonce.to_string(), pubsub_topic_class(topic).to_string());
            }
            socket
                .send(Message::Text(payload.to_string().into()))
                .await?;
        }

        let mut last_pong = Instant::now();
        let mut next_ping = std::pin::pin!(time::sleep(randomized_ping_delay(
            self.settings.ping_interval
        )));

        loop {
            tokio::select! {
                () = &mut next_ping => {
                    if pong_timed_out(last_pong, self.settings.pong_timeout) {
                        return Err(PubSubError::PongTimeout);
                    }
                    socket.send(Message::Text(ping_payload().to_string().into())).await?;
                    next_ping
                        .as_mut()
                        .reset(Instant::now() + randomized_ping_delay(self.settings.ping_interval));
                }
                message = socket.next() => {
                    let Some(message) = message else {
                        return Ok(());
                    };
                    match message? {
                        Message::Text(text) => {
                            handle_transport_frame(
                                text.as_ref(),
                                tracked_streamers,
                                username,
                                &sender,
                                &mut last_pong,
                                &mut pending_listens,
                            )
                            .await?;
                        }
                        Message::Binary(bytes) => {
                            let text = String::from_utf8(bytes.to_vec())?;
                            handle_transport_frame(
                                &text,
                                tracked_streamers,
                                username,
                                &sender,
                                &mut last_pong,
                                &mut pending_listens,
                            )
                            .await?;
                        }
                        Message::Ping(payload) => {
                            socket.send(Message::Pong(payload)).await?;
                        }
                        Message::Pong(_) => {
                            last_pong = Instant::now();
                            sender
                                .send(PubSubConnectionEvent::Heartbeat)
                                .await
                                .map_err(|_| PubSubError::EventChannelClosed)?;
                        }
                        Message::Close(_) => return Ok(()),
                        Message::Frame(_) => {}
                    }
                }
            }
        }
    }
}

pub(crate) fn randomized_ping_delay(base: Duration) -> Duration {
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % (PING_JITTER_SECONDS + 1);
    base + Duration::from_secs(jitter)
}

fn pong_timed_out(last_pong: Instant, timeout: Duration) -> bool {
    last_pong.elapsed() > timeout
}

async fn handle_transport_frame(
    raw: &str,
    tracked_streamers: &[Streamer],
    username: Option<&str>,
    sender: &mpsc::Sender<PubSubConnectionEvent>,
    last_pong: &mut Instant,
    pending_listens: &mut HashMap<String, String>,
) -> Result<(), PubSubError> {
    match parse_transport_message(raw, tracked_streamers)? {
        IncomingTransportMessage::Pong => {
            *last_pong = Instant::now();
            sender
                .send(PubSubConnectionEvent::Heartbeat)
                .await
                .map_err(|_| PubSubError::EventChannelClosed)?;
        }
        IncomingTransportMessage::Reconnect => {
            return Err(PubSubError::ReconnectRequested);
        }
        IncomingTransportMessage::ResponseOk { nonce } => {
            if let Some(topic_class) = nonce
                .as_deref()
                .and_then(|nonce| pending_listens.remove(nonce))
            {
                sender
                    .send(PubSubConnectionEvent::ListenAcknowledged { topic_class })
                    .await
                    .map_err(|_| PubSubError::EventChannelClosed)?;
            }
        }
        IncomingTransportMessage::ResponseError { nonce, is_bad_auth } => {
            let topic_class = nonce
                .as_deref()
                .and_then(|nonce| pending_listens.remove(nonce));
            if is_bad_auth {
                return Err(PubSubError::BadAuth {
                    cookie_file: bad_auth_cookie_file(username),
                });
            }
            sender
                .send(PubSubConnectionEvent::ResponseError { nonce, topic_class })
                .await
                .map_err(|_| PubSubError::EventChannelClosed)?;
        }
        IncomingTransportMessage::Event(event) => {
            sender
                .send(PubSubConnectionEvent::Event(event))
                .await
                .map_err(|_| PubSubError::EventChannelClosed)?;
        }
        IncomingTransportMessage::Ignore => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn partial_listen_results_preserve_success_and_classify_failure() {
        let (sender, mut receiver) = mpsc::channel(4);
        let mut last_pong = Instant::now();
        let mut pending = HashMap::from([
            (String::from("ok"), String::from("points-user")),
            (String::from("failed"), String::from("prediction-channel")),
        ]);

        handle_transport_frame(
            r#"{"type":"RESPONSE","error":"","nonce":"ok"}"#,
            &[],
            None,
            &sender,
            &mut last_pong,
            &mut pending,
        )
        .await
        .unwrap();
        handle_transport_frame(
            r#"{"type":"RESPONSE","error":"ERR_TOPIC","nonce":"failed"}"#,
            &[],
            None,
            &sender,
            &mut last_pong,
            &mut pending,
        )
        .await
        .unwrap();

        assert_eq!(
            receiver.recv().await,
            Some(PubSubConnectionEvent::ListenAcknowledged {
                topic_class: String::from("points-user"),
            })
        );
        assert_eq!(
            receiver.recv().await,
            Some(PubSubConnectionEvent::ResponseError {
                nonce: Some(String::from("failed")),
                topic_class: Some(String::from("prediction-channel")),
            })
        );
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn bad_auth_isolated_as_typed_error_without_emitting_payload() {
        let (sender, mut receiver) = mpsc::channel(1);
        let mut last_pong = Instant::now();
        let mut pending = HashMap::from([(String::from("auth"), String::from("points-user"))]);

        let error = handle_transport_frame(
            r#"{"type":"RESPONSE","error":"ERR_BADAUTH rejected","nonce":"auth"}"#,
            &[],
            None,
            &sender,
            &mut last_pong,
            &mut pending,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, PubSubError::BadAuth { .. }));
        assert!(receiver.try_recv().is_err());
        assert!(pending.is_empty());
    }

    #[test]
    fn pong_timeout_boundary_is_deterministic() {
        assert!(!pong_timed_out(Instant::now(), Duration::from_secs(1)));
        assert!(pong_timed_out(
            Instant::now() - Duration::from_secs(2),
            Duration::from_secs(1)
        ));
    }

    #[tokio::test]
    async fn reconnecting_client_resubscribes_and_acknowledges_listen() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for reconnect in [true, false] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                let message = socket.next().await.unwrap().unwrap();
                let Message::Text(raw) = message else {
                    panic!("expected PubSub LISTEN text frame");
                };
                let payload: serde_json::Value = serde_json::from_str(raw.as_ref()).unwrap();
                assert_eq!(payload["type"], "LISTEN");
                let nonce = payload["nonce"].as_str().unwrap();
                socket
                    .send(Message::Text(
                        serde_json::json!({"type":"RESPONSE","error":"","nonce":nonce})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                if reconnect {
                    socket
                        .send(Message::Text(
                            serde_json::json!({"type":"RECONNECT"}).to_string().into(),
                        ))
                        .await
                        .unwrap();
                } else {
                    socket.close(None).await.unwrap();
                }
            }
        });
        let client = PubSubClient::new(PubSubClientSettings {
            websocket_url: format!("ws://{address}"),
            ..PubSubClientSettings::default()
        });
        let topics = vec![String::from("community-points-user-v1.viewer")];
        let (sender, mut receiver) = mpsc::channel(4);

        assert!(matches!(
            client
                .connect_topics_and_listen(&topics, "token", None, &[], sender.clone())
                .await,
            Err(PubSubError::ReconnectRequested)
        ));
        client
            .connect_topics_and_listen(&topics, "token", None, &[], sender)
            .await
            .unwrap();

        let acknowledgements = [receiver.recv().await, receiver.recv().await];
        assert!(acknowledgements.iter().all(|event| matches!(
            event,
            Some(PubSubConnectionEvent::ListenAcknowledged { topic_class })
                if topic_class == "points-user"
        )));
        server.await.unwrap();
    }
}
