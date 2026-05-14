use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tm_domain::{
    ActiveMultiplier, CommunityGoal, OffsetDateTime, PredictionEvent, Streamer, WatchPriority,
};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamerSummary {
    pub username: String,
    pub current_points: String,
    pub total_points_line: String,
    pub history_lines: Vec<String>,
}
