use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_domain::{CommunityGoal, PredictionEvent};

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
