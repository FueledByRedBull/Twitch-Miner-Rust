use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tm_domain::{PredictionDecision, PredictionEvent, PredictionOutcome, Streamer};
use tm_events::{MinerEvent, PlaybackType, PredictionChannelKind};

use super::{EventSubError, EventSubMessage, MAX_SEEN_MESSAGE_IDS};

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
    // Deserialize directly into the domain's shared immutable ID. Keeping a
    // temporary `String` here would allocate and copy every outcome ID again
    // when constructing `PredictionOutcome` below.
    id: Arc<str>,
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
            // A subscription type this build does not model is an envelope-level
            // unknown, not corrupt data: ignore it rather than dropping the
            // socket. A payload for a type we do act on still fails closed here.
            if !is_supported_subscription_type(&payload.subscription.subscription_type) {
                return Ok(EventSubMessage::Unsupported);
            }
            let event = event_from_notification(
                &payload.subscription.subscription_type,
                &payload.event,
                tracked_streamers,
            )?;
            Ok(EventSubMessage::Notification {
                message_id: raw.metadata.message_id,
                event: Box::new(event),
            })
        }
        // Twitch reserves the right to add message types and expects clients to
        // ignore the ones they do not recognise.
        _ => Ok(EventSubMessage::Unsupported),
    }
}

#[must_use]
pub(super) fn is_supported_subscription_type(subscription_type: &str) -> bool {
    matches!(
        subscription_type,
        "stream.online"
            | "stream.offline"
            | "channel.raid"
            | "channel.prediction.begin"
            | "channel.prediction.progress"
            | "channel.prediction.lock"
            | "channel.prediction.end"
    )
}

pub(super) fn event_from_notification(
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
            if value.to_broadcaster_user_login.trim().is_empty() {
                return Err(EventSubError::Protocol("raid target login is empty"));
            }
            Ok(MinerEvent::Raid {
                channel_id: value.from_broadcaster_user_id,
                // EventSub observes raids but does not expose the legacy mutation ID.
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
                id: Arc::clone(&outcome.id),
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
            && !value.winning_outcome_id.as_ref().is_some_and(|winner| {
                value
                    .outcomes
                    .iter()
                    .any(|outcome| outcome.id.as_ref() == winner)
            })
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
pub(super) struct MessageDeduper {
    pub(super) ids: HashSet<String>,
    order: VecDeque<String>,
}

impl MessageDeduper {
    pub(super) fn insert(&mut self, message_id: String) -> bool {
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
