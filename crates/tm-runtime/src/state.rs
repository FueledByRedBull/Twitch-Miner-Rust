use std::collections::HashMap;

use tm_config::{build_base_streamer_settings, build_override_settings, ConfigFile};
use tm_domain::{
    normalize_game_list, normalize_streamer_list, parse_watch_priorities, pick_streamers_to_watch,
    should_join_chat, CommunityGoal, Game, OffsetDateTime, PredictionDecision, PredictionEvent,
    Stream, Streamer,
};
use tm_events::{
    CommunityGoalKind, MinerEvent, PlaybackType, PredictionChannelKind, PredictionUserKind,
};

use crate::effect::RuntimeEffect;
use crate::prediction::{build_prediction_settlement_effect, prediction_status_is_resolved};
use crate::summary::{apply_pubsub_gain, build_session_summary};
use crate::types::{
    ContextUpdate, EventApplication, RuntimeSession, RuntimeState, RuntimeSummary, SessionSummary,
    StreamUpdate,
};

const MAX_COMPLETED_PREDICTIONS: usize = 256;

const MAX_PROCESSED_MUTATION_IDS: usize = 128;
const STREAK_RESTART_CARRYOVER_SECONDS: i64 = 30 * 60;

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
            processed_prediction_ids: std::collections::VecDeque::new(),
            completed_predictions: std::collections::VecDeque::new(),
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
        let duration_micros = (now - self.started_at).whole_microseconds().max(0);
        let duration_micros = u64::try_from(duration_micros).unwrap_or(u64::MAX);
        let initial_points = self
            .initial_points
            .iter()
            .map(|(username, points)| (username.as_str(), *points))
            .collect::<Vec<_>>();
        build_session_summary(
            &self.streamers,
            &initial_points,
            &self.completed_predictions,
            anonymize,
            std::time::Duration::from_micros(duration_micros),
        )
    }

    #[allow(clippy::too_many_lines, clippy::redundant_closure_for_method_calls)]
    pub fn apply_event(&mut self, event: &MinerEvent, now: OffsetDateTime) -> Vec<RuntimeEffect> {
        self.apply_event_with_outcome(event, now).effects
    }

    #[allow(clippy::too_many_lines, clippy::redundant_closure_for_method_calls)]
    pub fn apply_event_with_outcome(
        &mut self,
        event: &MinerEvent,
        now: OffsetDateTime,
    ) -> EventApplication {
        match event {
            MinerEvent::PointsEarned {
                channel_id,
                earned,
                reason,
                balance,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return EventApplication::unchanged();
                };
                let event_key = format!("{earned}:{balance}:{}:{reason}", reason.len());
                let current_state_key = format!("{event_key}:{}", streamer.channel_points);
                if streamer
                    .processed_point_event_keys
                    .contains(&current_state_key)
                {
                    return EventApplication::unchanged();
                }
                apply_pubsub_gain(streamer, *earned, reason, *balance);
                if reason == "WATCH_STREAK" {
                    if let Some(stream) = streamer.stream.as_mut() {
                        stream.mark_watch_streak_resolved(now);
                    }
                }
                let applied_state_key = format!("{event_key}:{}", streamer.channel_points);
                remember_mutation_id(&mut streamer.processed_point_event_keys, &applied_state_key);
                EventApplication::changed(Vec::new())
            }
            MinerEvent::ClaimAvailable {
                channel_id,
                claim_id,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return EventApplication::unchanged();
                };
                if !remember_mutation_id(&mut streamer.processed_claim_ids, claim_id) {
                    return EventApplication::unchanged();
                }
                EventApplication::changed(vec![RuntimeEffect::ClaimBonus {
                    channel_id: channel_id.clone(),
                    claim_id: claim_id.clone(),
                }])
            }
            MinerEvent::Playback { channel_id, kind } => match kind {
                PlaybackType::StreamUp => EventApplication {
                    effects: Vec::new(),
                    changed: self.apply_presence(channel_id, true, now),
                },
                PlaybackType::StreamDown => EventApplication {
                    effects: Vec::new(),
                    changed: self.apply_presence(channel_id, false, now),
                },
                PlaybackType::Viewcount => EventApplication::unchanged(),
            },
            MinerEvent::Raid {
                channel_id,
                raid_id,
                target_login,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return EventApplication::unchanged();
                };
                if !streamer.settings.follow_raid
                    || raid_id.is_empty()
                    || streamer.last_raid_id == *raid_id
                {
                    return EventApplication::unchanged();
                }
                streamer.last_raid_id.clone_from(raid_id);
                EventApplication::changed(vec![RuntimeEffect::JoinRaid {
                    channel_id: channel_id.clone(),
                    raid_id: raid_id.clone(),
                    target_login: target_login.clone(),
                }])
            }
            MinerEvent::Moment {
                channel_id,
                moment_id,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return EventApplication::unchanged();
                };
                if !streamer.settings.claim_moments
                    || !remember_mutation_id(&mut streamer.processed_moment_ids, moment_id)
                {
                    return EventApplication::unchanged();
                }
                EventApplication::changed(vec![RuntimeEffect::ClaimMoment {
                    channel_id: channel_id.clone(),
                    moment_id: moment_id.clone(),
                }])
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
                        || !remember_mutation_id(
                            &mut self.processed_prediction_ids,
                            &event.event_id,
                        )
                    {
                        return EventApplication::unchanged();
                    }
                    self.predictions
                        .insert(event.event_id.clone(), event.as_ref().clone());
                    EventApplication::changed(vec![RuntimeEffect::EvaluatePrediction {
                        event_id: event.event_id.clone(),
                    }])
                }
                PredictionChannelKind::EventUpdated => {
                    let event_id = event.event_id.clone();
                    let (effect, state_changed) = {
                        let Some(existing) = self.predictions.get_mut(&event_id) else {
                            return EventApplication::unchanged();
                        };
                        let mut state_changed = existing.status != event.status;
                        existing.status.clone_from(&event.status);
                        if !event.outcomes.is_empty() {
                            state_changed |= existing.outcomes != event.outcomes;
                            existing.outcomes.clone_from(&event.outcomes);
                        }
                        if !existing.bet_placed
                            || existing.decision.amount <= 0
                            || !existing.result_type.is_empty()
                            || !prediction_status_is_resolved(&existing.status)
                        {
                            (None, state_changed)
                        } else {
                            (
                                build_prediction_settlement_effect(
                                    existing,
                                    winning_outcome_id.as_deref(),
                                ),
                                state_changed,
                            )
                        }
                    };
                    let Some(effect) = effect else {
                        return EventApplication {
                            effects: Vec::new(),
                            changed: state_changed,
                        };
                    };
                    if let Some(event) = self.predictions.remove(&event_id) {
                        self.remember_completed_prediction(event);
                    }
                    EventApplication::changed(vec![effect])
                }
            },
            MinerEvent::PredictionUser {
                event_id,
                kind,
                result,
            } => match kind {
                PredictionUserKind::PredictionMade => {
                    let Some(event) = self.predictions.get_mut(event_id) else {
                        return EventApplication::unchanged();
                    };
                    if event.bet_confirmed {
                        return EventApplication::unchanged();
                    }
                    event.bet_confirmed = true;
                    EventApplication::changed(Vec::new())
                }
                PredictionUserKind::PredictionResult => {
                    let result_type = result
                        .as_ref()
                        .and_then(|value| value.get("type"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    if !matches!(result_type, "WIN" | "LOSE" | "REFUND") {
                        return EventApplication::unchanged();
                    }
                    let points_won = result
                        .as_ref()
                        .and_then(|value| value.get("points_won"))
                        .and_then(|value| value.as_i64())
                        .unwrap_or_default();
                    let Some(mut event) = self.predictions.remove(event_id) else {
                        return EventApplication {
                            effects: Vec::new(),
                            changed: self.refine_completed_prediction(
                                event_id,
                                result_type,
                                points_won,
                            ),
                        };
                    };
                    if !event.bet_confirmed {
                        event.bet_confirmed = true;
                    }
                    let settlement = event.parse_result(result_type, points_won);
                    let effect = RuntimeEffect::PredictionSettled {
                        event_id: event_id.clone(),
                        streamer_username: event.streamer.username.clone(),
                        title: event.title.clone(),
                        decision_label: settlement.decision_label.clone(),
                        result_type: settlement.result_type.clone(),
                        result_string: settlement.result_string.clone(),
                    };
                    self.remember_completed_prediction(event);
                    EventApplication::changed(vec![effect])
                }
            },
            MinerEvent::CommunityGoal {
                channel_id,
                kind,
                goal,
                goal_id,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return EventApplication::unchanged();
                };
                if !streamer.settings.community_goals {
                    return EventApplication::unchanged();
                }
                match kind {
                    CommunityGoalKind::Created | CommunityGoalKind::Updated => {
                        let Some(goal) = goal.as_ref() else {
                            return EventApplication::unchanged();
                        };
                        if streamer.community_goals.get(&goal.id) == Some(goal) {
                            return EventApplication::unchanged();
                        }
                        streamer
                            .community_goals
                            .insert(goal.id.clone(), goal.clone());
                        EventApplication::changed(vec![RuntimeEffect::ContributeCommunityGoals {
                            channel_id: channel_id.clone(),
                        }])
                    }
                    CommunityGoalKind::Deleted => {
                        let changed = goal_id.as_ref().is_some_and(|goal_id| {
                            streamer.community_goals.remove(goal_id).is_some()
                        });
                        EventApplication {
                            effects: Vec::new(),
                            changed,
                        }
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

    fn streamer_mut_by_channel_id(&mut self, channel_id: &str) -> Option<&mut Streamer> {
        self.streamers
            .iter_mut()
            .find(|streamer| streamer.channel_id == channel_id)
    }

    pub fn update_streamer_login(&mut self, channel_id: &str, login: &str) -> bool {
        let login = login.trim().to_ascii_lowercase();
        if login.is_empty() {
            return false;
        }
        let Some(index) = self
            .streamers
            .iter()
            .position(|streamer| streamer.channel_id == channel_id)
        else {
            return false;
        };
        if self.streamers[index].username == login {
            return self.streamers[index].watch_suspended_until.take().is_some();
        }
        let old_login = std::mem::replace(&mut self.streamers[index].username, login.clone());
        self.streamers[index].watch_suspended_until = None;
        if let Some(initial_points) = self.initial_points.remove(&old_login) {
            self.initial_points.insert(login, initial_points);
        }
        true
    }

    pub fn suspend_watching(&mut self, channel_id: &str, until: OffsetDateTime) -> bool {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return false;
        };
        if streamer
            .watch_suspended_until
            .is_some_and(|current| current >= until)
        {
            return false;
        }
        streamer.watch_suspended_until = Some(until);
        true
    }

    pub(crate) fn apply_presence(
        &mut self,
        channel_id: &str,
        online: bool,
        now: OffsetDateTime,
    ) -> bool {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return false;
        };
        let prev_online = streamer.is_online;
        if !streamer.presence_known || prev_online != online {
            streamer.presence_known = true;
            streamer.is_online = online;
            if online {
                let short_restart = streamer.offline_at.is_some_and(|offline_at| {
                    (now - offline_at).whole_seconds() <= STREAK_RESTART_CARRYOVER_SECONDS
                });
                streamer.online_at = Some(now);
                streamer.offline_at = None;
                let stream = streamer.stream.get_or_insert_with(Stream::default);
                if !short_restart {
                    stream.watch_streak_missing = true;
                    stream.streak_carryover_until = None;
                }
                if stream.stream_up_at.is_none() {
                    stream.stream_up_at = Some(now);
                }
            } else {
                streamer.offline_at = Some(now);
                if let Some(stream) = streamer.stream.as_mut() {
                    stream.stream_up_at = None;
                    stream.streak_carryover_until = Some(
                        now + std::time::Duration::from_secs(
                            STREAK_RESTART_CARRYOVER_SECONDS.cast_unsigned(),
                        ),
                    );
                    stream.reset_watch_progress();
                }
            }
            true
        } else {
            false
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
        let game_changed = stream.game_name() != update.game_name.trim();
        if stream.stream_up_at.is_none() || broadcast_changed {
            stream.stream_up_at = Some(now);
        }
        if broadcast_changed {
            stream.reset_watch_progress();
            if stream
                .streak_carryover_until
                .is_none_or(|carryover_until| carryover_until < now)
            {
                stream.watch_streak_missing = true;
                stream.streak_carryover_until = None;
            }
        }
        if broadcast_changed || game_changed {
            stream.drop_campaign_eligible = None;
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

    pub fn set_drop_campaign_eligibility(&mut self, channel_id: &str, eligible: bool) {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return;
        };
        streamer
            .stream
            .get_or_insert_with(Stream::default)
            .drop_campaign_eligible = Some(eligible);
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

    pub fn mark_watch_streak_recovered(
        &mut self,
        channel_id: &str,
        streak_count: Option<u32>,
        resolved_at: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
    ) -> bool {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return false;
        };
        let stream = streamer.stream.get_or_insert_with(Stream::default);
        stream.apply_watch_streak_milestone(streak_count, resolved_at, expires_at);
        true
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
            if event.bet_placed {
                self.remember_completed_prediction(event);
            }
        }
    }

    fn remember_completed_prediction(&mut self, mut event: PredictionEvent) {
        if let Some(streamer) = self
            .streamers
            .iter()
            .find(|streamer| streamer.channel_id == event.streamer.channel_id)
        {
            event.streamer = streamer.clone();
        }
        if self.completed_predictions.len() == MAX_COMPLETED_PREDICTIONS {
            self.completed_predictions.pop_front();
        }
        self.completed_predictions.push_back(event);
    }

    fn refine_completed_prediction(
        &mut self,
        event_id: &str,
        result_type: &str,
        points_won: i64,
    ) -> bool {
        let Some(event) = self
            .completed_predictions
            .iter_mut()
            .rev()
            .find(|event| event.event_id == event_id)
        else {
            return false;
        };
        let changed = !event.bet_confirmed
            || event.result_type != result_type
            || event.result_string != prediction_result_string(event, result_type, points_won);
        event.bet_confirmed = true;
        event.parse_result(result_type, points_won);
        changed
    }
}

fn prediction_result_string(event: &PredictionEvent, result_type: &str, points_won: i64) -> String {
    let mut event = event.clone();
    event.parse_result(result_type, points_won).result_string
}

fn remember_mutation_id(values: &mut std::collections::VecDeque<String>, value: &str) -> bool {
    if value.trim().is_empty() || values.iter().any(|existing| existing == value) {
        return false;
    }
    if values.len() == MAX_PROCESSED_MUTATION_IDS {
        values.pop_front();
    }
    values.push_back(value.to_string());
    true
}

fn tm_twitch_drop_id() -> &'static str {
    "c2542d6d-cd10-4532-919b-3d19f30a768b"
}
