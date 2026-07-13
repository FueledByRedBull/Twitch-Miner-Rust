use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tm_domain::Streamer;

use crate::errors::PubSubError;
use crate::policy::{PredictionSource, TransportSourcePolicy};

pub const PUBSUB_MAX_TOPICS_PER_CONNECTION: usize = 50;
pub const PUBSUB_MAX_CONNECTIONS: usize = 10;
pub const PUBSUB_MAX_TOPICS: usize = PUBSUB_MAX_TOPICS_PER_CONNECTION * PUBSUB_MAX_CONNECTIONS;
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubSubCapabilityStatus {
    pub topic_class: String,
    pub configured_topics: usize,
    pub acknowledged_topics: usize,
    pub last_message_unix: Option<u64>,
    pub reconnects: u64,
    pub failure_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubSubSetupReport {
    pub connection_count: usize,
    pub total_topics: usize,
    pub capabilities: Vec<PubSubCapabilityStatus>,
}

pub fn build_topics(user_id: &str, streamers: &[Streamer]) -> Result<Vec<String>, PubSubError> {
    build_topics_with_policy(user_id, streamers, TransportSourcePolicy::legacy_pubsub())
}

pub fn build_topics_with_policy(
    user_id: &str,
    streamers: &[Streamer],
    policy: TransportSourcePolicy,
) -> Result<Vec<String>, PubSubError> {
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

    if policy.prediction_source == PredictionSource::PubSubCompatibility
        && streamers
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
        if policy.pubsub_presence {
            push_unique(format!("video-playback-by-id.{channel_id}"));
        }
        if streamer.settings.follow_raid {
            push_unique(format!("raid.{channel_id}"));
        }
        if streamer.settings.make_predictions
            && policy.prediction_source == PredictionSource::PubSubCompatibility
        {
            push_unique(format!("predictions-channel-v1.{channel_id}"));
        }
        if streamer.settings.claim_moments {
            push_unique(format!("community-moments-channel-v1.{channel_id}"));
        }
        if streamer.settings.community_goals {
            push_unique(format!("community-points-channel-v1.{channel_id}"));
        }
    }

    Ok(topics)
}

pub fn build_topic_batches(
    user_id: &str,
    streamers: &[Streamer],
) -> Result<Vec<Vec<String>>, PubSubError> {
    checked_topic_batches(&build_topics(user_id, streamers)?)
}

pub fn build_topic_batches_with_policy(
    user_id: &str,
    streamers: &[Streamer],
    policy: TransportSourcePolicy,
) -> Result<Vec<Vec<String>>, PubSubError> {
    checked_topic_batches(&build_topics_with_policy(user_id, streamers, policy)?)
}

fn checked_topic_batches(topics: &[String]) -> Result<Vec<Vec<String>>, PubSubError> {
    if topics.len() > PUBSUB_MAX_TOPICS {
        return Err(PubSubError::CapacityExceeded {
            configured: topics.len(),
            maximum: PUBSUB_MAX_TOPICS,
        });
    }
    Ok(chunk_topics(topics))
}

#[must_use]
pub fn chunk_topics(topics: &[String]) -> Vec<Vec<String>> {
    if topics.is_empty() {
        return Vec::new();
    }

    topics
        .chunks(PUBSUB_MAX_TOPICS_PER_CONNECTION)
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

#[must_use]
pub fn pubsub_topic_class(topic: &str) -> &'static str {
    if topic.starts_with("community-points-user-v1.") {
        "points-user"
    } else if topic.starts_with("predictions-user-v1.") {
        "prediction-user"
    } else if topic.starts_with("video-playback-by-id.") {
        "presence"
    } else if topic.starts_with("raid.") {
        "raid"
    } else if topic.starts_with("community-moments-channel-v1.") {
        "moments"
    } else if topic.starts_with("predictions-channel-v1.") {
        "prediction-channel"
    } else if topic.starts_with("community-points-channel-v1.") {
        "community-goals"
    } else {
        "unknown"
    }
}

#[must_use]
pub fn pubsub_setup_report(topic_batches: &[Vec<String>]) -> PubSubSetupReport {
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for topic in topic_batches.iter().flatten() {
        *counts.entry(pubsub_topic_class(topic)).or_default() += 1;
    }
    PubSubSetupReport {
        connection_count: topic_batches.len(),
        total_topics: topic_batches.iter().map(Vec::len).sum(),
        capabilities: counts
            .into_iter()
            .map(|(topic_class, configured_topics)| PubSubCapabilityStatus {
                topic_class: topic_class.to_string(),
                configured_topics,
                acknowledged_topics: 0,
                last_message_unix: None,
                reconnects: 0,
                failure_class: None,
            })
            .collect(),
    }
}

fn random_nonce() -> String {
    format!("{:016x}", NONCE_COUNTER.fetch_add(1, Ordering::Relaxed))
}
