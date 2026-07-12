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

#[must_use]
pub fn bad_auth_cookie_file(username: Option<&str>) -> String {
    match username.map(str::trim).filter(|value| !value.is_empty()) {
        Some(username) => format!("cookies/{username}.json"),
        None => String::from("cookies/<username>.json"),
    }
}
