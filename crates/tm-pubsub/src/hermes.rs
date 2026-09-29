//! Offline, observation-only replay support for the Hermes framing seen in the
//! inspected donor implementation.
//!
//! This adapter deliberately does not open sockets, authenticate, subscribe,
//! reconnect, or execute runtime effects. It only validates recorded frames,
//! maps a known subscription ID to an already configured `PubSub` topic, and
//! reuses [`crate::parse_message`] to produce the existing [`MinerEvent`]
//! values.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use thiserror::Error;
use tm_domain::{MinerEvent, Streamer};

use crate::{parse_message, pubsub_topic_class, PubSubError};

pub const HERMES_MAX_FRAME_BYTES: usize = 256 * 1024;
pub const HERMES_MAX_SUBSCRIPTIONS: usize = 512;
pub const HERMES_MAX_SUBSCRIPTION_ID_BYTES: usize = 128;
pub const HERMES_MAX_TOPIC_BYTES: usize = 512;
pub const HERMES_MAX_REPLAY_FRAMES: usize = 100_000;

#[derive(Debug, Error)]
pub enum HermesObserverError {
    #[error("Hermes replay frame exceeds {HERMES_MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("invalid Hermes replay JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid Hermes replay protocol: {0}")]
    Protocol(&'static str),
    #[error("invalid embedded PubSub message: {0}")]
    PubSub(#[from] PubSubError),
}

#[derive(Debug, Clone, PartialEq)]
pub enum HermesObservation {
    Welcome,
    Event(Box<MinerEvent>),
    IgnoredUnknownSubscription,
    IgnoredNoEvent,
}

#[derive(Debug, Default)]
pub struct HermesObserver {
    subscriptions: BTreeMap<String, String>,
    welcomed: bool,
}

impl HermesObserver {
    pub fn new(
        subscriptions: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, HermesObserverError> {
        let mut known = BTreeMap::new();
        for (subscription_id, topic) in subscriptions {
            let subscription_id = subscription_id.trim();
            let topic = topic.trim();
            if subscription_id.is_empty()
                || subscription_id.len() > HERMES_MAX_SUBSCRIPTION_ID_BYTES
            {
                return Err(HermesObserverError::Protocol(
                    "Hermes subscription ID is empty or too long",
                ));
            }
            if topic.is_empty() || topic.len() > HERMES_MAX_TOPIC_BYTES {
                return Err(HermesObserverError::Protocol(
                    "Hermes subscription topic is empty or too long",
                ));
            }
            if pubsub_topic_class(topic) == "unknown" {
                return Err(HermesObserverError::Protocol(
                    "Hermes subscription topic is not a supported PubSub topic",
                ));
            }
            if known
                .insert(subscription_id.to_string(), topic.to_string())
                .is_some()
            {
                return Err(HermesObserverError::Protocol(
                    "Hermes subscription ID is duplicated",
                ));
            }
            if known.len() > HERMES_MAX_SUBSCRIPTIONS {
                return Err(HermesObserverError::Protocol(
                    "Hermes subscription map is too large",
                ));
            }
        }
        Ok(Self {
            subscriptions: known,
            welcomed: false,
        })
    }

    #[must_use]
    pub fn welcomed(&self) -> bool {
        self.welcomed
    }

    pub fn observe_frame(
        &mut self,
        raw: &str,
        tracked_streamers: &[Streamer],
    ) -> Result<HermesObservation, HermesObserverError> {
        if raw.len() > HERMES_MAX_FRAME_BYTES {
            return Err(HermesObserverError::FrameTooLarge);
        }
        let envelope: Value = serde_json::from_str(raw)?;
        let frame_type = envelope
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if frame_type.eq_ignore_ascii_case("welcome") {
            validate_welcome(&envelope)?;
            self.welcomed = true;
            return Ok(HermesObservation::Welcome);
        }
        if !frame_type.eq_ignore_ascii_case("notification") {
            return Err(HermesObserverError::Protocol(
                "Hermes frame type is unsupported",
            ));
        }
        if !self.welcomed {
            return Err(HermesObserverError::Protocol(
                "Hermes notification arrived before welcome",
            ));
        }

        let notification = envelope
            .get("notification")
            .and_then(Value::as_object)
            .ok_or(HermesObserverError::Protocol(
                "Hermes notification body is missing",
            ))?;
        let notification_type = notification
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !notification_type.eq_ignore_ascii_case("pubsub") {
            return Err(HermesObserverError::Protocol(
                "Hermes notification type is unsupported",
            ));
        }
        let subscription_id = notification
            .get("subscription")
            .and_then(Value::as_object)
            .and_then(|subscription| subscription.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= HERMES_MAX_SUBSCRIPTION_ID_BYTES)
            .ok_or(HermesObserverError::Protocol(
                "Hermes subscription ID is missing or too long",
            ))?;
        let Some(topic) = self.subscriptions.get(subscription_id) else {
            return Ok(HermesObservation::IgnoredUnknownSubscription);
        };
        let pubsub = notification
            .get("pubsub")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty() && value.len() <= HERMES_MAX_FRAME_BYTES)
            .ok_or(HermesObserverError::Protocol(
                "Hermes embedded PubSub message is missing or too large",
            ))?;
        // Hermes carries the PubSub event body in `notification.pubsub`.
        // The regular parser consumes Twitch's legacy MESSAGE envelope, so
        // supply the topic from the verified subscription mapping here rather
        // than trusting an event-body field that Hermes does not define.
        let _: Value = serde_json::from_str(pubsub)?;
        let message = json!({
            "type": "MESSAGE",
            "data": {
                "topic": topic,
                "message": pubsub,
            }
        });
        Ok(parse_message(&message.to_string(), tracked_streamers)?
            .map_or(HermesObservation::IgnoredNoEvent, |event| {
                HermesObservation::Event(Box::new(event))
            }))
    }
}

fn validate_welcome(envelope: &Value) -> Result<(), HermesObserverError> {
    let envelope = envelope.as_object().ok_or(HermesObserverError::Protocol(
        "Hermes welcome frame is not an object",
    ))?;
    bounded_nonempty_string(envelope, "id", 128)?;
    bounded_nonempty_string(envelope, "timestamp", 128)?;
    let welcome =
        envelope
            .get("welcome")
            .and_then(Value::as_object)
            .ok_or(HermesObserverError::Protocol(
                "Hermes welcome body is missing",
            ))?;
    let keepalive_seconds = welcome
        .get("keepaliveSec")
        .and_then(Value::as_u64)
        .filter(|seconds| (1..=86_400).contains(seconds))
        .ok_or(HermesObserverError::Protocol(
            "Hermes welcome keepaliveSec is invalid",
        ))?;
    let _ = keepalive_seconds;
    bounded_nonempty_string(welcome, "recoveryUrl", 2_048)?;
    bounded_nonempty_string(welcome, "sessionId", 128)?;
    Ok(())
}

fn bounded_nonempty_string(
    object: &serde_json::Map<String, Value>,
    field: &str,
    max_bytes: usize,
) -> Result<(), HermesObserverError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= max_bytes)
        .ok_or(HermesObserverError::Protocol(
            "Hermes required string field is missing or too long",
        ))?;
    let _ = value;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SUBSCRIPTION_ID: &str = "subscription-1";
    const TOPIC: &str = "community-points-user-v1.viewer";

    fn observer() -> HermesObserver {
        HermesObserver::new([(SUBSCRIPTION_ID.to_string(), TOPIC.to_string())])
            .expect("valid Hermes test mapping")
    }

    fn welcome() -> String {
        json!({
            "id": "hermes-session",
            "type": "welcome",
            "timestamp": "2026-09-05T12:00:00Z",
            "welcome": {
                "keepaliveSec": 10,
                "recoveryUrl": "wss://example.invalid/recover",
                "sessionId": "session-1"
            }
        })
        .to_string()
    }

    fn notification(subscription_id: &str, payload: &str) -> String {
        json!({
            "type": "notification",
            "notification": {
                "type": "pubsub",
                "subscription": {"id": subscription_id},
                "pubsub": payload
            }
        })
        .to_string()
    }

    fn points_message() -> String {
        json!({
            "type": "points-earned",
            "data": {
                "channel_id": "100",
                "point_gain": {"total_points": 10, "reason_code": "WATCH"},
                "balance": {"balance": 110},
                "timestamp": "2026-09-05T12:00:00Z"
            }
        })
        .to_string()
    }

    #[test]
    fn welcome_is_validated_and_changes_observer_state() {
        let mut observer = observer();
        assert!(!observer.welcomed());
        assert_eq!(
            observer.observe_frame(&welcome(), &[]).unwrap(),
            HermesObservation::Welcome
        );
        assert!(observer.welcomed());
    }

    #[test]
    fn unknown_subscription_is_ignored_without_parsing_untrusted_payload() {
        let mut observer = observer();
        observer.observe_frame(&welcome(), &[]).unwrap();
        let frame = notification("unknown", "not-json");
        assert_eq!(
            observer.observe_frame(&frame, &[]).unwrap(),
            HermesObservation::IgnoredUnknownSubscription
        );
    }

    #[test]
    fn malformed_frames_fail_closed() {
        let mut observer = observer();
        assert!(matches!(
            observer.observe_frame("{", &[]),
            Err(HermesObserverError::InvalidJson(_))
        ));
        observer.observe_frame(&welcome(), &[]).unwrap();
        let malformed = notification(SUBSCRIPTION_ID, "not-json");
        assert!(matches!(
            observer.observe_frame(&malformed, &[]),
            Err(HermesObserverError::InvalidJson(_))
        ));
    }

    #[test]
    fn duplicate_notifications_are_normalized_independently_for_observation() {
        let mut observer = observer();
        observer.observe_frame(&welcome(), &[]).unwrap();
        let frame = notification(SUBSCRIPTION_ID, &points_message());
        let first = observer.observe_frame(&frame, &[]).unwrap();
        let second = observer.observe_frame(&frame, &[]).unwrap();
        assert!(matches!(first, HermesObservation::Event(_)));
        assert!(matches!(second, HermesObservation::Event(_)));
        assert_eq!(first, second);
    }

    #[test]
    fn subscription_mapping_supplies_topic_to_existing_parser() {
        let mut observer = HermesObserver::new([(
            SUBSCRIPTION_ID.to_string(),
            String::from("video-playback-by-id.100"),
        )])
        .unwrap();
        observer.observe_frame(&welcome(), &[]).unwrap();
        let body = json!({
            "type": "stream-up",
            "data": {"channel_id": "100"}
        })
        .to_string();
        let observation = observer
            .observe_frame(&notification(SUBSCRIPTION_ID, &body), &[])
            .unwrap();
        assert_eq!(
            observation,
            HermesObservation::Event(Box::new(MinerEvent::Playback {
                channel_id: String::from("100"),
                kind: tm_domain::PlaybackType::StreamUp,
            }))
        );
    }
}
