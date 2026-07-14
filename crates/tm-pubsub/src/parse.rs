use serde_json::{json, Value};
use tm_domain::{CommunityGoal, Streamer};

use crate::errors::PubSubError;
use crate::prediction::{parse_prediction_event, winning_outcome_id};
use crate::types::{IncomingTransportMessage, PubSubEvent};
use tm_events::{CommunityGoalKind, PlaybackType, PredictionChannelKind, PredictionUserKind};

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
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(PubSubError::Protocol("PubSub message topic is missing"))?
        .to_string();
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(PubSubError::Protocol("PubSub message body is missing"))?;

    let payload: Value = serde_json::from_str(message)?;
    let payload_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let payload_channel_id = channel_id_from_payload_only(&payload);
    let channel_id = payload_channel_id
        .clone()
        .or_else(|| topic_channel_id(&topic))
        .unwrap_or_default();

    if payload_type == "points-earned" {
        return parse_points_earned_event(&payload, payload_channel_id.unwrap_or_default())
            .map(Some);
    }
    if payload_type == "claim-available" {
        return parse_claim_available_event(
            &payload,
            payload_channel_id.unwrap_or_default(),
            tracked_streamers,
        );
    }
    if topic.starts_with("video-playback-by-id.") {
        return parse_playback_event(&payload_type, channel_id);
    }
    if topic.starts_with("raid.") {
        return parse_raid_event(&payload, channel_id);
    }
    if topic.starts_with("community-moments-channel-v1.") {
        return parse_moment_event(&payload, &payload_type, channel_id);
    }
    if topic.starts_with("predictions-channel-v1.") {
        return parse_prediction_channel_event(
            &payload,
            &payload_type,
            &channel_id,
            tracked_streamers,
        );
    }
    if topic.starts_with("predictions-user-v1.") {
        return parse_prediction_user_event(&payload, &payload_type);
    }
    if topic.starts_with("community-points-channel-v1.") {
        return parse_community_goal_event(&payload, &payload_type, channel_id);
    }

    Ok(None)
}

fn parse_points_earned_event(
    payload: &Value,
    channel_id: String,
) -> Result<PubSubEvent, PubSubError> {
    if channel_id.trim().is_empty() {
        return Err(PubSubError::Protocol("points-earned channel id is missing"));
    }
    let point_gain = payload
        .pointer("/data/point_gain")
        .ok_or(PubSubError::Protocol("points-earned gain is missing"))?;
    let earned = point_gain
        .get("total_points")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or(PubSubError::Protocol(
            "points-earned amount is missing or invalid",
        ))?;
    let reason = point_gain
        .get("reason_code")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| valid_reason_code(value))
        .ok_or(PubSubError::Protocol(
            "points-earned reason is missing or invalid",
        ))?
        .to_uppercase();
    let balance = payload
        .pointer("/data/balance/balance")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or(PubSubError::Protocol(
            "points-earned balance is missing or invalid",
        ))?;
    Ok(PubSubEvent::PointsEarned {
        channel_id,
        earned,
        reason,
        balance,
    })
}

fn parse_claim_available_event(
    payload: &Value,
    channel_id: String,
    tracked_streamers: &[Streamer],
) -> Result<Option<PubSubEvent>, PubSubError> {
    let claim_id = payload
        .pointer("/data/claim/id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(PubSubError::Protocol("claim-available id is missing"))?
        .to_string();
    let resolved_channel_id = if channel_id.is_empty() && tracked_streamers.len() == 1 {
        tracked_streamers[0].channel_id.clone()
    } else {
        channel_id
    };
    if resolved_channel_id.trim().is_empty() {
        return Err(PubSubError::Protocol(
            "claim-available channel id is missing",
        ));
    }
    Ok(Some(PubSubEvent::ClaimAvailable {
        channel_id: resolved_channel_id,
        claim_id,
    }))
}

fn parse_playback_event(
    payload_type: &str,
    channel_id: String,
) -> Result<Option<PubSubEvent>, PubSubError> {
    let kind = match payload_type {
        "stream-up" => PlaybackType::StreamUp,
        "viewcount" => PlaybackType::Viewcount,
        "stream-down" => PlaybackType::StreamDown,
        _ => return Ok(None),
    };
    if channel_id.trim().is_empty() {
        return Err(PubSubError::Protocol("playback channel id is missing"));
    }
    Ok(Some(PubSubEvent::Playback { channel_id, kind }))
}

fn parse_raid_event(
    payload: &Value,
    channel_id: String,
) -> Result<Option<PubSubEvent>, PubSubError> {
    if channel_id.trim().is_empty() {
        return Err(PubSubError::Protocol("raid channel id is missing"));
    }
    let raid = payload.get("raid").cloned().unwrap_or(Value::Null);
    let raid_id = raid
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if raid_id.is_empty() {
        return Err(PubSubError::Protocol("raid id is missing"));
    }
    let target_login = raid
        .get("target_login")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(PubSubError::Protocol("raid target login is missing"))?;
    Ok(Some(PubSubEvent::Raid {
        channel_id,
        raid_id,
        target_login: target_login.to_string(),
    }))
}

fn parse_moment_event(
    payload: &Value,
    payload_type: &str,
    channel_id: String,
) -> Result<Option<PubSubEvent>, PubSubError> {
    if payload_type != "active" {
        return Ok(None);
    }
    if channel_id.trim().is_empty() {
        return Err(PubSubError::Protocol("moment channel id is missing"));
    }
    let moment_id = payload
        .pointer("/data/moment_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if moment_id.is_empty() {
        return Err(PubSubError::Protocol("moment id is missing"));
    }
    Ok(Some(PubSubEvent::Moment {
        channel_id,
        moment_id,
    }))
}

fn parse_prediction_channel_event(
    payload: &Value,
    payload_type: &str,
    channel_id: &str,
    tracked_streamers: &[Streamer],
) -> Result<Option<PubSubEvent>, PubSubError> {
    let kind = match payload_type {
        "event-created" => PredictionChannelKind::EventCreated,
        "event-updated" => PredictionChannelKind::EventUpdated,
        _ => return Ok(None),
    };
    let streamer = tracked_streamers
        .iter()
        .find(|streamer| streamer.channel_id == channel_id)
        .cloned();
    let Some(streamer) = streamer else {
        return Ok(None);
    };
    let raw_event = payload
        .get("data")
        .and_then(|data| data.get("event"))
        .ok_or(PubSubError::Protocol("prediction event body is missing"))?;
    let event = parse_prediction_event(
        &streamer,
        raw_event,
        matches!(kind, PredictionChannelKind::EventCreated),
    )
    .map_err(PubSubError::Protocol)?;
    let winning_outcome_id = winning_outcome_id(raw_event);
    Ok(Some(PubSubEvent::PredictionChannel {
        kind: kind.clone(),
        event: Box::new(event),
        winning_outcome_id,
    }))
}

fn parse_prediction_user_event(
    payload: &Value,
    payload_type: &str,
) -> Result<Option<PubSubEvent>, PubSubError> {
    let kind = match payload_type {
        "prediction-made" => PredictionUserKind::PredictionMade,
        "prediction-result" => PredictionUserKind::PredictionResult,
        _ => return Ok(None),
    };
    let event_id = payload
        .pointer("/data/prediction/event_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(PubSubError::Protocol(
            "viewer prediction event id is missing",
        ))?;
    let result = if kind == PredictionUserKind::PredictionResult {
        let result = payload
            .pointer("/data/prediction/result")
            .ok_or(PubSubError::Protocol("viewer prediction result is missing"))?;
        let result_type = result
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .map(str::to_uppercase)
            .filter(|value| matches!(value.as_str(), "WIN" | "LOSE" | "REFUND"))
            .ok_or(PubSubError::Protocol(
                "viewer prediction result type is unsupported",
            ))?;
        let points_won = result
            .get("points_won")
            .and_then(Value::as_i64)
            .filter(|value| *value >= 0);
        if result_type == "WIN" && points_won.is_none() {
            return Err(PubSubError::Protocol(
                "viewer prediction win points are missing or invalid",
            ));
        }
        Some(match points_won {
            Some(points_won) => json!({"type": result_type, "points_won": points_won}),
            None => json!({"type": result_type}),
        })
    } else {
        None
    };
    Ok(Some(PubSubEvent::PredictionUser {
        event_id: event_id.to_string(),
        result,
        kind,
    }))
}

fn parse_community_goal_event(
    payload: &Value,
    payload_type: &str,
    channel_id: String,
) -> Result<Option<PubSubEvent>, PubSubError> {
    let kind = match payload_type {
        "community-goal-created" => CommunityGoalKind::Created,
        "community-goal-updated" => CommunityGoalKind::Updated,
        "community-goal-deleted" => CommunityGoalKind::Deleted,
        _ => return Ok(None),
    };
    if channel_id.trim().is_empty() {
        return Err(PubSubError::Protocol(
            "community goal channel id is missing",
        ));
    }
    let goal_value = payload.pointer("/data/community_goal").cloned();
    let goal_id = goal_value
        .as_ref()
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let goal = if kind == CommunityGoalKind::Deleted {
        None
    } else {
        goal_value
            .as_ref()
            .map(|value| {
                serde_json::from_value::<CommunityGoal>(normalize_goal_value(value))
                    .map_err(|_| PubSubError::Protocol("community goal body is invalid"))
            })
            .transpose()?
    };
    match kind {
        CommunityGoalKind::Created | CommunityGoalKind::Updated => {
            let goal = goal
                .as_ref()
                .ok_or(PubSubError::Protocol("community goal body is missing"))?;
            validate_community_goal(goal)?;
        }
        CommunityGoalKind::Deleted if goal_id.is_none() => {
            return Err(PubSubError::Protocol("community goal id is missing"));
        }
        CommunityGoalKind::Deleted => {}
    }
    Ok(Some(PubSubEvent::CommunityGoal {
        channel_id,
        kind,
        goal,
        goal_id,
    }))
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
                let nonce = envelope
                    .get("nonce")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or(PubSubError::Protocol("PubSub response nonce is missing"))?;
                return Ok(IncomingTransportMessage::ResponseOk {
                    nonce: Some(nonce.to_string()),
                });
            }
            let nonce = envelope
                .get("nonce")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(IncomingTransportMessage::ResponseError {
                is_bad_auth: error.contains("ERR_BADAUTH"),
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
    channel_id_from_payload_only(payload)
        .or_else(|| topic_channel_id(topic))
        .unwrap_or_default()
}

fn channel_id_from_payload_only(payload: &Value) -> Option<String> {
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
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn topic_channel_id(topic: &str) -> Option<String> {
    topic
        .split_once('.')
        .map(|(_, suffix)| suffix.trim())
        .filter(|suffix| !suffix.is_empty())
        .map(str::to_string)
}

fn valid_reason_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_community_goal(goal: &CommunityGoal) -> Result<(), PubSubError> {
    if goal.id.trim().is_empty()
        || goal.title.trim().is_empty()
        || goal.status.trim().is_empty()
        || goal.points_contributed < 0
        || goal.amount_needed < 0
        || goal.per_stream_user_maximum_contribution < 0
    {
        return Err(PubSubError::Protocol(
            "community goal required fields are invalid",
        ));
    }
    Ok(())
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

#[must_use]
pub fn bad_auth_cookie_file(username: Option<&str>) -> String {
    match username.map(str::trim).filter(|value| !value.is_empty()) {
        Some(username) => format!("cookies/{username}.json"),
        None => String::from("cookies/<username>.json"),
    }
}
