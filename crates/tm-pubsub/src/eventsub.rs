use std::collections::HashSet;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use time::format_description::well_known::Rfc2822;
use time::OffsetDateTime;
use tm_domain::{MinerEvent, Streamer};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

mod planning;
mod protocol;

pub use planning::plan_eventsub_capacity;
use planning::subscription_plan_with_capacity;
#[cfg(test)]
use planning::{subscription_plan, subscription_requests};
#[cfg(test)]
use protocol::event_from_notification;
pub use protocol::parse_eventsub_message;
use protocol::MessageDeduper;

pub const EVENTSUB_WEBSOCKET_URL: &str =
    "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30";
pub const EVENTSUB_SUBSCRIPTIONS_URL: &str = "https://api.twitch.tv/helix/eventsub/subscriptions";
const MAX_SEEN_MESSAGE_IDS: usize = 4096;
const EVENTSUB_MAX_TOTAL_COST: u32 = 10;
const EVENTSUB_ASSUMED_SUBSCRIPTION_COST: u32 = 1;
const EVENTSUB_MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 300;
const EVENTSUB_MAX_LIST_PAGES: usize = 10;
const EVENTSUB_MAX_READ_ATTEMPTS: usize = 3;
const EVENTSUB_MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const EVENTSUB_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const EVENTSUB_WELCOME_TIMEOUT: Duration = Duration::from_secs(15);
const EVENTSUB_SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(4 * 60);
const EVENTSUB_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const EVENTSUB_KEEPALIVE_GRACE: Duration = Duration::from_secs(5);

type EventSubSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Copy)]
struct EventSubDeadlines {
    connect: Duration,
    welcome: Duration,
    session_setup: Duration,
}

impl EventSubDeadlines {
    const PRODUCTION: Self = Self {
        connect: EVENTSUB_CONNECT_TIMEOUT,
        welcome: EVENTSUB_WELCOME_TIMEOUT,
        session_setup: EVENTSUB_SESSION_SETUP_TIMEOUT,
    };
}

/// Immutable connection and authorization settings.
#[derive(Clone)]
pub struct EventSubClientSettings {
    pub client_id: String,
    pub auth_token: String,
    pub websocket_url: String,
    pub subscriptions_url: String,
    pub allow_prediction_scope_fallback: bool,
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
            authorized_prediction_broadcaster_id: None,
            verify_subscriptions: false,
            http_client: reqwest::Client::new(),
        }
    }
}

/// `EventSub` transport client.
///
/// Connections validate the welcome, plan and verify subscription capacity,
/// bound message deduplication, and forward only typed events for tracked
/// channels.
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
    Timeout(EventSubTimeoutStage),
    #[error("eventsub keepalive timeout")]
    KeepaliveTimeout,
    #[error("eventsub reconnect requested")]
    ReconnectRequested { reconnect_url: String },
}

/// Network stage whose deadline expired.
///
/// The variants deliberately contain no URL, session, token, or broadcaster
/// data so callers can safely expose their stable failure classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSubTimeoutStage {
    WebSocketConnect,
    Welcome,
    SessionSetup,
    CreateSubscription,
    ListSubscriptions,
}

impl EventSubTimeoutStage {
    #[must_use]
    pub const fn failure_class(self) -> &'static str {
        match self {
            Self::WebSocketConnect => "connect-timeout",
            Self::Welcome => "welcome-timeout",
            Self::SessionSetup => "setup-timeout",
            Self::CreateSubscription => "subscription-create-timeout",
            Self::ListSubscriptions => "subscription-list-timeout",
        }
    }
}

impl std::fmt::Display for EventSubTimeoutStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::WebSocketConnect => "websocket connect",
            Self::Welcome => "welcome",
            Self::SessionSetup => "session setup",
            Self::CreateSubscription => "create subscription",
            Self::ListSubscriptions => "list subscriptions",
        })
    }
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
        event: Box<MinerEvent>,
    },
    /// A message or subscription type this build does not model. Ignored so an
    /// additive Twitch change cannot force a reconnect loop.
    Unsupported,
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
        self.connect_and_listen_with_deadlines(
            tracked_streamers,
            sender,
            EventSubDeadlines::PRODUCTION,
        )
        .await
    }

    async fn connect_and_listen_with_deadlines(
        &self,
        tracked_streamers: &[Streamer],
        sender: mpsc::Sender<EventSubConnectionEvent>,
        deadlines: EventSubDeadlines,
    ) -> Result<(), EventSubError> {
        if tracked_streamers.is_empty() {
            return Err(EventSubError::NoSubscriptions);
        }

        let mut deduper = MessageDeduper::default();
        let (mut socket, mut keepalive_timeout, mut report) = self
            .connect_socket(
                &self.settings.websocket_url,
                tracked_streamers,
                deadlines,
                None,
            )
            .await?;
        send_setup(&sender, &report).await?;

        loop {
            let carried_report = report.clone();
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
                    validate_reconnect_url(&reconnect_url)?;

                    // Twitch carries subscriptions to the supplied reconnect URL. Keep the
                    // old socket alive while the replacement reaches Welcome; no resubscribe
                    // calls are made, and only then does assignment drop the old socket.
                    let mut replacement = Box::pin(self.connect_socket(
                        &reconnect_url,
                        tracked_streamers,
                        deadlines,
                        Some(carried_report),
                    ));
                    let mut old_socket = Box::pin(listen_socket(
                        &mut socket,
                        tracked_streamers,
                        &sender,
                        &mut deduper,
                        keepalive_timeout,
                    ));
                    let (replacement_socket, replacement_keepalive, replacement_report) = tokio::select! {
                        biased;
                        result = &mut old_socket => match result {
                            // The old socket is no longer usable, but the replacement
                            // remains the authoritative connection attempt. A peer may
                            // reset it without a close handshake during this overlap.
                            Err(EventSubError::Protocol(message))
                                if message == "event channel closed" =>
                            {
                                return Err(EventSubError::Protocol(message));
                            }
                            _ => replacement.await?,
                        },
                        result = &mut replacement => result?,
                    };
                    drop(old_socket);
                    send_setup(&sender, &replacement_report).await?;
                    socket = replacement_socket;
                    keepalive_timeout = replacement_keepalive;
                    report = replacement_report;
                }
                result => return result,
            }
        }
    }

    async fn connect_socket(
        &self,
        websocket_url: &str,
        tracked_streamers: &[Streamer],
        deadlines: EventSubDeadlines,
        inherited_subscriptions: Option<EventSubSetupReport>,
    ) -> Result<(EventSubSocket, Duration, EventSubSetupReport), EventSubError> {
        let setup = async {
            let (mut socket, _) =
                tokio::time::timeout(deadlines.connect, connect_async(websocket_url))
                    .await
                    .map_err(|_| {
                        EventSubError::Timeout(EventSubTimeoutStage::WebSocketConnect)
                    })??;
            let welcome = tokio::time::timeout(
                deadlines.welcome,
                read_welcome(&mut socket, tracked_streamers),
            )
            .await
            .map_err(|_| EventSubError::Timeout(EventSubTimeoutStage::Welcome))??;
            let EventSubMessage::Welcome {
                session_id,
                keepalive_timeout,
                ..
            } = welcome
            else {
                return Err(EventSubError::Protocol("welcome message was not decoded"));
            };
            let report = match inherited_subscriptions {
                // Twitch carries the subscriptions to the reconnect URL, but the count must be
                // re-derived for the new session rather than reported from memory.
                Some(previous) => {
                    self.reconcile_inherited_report(&session_id, previous)
                        .await?
                }
                None => {
                    self.create_subscriptions(&session_id, tracked_streamers)
                        .await?
                }
            };
            if report.active_subscriptions == 0 {
                return Err(EventSubError::NoSubscriptions);
            }
            Ok((socket, keepalive_timeout, report))
        };
        tokio::time::timeout(deadlines.session_setup, setup)
            .await
            .map_err(|_| EventSubError::Timeout(EventSubTimeoutStage::SessionSetup))?
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

    /// Re-derives the active subscription count for a session inherited through
    /// a reconnect. The previous plan is retained for capability detail, but the
    /// counts and cost come from Twitch rather than from the prior session.
    async fn reconcile_inherited_report(
        &self,
        session_id: &str,
        previous: EventSubSetupReport,
    ) -> Result<EventSubSetupReport, EventSubError> {
        let mut expected_types = previous
            .capabilities
            .iter()
            .flat_map(|capability| capability.active_subscription_types.iter().cloned())
            .collect::<Vec<_>>();
        expected_types.sort_unstable();
        let mut report = previous;
        let mut active_types = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..EVENTSUB_MAX_LIST_PAGES {
            let response = self.list_subscriptions_page(cursor.as_deref()).await?;
            report.total_cost = response.total_cost;
            report.max_total_cost = response.max_total_cost;
            active_types.extend(response.data.iter().filter_map(|subscription| {
                if subscription.transport.method == "websocket"
                    && subscription.transport.session_id == session_id
                    && subscription.status == "enabled"
                {
                    Some(subscription.subscription_type.clone())
                } else {
                    None
                }
            }));
            cursor = response
                .pagination
                .cursor
                .filter(|value| !value.trim().is_empty());
            if cursor.is_none() {
                active_types.sort_unstable();
                if active_types != expected_types {
                    return Err(EventSubError::Protocol(
                        "inherited EventSub subscriptions did not match prior session",
                    ));
                }
                report.active_subscriptions = active_types.len();
                report.verified = true;
                return Ok(report);
            }
        }
        Err(EventSubError::Protocol(
            "subscription list exceeded the bounded page limit",
        ))
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
        .map_err(|_| EventSubError::Timeout(EventSubTimeoutStage::CreateSubscription))??;
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
                .map_err(|_| EventSubError::Timeout(EventSubTimeoutStage::ListSubscriptions))??;
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

async fn send_setup(
    sender: &mpsc::Sender<EventSubConnectionEvent>,
    report: &EventSubSetupReport,
) -> Result<(), EventSubError> {
    sender
        .send(EventSubConnectionEvent::Setup(Box::new(report.clone())))
        .await
        .map_err(|_| EventSubError::Protocol("event channel closed"))?;
    sender
        .send(EventSubConnectionEvent::Heartbeat)
        .await
        .map_err(|_| EventSubError::Protocol("event channel closed"))
}

fn validate_reconnect_url(reconnect_url: &str) -> Result<(), EventSubError> {
    let url = Url::parse(reconnect_url)
        .map_err(|_| EventSubError::Protocol("invalid EventSub reconnect URL: parse"))?;

    #[cfg(test)]
    if url.scheme() == "ws" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1")) {
        // Local WebSocket servers exercise the reconnect state machine without a public
        // EventSub endpoint. This branch is compiled only into tests.
        return Ok(());
    }

    // Rejections carry a fixed predicate name only. The URL, its path, query, session
    // material, and any observed host value are never returned or logged.
    if url.scheme() != "wss" {
        return Err(EventSubError::Protocol(
            "invalid EventSub reconnect URL: scheme",
        ));
    }
    if !url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("eventsub.wss.twitch.tv"))
    {
        return Err(EventSubError::Protocol(
            "invalid EventSub reconnect URL: host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EventSubError::Protocol(
            "invalid EventSub reconnect URL: credentials",
        ));
    }
    if url.port().is_some_and(|port| port != 443) {
        return Err(EventSubError::Protocol(
            "invalid EventSub reconnect URL: port",
        ));
    }
    if url.fragment().is_some() {
        return Err(EventSubError::Protocol(
            "invalid EventSub reconnect URL: fragment",
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
        EventSubError::Timeout(stage) => stage.failure_class(),
        EventSubError::Http(_) => "http-error",
        EventSubError::Json(_) | EventSubError::Protocol(_) | EventSubError::Timestamp => {
            "protocol"
        }
        EventSubError::WebSocket(_)
        | EventSubError::KeepaliveTimeout
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
        match decode_frame(socket, message?).await? {
            DecodedFrame::Ignored => {}
            DecodedFrame::Closed => {
                return Err(EventSubError::Protocol("closed before welcome"));
            }
            DecodedFrame::Text(text) => match parse_eventsub_message(&text, tracked_streamers)? {
                parsed @ EventSubMessage::Welcome { .. } => return Ok(parsed),
                // An unmodelled frame before the welcome is ignored rather than
                // treated as a protocol violation.
                EventSubMessage::Unsupported => {}
                _ => return Err(EventSubError::Protocol("welcome was not the first message")),
            },
        }
    }
    Err(EventSubError::Protocol("socket ended before welcome"))
}

enum DecodedFrame {
    Text(String),
    Ignored,
    Closed,
}

/// Normalizes one websocket frame, answering pings so both the welcome and the
/// listen loop share a single decoding path.
async fn decode_frame<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: Message,
) -> Result<DecodedFrame, EventSubError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match message {
        Message::Text(text) => Ok(DecodedFrame::Text(text.as_str().to_owned())),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .map(DecodedFrame::Text)
            .map_err(|_| EventSubError::Protocol("binary frame is not UTF-8")),
        Message::Ping(payload) => {
            socket.send(Message::Pong(payload)).await?;
            Ok(DecodedFrame::Ignored)
        }
        Message::Pong(_) | Message::Frame(_) => Ok(DecodedFrame::Ignored),
        Message::Close(_) => Ok(DecodedFrame::Closed),
    }
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
        let Some(message) = tokio::time::timeout(
            keepalive_timeout.saturating_add(EVENTSUB_KEEPALIVE_GRACE),
            socket.next(),
        )
        .await
        .map_err(|_| EventSubError::KeepaliveTimeout)?
        else {
            return Ok(());
        };
        let text = match decode_frame(socket, message?).await? {
            DecodedFrame::Ignored => continue,
            DecodedFrame::Closed => return Ok(()),
            DecodedFrame::Text(text) => text,
        };
        match parse_eventsub_message(&text, tracked_streamers)? {
            EventSubMessage::Keepalive => sender
                .send(EventSubConnectionEvent::Heartbeat)
                .await
                .map_err(|_| EventSubError::Protocol("event channel closed"))?,
            EventSubMessage::Notification { message_id, event } => {
                if deduper.insert(message_id) {
                    sender
                        .send(EventSubConnectionEvent::Event(event))
                        .await
                        .map_err(|_| EventSubError::Protocol("event channel closed"))?;
                }
            }
            EventSubMessage::Unsupported => {}
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
}

#[cfg(test)]
#[path = "../tests/unit/eventsub_tests.rs"]
mod tests;
