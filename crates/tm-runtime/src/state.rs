use std::collections::HashMap;

use tm_config::{build_base_streamer_settings, build_override_settings, ConfigFile};
use tm_domain::{
    normalize_game_list, normalize_streamer_list, parse_watch_priorities, pick_streamers_to_watch,
    should_join_chat, CommunityGoal, Game, OffsetDateTime, PredictionDecision, Stream, Streamer,
};
use tm_events::{
    CommunityGoalKind, MinerEvent, PlaybackType, PredictionChannelKind, PredictionUserKind,
};

use crate::effect::RuntimeEffect;
use crate::prediction::{build_prediction_settlement_effect, prediction_status_is_resolved};
use crate::summary::{apply_pubsub_gain, build_session_summary};
use crate::types::{
    ContextUpdate, RuntimeSession, RuntimeState, RuntimeSummary, SessionSummary, StreamUpdate,
};

impl RuntimeSession {
    #[must_use]
    pub fn from_state(mut state: RuntimeState) -> Self {
        state.capture_initial_points();
        let summary = RuntimeSummary {
            configured_streamers: state.streamers.len(),
            follower_mode: state.follower_mode,
        };
        Self { summary, state }
    }

    #[must_use]
    pub fn session_summary(&self, anonymize: bool, now: OffsetDateTime) -> SessionSummary {
        self.state.session_summary(anonymize, now)
    }

    #[must_use]
    pub fn current_session_summary(&self, anonymize: bool) -> SessionSummary {
        self.session_summary(anonymize, OffsetDateTime::now_utc())
    }
}

impl RuntimeState {
    #[must_use]
    pub fn from_config(config: &ConfigFile, started_at: OffsetDateTime) -> Self {
        let targets = normalize_streamer_list(&config.streamers);
        Self::from_targets(config, &targets, started_at)
    }

    #[must_use]
    pub fn from_targets(
        config: &ConfigFile,
        targets: &[String],
        started_at: OffsetDateTime,
    ) -> Self {
        let base_settings = build_base_streamer_settings(config);
        let overrides = build_override_settings(&base_settings, &config.streamer_overrides);
        let excluded = normalize_streamer_list(&config.streamers_exclude);
        let streamers = normalize_streamer_list(targets)
            .into_iter()
            .filter(|login| !excluded.contains(login))
            .map(|login| Streamer {
                username: login.clone(),
                settings: overrides
                    .get(&login)
                    .cloned()
                    .unwrap_or_else(|| base_settings.clone()),
                ..Streamer::default()
            })
            .collect();

        Self {
            started_at,
            follower_mode: config.streamers.is_empty(),
            watch_priorities: parse_watch_priorities(&config.watch_priority),
            game_priority: normalize_game_list(&config.game_priority),
            game_exclusions: normalize_game_list(&config.game_exclude),
            streamers,
            initial_points: HashMap::new(),
            predictions: HashMap::new(),
        }
    }

    pub fn capture_initial_points(&mut self) {
        self.initial_points = self
            .streamers
            .iter()
            .map(|streamer| (streamer.username.clone(), streamer.channel_points))
            .collect();
    }

    #[must_use]
    pub fn watch_target_indices(&self, now: OffsetDateTime) -> Vec<usize> {
        pick_streamers_to_watch(
            &self.streamers,
            &self.watch_priorities,
            &self.game_priority,
            &self.game_exclusions,
            Some(self.started_at),
            now,
        )
    }

    #[must_use]
    pub fn watch_target_logins(&self, now: OffsetDateTime) -> Vec<String> {
        self.watch_target_indices(now)
            .into_iter()
            .filter_map(|idx| self.streamers.get(idx))
            .map(|streamer| streamer.username.clone())
            .collect()
    }

    #[must_use]
    pub fn desired_chat_logins(&self) -> Vec<String> {
        self.streamers
            .iter()
            .filter(|streamer| should_join_chat(streamer.settings.irc_mode, streamer.is_online))
            .map(|streamer| streamer.username.clone())
            .collect()
    }

    #[must_use]
    pub fn session_summary(&self, anonymize: bool, now: OffsetDateTime) -> SessionSummary {
        let duration = (now - self.started_at)
            .whole_seconds()
            .max(0)
            .cast_unsigned();
        let initial_points = self
            .initial_points
            .iter()
            .map(|(username, points)| (username.as_str(), *points))
            .collect::<Vec<_>>();
        build_session_summary(
            &self.streamers,
            &initial_points,
            anonymize,
            std::time::Duration::from_secs(duration),
        )
    }

    #[allow(clippy::too_many_lines, clippy::redundant_closure_for_method_calls)]
    pub fn apply_event(&mut self, event: &MinerEvent, now: OffsetDateTime) -> Vec<RuntimeEffect> {
        match event {
            MinerEvent::PointsEarned {
                channel_id,
                earned,
                reason,
                balance,
            } => self
                .streamer_mut_by_channel_id(channel_id)
                .map(|streamer| {
                    apply_pubsub_gain(streamer, *earned, reason, *balance);
                    Vec::new()
                })
                .unwrap_or_default(),
            MinerEvent::ClaimAvailable {
                channel_id,
                claim_id,
            } => self
                .streamer_by_channel_id(channel_id)
                .map(|_| {
                    vec![RuntimeEffect::ClaimBonus {
                        channel_id: channel_id.clone(),
                        claim_id: claim_id.clone(),
                    }]
                })
                .unwrap_or_default(),
            MinerEvent::Playback { channel_id, kind } => match kind {
                PlaybackType::StreamUp => {
                    self.apply_presence(channel_id, true, now);
                    Vec::new()
                }
                PlaybackType::StreamDown => {
                    self.apply_presence(channel_id, false, now);
                    Vec::new()
                }
                PlaybackType::Viewcount => Vec::new(),
            },
            MinerEvent::Raid {
                channel_id,
                raid_id,
                target_login,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return Vec::new();
                };
                if !streamer.settings.follow_raid
                    || raid_id.is_empty()
                    || streamer.last_raid_id == *raid_id
                {
                    return Vec::new();
                }
                streamer.last_raid_id.clone_from(raid_id);
                vec![RuntimeEffect::JoinRaid {
                    channel_id: channel_id.clone(),
                    raid_id: raid_id.clone(),
                    target_login: target_login.clone(),
                }]
            }
            MinerEvent::Moment {
                channel_id,
                moment_id,
            } => {
                let Some(streamer) = self.streamer_by_channel_id(channel_id) else {
                    return Vec::new();
                };
                if !streamer.settings.claim_moments || moment_id.is_empty() {
                    return Vec::new();
                }
                vec![RuntimeEffect::ClaimMoment {
                    channel_id: channel_id.clone(),
                    moment_id: moment_id.clone(),
                }]
            }
            MinerEvent::PredictionChannel {
                kind,
                event,
                winning_outcome_id,
            } => match kind {
                PredictionChannelKind::EventCreated => {
                    if event.event_id.is_empty()
                        || event.status != "ACTIVE"
                        || !event.streamer.settings.make_predictions
                    {
                        return Vec::new();
                    }
                    self.predictions
                        .insert(event.event_id.clone(), event.as_ref().clone());
                    vec![RuntimeEffect::EvaluatePrediction {
                        event_id: event.event_id.clone(),
                    }]
                }
                PredictionChannelKind::EventUpdated => {
                    let event_id = event.event_id.clone();
                    let effect = {
                        let Some(existing) = self.predictions.get_mut(&event_id) else {
                            return Vec::new();
                        };
                        existing.status.clone_from(&event.status);
                        existing.title.clone_from(&event.title);
                        existing.created_at = event.created_at;
                        existing.outcomes.clone_from(&event.outcomes);
                        if !existing.bet_placed
                            || existing.decision.amount <= 0
                            || !existing.result_type.is_empty()
                            || !prediction_status_is_resolved(&existing.status)
                        {
                            None
                        } else {
                            build_prediction_settlement_effect(
                                existing,
                                winning_outcome_id.as_deref(),
                            )
                        }
                    };
                    let Some(effect) = effect else {
                        return Vec::new();
                    };
                    self.predictions.remove(&event_id);
                    vec![effect]
                }
            },
            MinerEvent::PredictionUser {
                event_id,
                kind,
                result,
            } => match kind {
                PredictionUserKind::PredictionMade => {
                    let Some(event) = self.predictions.get_mut(event_id) else {
                        return Vec::new();
                    };
                    event.bet_confirmed = true;
                    Vec::new()
                }
                PredictionUserKind::PredictionResult => {
                    let Some(mut event) = self.predictions.remove(event_id) else {
                        return Vec::new();
                    };
                    if !event.bet_confirmed {
                        event.bet_confirmed = true;
                    }
                    let settlement = event.parse_result(
                        result
                            .as_ref()
                            .and_then(|value| value.get("type"))
                            .and_then(|value| value.as_str())
                            .unwrap_or_default(),
                        result
                            .as_ref()
                            .and_then(|value| value.get("points_won"))
                            .and_then(|value| value.as_i64())
                            .unwrap_or_default(),
                    );
                    vec![RuntimeEffect::PredictionSettled {
                        event_id: event_id.clone(),
                        streamer_username: event.streamer.username.clone(),
                        title: event.title.clone(),
                        decision_label: settlement.decision_label,
                        result_type: settlement.result_type,
                        result_string: settlement.result_string,
                    }]
                }
            },
            MinerEvent::CommunityGoal {
                channel_id,
                kind,
                goal,
                goal_id,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return Vec::new();
                };
                if !streamer.settings.community_goals {
                    return Vec::new();
                }
                match kind {
                    CommunityGoalKind::Created | CommunityGoalKind::Updated => {
                        let Some(goal) = goal.as_ref() else {
                            return Vec::new();
                        };
                        streamer
                            .community_goals
                            .insert(goal.id.clone(), goal.clone());
                        vec![RuntimeEffect::ContributeCommunityGoals {
                            channel_id: channel_id.clone(),
                        }]
                    }
                    CommunityGoalKind::Deleted => {
                        if let Some(goal_id) = goal_id {
                            streamer.community_goals.remove(goal_id);
                        }
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Compatibility wrapper for callers that still use the former transport name.
    pub fn apply_pubsub_event(
        &mut self,
        event: &MinerEvent,
        now: OffsetDateTime,
    ) -> Vec<RuntimeEffect> {
        self.apply_event(event, now)
    }

    fn streamer_by_channel_id(&self, channel_id: &str) -> Option<&Streamer> {
        self.streamers
            .iter()
            .find(|streamer| streamer.channel_id == channel_id)
    }

    fn streamer_mut_by_channel_id(&mut self, channel_id: &str) -> Option<&mut Streamer> {
        self.streamers
            .iter_mut()
            .find(|streamer| streamer.channel_id == channel_id)
    }

    pub(crate) fn apply_presence(&mut self, channel_id: &str, online: bool, now: OffsetDateTime) {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return;
        };
        let prev_online = streamer.is_online;
        if !streamer.presence_known || prev_online != online {
            streamer.presence_known = true;
            streamer.is_online = online;
            if online {
                streamer.online_at = Some(now);
                streamer.offline_at = None;
                let stream = streamer.stream.get_or_insert_with(Stream::default);
                if stream.stream_up_at.is_none() {
                    stream.stream_up_at = Some(now);
                }
            } else {
                streamer.offline_at = Some(now);
                if let Some(stream) = streamer.stream.as_mut() {
                    stream.stream_up_at = None;
                    stream.reset_watch_progress();
                }
            }
        }
    }

    pub fn apply_context_update(&mut self, update: &ContextUpdate) -> Vec<RuntimeEffect> {
        let Some(streamer) = self.streamer_mut_by_channel_id(&update.channel_id) else {
            return Vec::new();
        };
        streamer.apply_channel_points_context(
            update.balance,
            &update.active_multipliers,
            &update.community_goals,
        );
        if streamer.settings.community_goals
            && streamer
                .community_goals
                .values()
                .any(CommunityGoal::is_active)
        {
            return vec![RuntimeEffect::ContributeCommunityGoals {
                channel_id: update.channel_id.clone(),
            }];
        }
        Vec::new()
    }

    pub fn apply_stream_update(&mut self, update: &StreamUpdate, now: OffsetDateTime) {
        let Some(streamer) = self.streamer_mut_by_channel_id(&update.channel_id) else {
            return;
        };
        let stream = streamer.stream.get_or_insert_with(Stream::default);
        let broadcast_changed = !stream.broadcast_id.is_empty() && stream.broadcast_id != update.id;
        if stream.stream_up_at.is_none() || broadcast_changed {
            stream.stream_up_at = Some(now);
        }
        if broadcast_changed {
            stream.reset_watch_progress();
            stream.watch_streak_missing = true;
        }
        stream.update(
            &update.id,
            &update.title,
            Game {
                display_name: (!update.game_name.trim().is_empty())
                    .then(|| update.game_name.clone()),
                name: (!update.game_name.trim().is_empty()).then(|| update.game_name.clone()),
            },
            &update.tags,
            update.viewers_count,
            tm_twitch_drop_id(),
            now,
        );
    }

    pub fn mark_minute_watched(&mut self, channel_id: &str, now: OffsetDateTime) {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return;
        };
        let Some(stream) = streamer.stream.as_mut() else {
            return;
        };
        stream.update_minute_watched(now);
    }

    pub fn record_prediction_placed(
        &mut self,
        event_id: &str,
        decision: &PredictionDecision,
        deduct_stake: bool,
    ) {
        let Some(mut event) = self.predictions.remove(event_id) else {
            return;
        };
        event.decision.clone_from(decision);
        event.bet_placed = true;
        event.bet_confirmed = true;
        if let Some(streamer) = self.streamer_mut_by_channel_id(&event.streamer.channel_id) {
            event.streamer = streamer.clone();
            if deduct_stake && decision.amount > 0 {
                apply_pubsub_gain(streamer, -decision.amount, "PREDICTION", 0);
                event.streamer = streamer.clone();
            }
        }
        self.predictions.insert(event.event_id.clone(), event);
    }

    pub fn stop_tracking_prediction(&mut self, event_id: &str, result_type: &str) {
        if let Some(mut event) = self.predictions.remove(event_id) {
            event.result_type = result_type.to_string();
        }
    }
}

fn tm_twitch_drop_id() -> &'static str {
    "c2542d6d-cd10-4532-919b-3d19f30a768b"
}
