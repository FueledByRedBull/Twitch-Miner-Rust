use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use time::format_description::well_known::Rfc2822;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tm_domain::{PredictionDecision, PredictionEvent, PredictionOutcome, Streamer};
use tm_events::{MinerEvent, PlaybackType, PredictionChannelKind};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::policy::{PredictionSource, TransportSourcePolicy};

pub const EVENTSUB_WEBSOCKET_URL: &str = "wss://eventsub.wss.twitch.tv/ws";
pub const EVENTSUB_SUBSCRIPTIONS_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";
const MAX_SEEN_MESSAGE_IDS: usize = 4096;
const EVENTSUB_MAX_TOTAL_COST: u32 = 10;
const EVENTSUB_ASSUMED_SUBSCRIPTION_COST: u32 = 1;
const EVENTSUB_MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 300;
const EVENTSUB_MAX_LIST_PAGES: usize = 10;
const EVENTSUB_MAX_READ_ATTEMPTS: usize = 3;
const EVENTSUB_MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const EVENTSUB_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const EVENTSUB_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct EventSubClientSettings {
    pub client_id: String,
    pub auth_token: String,
    pub websocket_url: String,
    pub subscriptions_url: String,
    pub allow_prediction_scope_fallback: bool,
    pub source_policy: TransportSourcePolicy,
    pub authorized_prediction_broadcaster_id: Option<String>,
    pub verify_subscriptions: bool,
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
            source_policy: TransportSourcePolicy::viewer_compatibility(),
            authorized_prediction_broadcaster_id: None,
            verify_subscriptions: false,
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
    Setup(Box<EventSubSetupReport>),
    Heartbeat,
    Event(Box<MinerEvent>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSubStreamerCapability {
    pub streamer_index: usize,
    pub presence_source: String,
    pub prediction_source: String,
    pub raid_source: String,
    pub planned_subscription_types: Vec<String>,
    pub active_subscription_types: Vec<String>,
    pub skipped_subscription_types: Vec<String>,
    pub failure_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSubSetupReport {
    pub planned_subscriptions: usize,
    pub active_subscriptions: usize,
    pub failed_subscriptions: usize,
    pub overflow_streamers: usize,
    pub total_cost: u32,
    pub max_total_cost: u32,
    pub verified: bool,
    pub capabilities: Vec<EventSubStreamerCapability>,
}

#[derive(Debug, Clone)]
struct SubscriptionRequest {
    streamer_index: usize,
    subscription_type: String,
    condition: Value,
}

#[derive(Debug, Deserialize)]
struct SubscriptionListResponse {
    data: Vec<SubscriptionResponseEntry>,
    #[serde(rename = "total")]
    _total: u32,
    total_cost: u32,
    max_total_cost: u32,
    pagination: SubscriptionPagination,
}

#[derive(Debug, Deserialize)]
struct SubscriptionCreateResponse {
    data: Vec<SubscriptionResponseEntry>,
    total: u32,
    total_cost: u32,
    max_total_cost: u32,
}

#[derive(Debug, Deserialize)]
struct SubscriptionResponseEntry {
    id: String,
    status: String,
    #[serde(rename = "type")]
    subscription_type: String,
    cost: u32,
    condition: Value,
    transport: SubscriptionTransport,
}

#[derive(Debug, Deserialize)]
struct SubscriptionTransport {
    method: String,
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct SubscriptionPagination {
    #[serde(default)]
    cursor: Option<String>,
}

struct CreatedSubscriptionMetadata {
    id: String,
    total_cost: u32,
    max_total_cost: u32,
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
    #[serde(default)]
    locked_at: Option<String>,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    winning_outcome_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PredictionOutcomeWire {
    id: String,
    title: String,
    color: String,
    users: i64,
    channel_points: i64,
    #[serde(default)]
    top_predictors: Vec<PredictionTopPredictorWire>,
}

#[derive(Debug, Deserialize)]
struct PredictionTopPredictorWire {
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
                let report = self
                    .create_subscriptions(&session_id, tracked_streamers)
                    .await?;
                if report.active_subscriptions == 0 {
                    return Err(EventSubError::NoSubscriptions);
                }
                sender
                    .send(EventSubConnectionEvent::Setup(Box::new(report)))
                    .await
                    .map_err(|_| EventSubError::Protocol("event channel closed"))?;
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
    ) -> Result<EventSubSetupReport, EventSubError> {
        let existing = self.list_subscriptions_page(None).await?;
        if existing.max_total_cost == 0 || existing.total_cost > existing.max_total_cost {
            return Err(EventSubError::Protocol(
                "subscription list has invalid cost metadata",
            ));
        }
        let available_cost = existing.max_total_cost - existing.total_cost;
        let (requests, mut report) = subscription_plan_with_capacity(
            tracked_streamers,
            self.settings.source_policy,
            self.settings
                .authorized_prediction_broadcaster_id
                .as_deref(),
            available_cost,
            existing.total_cost,
            existing.max_total_cost,
        );
        let mut created_ids = HashSet::new();
        for request in requests {
            match self
                .create_subscription(&request.subscription_type, session_id, &request.condition)
                .await
            {
                Ok(metadata) => {
                    created_ids.insert(metadata.id);
                    report.active_subscriptions += 1;
                    report.total_cost = metadata.total_cost;
                    report.max_total_cost = metadata.max_total_cost;
                    report.capabilities[request.streamer_index]
                        .active_subscription_types
                        .push(request.subscription_type);
                }
                Err(error @ EventSubError::HttpStatus { status, .. })
                    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                        && request.subscription_type.starts_with("channel.prediction.") =>
                {
                    if !self.settings.allow_prediction_scope_fallback {
                        return Err(error);
                    }
                    record_subscription_failure(&mut report, &request, "unauthorized");
                    // Existing sessions may predate the optional prediction scope. Keep
                    // stream presence available and report the missing prediction capability.
                    tracing::warn!(
                        error_class = "eventsub-scope",
                        subscription_type = %request.subscription_type,
                        "EventSub prediction subscription was not authorized"
                    );
                }
                Err(error) => {
                    let failure_class = subscription_failure_class(&error);
                    record_subscription_failure(&mut report, &request, failure_class);
                    tracing::warn!(
                        error_class = failure_class,
                        subscription_type = %request.subscription_type,
                        "EventSub subscription creation failed; retaining active subscriptions"
                    );
                }
            }
        }
        if self.settings.verify_subscriptions && !created_ids.is_empty() {
            self.verify_created_subscriptions(session_id, &created_ids)
                .await?;
            report.verified = true;
        }
        Ok(report)
    }

    async fn create_subscription(
        &self,
        subscription_type: &str,
        session_id: &str,
        condition: &Value,
    ) -> Result<CreatedSubscriptionMetadata, EventSubError> {
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
        let response: SubscriptionCreateResponse = response.json().await?;
        let [subscription] = response.data.as_slice() else {
            return Err(EventSubError::Protocol(
                "create subscription response must contain exactly one entry",
            ));
        };
        validate_created_subscription(subscription, subscription_type, session_id)?;
        if response.max_total_cost == 0 || response.total_cost > response.max_total_cost {
            return Err(EventSubError::Protocol(
                "create subscription response has invalid cost metadata",
            ));
        }
        let _ = response.total;
        Ok(CreatedSubscriptionMetadata {
            id: subscription.id.clone(),
            total_cost: response.total_cost,
            max_total_cost: response.max_total_cost,
        })
    }

    async fn verify_created_subscriptions(
        &self,
        session_id: &str,
        created_ids: &HashSet<String>,
    ) -> Result<(), EventSubError> {
        let mut enabled_ids = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..EVENTSUB_MAX_LIST_PAGES {
            let response = self.list_subscriptions_page(cursor.as_deref()).await?;
            for subscription in response.data {
                if subscription.transport.method == "websocket"
                    && subscription.transport.session_id == session_id
                    && subscription.status == "enabled"
                {
                    if subscription.id.trim().is_empty() {
                        return Err(EventSubError::Protocol("listed subscription id is empty"));
                    }
                    enabled_ids.insert(subscription.id);
                }
            }
            cursor = response
                .pagination
                .cursor
                .filter(|value| !value.trim().is_empty());
            if cursor.is_none() {
                return if enabled_ids == *created_ids {
                    Ok(())
                } else {
                    Err(EventSubError::Protocol(
                        "listed subscriptions do not match the created session set",
                    ))
                };
            }
        }
        Err(EventSubError::Protocol(
            "subscription list exceeded the bounded page limit",
        ))
    }

    async fn list_subscriptions_page(
        &self,
        cursor: Option<&str>,
    ) -> Result<SubscriptionListResponse, EventSubError> {
        for attempt in 0..EVENTSUB_MAX_READ_ATTEMPTS {
            let mut request = self
                .settings
                .http_client
                .get(&self.settings.subscriptions_url)
                .header(
                    "Authorization",
                    format!("Bearer {}", self.settings.auth_token),
                )
                .header("Client-Id", &self.settings.client_id)
                .query(&[("status", "enabled"), ("first", "100")]);
            if let Some(after) = cursor {
                request = request.query(&[("after", after)]);
            }
            let response = tokio::time::timeout(EVENTSUB_HTTP_TIMEOUT, request.send())
                .await
                .map_err(|_| EventSubError::Timeout("list subscriptions"))??;
            let status = response.status();
            if status.is_success() {
                return response.json().await.map_err(EventSubError::from);
            }
            if attempt + 1 == EVENTSUB_MAX_READ_ATTEMPTS
                || !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            {
                return Err(EventSubError::HttpStatus {
                    status,
                    context: "list eventsub subscriptions",
                });
            }
            tokio::time::sleep(eventsub_retry_delay(&response, attempt)).await;
        }
        Err(EventSubError::Protocol(
            "subscription list retry loop ended unexpectedly",
        ))
    }
}

fn eventsub_retry_delay(response: &reqwest::Response, attempt: usize) -> Duration {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after)
        .or_else(|| {
            response
                .headers()
                .get("ratelimit-reset")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(|unix| OffsetDateTime::from_unix_timestamp(unix).ok())
                .map(|reset| {
                    (reset - OffsetDateTime::now_utc())
                        .whole_seconds()
                        .max(0)
                        .cast_unsigned()
                })
                .map(Duration::from_secs)
        })
        .unwrap_or_else(|| Duration::from_secs(1_u64 << attempt.min(5)))
        .min(EVENTSUB_MAX_RETRY_DELAY)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    OffsetDateTime::parse(value.trim(), &Rfc2822)
        .ok()
        .map(|at| {
            (at - OffsetDateTime::now_utc())
                .whole_seconds()
                .max(0)
                .cast_unsigned()
        })
        .map(Duration::from_secs)
}

fn validate_created_subscription(
    subscription: &SubscriptionResponseEntry,
    expected_type: &str,
    expected_session_id: &str,
) -> Result<(), EventSubError> {
    if subscription.id.trim().is_empty()
        || subscription.status != "enabled"
        || subscription.subscription_type != expected_type
        || subscription.cost > EVENTSUB_MAX_TOTAL_COST
        || !subscription.condition.is_object()
        || subscription.transport.method != "websocket"
        || subscription.transport.session_id != expected_session_id
    {
        return Err(EventSubError::Protocol(
            "create subscription response does not match the request",
        ));
    }
    Ok(())
}

fn subscription_failure_class(error: &EventSubError) -> &'static str {
    match error {
        EventSubError::HttpStatus { status, .. } if matches!(status.as_u16(), 401 | 403) => {
            "unauthorized"
        }
        EventSubError::HttpStatus { status, .. } if status.as_u16() == 429 => "rate-limited",
        EventSubError::HttpStatus { status, .. } if status.is_server_error() => "server-error",
        EventSubError::HttpStatus { .. } => "http-status",
        EventSubError::Timeout(_) => "timeout",
        EventSubError::Http(_) => "http-error",
        EventSubError::Json(_) | EventSubError::Protocol(_) | EventSubError::Timestamp => {
            "protocol"
        }
        EventSubError::WebSocket(_)
        | EventSubError::Revoked { .. }
        | EventSubError::NoSubscriptions
        | EventSubError::ReconnectRequested { .. } => "transport",
    }
}

fn record_subscription_failure(
    report: &mut EventSubSetupReport,
    request: &SubscriptionRequest,
    failure_class: &str,
) {
    report.failed_subscriptions += 1;
    let capability = &mut report.capabilities[request.streamer_index];
    capability
        .skipped_subscription_types
        .push(request.subscription_type.clone());
    capability
        .failure_class
        .get_or_insert_with(|| failure_class.to_string());
    if matches!(
        request.subscription_type.as_str(),
        "stream.online" | "stream.offline"
    ) {
        capability.presence_source = String::from("gql-polling");
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

#[cfg(test)]
fn subscription_requests(session_id: &str, tracked_streamers: &[Streamer]) -> Vec<(String, Value)> {
    subscription_requests_with_policy(
        session_id,
        tracked_streamers,
        TransportSourcePolicy::broadcaster_eventsub(),
    )
}

#[cfg(test)]
fn subscription_requests_with_policy(
    _session_id: &str,
    tracked_streamers: &[Streamer],
    policy: TransportSourcePolicy,
) -> Vec<(String, Value)> {
    subscription_plan(tracked_streamers, policy, None)
        .0
        .into_iter()
        .map(|request| (request.subscription_type, request.condition))
        .collect()
}

#[must_use]
pub fn plan_eventsub_capacity(
    tracked_streamers: &[Streamer],
    policy: TransportSourcePolicy,
) -> EventSubSetupReport {
    subscription_plan(tracked_streamers, policy, None).1
}

fn subscription_plan(
    tracked_streamers: &[Streamer],
    policy: TransportSourcePolicy,
    authorized_prediction_broadcaster_id: Option<&str>,
) -> (Vec<SubscriptionRequest>, EventSubSetupReport) {
    subscription_plan_with_capacity(
        tracked_streamers,
        policy,
        authorized_prediction_broadcaster_id,
        EVENTSUB_MAX_TOTAL_COST,
        0,
        EVENTSUB_MAX_TOTAL_COST,
    )
}

fn subscription_plan_with_capacity(
    tracked_streamers: &[Streamer],
    policy: TransportSourcePolicy,
    authorized_prediction_broadcaster_id: Option<&str>,
    available_cost: u32,
    current_total_cost: u32,
    max_total_cost: u32,
) -> (Vec<SubscriptionRequest>, EventSubSetupReport) {
    let mut requests = Vec::new();
    let mut capabilities = initial_eventsub_capabilities(
        tracked_streamers,
        policy,
        authorized_prediction_broadcaster_id,
    );

    let mut remaining_cost = available_cost;
    let mut overflow_streamers = 0_usize;

    for (streamer_index, streamer) in tracked_streamers.iter().enumerate() {
        if streamer.channel_id.trim().is_empty() {
            capabilities[streamer_index].failure_class = Some(String::from("missing-channel-id"));
            continue;
        }
        let presence_types = ["stream.online", "stream.offline"];
        let required_cost = subscription_cost(streamer, authorized_prediction_broadcaster_id)
            .saturating_mul(u32::try_from(presence_types.len()).unwrap_or(u32::MAX));
        if reserve_capacity(&mut remaining_cost, required_cost) {
            capabilities[streamer_index].presence_source = String::from("eventsub+gql-polling");
            plan_types(
                &mut requests,
                &mut capabilities[streamer_index],
                streamer_index,
                &presence_types,
                || json!({ "broadcaster_user_id": streamer.channel_id }),
            );
        } else {
            overflow_streamers += 1;
            capabilities[streamer_index]
                .skipped_subscription_types
                .extend(presence_types.into_iter().map(String::from));
            capabilities[streamer_index].failure_class = Some(String::from("capacity-overflow"));
        }
    }

    for (streamer_index, streamer) in tracked_streamers.iter().enumerate() {
        if streamer.channel_id.trim().is_empty() || !streamer.settings.follow_raid {
            continue;
        }
        let raid_types = ["channel.raid"];
        let required_cost = subscription_cost(streamer, authorized_prediction_broadcaster_id);
        if reserve_capacity(&mut remaining_cost, required_cost) {
            plan_types(
                &mut requests,
                &mut capabilities[streamer_index],
                streamer_index,
                &raid_types,
                || json!({ "from_broadcaster_user_id": streamer.channel_id }),
            );
        } else {
            capabilities[streamer_index]
                .skipped_subscription_types
                .push(String::from("channel.raid"));
            capabilities[streamer_index]
                .failure_class
                .get_or_insert_with(|| String::from("capacity-overflow"));
        }
    }

    for (streamer_index, streamer) in tracked_streamers.iter().enumerate() {
        if streamer.channel_id.trim().is_empty()
            || !uses_eventsub_predictions(streamer, policy, authorized_prediction_broadcaster_id)
        {
            continue;
        }
        let prediction_types = [
            "channel.prediction.begin",
            "channel.prediction.progress",
            "channel.prediction.lock",
            "channel.prediction.end",
        ];
        let required_cost = subscription_cost(streamer, authorized_prediction_broadcaster_id)
            .saturating_mul(u32::try_from(prediction_types.len()).unwrap_or(u32::MAX));
        if reserve_capacity(&mut remaining_cost, required_cost) {
            plan_types(
                &mut requests,
                &mut capabilities[streamer_index],
                streamer_index,
                &prediction_types,
                || json!({ "broadcaster_user_id": streamer.channel_id }),
            );
        } else {
            capabilities[streamer_index]
                .skipped_subscription_types
                .extend(prediction_types.into_iter().map(String::from));
            capabilities[streamer_index]
                .failure_class
                .get_or_insert_with(|| String::from("capacity-overflow"));
        }
    }

    let report = EventSubSetupReport {
        planned_subscriptions: requests.len(),
        active_subscriptions: 0,
        failed_subscriptions: 0,
        overflow_streamers,
        total_cost: current_total_cost,
        max_total_cost,
        verified: false,
        capabilities,
    };
    (requests, report)
}

fn initial_eventsub_capabilities(
    tracked_streamers: &[Streamer],
    policy: TransportSourcePolicy,
    authorized_prediction_broadcaster_id: Option<&str>,
) -> Vec<EventSubStreamerCapability> {
    tracked_streamers
        .iter()
        .enumerate()
        .map(|(streamer_index, streamer)| EventSubStreamerCapability {
            streamer_index,
            presence_source: String::from("gql-polling"),
            prediction_source: if !streamer.settings.make_predictions {
                String::from("disabled")
            } else if uses_eventsub_predictions(
                streamer,
                policy,
                authorized_prediction_broadcaster_id,
            ) {
                String::from("eventsub-broadcaster")
            } else {
                String::from("pubsub-compatibility")
            },
            raid_source: if streamer.settings.follow_raid {
                String::from("eventsub-observation+pubsub-compatibility")
            } else {
                String::from("disabled")
            },
            planned_subscription_types: Vec::new(),
            active_subscription_types: Vec::new(),
            skipped_subscription_types: Vec::new(),
            failure_class: None,
        })
        .collect()
}

fn uses_eventsub_predictions(
    streamer: &Streamer,
    policy: TransportSourcePolicy,
    authorized_prediction_broadcaster_id: Option<&str>,
) -> bool {
    policy.prediction_source == PredictionSource::EventSubBroadcaster
        || authorized_prediction_broadcaster_id.is_some_and(|broadcaster_id| {
            !broadcaster_id.trim().is_empty() && streamer.channel_id.trim() == broadcaster_id.trim()
        })
}

fn subscription_cost(streamer: &Streamer, authorized_broadcaster_id: Option<&str>) -> u32 {
    if authorized_broadcaster_id.is_some_and(|authorized| {
        !authorized.trim().is_empty() && authorized.trim() == streamer.channel_id.trim()
    }) {
        0
    } else {
        EVENTSUB_ASSUMED_SUBSCRIPTION_COST
    }
}

fn reserve_capacity(remaining_cost: &mut u32, required_cost: u32) -> bool {
    if required_cost > *remaining_cost {
        return false;
    }
    *remaining_cost -= required_cost;
    true
}

fn plan_types(
    requests: &mut Vec<SubscriptionRequest>,
    capability: &mut EventSubStreamerCapability,
    streamer_index: usize,
    subscription_types: &[&str],
    condition: impl Fn() -> Value,
) {
    for subscription_type in subscription_types {
        if requests.len() == EVENTSUB_MAX_SUBSCRIPTIONS_PER_CONNECTION {
            capability
                .skipped_subscription_types
                .push((*subscription_type).to_string());
            capability.failure_class = Some(String::from("subscription-count-overflow"));
            continue;
        }
        capability
            .planned_subscription_types
            .push((*subscription_type).to_string());
        requests.push(SubscriptionRequest {
            streamer_index,
            subscription_type: (*subscription_type).to_string(),
            condition: condition(),
        });
    }
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
                kind: crate::PlaybackType::StreamDown,
            })
        }
        "channel.raid" => {
            let value: RaidEvent = serde_json::from_value(event.clone())?;
            ensure_tracked(&value.from_broadcaster_user_id, tracked_streamers)?;
            if value.to_broadcaster_user_login.trim().is_empty() {
                return Err(EventSubError::Protocol("raid target login is empty"));
            }
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
            let mut value: PredictionEventWire = serde_json::from_value(event.clone())?;
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
            validate_prediction_wire(&mut value, subscription_type)?;
            let winning_outcome_id = value.winning_outcome_id.clone();
            Ok(MinerEvent::PredictionChannel {
                kind,
                event: Box::new(prediction_event_from_wire(
                    &value,
                    streamer,
                    subscription_type,
                )?),
                winning_outcome_id,
            })
        }
        _ => Err(EventSubError::Protocol("unsupported EventSub subscription")),
    }
}

fn prediction_event_from_wire(
    value: &PredictionEventWire,
    streamer: &Streamer,
    subscription_type: &str,
) -> Result<PredictionEvent, EventSubError> {
    let created_at =
        OffsetDateTime::parse(&value.started_at, &Rfc3339).map_err(|_| EventSubError::Timestamp)?;
    let boundary = match subscription_type {
        "channel.prediction.begin" | "channel.prediction.progress" => value.locks_at.as_deref(),
        "channel.prediction.lock" => value.locked_at.as_deref(),
        "channel.prediction.end" => value.ended_at.as_deref(),
        _ => None,
    }
    .ok_or(EventSubError::Protocol(
        "prediction lifecycle timestamp is missing",
    ))?;
    let boundary =
        OffsetDateTime::parse(boundary, &Rfc3339).map_err(|_| EventSubError::Timestamp)?;
    let window_seconds = (boundary - created_at).as_seconds_f64();
    if !window_seconds.is_finite() || window_seconds < 0.0 {
        return Err(EventSubError::Protocol(
            "prediction lifecycle timestamp precedes start",
        ));
    }
    let status = match subscription_type {
        "channel.prediction.begin" | "channel.prediction.progress" => "ACTIVE",
        "channel.prediction.lock" => "LOCKED",
        "channel.prediction.end" => value
            .status
            .as_deref()
            .ok_or(EventSubError::Protocol("prediction end status is missing"))?,
        _ => return Err(EventSubError::Protocol("unsupported prediction lifecycle")),
    }
    .trim()
    .to_uppercase();
    let mut event = PredictionEvent {
        streamer: streamer.clone(),
        event_id: value.id.clone(),
        title: value.title.clone(),
        status,
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

fn validate_prediction_wire(
    value: &mut PredictionEventWire,
    subscription_type: &str,
) -> Result<(), EventSubError> {
    if value.id.trim().is_empty()
        || value.broadcaster_user_id.trim().is_empty()
        || value.title.trim().is_empty()
        || value.started_at.trim().is_empty()
        || value.outcomes.len() < 2
    {
        return Err(EventSubError::Protocol(
            "prediction required fields are missing",
        ));
    }
    for outcome in &value.outcomes {
        let color = outcome.color.trim().to_ascii_uppercase();
        if outcome.id.trim().is_empty()
            || outcome.title.trim().is_empty()
            || !matches!(color.as_str(), "BLUE" | "PINK")
            || outcome.users < 0
            || outcome.channel_points < 0
            || outcome
                .top_predictors
                .iter()
                .any(|predictor| predictor.channel_points_used < 0)
        {
            return Err(EventSubError::Protocol(
                "prediction outcome fields are invalid",
            ));
        }
    }
    value.winning_outcome_id = value
        .winning_outcome_id
        .take()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if subscription_type == "channel.prediction.end" {
        let status = value
            .status
            .as_deref()
            .map(str::trim)
            .map(str::to_uppercase)
            .ok_or(EventSubError::Protocol("prediction end status is missing"))?;
        if !matches!(status.as_str(), "RESOLVED" | "CANCELED" | "CANCELLED") {
            return Err(EventSubError::Protocol(
                "prediction end status is unsupported",
            ));
        }
        if status == "RESOLVED"
            && !value
                .winning_outcome_id
                .as_ref()
                .is_some_and(|winner| value.outcomes.iter().any(|outcome| outcome.id == *winner))
        {
            return Err(EventSubError::Protocol(
                "resolved prediction winner is missing or unknown",
            ));
        }
    }
    Ok(())
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
        event_from_notification, parse_eventsub_message, subscription_plan_with_capacity,
        subscription_requests, subscription_requests_with_policy, EventSubClient,
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
                    return serde_json::from_slice(
                        &request[body_start..body_start + content_length],
                    )
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
            include_str!("../../../tests/fixtures/eventsub.stream_offline.2026.json"),
            &tracked,
        )
        .unwrap();
        let EventSubMessage::Notification {
            subscription_type,
            event,
            ..
        } = message
        else {
            panic!("expected notification");
        };

        assert_eq!(
            event_from_notification(&subscription_type, &event, &tracked).unwrap(),
            MinerEvent::Playback {
                channel_id: String::from("100"),
                kind: crate::PlaybackType::StreamDown,
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
}
