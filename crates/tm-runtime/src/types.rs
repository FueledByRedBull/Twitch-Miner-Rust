use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use tm_domain::{
    ActiveMultiplier, CommunityGoal, OffsetDateTime, PredictionEvent, Streamer, WatchPriority,
};

use crate::RuntimeEffect;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSummary {
    pub configured_streamers: usize,
    pub follower_mode: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeSession {
    pub summary: RuntimeSummary,
    pub state: RuntimeState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeState {
    pub started_at: OffsetDateTime,
    pub follower_mode: bool,
    pub watch_priorities: Vec<WatchPriority>,
    pub game_priority: Vec<String>,
    pub game_exclusions: Vec<String>,
    pub streamers: Vec<Streamer>,
    pub initial_points: HashMap<String, i64>,
    pub predictions: HashMap<String, PredictionEvent>,
    pub processed_prediction_ids: VecDeque<String>,
    pub completed_predictions: VecDeque<PredictionEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextUpdate {
    pub channel_id: String,
    pub balance: i64,
    pub active_multipliers: Vec<ActiveMultiplier>,
    pub community_goals: Vec<CommunityGoal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamUpdate {
    pub channel_id: String,
    pub id: String,
    pub title: String,
    pub game_name: String,
    pub game_id: Option<String>,
    pub viewers_count: u32,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub duration: String,
    pub total_points_line: String,
    pub streamers: Vec<StreamerSummary>,
    pub predictions: Vec<PredictionSummary>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventApplication {
    pub effects: Vec<RuntimeEffect>,
    pub changed: bool,
}

impl EventApplication {
    #[must_use]
    pub const fn unchanged() -> Self {
        Self {
            effects: Vec::new(),
            changed: false,
        }
    }

    #[must_use]
    pub const fn changed(effects: Vec<RuntimeEffect>) -> Self {
        Self {
            effects,
            changed: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamerSummary {
    pub channel_id: String,
    pub username: String,
    pub current_points: String,
    pub total_points_line: String,
    pub history_lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictionSummary {
    pub event_line: String,
    pub streamer_line: String,
    pub bet_settings_line: String,
    pub bet_line: String,
    pub outcome_lines: Vec<String>,
    pub result_line: String,
}
