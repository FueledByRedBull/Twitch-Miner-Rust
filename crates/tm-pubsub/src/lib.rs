//! Twitch `EventSub` plus isolated legacy `PubSub` compatibility transports.
//!
//! `EventSub` and `PubSub` are independently supervised and expose typed,
//! transport-neutral events. Subscription capacity is planned deterministically;
//! malformed, untracked, or unsupported payloads fail closed before runtime
//! application.

pub const WEBSOCKET_URL: &str = "wss://pubsub-edge.twitch.tv/v1";

mod client;
mod errors;
mod eventsub;
mod parse;
mod prediction;
mod topics;
mod types;

pub use client::{PubSubClient, PubSubClientSettings, PubSubConnectionEvent};
pub use errors::PubSubError;
pub use eventsub::{
    parse_eventsub_message, plan_eventsub_capacity, EventSubClient, EventSubClientSettings,
    EventSubConnectionEvent, EventSubError, EventSubMessage, EventSubSetupReport,
    EventSubStreamerCapability, EventSubTimeoutStage, EVENTSUB_SUBSCRIPTIONS_URL,
    EVENTSUB_WEBSOCKET_URL,
};
pub use parse::{
    bad_auth_cookie_file, channel_id_from_payload, parse_message, parse_transport_message,
};
pub use tm_domain::{
    CommunityGoalKind, MinerEvent, PlaybackType, PredictionChannelKind, PredictionUserKind,
};
pub use topics::{
    build_topic_batches, build_topics, chunk_topics, listen_payload, listen_payload_with_nonce,
    listen_payloads, ping_payload, pubsub_setup_report, pubsub_topic_class, topic_requires_auth,
    PubSubCapabilityStatus, PubSubSetupReport, PUBSUB_MAX_CONNECTIONS, PUBSUB_MAX_TOPICS,
    PUBSUB_MAX_TOPICS_PER_CONNECTION,
};
pub use types::IncomingTransportMessage;

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
mod tests;
