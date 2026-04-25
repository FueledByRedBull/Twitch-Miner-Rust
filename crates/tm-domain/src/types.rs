#![allow(clippy::cast_precision_loss, clippy::struct_excessive_bools)]

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FollowersOrder {
    #[serde(rename = "ASC")]
    Asc,
    #[serde(rename = "DESC")]
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Strategy {
    #[serde(rename = "MOST_VOTED")]
    MostVoted,
    #[serde(rename = "HIGH_ODDS")]
    HighOdds,
    #[serde(rename = "PERCENTAGE")]
    Percentage,
    #[serde(rename = "SMART_MONEY")]
    SmartMoney,
    #[default]
    #[serde(rename = "SMART")]
    Smart,
    #[serde(rename = "NUMBER_1")]
    Number1,
    #[serde(rename = "NUMBER_2")]
    Number2,
    #[serde(rename = "NUMBER_3")]
    Number3,
    #[serde(rename = "NUMBER_4")]
    Number4,
    #[serde(rename = "NUMBER_5")]
    Number5,
    #[serde(rename = "NUMBER_6")]
    Number6,
    #[serde(rename = "NUMBER_7")]
    Number7,
    #[serde(rename = "NUMBER_8")]
    Number8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DelayMode {
    #[serde(rename = "FROM_START")]
    FromStart,
    #[default]
    #[serde(rename = "FROM_END")]
    FromEnd,
    #[serde(rename = "PERCENTAGE")]
    Percentage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IrcMode {
    #[serde(rename = "ALWAYS")]
    Always,
    #[serde(rename = "NEVER")]
    Never,
    #[default]
    #[serde(rename = "ONLINE")]
    Online,
    #[serde(rename = "OFFLINE")]
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    #[serde(rename = "GT")]
    Gt,
    #[serde(rename = "LT")]
    Lt,
    #[serde(rename = "GTE")]
    Gte,
    #[serde(rename = "LTE")]
    Lte,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutcomeKey {
    #[serde(rename = "PERCENTAGE_USERS")]
    PercentageUsers,
    #[serde(rename = "ODDS")]
    Odds,
    #[serde(rename = "ODDS_PERCENTAGE")]
    OddsPercentage,
    #[serde(rename = "TOP_POINTS")]
    TopPoints,
    #[serde(rename = "TOTAL_USERS")]
    TotalUsers,
    #[serde(rename = "TOTAL_POINTS")]
    TotalPoints,
    #[serde(rename = "DECISION_USERS")]
    DecisionUsers,
    #[serde(rename = "DECISION_POINTS")]
    DecisionPoints,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterCondition {
    pub by: OutcomeKey,
    #[serde(rename = "where")]
    pub condition: Condition,
    pub value: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetSettings {
    pub strategy: Strategy,
    pub percentage: Option<u32>,
    pub percentage_gap: Option<u32>,
    pub max_points: Option<u32>,
    pub minimum_points: Option<u32>,
    pub stealth_mode: Option<bool>,
    pub deduct_stake_on_place: Option<bool>,
    pub filter_condition: Option<FilterCondition>,
    pub delay: Option<f64>,
    pub delay_mode: DelayMode,
}

impl Default for BetSettings {
    fn default() -> Self {
        Self {
            strategy: Strategy::Smart,
            percentage: Some(5),
            percentage_gap: Some(20),
            max_points: Some(50_000),
            minimum_points: Some(0),
            stealth_mode: Some(false),
            deduct_stake_on_place: Some(true),
            filter_condition: None,
            delay: Some(6.0),
            delay_mode: DelayMode::FromEnd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamerSettings {
    pub make_predictions: bool,
    pub follow_raid: bool,
    pub claim_drops: bool,
    pub claim_moments: bool,
    pub watch_streak: bool,
    pub community_goals: bool,
    pub bet: BetSettings,
    #[serde(rename = "chat_presence")]
    pub irc_mode: IrcMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ActiveMultiplier {
    pub factor: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HistoryEntry {
    pub count: u32,
    pub amount: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Game {
    pub display_name: Option<String>,
    pub name: Option<String>,
}

impl Game {
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        let value = (!name.trim().is_empty()).then(|| name.to_string());
        Self {
            display_name: value.clone(),
            name: value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    pub broadcast_id: String,
    pub title: String,
    pub game: Option<Game>,
    pub drops_tags: bool,
    pub viewers_count: u32,
    pub payload: Vec<serde_json::Value>,
    pub watch_streak_missing: bool,
    pub minute_watched: f64,
    pub stream_up_at: Option<OffsetDateTime>,
    pub last_update: Option<OffsetDateTime>,
    pub last_minute_update: Option<OffsetDateTime>,
}

impl Default for Stream {
    fn default() -> Self {
        Self {
            broadcast_id: String::new(),
            title: String::new(),
            game: None,
            drops_tags: false,
            viewers_count: 0,
            payload: Vec::new(),
            watch_streak_missing: true,
            minute_watched: 0.0,
            stream_up_at: None,
            last_update: None,
            last_minute_update: None,
        }
    }
}

impl Stream {
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        broadcast_id: impl Into<String>,
        title: impl Into<String>,
        game: Game,
        tag_ids: &[String],
        viewers: u32,
        drop_id: &str,
        now: OffsetDateTime,
    ) {
        self.broadcast_id = broadcast_id.into();
        self.title = title.into().trim().to_string();
        self.drops_tags = tag_ids.iter().any(|tag| tag == drop_id)
            && (game.display_name.is_some() || game.name.is_some());
        self.game = Some(game);
        self.viewers_count = viewers;
        self.last_update = Some(now);
    }

    #[must_use]
    pub fn update_required_at(&self, now: OffsetDateTime) -> bool {
        match self.last_update {
            Some(last_update) => now - last_update >= time::Duration::seconds(120),
            None => true,
        }
    }

    pub fn update_minute_watched(&mut self, now: OffsetDateTime) {
        if let Some(last_minute_update) = self.last_minute_update {
            let elapsed = now - last_minute_update;
            self.minute_watched += elapsed.whole_milliseconds() as f64 / 60_000.0;
        }
        self.last_minute_update = Some(now);
    }

    pub fn reset_watch_progress(&mut self) {
        self.minute_watched = 0.0;
        self.last_minute_update = None;
    }

    #[must_use]
    pub fn last_update_ago_at(&self, now: OffsetDateTime) -> Duration {
        self.last_update
            .map(|last_update| {
                Duration::from_secs((now - last_update).whole_seconds().max(0).cast_unsigned())
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn stream_up_elapsed_at(&self, now: OffsetDateTime) -> bool {
        self.stream_up_at
            .is_none_or(|started| now - started > time::Duration::minutes(2))
    }

    pub fn encode_payload(&self) -> Result<HashMap<String, String>, serde_json::Error> {
        let raw = serde_json::to_vec(&self.payload)?;
        let mut result = HashMap::new();
        result.insert(
            String::from("data"),
            base64::engine::general_purpose::STANDARD.encode(raw),
        );
        Ok(result)
    }

    #[must_use]
    pub fn game_name(&self) -> String {
        self.game
            .as_ref()
            .and_then(|game| {
                game.display_name
                    .as_ref()
                    .or(game.name.as_ref())
                    .map(|value| value.trim().to_string())
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CommunityGoal {
    pub id: String,
    pub title: String,
    pub is_in_stock: bool,
    pub points_contributed: i64,
    pub amount_needed: i64,
    pub per_stream_user_maximum_contribution: i64,
    pub status: String,
}

impl CommunityGoal {
    #[must_use]
    pub fn amount_left(&self) -> i64 {
        self.amount_needed - self.points_contributed
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.id.trim().is_empty()
            && self.is_in_stock
            && self.status.trim().eq_ignore_ascii_case("STARTED")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Streamer {
    pub username: String,
    pub channel_id: String,
    pub channel_points: i64,
    pub settings: StreamerSettings,
    pub is_online: bool,
    pub presence_known: bool,
    pub online_at: Option<OffsetDateTime>,
    pub offline_at: Option<OffsetDateTime>,
    pub stream: Option<Stream>,
    pub points_init: bool,
    pub active_multipliers: Vec<ActiveMultiplier>,
    pub last_raid_id: String,
    pub history: HashMap<String, HistoryEntry>,
    pub community_goals: HashMap<String, CommunityGoal>,
}

impl Streamer {
    #[must_use]
    pub fn has_active_multipliers(&self) -> bool {
        !self.active_multipliers.is_empty()
    }

    #[must_use]
    pub fn total_multiplier(&self) -> f64 {
        self.active_multipliers.iter().map(|item| item.factor).sum()
    }

    #[must_use]
    pub fn prediction_window_seconds(&self, prediction_window: f64) -> f64 {
        let delay = self.settings.bet.delay.unwrap_or_default();
        match self.settings.bet.delay_mode {
            DelayMode::FromStart => delay.min(prediction_window),
            DelayMode::FromEnd => (prediction_window - delay).max(0.0),
            DelayMode::Percentage => prediction_window * delay,
        }
    }

    pub fn apply_channel_points_context(
        &mut self,
        balance: i64,
        active_multipliers: &[ActiveMultiplier],
        community_goals: &[CommunityGoal],
    ) {
        self.channel_points = balance.max(0);
        self.active_multipliers.clear();
        self.active_multipliers
            .extend_from_slice(active_multipliers);
        self.community_goals = community_goals
            .iter()
            .cloned()
            .map(|goal| (goal.id.clone(), goal))
            .collect();
        self.points_init = true;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn bet_defaults_match_go() {
        let bet = BetSettings::default();
        assert_eq!(bet.strategy, Strategy::Smart);
        assert_eq!(bet.percentage, Some(5));
        assert_eq!(bet.percentage_gap, Some(20));
        assert_eq!(bet.max_points, Some(50_000));
        assert_eq!(bet.minimum_points, Some(0));
        assert_eq!(bet.stealth_mode, Some(false));
        assert_eq!(bet.deduct_stake_on_place, Some(true));
        assert_eq!(bet.delay_mode, DelayMode::FromEnd);
        assert_eq!(bet.delay, Some(6.0));
    }

    #[test]
    fn streamer_defaults_propagate() {
        let streamer = StreamerSettings::default();
        assert_eq!(streamer.bet.strategy, Strategy::Smart);
        assert_eq!(streamer.bet.delay, Some(6.0));
        assert_eq!(streamer.irc_mode, IrcMode::Online);
    }

    #[test]
    fn multipliers_sum() {
        let streamer = Streamer {
            active_multipliers: vec![
                ActiveMultiplier { factor: 1.5 },
                ActiveMultiplier { factor: 2.0 },
            ],
            ..Streamer::default()
        };
        assert!(streamer.has_active_multipliers());
        assert_eq!(streamer.total_multiplier(), 3.5);
    }

    #[test]
    fn streamer_applies_channel_points_context() {
        let mut streamer = Streamer::default();
        streamer.apply_channel_points_context(
            -50,
            &[ActiveMultiplier { factor: 1.5 }],
            &[CommunityGoal {
                id: String::from("goal-1"),
                title: String::from("Goal"),
                is_in_stock: true,
                points_contributed: 5,
                amount_needed: 10,
                per_stream_user_maximum_contribution: 5,
                status: String::from("STARTED"),
            }],
        );

        assert_eq!(streamer.channel_points, 0);
        assert_eq!(
            streamer.active_multipliers,
            vec![ActiveMultiplier { factor: 1.5 }]
        );
        assert!(streamer.community_goals.contains_key("goal-1"));
        assert!(streamer.points_init);
    }

    #[test]
    fn community_goal_active_matches_runtime_rules() {
        assert!(CommunityGoal {
            id: String::from("goal-1"),
            title: String::from("Goal"),
            is_in_stock: true,
            points_contributed: 0,
            amount_needed: 10,
            per_stream_user_maximum_contribution: 5,
            status: String::from("STARTED"),
        }
        .is_active());
        assert!(!CommunityGoal {
            id: String::new(),
            title: String::from("Goal"),
            is_in_stock: true,
            points_contributed: 0,
            amount_needed: 10,
            per_stream_user_maximum_contribution: 5,
            status: String::from("STARTED"),
        }
        .is_active());
    }

    #[test]
    fn prediction_window_modes() {
        let mut streamer = Streamer::default();
        streamer.settings.bet.delay = Some(2.0);
        streamer.settings.bet.delay_mode = DelayMode::FromStart;
        assert_eq!(streamer.prediction_window_seconds(5.0), 2.0);

        streamer.settings.bet.delay_mode = DelayMode::FromEnd;
        assert_eq!(streamer.prediction_window_seconds(5.0), 3.0);

        streamer.settings.bet.delay_mode = DelayMode::Percentage;
        streamer.settings.bet.delay = Some(0.5);
        assert_eq!(streamer.prediction_window_seconds(10.0), 5.0);
    }

    #[test]
    fn stream_helpers() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut stream = Stream::default();
        assert!(stream.update_required_at(now));
        stream.update(
            "id",
            "title",
            Game {
                display_name: Some(String::from("Game")),
                name: None,
            },
            &[String::from("drop-tag")],
            100,
            "drop-tag",
            now,
        );
        assert_eq!(stream.title, "title");
        assert_eq!(stream.broadcast_id, "id");
        assert!(stream.drops_tags);
        assert!(!stream.update_required_at(now));
    }

    #[test]
    fn stream_watch_progress_and_game_name() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut stream = Stream::default();
        stream.last_minute_update = Some(now - time::Duration::minutes(2));
        stream.update_minute_watched(now);
        assert!(stream.minute_watched > 1.9 && stream.minute_watched < 2.1);
        stream.reset_watch_progress();
        assert_eq!(stream.minute_watched, 0.0);
        assert!(stream.last_minute_update.is_none());

        assert_eq!(stream.game_name(), "");
        stream.game = Some(Game {
            display_name: Some(String::from("My Game")),
            name: None,
        });
        assert_eq!(stream.game_name(), "My Game");
        stream.game = Some(Game {
            display_name: None,
            name: Some(String::from("Other")),
        });
        assert_eq!(stream.game_name(), "Other");
    }
}
