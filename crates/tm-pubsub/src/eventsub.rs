use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tm_domain::{PredictionDecision, PredictionEvent, PredictionOutcome, Streamer};
use tm_events::{MinerEvent, PlaybackType, PredictionChannelKind};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

pub const EVENTSUB_WEBSOCKET_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
pub const EVENTSUB_SUBSCRIPTIONS_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";
const MAX_SEEN_MESSAGE_IDS: usize = 4096;
const MAX_SUBSCRIPTION_TYPES: usize = 7;
const EVENTSUB_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const EVENTSUB_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct EventSubClientSettings {
    pub client_id: String,
    pub auth_token: String,
    pub websocket_url: String,
    pub subscriptions_url: String,
    pub allow_prediction_scope_fallback: bool,
    pub http_client: reqwest::Client,
}

impl EventSubClientSettings {
    #[must_use]
    pub fn new(client_id: impl Into<String>, auth_token: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            auth_token: auth_token.into(),
            websocket_url: EVENTSUB_WEBSOCKET_URL.to_string(),
            subscriptions_url: EVENTSUB_SUBSCRIPTIONS_URL.to_string(),
            allow_prediction_scope_fallback: true,
            http_client: reqwest::Client::new(),
        }
    }
}

pub struct EventSubClient {
    settings: EventSubClientSettings,
}

#[derive(Debug, Error)]
pub enum EventSubError {
    #[error("eventsub websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("eventsub http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("eventsub response status {status} for {context}")]
    HttpStatus {
        status: StatusCode,
        context: &'static str,
    },
    #[error("eventsub protocol error: {0}")]
    Protocol(&'static str),
    #[error("eventsub frame is not valid JSON")]
    Json(#[from] serde_json::Error),
    #[error("eventsub timestamp is invalid")]
    Timestamp,
    #[error("eventsub subscription was revoked: {reason}")]
    Revoked { reason: String },
    #[error("eventsub has no usable subscriptions")]
    NoSubscriptions,
    #[error("eventsub operation timed out: {0}")]
    Timeout(&'static str),
    #[error("eventsub reconnect requested")]
    ReconnectRequested { reconnect_url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventSubConnectionEvent {
    Heartbeat,
    Event(Box<MinerEvent>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventSubMessage {
    Welcome {
        session_id: String,
        keepalive_timeout: Duration,
        reconnect_url: Option<String>,
    },
    Keepalive,
    Reconnect {
        reconnect_url: String,
    },
    Revocation {
        reason: String,
    },
    Notification {
        message_id: String,
        subscription_type: String,
        event: Value,
    },
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    metadata: RawMetadata,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct RawMetadata {
    message_id: String,
    message_type: String,
}

#[derive(Debug, Deserialize)]
struct WelcomePayload {
    session: WelcomeSession,
}

#[derive(Debug, Deserialize)]
struct WelcomeSession {
    id: String,
    keepalive_timeout_seconds: u64,
    reconnect_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReconnectPayload {
    session: ReconnectSession,
}

#[derive(Debug, Deserialize)]
struct ReconnectSession {
    reconnect_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RevocationPayload {
    subscription: RevokedSubscription,
}

#[derive(Debug, Deserialize)]
struct RevokedSubscription {
    status: String,
}

#[derive(Debug, Deserialize)]
struct NotificationPayload {
    subscription: NotificationSubscription,
    event: Value,
}

#[derive(Debug, Deserialize)]
struct NotificationSubscription {
    #[serde(rename = "type")]
    subscription_type: String,
}

#[derive(Debug, Deserialize)]
struct StreamOnlineEvent {
    broadcaster_user_id: String,
}

#[derive(Debug, Deserialize)]
struct StreamOfflineEvent {
    broadcaster_user_id: String,
}

#[derive(Debug, Deserialize)]
struct RaidEvent {
    from_broadcaster_user_id: String,
    to_broadcaster_user_login: String,
}

#[derive(Debug, Deserialize)]
struct PredictionEventWire {
    id: String,
    broadcaster_user_id: String,
    title: String,
    outcomes: Vec<PredictionOutcomeWire>,
    started_at: String,
    #[serde(default)]
    locks_at: Option<String>,
    status: String,
    #[serde(default)]
    winning_outcome_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PredictionOutcomeWire {
    id: String,
    title: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    users: i64,
    #[serde(default)]
    channel_points: i64,
    #[serde(default)]
    top_predictors: Vec<PredictionTopPredictorWire>,
}

#[derive(Debug, Deserialize)]
struct PredictionTopPredictorWire {
    #[serde(default)]
    channel_points_used: i64,
}

pub fn parse_eventsub_message(
    raw: &str,
    tracked_streamers: &[Streamer],
) -> Result<EventSubMessage, EventSubError> {
    let raw: RawMessage = serde_json::from_str(raw)?;
    if raw.metadata.message_id.trim().is_empty() {
        return Err(EventSubError::Protocol("message_id is empty"));
    }
    match raw.metadata.message_type.as_str() {
        "session_welcome" => {
            let payload: WelcomePayload = serde_json::from_value(raw.payload)?;
            if payload.session.id.trim().is_empty() {
                return Err(EventSubError::Protocol("welcome session id is empty"));
            }
            if payload.session.keepalive_timeout_seconds == 0 {
                return Err(EventSubError::Protocol("welcome keepalive timeout is zero"));
            }
            Ok(EventSubMessage::Welcome {
                session_id: payload.session.id,
                keepalive_timeout: Duration::from_secs(payload.session.keepalive_timeout_seconds),
                reconnect_url: payload.session.reconnect_url,
            })
        }
        "session_keepalive" => Ok(EventSubMessage::Keepalive),
        "session_reconnect" => {
            let payload: ReconnectPayload = serde_json::from_value(raw.payload)?;
            let reconnect_url = payload
                .session
                .reconnect_url
                .filter(|url| !url.trim().is_empty())
                .ok_or(EventSubError::Protocol("reconnect URL is missing"))?;
            Ok(EventSubMessage::Reconnect { reconnect_url })
        }
        "revocation" => {
            let payload: RevocationPayload = serde_json::from_value(raw.payload)?;
            Ok(EventSubMessage::Revocation {
                reason: payload.subscription.status,
            })
        }
        "notification" => {
            let payload: NotificationPayload = serde_json::from_value(raw.payload)?;
            if payload.subscription.subscription_type.trim().is_empty() {
                return Err(EventSubError::Protocol(
                    "notification subscription type is empty",
                ));
            }
            // Validate and normalize supported payloads at the transport boundary. Unsupported
            // EventSub notifications are deliberately rejected instead of becoming empty events.
            let _ = event_from_notification(
                &payload.subscription.subscription_type,
                &payload.event,
                tracked_streamers,
            )?;
            Ok(EventSubMessage::Notification {
                message_id: raw.metadata.message_id,
                subscription_type: payload.subscription.subscription_type,
                event: payload.event,
            })
        }
        _ => Err(EventSubError::Protocol("unknown message type")),
    }
}

impl EventSubClient {
    #[must_use]
    pub fn new(settings: EventSubClientSettings) -> Self {
        Self { settings }
    }

    pub async fn connect_and_listen(
        &self,
        tracked_streamers: &[Streamer],
        sender: mpsc::Sender<EventSubConnectionEvent>,
    ) -> Result<(), EventSubError> {
        if tracked_streamers.is_empty() {
            return Err(EventSubError::NoSubscriptions);
        }

        let mut deduper = MessageDeduper::default();
        let mut websocket_url = self.settings.websocket_url.clone();
        let mut inherited_subscriptions = false;
        loop {
            let (mut socket, _) =
                tokio::time::timeout(EVENTSUB_CONNECT_TIMEOUT, connect_async(&websocket_url))
                    .await
                    .map_err(|_| EventSubError::Timeout("websocket connect"))??;
            let welcome = read_welcome(&mut socket, tracked_streamers).await?;
            let EventSubMessage::Welcome {
                session_id,
                keepalive_timeout,
                ..
            } = welcome
            else {
                return Err(EventSubError::Protocol("welcome message was not decoded"));
            };
            if !inherited_subscriptions {
                let subscribed = self
                    .create_subscriptions(&session_id, tracked_streamers)
                    .await?;
                if subscribed == 0 {
                    return Err(EventSubError::NoSubscriptions);
                }
            }
            sender
                .send(EventSubConnectionEvent::Heartbeat)
                .await
                .map_err(|_| EventSubError::Protocol("event channel closed"))?;

            match listen_socket(
                &mut socket,
                tracked_streamers,
                &sender,
                &mut deduper,
                keepalive_timeout,
            )
            .await
            {
                Err(EventSubError::ReconnectRequested { reconnect_url }) => {
                    // Twitch keeps the subscriptions attached to the reconnect URL. Do not
                    // recreate them, which would produce duplicate-subscription errors.
                    websocket_url = reconnect_url;
                    inherited_subscriptions = true;
                }
                result => return result,
            }
        }
    }

    async fn create_subscriptions(
        &self,
        session_id: &str,
        tracked_streamers: &[Streamer],
    ) -> Result<usize, EventSubError> {
        let requests = subscription_requests(session_id, tracked_streamers);
        let mut created = 0;
        for (subscription_type, condition) in requests {
            match self
                .create_subscription(&subscription_type, session_id, condition)
                .await
            {
                Ok(()) => created += 1,
                Err(error @ EventSubError::HttpStatus { status, .. })
                    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                        && subscription_type.starts_with("channel.prediction.") =>
                {
                    if !self.settings.allow_prediction_scope_fallback {
                        return Err(error);
                    }
                    // Existing sessions may predate the optional prediction scope. Keep
                    // stream presence available and leave prediction decisions to GQL/polling.
                    tracing::warn!(
                        error_class = "eventsub-scope",
                        subscription_type,
                        "EventSub prediction subscription was not authorized"
                    );
                }
                Err(error) => return Err(error),
            }
        }
        Ok(created)
    }

    async fn create_subscription(
        &self,
        subscription_type: &str,
        session_id: &str,
        condition: Value,
    ) -> Result<(), EventSubError> {
        let response = tokio::time::timeout(
            EVENTSUB_HTTP_TIMEOUT,
            self.settings
                .http_client
                .post(&self.settings.subscriptions_url)
                .header(
                    "Authorization",
                    format!("Bearer {}", self.settings.auth_token),
                )
                .header("Client-Id", &self.settings.client_id)
                .header("Content-Type", "application/json")
                .json(&json!({
                    "type": subscription_type,
                    "version": "1",
                    "condition": condition,
                    "transport": {
                        "method": "websocket",
                        "session_id": session_id,
                    }
                }))
                .send(),
        )
        .await
        .map_err(|_| EventSubError::Timeout("create subscription"))??;
        if !response.status().is_success() {
            return Err(EventSubError::HttpStatus {
                status: response.status(),
                context: "create eventsub subscription",
            });
        }
        Ok(())
    }
}

async fn read_welcome<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    tracked_streamers: &[Streamer],
) -> Result<EventSubMessage, EventSubError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(message) = socket.next().await {
        match message? {
            Message::Text(text) => {
                let parsed = parse_eventsub_message(text.as_ref(), tracked_streamers)?;
                if matches!(parsed, EventSubMessage::Welcome { .. }) {
                    return Ok(parsed);
                }
                return Err(EventSubError::Protocol("welcome was not the first message"));
            }
            Message::Binary(bytes) => {
                let text = String::from_utf8(bytes.to_vec())
                    .map_err(|_| EventSubError::Protocol("binary frame is not UTF-8"))?;
                let parsed = parse_eventsub_message(&text, tracked_streamers)?;
                if matches!(parsed, EventSubMessage::Welcome { .. }) {
                    return Ok(parsed);
                }
                return Err(EventSubError::Protocol("welcome was not the first message"));
            }
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload)).await?;
            }
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => return Err(EventSubError::Protocol("closed before welcome")),
        }
    }
    Err(EventSubError::Protocol("socket ended before welcome"))
}

async fn listen_socket<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    tracked_streamers: &[Streamer],
    sender: &mpsc::Sender<EventSubConnectionEvent>,
    deduper: &mut MessageDeduper,
    keepalive_timeout: Duration,
) -> Result<(), EventSubError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let Some(message) = tokio::time::timeout(keepalive_timeout, socket.next())
            .await
            .map_err(|_| EventSubError::Protocol("eventsub keepalive timeout"))?
        else {
            return Ok(());
        };
        match message? {
            Message::Text(text) => {
                match parse_eventsub_message(text.as_ref(), tracked_streamers)? {
                    EventSubMessage::Keepalive => sender
                        .send(EventSubConnectionEvent::Heartbeat)
                        .await
                        .map_err(|_| EventSubError::Protocol("event channel closed"))?,
                    EventSubMessage::Notification {
                        message_id,
                        subscription_type,
                        event,
                    } => {
                        if deduper.insert(message_id) {
                            let event = event_from_notification(
                                &subscription_type,
                                &event,
                                tracked_streamers,
                            )?;
                            sender
                                .send(EventSubConnectionEvent::Event(Box::new(event)))
                                .await
                                .map_err(|_| EventSubError::Protocol("event channel closed"))?;
                        }
                    }
                    EventSubMessage::Reconnect { reconnect_url } => {
                        return Err(EventSubError::ReconnectRequested { reconnect_url });
                    }
                    EventSubMessage::Revocation { reason } => {
                        return Err(EventSubError::Revoked { reason });
                    }
                    EventSubMessage::Welcome { .. } => {
                        return Err(EventSubError::Protocol("unexpected welcome message"));
                    }
                }
            }
            Message::Binary(bytes) => {
                let text = String::from_utf8(bytes.to_vec())
                    .map_err(|_| EventSubError::Protocol("binary frame is not UTF-8"))?;
                match parse_eventsub_message(&text, tracked_streamers)? {
                    EventSubMessage::Keepalive => sender
                        .send(EventSubConnectionEvent::Heartbeat)
                        .await
                        .map_err(|_| EventSubError::Protocol("event channel closed"))?,
                    EventSubMessage::Notification {
                        message_id,
                        subscription_type,
                        event,
                    } => {
                        if deduper.insert(message_id) {
                            let event = event_from_notification(
                                &subscription_type,
                                &event,
                                tracked_streamers,
                            )?;
                            sender
                                .send(EventSubConnectionEvent::Event(Box::new(event)))
                                .await
                                .map_err(|_| EventSubError::Protocol("event channel closed"))?;
                        }
                    }
                    EventSubMessage::Reconnect { reconnect_url } => {
                        return Err(EventSubError::ReconnectRequested { reconnect_url });
                    }
                    EventSubMessage::Revocation { reason } => {
                        return Err(EventSubError::Revoked { reason });
                    }
                    EventSubMessage::Welcome { .. } => {
                        return Err(EventSubError::Protocol("unexpected welcome message"));
                    }
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => return Ok(()),
        }
    }
}

fn subscription_requests(
    _session_id: &str,
    tracked_streamers: &[Streamer],
) -> Vec<(String, Value)> {
    let mut requests = Vec::new();
    for streamer in tracked_streamers {
        if streamer.channel_id.trim().is_empty() {
            continue;
        }
        requests.push((
            String::from("stream.online"),
            json!({ "broadcaster_user_id": streamer.channel_id }),
        ));
        requests.push((
            String::from("stream.offline"),
            json!({ "broadcaster_user_id": streamer.channel_id }),
        ));
        if streamer.settings.follow_raid {
            requests.push((
                String::from("channel.raid"),
                json!({ "from_broadcaster_user_id": streamer.channel_id }),
            ));
        }
        if streamer.settings.make_predictions {
            requests.push((
                String::from("channel.prediction.begin"),
                json!({ "broadcaster_user_id": streamer.channel_id }),
            ));
            requests.push((
                String::from("channel.prediction.progress"),
                json!({ "broadcaster_user_id": streamer.channel_id }),
            ));
            requests.push((
                String::from("channel.prediction.lock"),
                json!({ "broadcaster_user_id": streamer.channel_id }),
            ));
            requests.push((
                String::from("channel.prediction.end"),
                json!({ "broadcaster_user_id": streamer.channel_id }),
            ));
        }
    }
    requests.truncate(MAX_SUBSCRIPTION_TYPES.saturating_mul(tracked_streamers.len()));
    requests
}

fn event_from_notification(
    subscription_type: &str,
    event: &Value,
    tracked_streamers: &[Streamer],
) -> Result<MinerEvent, EventSubError> {
    match subscription_type {
        "stream.online" => {
            let value: StreamOnlineEvent = serde_json::from_value(event.clone())?;
            ensure_tracked(&value.broadcaster_user_id, tracked_streamers)?;
            Ok(MinerEvent::Playback {
                channel_id: value.broadcaster_user_id,
                kind: PlaybackType::StreamUp,
            })
        }
        "stream.offline" => {
            let value: StreamOfflineEvent = serde_json::from_value(event.clone())?;
            ensure_tracked(&value.broadcaster_user_id, tracked_streamers)?;
            Ok(MinerEvent::Playback {
                channel_id: value.broadcaster_user_id,
                kind: PlaybackType::StreamDown,
            })
        }
        "channel.raid" => {
            let value: RaidEvent = serde_json::from_value(event.clone())?;
            ensure_tracked(&value.from_broadcaster_user_id, tracked_streamers)?;
            Ok(MinerEvent::Raid {
                channel_id: value.from_broadcaster_user_id,
                // EventSub's channel.raid notification does not expose the legacy raid ID.
                // Keeping this empty makes the runtime observe the event without issuing an
                // unsafe JoinRaid mutation with an invented identifier.
                raid_id: String::new(),
                target_login: value.to_broadcaster_user_login,
            })
        }
        "channel.prediction.begin"
        | "channel.prediction.progress"
        | "channel.prediction.lock"
        | "channel.prediction.end" => {
            let value: PredictionEventWire = serde_json::from_value(event.clone())?;
            let streamer = tracked_streamers
                .iter()
                .find(|streamer| streamer.channel_id == value.broadcaster_user_id)
                .ok_or(EventSubError::Protocol(
                    "prediction broadcaster is not tracked",
                ))?;
            let kind = if subscription_type == "channel.prediction.begin" {
                PredictionChannelKind::EventCreated
            } else {
                PredictionChannelKind::EventUpdated
            };
            Ok(MinerEvent::PredictionChannel {
                kind,
                event: Box::new(prediction_event_from_wire(&value, streamer)?),
                winning_outcome_id: value.winning_outcome_id,
            })
        }
        _ => Err(EventSubError::Protocol("unsupported EventSub subscription")),
    }
}

fn prediction_event_from_wire(
    value: &PredictionEventWire,
    streamer: &Streamer,
) -> Result<PredictionEvent, EventSubError> {
    let created_at =
        OffsetDateTime::parse(&value.started_at, &Rfc3339).map_err(|_| EventSubError::Timestamp)?;
    let window_seconds = value
        .locks_at
        .as_deref()
        .and_then(|locks_at| OffsetDateTime::parse(locks_at, &Rfc3339).ok())
        .map(|locks_at| (locks_at - created_at).as_seconds_f64())
        .unwrap_or_default();
    let mut event = PredictionEvent {
        streamer: streamer.clone(),
        event_id: value.id.clone(),
        title: value.title.clone(),
        status: value.status.trim().to_uppercase(),
        created_at,
        window_seconds,
        outcomes: value
            .outcomes
            .iter()
            .map(|outcome| PredictionOutcome {
                id: outcome.id.clone(),
                title: outcome.title.clone(),
                color: outcome.color.clone(),
                total_users: outcome.users,
                total_points: outcome.channel_points,
                top_points: outcome
                    .top_predictors
                    .first()
                    .map_or(0, |predictor| predictor.channel_points_used),
                ..PredictionOutcome::default()
            })
            .collect(),
        decision: PredictionDecision::default(),
        bet_placed: false,
        bet_confirmed: false,
        result_type: String::new(),
        result_string: String::new(),
    };
    event.update_outcomes();
    Ok(event)
}

fn ensure_tracked(channel_id: &str, tracked_streamers: &[Streamer]) -> Result<(), EventSubError> {
    tracked_streamers
        .iter()
        .any(|streamer| streamer.channel_id == channel_id)
        .then_some(())
        .ok_or(EventSubError::Protocol("event broadcaster is not tracked"))
}

#[derive(Default)]
struct MessageDeduper {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl MessageDeduper {
    fn insert(&mut self, message_id: String) -> bool {
        if !self.ids.insert(message_id.clone()) {
            return false;
        }
        self.order.push_back(message_id);
        while self.order.len() > MAX_SEEN_MESSAGE_IDS {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::{
        event_from_notification, parse_eventsub_message, subscription_requests, EventSubClient,
        EventSubClientSettings, EventSubMessage, MessageDeduper,
    };
    use futures_util::SinkExt;
    use serde_json::json;
    use tm_domain::{IrcMode, Streamer, StreamerSettings};
    use tm_events::MinerEvent;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

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
                        "outcomes":[{"id":"outcome-1","title":"Yes","color":"BLUE"}],
                        "started_at":"2026-07-12T10:00:00Z","locks_at":"2026-07-12T10:01:00Z",
                        "status":"ACTIVE"
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
                        "outcomes":[{"id":"outcome-1","title":"Yes","color":"BLUE"}],
                        "started_at":"2026-07-12T10:00:00Z","locks_at":"2026-07-12T10:01:00Z",
                        "status":"LOCKED"
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
    fn raid_subscription_is_only_requested_when_follow_raid_is_enabled() {
        let without_raid = subscription_requests("session", &[streamer()]);
        assert!(!without_raid.iter().any(|(kind, _)| kind == "channel.raid"));

        let mut raid_streamer = streamer();
        raid_streamer.settings.follow_raid = true;
        let with_raid = subscription_requests("session", &[raid_streamer]);
        assert!(with_raid.iter().any(|(kind, _)| kind == "channel.raid"));
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn websocket_reconnects_without_duplicate_subscriptions_and_delivers_events() {
        let websocket_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let websocket_address = websocket_listener.local_addr().unwrap();
        let subscriptions_listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let subscriptions_address = subscriptions_listener.local_addr().unwrap();
        let subscription_count = Arc::new(AtomicUsize::new(0));
        let subscription_count_for_server = Arc::clone(&subscription_count);

        let subscriptions_server = tokio::spawn(async move {
            for _ in 0..6 {
                let (mut stream, _) = subscriptions_listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 16 * 1024);
                }
                stream
                    .write_all(
                        b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                subscription_count_for_server.fetch_add(1, Ordering::SeqCst);
            }
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
            for status in [202_u16, 202, 401] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).await.unwrap();
                let reason = if status == 401 {
                    "Unauthorized"
                } else {
                    "Accepted"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut settings = EventSubClientSettings::new("client", "token");
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
}
