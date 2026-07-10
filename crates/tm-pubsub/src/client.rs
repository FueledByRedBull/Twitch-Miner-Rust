use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tm_domain::Streamer;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::errors::PubSubError;
use crate::parse::{bad_auth_cookie_file, parse_transport_message};
use crate::topics::{build_topics, listen_payloads, ping_payload};
use crate::types::{IncomingTransportMessage, PubSubEvent};
use crate::WEBSOCKET_URL;

const PING_JITTER_SECONDS: u64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubSubClientSettings {
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PubSubConnectionEvent {
    Heartbeat,
    Event(Box<PubSubEvent>),
    ResponseError {
        error: String,
        nonce: Option<String>,
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
        let (mut socket, _) = connect_async(WEBSOCKET_URL).await?;
        for payload in listen_payloads(topics, auth_token) {
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
                    if last_pong.elapsed() > self.settings.pong_timeout {
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

async fn handle_transport_frame(
    raw: &str,
    tracked_streamers: &[Streamer],
    username: Option<&str>,
    sender: &mpsc::Sender<PubSubConnectionEvent>,
    last_pong: &mut Instant,
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
        IncomingTransportMessage::ResponseError {
            error,
            nonce,
            is_bad_auth,
        } => {
            if is_bad_auth {
                return Err(PubSubError::BadAuth {
                    cookie_file: bad_auth_cookie_file(username),
                    error,
                });
            }
            sender
                .send(PubSubConnectionEvent::ResponseError { error, nonce })
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
