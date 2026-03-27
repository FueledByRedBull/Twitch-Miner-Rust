use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ::time::format_description::well_known::Rfc3339;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tm_domain::{
    CommunityGoal, OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome, Streamer,
};
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{self, Message};

const TOPIC_BATCH_SIZE: usize = 50;
pub const WEBSOCKET_URL: &str = "wss://pubsub-edge.twitch.tv";
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum PubSubError {
    #[error("no user id for pubsub")]
    MissingUserId,
    #[error("invalid pubsub payload: {0}")]
    InvalidPayload(#[from] serde_json::Error),
    #[error("invalid pubsub text payload: {0}")]
    InvalidText(#[from] std::string::FromUtf8Error),
    #[error("websocket error: {0}")]
    WebSocket(#[from] tungstenite::Error),
    #[error("event channel closed")]
    EventChannelClosed,
    #[error("pubsub reconnect requested")]
    ReconnectRequested,
    #[error("pubsub bad auth for {cookie_file}: {error}")]
    BadAuth { cookie_file: String, error: String },
    #[error("pubsub pong timeout")]
    PongTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubSubClientSettings {
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PubSubConnectionEvent {
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

#[derive(Debug, Clone, PartialEq)]
pub enum IncomingTransportMessage {
    Pong,
    Reconnect,
    ResponseError {
        error: String,
        nonce: Option<String>,
        is_bad_auth: bool,
    },
    Event(Box<PubSubEvent>),
    Ignore,
}

impl Default for PubSubClientSettings {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(4 * 60),
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

        let mut ping_interval = time::interval(self.settings.ping_interval);
        ping_interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        let mut last_pong = Instant::now();

        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    if last_pong.elapsed() > self.settings.pong_timeout {
                        return Err(PubSubError::PongTimeout);
                    }
                    socket.send(Message::Text(ping_payload().to_string().into())).await?;
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
                        }
                        Message::Close(_) => return Ok(()),
                        Message::Frame(_) => {}
                    }
                }
            }
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackType {
    StreamUp,
    Viewcount,
    StreamDown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommunityGoalKind {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredictionChannelKind {
    EventCreated,
    EventUpdated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredictionUserKind {
    PredictionMade,
    PredictionResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PubSubEvent {
    PointsEarned {
        channel_id: String,
        earned: i64,
        reason: String,
        balance: i64,
    },
    ClaimAvailable {
        channel_id: String,
        claim_id: String,
    },
    Playback {
        channel_id: String,
        kind: PlaybackType,
    },
    Raid {
        channel_id: String,
        raid_id: String,
        target_login: String,
    },
    Moment {
        channel_id: String,
        moment_id: String,
    },
    PredictionChannel {
        kind: PredictionChannelKind,
        event: Box<PredictionEvent>,
        winning_outcome_id: Option<String>,
    },
    PredictionUser {
        event_id: String,
        kind: PredictionUserKind,
        result: Option<Value>,
    },
    CommunityGoal {
        channel_id: String,
        kind: CommunityGoalKind,
        goal: Option<CommunityGoal>,
        goal_id: Option<String>,
    },
}

pub fn build_topics(user_id: &str, streamers: &[Streamer]) -> Result<Vec<String>, PubSubError> {
    if user_id.trim().is_empty() {
        return Err(PubSubError::MissingUserId);
    }

    let mut topics = Vec::new();
    let mut push_unique = |topic: String| {
        if !topic.is_empty() && !topics.contains(&topic) {
            topics.push(topic);
        }
    };

    push_unique(format!("community-points-user-v1.{}", user_id.trim()));

    if streamers
        .iter()
        .any(|streamer| streamer.settings.make_predictions)
    {
        push_unique(format!("predictions-user-v1.{}", user_id.trim()));
    }

    for streamer in streamers {
        if streamer.channel_id.trim().is_empty() {
            continue;
        }
        let channel_id = streamer.channel_id.trim();
        push_unique(format!("video-playback-by-id.{channel_id}"));
        if streamer.settings.follow_raid {
            push_unique(format!("raid.{channel_id}"));
        }
        if streamer.settings.make_predictions {
            push_unique(format!("predictions-channel-v1.{channel_id}"));
        }
        if streamer.settings.claim_moments {
            push_unique(format!("community-moments-channel-v1.{channel_id}"));
        }
        push_unique(format!("community-points-channel-v1.{channel_id}"));
    }

    Ok(topics)
}

pub fn build_topic_batches(
    user_id: &str,
    streamers: &[Streamer],
) -> Result<Vec<Vec<String>>, PubSubError> {
    Ok(chunk_topics(&build_topics(user_id, streamers)?))
}

#[must_use]
pub fn chunk_topics(topics: &[String]) -> Vec<Vec<String>> {
    if topics.is_empty() {
        return Vec::new();
    }

    topics
        .chunks(TOPIC_BATCH_SIZE)
        .map(<[String]>::to_vec)
        .collect()
}

#[must_use]
pub fn listen_payload(topic: &str, auth_token: &str) -> Value {
    listen_payload_with_nonce(topic, auth_token, "nonce")
}

#[must_use]
pub fn listen_payload_with_nonce(topic: &str, auth_token: &str, nonce: &str) -> Value {
    let mut data = json!({ "topics": [topic] });
    if topic_requires_auth(topic) {
        data["auth_token"] = Value::String(auth_token.to_string());
    }
    json!({
        "type": "LISTEN",
        "nonce": nonce,
        "data": data,
    })
}

#[must_use]
pub fn listen_payloads(topics: &[String], auth_token: &str) -> Vec<Value> {
    topics
        .iter()
        .map(|topic| listen_payload_with_nonce(topic, auth_token, &random_nonce()))
        .collect()
}

#[must_use]
pub fn ping_payload() -> Value {
    json!({ "type": "PING" })
}

#[must_use]
pub fn topic_requires_auth(topic: &str) -> bool {
    topic.starts_with("community-points-user-v1.") || topic.starts_with("predictions-user-v1.")
}

pub fn parse_message(
    raw: &str,
    tracked_streamers: &[Streamer],
) -> Result<Option<PubSubEvent>, PubSubError> {
    let envelope: Value = serde_json::from_str(raw)?;
    let envelope_type = envelope
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_uppercase();
    if envelope_type != "MESSAGE" {
        return Ok(None);
    }

    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let topic = data
        .get("topic")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if message.is_empty() {
        return Ok(None);
    }

    let payload: Value = serde_json::from_str(message)?;
    let payload_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let channel_id = channel_id_from_payload(&payload, &topic);

    if payload_type == "points-earned" {
        return Ok(Some(parse_points_earned_event(&payload, channel_id)));
    }
    if payload_type == "claim-available" {
        return Ok(parse_claim_available_event(
            &payload,
            channel_id,
            tracked_streamers,
        ));
    }
    if topic.starts_with("video-playback-by-id.") {
        return Ok(parse_playback_event(&payload_type, channel_id));
    }
    if topic.starts_with("raid.") {
        return Ok(parse_raid_event(&payload, channel_id));
    }
    if topic.starts_with("community-moments-channel-v1.") {
        return Ok(parse_moment_event(&payload, &payload_type, channel_id));
    }
    if topic.starts_with("predictions-channel-v1.") {
        return Ok(parse_prediction_channel_event(
            &payload,
            &payload_type,
            &channel_id,
            tracked_streamers,
        ));
    }
    if topic.starts_with("predictions-user-v1.") {
        return Ok(parse_prediction_user_event(&payload, &payload_type));
    }
    if topic.starts_with("community-points-channel-v1.") {
        return Ok(parse_community_goal_event(
            &payload,
            &payload_type,
            channel_id,
        ));
    }

    Ok(None)
}

fn parse_points_earned_event(payload: &Value, channel_id: String) -> PubSubEvent {
    let point_gain = payload
        .pointer("/data/point_gain")
        .cloned()
        .unwrap_or(Value::Null);
    PubSubEvent::PointsEarned {
        channel_id,
        earned: point_gain
            .get("total_points")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        reason: point_gain
            .get("reason_code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_uppercase(),
        balance: payload
            .pointer("/data/balance/balance")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
    }
}

fn parse_claim_available_event(
    payload: &Value,
    channel_id: String,
    tracked_streamers: &[Streamer],
) -> Option<PubSubEvent> {
    let claim_id = payload
        .pointer("/data/claim/id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if claim_id.is_empty() {
        return None;
    }
    let resolved_channel_id = if channel_id.is_empty() && tracked_streamers.len() == 1 {
        tracked_streamers[0].channel_id.clone()
    } else {
        channel_id
    };
    Some(PubSubEvent::ClaimAvailable {
        channel_id: resolved_channel_id,
        claim_id,
    })
}

fn parse_playback_event(payload_type: &str, channel_id: String) -> Option<PubSubEvent> {
    let kind = match payload_type {
        "stream-up" => PlaybackType::StreamUp,
        "viewcount" => PlaybackType::Viewcount,
        "stream-down" => PlaybackType::StreamDown,
        _ => return None,
    };
    Some(PubSubEvent::Playback { channel_id, kind })
}

fn parse_raid_event(payload: &Value, channel_id: String) -> Option<PubSubEvent> {
    let raid = payload.get("raid").cloned().unwrap_or(Value::Null);
    let raid_id = raid
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if raid_id.is_empty() {
        return None;
    }
    Some(PubSubEvent::Raid {
        channel_id,
        raid_id,
        target_login: raid
            .get("target_login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_moment_event(
    payload: &Value,
    payload_type: &str,
    channel_id: String,
) -> Option<PubSubEvent> {
    if payload_type != "active" {
        return None;
    }
    let moment_id = payload
        .pointer("/data/moment_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if moment_id.is_empty() {
        return None;
    }
    Some(PubSubEvent::Moment {
        channel_id,
        moment_id,
    })
}

fn parse_prediction_channel_event(
    payload: &Value,
    payload_type: &str,
    channel_id: &str,
    tracked_streamers: &[Streamer],
) -> Option<PubSubEvent> {
    let kind = match payload_type {
        "event-created" => PredictionChannelKind::EventCreated,
        "event-updated" => PredictionChannelKind::EventUpdated,
        _ => return None,
    };
    let streamer = tracked_streamers
        .iter()
        .find(|streamer| streamer.channel_id == channel_id)
        .cloned()?;
    let raw_event = payload.get("data").and_then(|data| data.get("event"))?;
    Some(PubSubEvent::PredictionChannel {
        kind: kind.clone(),
        event: Box::new(parse_prediction_event(
            &streamer,
            raw_event,
            matches!(kind, PredictionChannelKind::EventCreated),
        )),
        winning_outcome_id: winning_outcome_id(raw_event),
    })
}

fn parse_prediction_user_event(payload: &Value, payload_type: &str) -> Option<PubSubEvent> {
    let kind = match payload_type {
        "prediction-made" => PredictionUserKind::PredictionMade,
        "prediction-result" => PredictionUserKind::PredictionResult,
        _ => return None,
    };
    Some(PubSubEvent::PredictionUser {
        event_id: payload
            .pointer("/data/prediction/event_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        result: payload.pointer("/data/prediction/result").cloned(),
        kind,
    })
}

fn parse_community_goal_event(
    payload: &Value,
    payload_type: &str,
    channel_id: String,
) -> Option<PubSubEvent> {
    let kind = match payload_type {
        "community-goal-created" => CommunityGoalKind::Created,
        "community-goal-updated" => CommunityGoalKind::Updated,
        "community-goal-deleted" => CommunityGoalKind::Deleted,
        _ => return None,
    };
    let goal_value = payload.pointer("/data/community_goal").cloned();
    let goal = goal_value.as_ref().and_then(|value| {
        serde_json::from_value::<CommunityGoal>(normalize_goal_value(value)).ok()
    });
    let goal_id = goal_value
        .as_ref()
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(PubSubEvent::CommunityGoal {
        channel_id,
        kind,
        goal,
        goal_id,
    })
}

pub fn parse_transport_message(
    raw: &str,
    tracked_streamers: &[Streamer],
) -> Result<IncomingTransportMessage, PubSubError> {
    let envelope: Value = serde_json::from_str(raw)?;
    let message_type = envelope
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_uppercase();
    match message_type.as_str() {
        "PONG" => Ok(IncomingTransportMessage::Pong),
        "RECONNECT" => Ok(IncomingTransportMessage::Reconnect),
        "RESPONSE" => {
            let error = envelope
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if error.is_empty() {
                return Ok(IncomingTransportMessage::Ignore);
            }
            let nonce = envelope
                .get("nonce")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(IncomingTransportMessage::ResponseError {
                is_bad_auth: error.contains("ERR_BADAUTH"),
                error,
                nonce,
            })
        }
        "MESSAGE" => parse_message(raw, tracked_streamers).map(|event| {
            event.map_or(IncomingTransportMessage::Ignore, |event| {
                IncomingTransportMessage::Event(Box::new(event))
            })
        }),
        _ => Ok(IncomingTransportMessage::Ignore),
    }
}

#[must_use]
pub fn channel_id_from_payload(payload: &Value, topic: &str) -> String {
    payload
        .pointer("/data/prediction/channel_id")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .pointer("/data/claim/channel_id")
                .and_then(Value::as_str)
        })
        .or_else(|| payload.pointer("/data/channel_id").and_then(Value::as_str))
        .or_else(|| {
            payload
                .pointer("/data/balance/channel_id")
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .or_else(|| {
            topic
                .split_once('.')
                .map(|(_, suffix)| suffix.trim().to_string())
        })
        .unwrap_or_default()
}

fn normalize_goal_value(value: &Value) -> Value {
    json!({
        "id": value.get("id").cloned().unwrap_or(Value::Null),
        "title": value.get("title").cloned().unwrap_or(Value::Null),
        "is_in_stock": value.get("is_in_stock").cloned().unwrap_or(Value::Null),
        "points_contributed": value.get("points_contributed").cloned().unwrap_or(Value::Null),
        "amount_needed": value.get("goal_amount").cloned().unwrap_or(Value::Null),
        "per_stream_user_maximum_contribution": value
            .get("per_stream_maximum_user_contribution")
            .cloned()
            .unwrap_or(Value::Null),
        "status": value.get("status").cloned().unwrap_or(Value::Null),
    })
}

fn parse_prediction_event(
    streamer: &Streamer,
    raw_event: &Value,
    apply_streamer_delay: bool,
) -> PredictionEvent {
    let created_at = raw_event
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    let raw_window = raw_event
        .get("prediction_window_seconds")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let window_seconds = if apply_streamer_delay {
        streamer.prediction_window_seconds(raw_window)
    } else {
        raw_window
    };
    let outcomes = raw_event
        .get("outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(parse_prediction_outcome)
        .collect::<Vec<_>>();
    let mut event = PredictionEvent {
        streamer: streamer.clone(),
        event_id: raw_event
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: raw_event
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        status: raw_event
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_uppercase(),
        created_at,
        window_seconds,
        outcomes,
        decision: PredictionDecision::default(),
        bet_placed: false,
        bet_confirmed: false,
        result_type: String::new(),
        result_string: String::new(),
    };
    event.update_outcomes();
    event
}

fn parse_prediction_outcome(raw: &Value) -> PredictionOutcome {
    PredictionOutcome {
        id: raw
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: raw
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        color: raw
            .get("color")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        total_users: raw
            .get("total_users")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        total_points: raw
            .get("total_points")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        top_points: raw
            .get("top_predictors")
            .and_then(Value::as_array)
            .and_then(|predictors| predictors.first())
            .and_then(|predictor| predictor.get("points"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        percentage_users: 0.0,
        odds: 0.0,
        odds_percentage: 0.0,
    }
}

fn winning_outcome_id(raw_event: &Value) -> Option<String> {
    raw_event
        .get("winning_outcome_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            raw_event
                .get("outcomes")
                .and_then(Value::as_array)
                .and_then(|outcomes| {
                    outcomes.iter().find_map(|outcome| {
                        if outcome
                            .get("is_winning_outcome")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            return outcome
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                        let state = outcome
                            .get("state")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_uppercase();
                        if matches!(state.as_str(), "RESOLVED" | "WINNER" | "WIN") {
                            outcome
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        } else {
                            None
                        }
                    })
                })
        })
}

#[must_use]
pub fn bad_auth_cookie_file(username: Option<&str>) -> String {
    match username.map(str::trim).filter(|value| !value.is_empty()) {
        Some(username) => format!("cookies/{username}.json"),
        None => String::from("cookies/<username>.json"),
    }
}

fn random_nonce() -> String {
    format!("{:016x}", NONCE_COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_domain::{IrcMode, StreamerSettings};

    fn streamer(id: &str) -> Streamer {
        Streamer {
            channel_id: id.to_string(),
            settings: StreamerSettings {
                make_predictions: true,
                follow_raid: true,
                claim_moments: true,
                community_goals: true,
                claim_drops: true,
                irc_mode: IrcMode::Online,
                ..StreamerSettings::default()
            },
            ..Streamer::default()
        }
    }

    #[test]
    fn builds_topics_from_streamer_settings() {
        let topics = build_topics("user-1", &[streamer("100"), streamer("200")]).unwrap();
        assert!(topics.contains(&"community-points-user-v1.user-1".to_string()));
        assert!(topics.contains(&"predictions-user-v1.user-1".to_string()));
        assert!(topics.contains(&"video-playback-by-id.100".to_string()));
        assert!(topics.contains(&"raid.100".to_string()));
        assert!(topics.contains(&"predictions-channel-v1.100".to_string()));
        assert!(topics.contains(&"community-moments-channel-v1.100".to_string()));
        assert!(topics.contains(&"community-points-channel-v1.100".to_string()));
    }

    #[test]
    fn builds_bonus_claim_topic_even_without_community_goals() {
        let topics = build_topics(
            "user-1",
            &[Streamer {
                channel_id: String::from("100"),
                settings: StreamerSettings {
                    community_goals: false,
                    ..StreamerSettings::default()
                },
                ..Streamer::default()
            }],
        )
        .unwrap();

        assert!(topics.contains(&"community-points-channel-v1.100".to_string()));
    }

    #[test]
    fn builds_topic_batches_with_auth_topic_and_fifty_max_topics() {
        let streamers = (0..51)
            .map(|index| Streamer {
                channel_id: format!("channel-{index}"),
                ..Streamer::default()
            })
            .collect::<Vec<_>>();

        let batches = build_topic_batches("user-1", &streamers).unwrap();
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 50);
        assert_eq!(batches[1].len(), 50);
        assert_eq!(batches[2].len(), 3);
        assert_eq!(batches[0][0], "community-points-user-v1.user-1");
        assert!(!batches[2]
            .iter()
            .any(|topic| topic == "community-points-user-v1.user-1"));
    }

    #[test]
    fn chunks_topics_at_fifty() {
        let topics: Vec<String> = (0..101).map(|value| format!("topic-{value}")).collect();
        let chunks = chunk_topics(&topics);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 50);
        assert_eq!(chunks[1].len(), 50);
        assert_eq!(chunks[2].len(), 1);
    }

    #[test]
    fn listen_payload_only_adds_auth_for_user_topics() {
        let auth_payload = listen_payload("community-points-user-v1.user", "secret");
        assert_eq!(auth_payload["data"]["auth_token"], "secret");

        let no_auth_payload = listen_payload("video-playback-by-id.123", "secret");
        assert!(no_auth_payload["data"].get("auth_token").is_none());
    }

    #[test]
    fn listen_payloads_generate_unique_nonces_and_ping_payload() {
        let payloads = listen_payloads(
            &[
                "community-points-user-v1.user".to_string(),
                "video-playback-by-id.123".to_string(),
            ],
            "secret",
        );
        assert_eq!(payloads.len(), 2);
        assert_ne!(payloads[0]["nonce"], payloads[1]["nonce"]);
        assert_eq!(ping_payload()["type"], "PING");
    }

    #[test]
    fn channel_id_falls_back_across_payload_shapes() {
        let payload = json!({
            "data": {
                "prediction": { "channel_id": "prediction-id" }
            }
        });
        assert_eq!(
            channel_id_from_payload(&payload, "topic.ignore"),
            "prediction-id"
        );

        let payload = json!({
            "data": {
                "claim": { "channel_id": "claim-id" }
            }
        });
        assert_eq!(
            channel_id_from_payload(&payload, "topic.ignore"),
            "claim-id"
        );

        let payload = json!({
            "data": {
                "balance": { "channel_id": "balance-id" }
            }
        });
        assert_eq!(
            channel_id_from_payload(&payload, "topic.ignore"),
            "balance-id"
        );

        let payload = json!({});
        assert_eq!(
            channel_id_from_payload(&payload, "video-playback-by-id.topic-suffix"),
            "topic-suffix"
        );
    }

    #[test]
    fn parses_claim_available_with_single_streamer_fallback() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "",
                "message": "{\"type\":\"claim-available\",\"data\":{\"claim\":{\"id\":\"claim-1\"}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[streamer("fallback-channel")]).unwrap();
        assert_eq!(
            parsed,
            Some(PubSubEvent::ClaimAvailable {
                channel_id: String::from("fallback-channel"),
                claim_id: String::from("claim-1"),
            })
        );
    }

    #[test]
    fn parses_prediction_result_without_prediction_made() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "predictions-user-v1.user",
                "message": "{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"event-1\",\"result\":{\"type\":\"WIN\"}}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[]).unwrap();
        assert_eq!(
            parsed,
            Some(PubSubEvent::PredictionUser {
                event_id: String::from("event-1"),
                kind: PredictionUserKind::PredictionResult,
                result: Some(json!({ "type": "WIN" })),
            })
        );
    }

    #[test]
    fn parses_prediction_channel_event_with_outcomes() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "predictions-channel-v1.123",
                "message": "{\"type\":\"event-created\",\"data\":{\"event\":{\"id\":\"event-1\",\"title\":\"Will it happen?\",\"status\":\"ACTIVE\",\"created_at\":\"2026-03-27T06:00:00Z\",\"prediction_window_seconds\":120,\"outcomes\":[{\"id\":\"a\",\"title\":\"Yes\",\"color\":\"blue\",\"total_users\":10,\"total_points\":100,\"top_predictors\":[{\"points\":30}]},{\"id\":\"b\",\"title\":\"No\",\"color\":\"pink\",\"total_users\":5,\"total_points\":50,\"top_predictors\":[{\"points\":25}]}]}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[streamer("123")]).unwrap();
        let Some(PubSubEvent::PredictionChannel {
            kind,
            event,
            winning_outcome_id,
        }) = parsed
        else {
            panic!("expected prediction channel event");
        };
        assert_eq!(kind, PredictionChannelKind::EventCreated);
        assert_eq!(event.event_id, "event-1");
        assert_eq!(event.streamer.channel_id, "123");
        assert_eq!(event.outcomes[0].top_points, 30);
        assert!((event.outcomes[0].odds - 1.5).abs() < f64::EPSILON);
        assert!((event.outcomes[1].odds_percentage - 33.333_333_333_333_336).abs() < f64::EPSILON);
        assert_eq!(winning_outcome_id, None);
    }

    #[test]
    fn parses_community_goal_message() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "community-points-channel-v1.123",
                "message": "{\"type\":\"community-goal-created\",\"data\":{\"community_goal\":{\"id\":\"goal-1\",\"title\":\"Goal\",\"is_in_stock\":true,\"points_contributed\":100,\"goal_amount\":500,\"per_stream_maximum_user_contribution\":50,\"status\":\"ACTIVE\"}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[]).unwrap();
        assert_eq!(
            parsed,
            Some(PubSubEvent::CommunityGoal {
                channel_id: String::from("123"),
                kind: CommunityGoalKind::Created,
                goal: Some(CommunityGoal {
                    id: String::from("goal-1"),
                    title: String::from("Goal"),
                    is_in_stock: true,
                    points_contributed: 100,
                    amount_needed: 500,
                    per_stream_user_maximum_contribution: 50,
                    status: String::from("ACTIVE"),
                }),
                goal_id: Some(String::from("goal-1")),
            })
        );
    }

    #[test]
    fn parse_transport_message_handles_control_frames_and_bad_auth() {
        assert_eq!(
            parse_transport_message(r#"{"type":"PONG"}"#, &[]).unwrap(),
            IncomingTransportMessage::Pong
        );
        assert_eq!(
            parse_transport_message(r#"{"type":"RECONNECT"}"#, &[]).unwrap(),
            IncomingTransportMessage::Reconnect
        );
        assert_eq!(
            parse_transport_message(
                r#"{"type":"RESPONSE","error":"ERR_BADAUTH bad token","nonce":"abc"}"#,
                &[]
            )
            .unwrap(),
            IncomingTransportMessage::ResponseError {
                error: "ERR_BADAUTH bad token".into(),
                nonce: Some("abc".into()),
                is_bad_auth: true,
            }
        );
    }

    #[test]
    fn bad_auth_cookie_file_matches_go_shape() {
        assert_eq!(bad_auth_cookie_file(Some("alice")), "cookies/alice.json");
        assert_eq!(bad_auth_cookie_file(None), "cookies/<username>.json");
    }
}
