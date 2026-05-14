use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use tm_domain::Streamer;

use crate::errors::PubSubError;

const TOPIC_BATCH_SIZE: usize = 50;
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);

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

fn random_nonce() -> String {
    format!("{:016x}", NONCE_COUNTER.fetch_add(1, Ordering::Relaxed))
}
