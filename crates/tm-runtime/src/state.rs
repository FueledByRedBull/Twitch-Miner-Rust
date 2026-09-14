use std::collections::HashMap;

use tm_config::{build_base_streamer_settings, build_override_settings, ConfigFile};
use tm_domain::{
    normalize_game_list, normalize_streamer_list, parse_watch_priorities, pick_streamers_to_watch,
    should_join_chat, CommunityGoal, CommunityGoalKind, Game, MinerEvent, OffsetDateTime,
    PlaybackType, PredictionChannelKind, PredictionDecision, PredictionEvent, PredictionUserKind,
    Stream, Streamer, WatchPriority,
};

use crate::effect::RuntimeEffect;
use crate::prediction::{build_prediction_settlement_effect, prediction_status_is_resolved};
use crate::summary::{apply_pubsub_gain, build_session_summary};
use crate::types::{
    ContextRequestToken, ContextUpdate, EventApplication, RuntimeState, SessionSummary,
    StreamUpdate,
};

const MAX_ACTIVE_PREDICTIONS_PER_CHANNEL: usize = 8;
const MAX_ACTIVE_PREDICTIONS: usize = 256;
const MAX_COMPLETED_PREDICTIONS: usize = 256;

const MAX_PROCESSED_MUTATION_IDS: usize = 128;
// Prediction point deductions must remain recognizable for as long as the
// corresponding active or completed event is retained. Protected markers are
// bounded by those two retention limits; ordinary transport keys retain their
// existing smaller cap.
const MAX_PROTECTED_PREDICTION_MARKERS: usize = MAX_ACTIVE_PREDICTIONS + MAX_COMPLETED_PREDICTIONS;
const STREAK_RESTART_CARRYOVER_SECONDS: i64 = 30 * 60;
// At two selected channels, each pass can spend 90 seconds inside the bounded
// request plus 10 seconds on its scheduler interval. Two complete passes allow
// one failed tick before a later success stops proving continuous watch time.
const MAX_CONFIRMED_WATCH_INTERVAL_SECONDS: i64 = 400;

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
            pending_prediction_winners: HashMap::new(),
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
    pub fn campaign_watch_logins(&self, now: OffsetDateTime) -> Vec<String> {
        if !self.watch_priorities.is_empty()
            && !self.watch_priorities.contains(&WatchPriority::Drops)
        {
            return Vec::new();
        }

        // Streak watch time resets on release. Using its transient rank for
        // the campaign pin makes it switch away and immediately back again.
        pick_streamers_to_watch(
            &self.streamers,
            &[WatchPriority::Drops],
            &self.game_priority,
            &self.game_exclusions,
            now,
        )
        .into_iter()
        .filter_map(|idx| self.streamers.get(idx))
        .filter(|streamer| streamer.can_watch_drop_campaign())
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

    pub fn apply_event(&mut self, event: &MinerEvent, now: OffsetDateTime) -> Vec<RuntimeEffect> {
        self.apply_event_with_outcome(event, now).effects
    }

    // The exhaustive reducer keeps every external event variant and its dedupe/effect decision in
    // one auditable match. Variant-specific network work lives outside this state-only boundary.
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
                source_id,
            } => {
                let (active_prediction_id, completed_prediction_id) =
                    if *earned < 0 && reason == "PREDICTION" {
                        earned
                            .checked_abs()
                            .map(|amount| {
                                let active = self
                                    .predictions
                                    .iter()
                                    .find(|(_, prediction)| {
                                        prediction.streamer.channel_id == *channel_id
                                            && prediction.bet_placed
                                            && prediction.result_type.is_empty()
                                            && prediction.decision.amount == amount
                                    })
                                    .map(|(event_id, _)| event_id.clone());
                                let completed = self
                                    .completed_predictions
                                    .iter()
                                    .rev()
                                    .find(|prediction| {
                                        prediction.streamer.channel_id == *channel_id
                                            && prediction.bet_placed
                                            && prediction.decision.amount == amount
                                    })
                                    .map(|prediction| prediction.event_id.clone());
                                (active, completed)
                            })
                            .unwrap_or_default()
                    } else {
                        (None, None)
                    };
                {
                    let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                        return EventApplication::unchanged();
                    };
                    if !streamer.can_earn_channel_points() {
                        return EventApplication::unchanged();
                    }
                    let event_key = source_id
                        .as_deref()
                        .filter(|source_id| !source_id.trim().is_empty())
                        .map_or_else(
                            || format!("fingerprint:{earned}:{balance}:{reason}"),
                            |source_id| format!("source:{source_id}"),
                        );
                    if streamer.processed_point_event_keys.contains(&event_key) {
                        return EventApplication::unchanged();
                    }
                    let matched_prediction_id = active_prediction_id
                        .as_ref()
                        .or(completed_prediction_id.as_ref());
                    let prediction_point_already_applied =
                        matched_prediction_id.is_some_and(|event_id| {
                            prediction_deduction_marker_exists(streamer, event_id)
                        });
                    if prediction_point_already_applied {
                        // The mutation response already accounted for this stake. The
                        // server's matching point event confirms the same spend and must
                        // not deduct or count it a second time.
                        remember_mutation_id(&mut streamer.processed_point_event_keys, &event_key);
                        return EventApplication::unchanged();
                    }
                    if let Some(event_id) = matched_prediction_id {
                        // Keep this event-specific accounting marker alongside the
                        // retained prediction record. Ordinary point keys are bounded
                        // more aggressively and may be evicted before a late replay.
                        if !remember_prediction_deduction(
                            &mut streamer.processed_point_event_keys,
                            event_id,
                        ) {
                            return EventApplication::unchanged();
                        }
                    }
                    apply_pubsub_gain(streamer, *earned, reason, *balance);
                    if *earned > 0 && matches!(reason.as_str(), "WATCH" | "WATCH_STREAK") {
                        streamer.last_server_confirmed_points_at = Some(now);
                    }
                    if reason == "WATCH_STREAK" {
                        if let Some(stream) = streamer.stream.as_mut() {
                            stream.mark_watch_streak_resolved(now);
                        }
                    }
                    remember_mutation_id(&mut streamer.processed_point_event_keys, &event_key);
                }
                EventApplication::changed(Vec::new())
            }
            MinerEvent::ClaimAvailable {
                channel_id,
                claim_id,
            } => {
                let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
                    return EventApplication::unchanged();
                };
                if !streamer.can_earn_channel_points() {
                    return EventApplication::unchanged();
                }
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
                    let can_earn = self
                        .streamers
                        .iter()
                        .find(|streamer| streamer.channel_id == event.streamer.channel_id)
                        .is_some_and(Streamer::can_earn_channel_points);
                    if event.event_id.is_empty()
                        || event.status != "ACTIVE"
                        || !event.streamer.settings.make_predictions
                        || !can_earn
                        || self
                            .processed_prediction_ids
                            .iter()
                            .any(|existing| existing == &event.event_id)
                    {
                        return EventApplication::unchanged();
                    }
                    if self.predictions.len() >= self.max_active_prediction_count() {
                        let Some(evict_id) = self.oldest_evictable_prediction_id() else {
                            // A full map containing only placed or confirmed
                            // bets must never evict spending state. Retain the
                            // event identity in the bounded dedupe queue and
                            // wait for an existing event to resolve.
                            remember_prediction_id(
                                &mut self.processed_prediction_ids,
                                &event.event_id,
                            );
                            return EventApplication::unchanged();
                        };
                        self.predictions.remove(&evict_id);
                    }
                    if !remember_prediction_id(&mut self.processed_prediction_ids, &event.event_id)
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
                        if existing.bet_placed
                            && !existing.bet_confirmed
                            && existing.status == "RESOLVED"
                        {
                            if let Some(winning_outcome_id) = winning_outcome_id.as_deref() {
                                let previous = self
                                    .pending_prediction_winners
                                    .insert(event_id.clone(), winning_outcome_id.to_string());
                                state_changed |= previous.as_deref() != Some(winning_outcome_id);
                            }
                        } else if !prediction_status_is_resolved(&existing.status) {
                            state_changed |=
                                self.pending_prediction_winners.remove(&event_id).is_some();
                        }
                        if !existing.bet_placed
                            || !existing.bet_confirmed
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
                    let should_settle = {
                        let Some(event) = self.predictions.get_mut(event_id) else {
                            return EventApplication::unchanged();
                        };
                        if event.bet_confirmed {
                            return EventApplication::unchanged();
                        }
                        event.bet_confirmed = true;
                        prediction_status_is_resolved(&event.status)
                    };
                    if should_settle {
                        if let Some(effect) = self.settle_confirmed_prediction(event_id) {
                            return EventApplication::changed(vec![effect]);
                        }
                    }
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
                    self.pending_prediction_winners.remove(event_id);
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
                if !streamer.settings.community_goals || !streamer.can_earn_channel_points() {
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

    fn streamer_mut_by_channel_id(&mut self, channel_id: &str) -> Option<&mut Streamer> {
        self.streamers
            .iter_mut()
            .find(|streamer| streamer.channel_id == channel_id)
    }

    fn max_active_prediction_count(&self) -> usize {
        self.streamers
            .len()
            .max(1)
            .saturating_mul(MAX_ACTIVE_PREDICTIONS_PER_CHANNEL)
            .min(MAX_ACTIVE_PREDICTIONS)
    }

    fn oldest_evictable_prediction_id(&self) -> Option<String> {
        self.predictions
            .iter()
            .filter(|(_, event)| {
                !event.bet_placed && !event.bet_confirmed && event.result_type.is_empty()
            })
            .min_by(|(_, left), (_, right)| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.event_id.cmp(&right.event_id))
            })
            .map(|(event_id, _)| event_id.clone())
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
        // A response fetched under the previous login must not apply after a
        // channel rename. Invalidate both request families before exposing the
        // new identity to callers.
        self.streamers[index].context_request_generation = self.streamers[index]
            .context_request_generation
            .saturating_add(1);
        self.streamers[index].stream_update_generation = self.streamers[index]
            .stream_update_generation
            .saturating_add(1);
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
                streamer.last_stream_ended_at = streamer.offline_at;
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
                // Invalidate metadata requests that were issued while this
                // broadcast was live. A late successful response must not
                // bring an offline stream back into the watch state.
                streamer.stream_update_generation =
                    streamer.stream_update_generation.saturating_add(1);
                streamer.offline_at = Some(now);
                streamer.last_stream_ended_at = Some(now);
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

    pub fn begin_context_update(&mut self, channel_id: &str) -> Option<ContextRequestToken> {
        let streamer = self.streamer_mut_by_channel_id(channel_id)?;
        streamer.context_request_generation = streamer.context_request_generation.saturating_add(1);
        Some(ContextRequestToken {
            request_generation: streamer.context_request_generation,
            balance_revision: streamer.context_balance_revision,
        })
    }

    pub fn begin_stream_update(&mut self, channel_id: &str) -> Option<u64> {
        let streamer = self.streamer_mut_by_channel_id(channel_id)?;
        streamer.stream_update_generation = streamer.stream_update_generation.saturating_add(1);
        Some(streamer.stream_update_generation)
    }

    #[must_use]
    pub fn prediction_channel_id(&self, event_id: &str) -> Option<String> {
        self.active_prediction_channel_id(event_id).or_else(|| {
            self.completed_predictions
                .iter()
                .rev()
                .find(|event| event.event_id == event_id)
                .map(|event| event.streamer.channel_id.clone())
        })
    }

    #[must_use]
    pub fn active_prediction_channel_id(&self, event_id: &str) -> Option<String> {
        self.predictions
            .get(event_id)
            .map(|event| event.streamer.channel_id.clone())
    }

    pub fn apply_context_update(&mut self, update: &ContextUpdate) -> (Vec<RuntimeEffect>, i64) {
        let Some(streamer) = self.streamer_mut_by_channel_id(&update.channel_id) else {
            return (Vec::new(), 0);
        };
        if streamer.context_request_generation != update.expected_request_generation {
            return (Vec::new(), 0);
        }
        streamer.last_context_observed_at = Some(update.observed_at);

        let previous_balance = streamer.channel_points;
        let balance_applied = streamer.apply_channel_points_context_with_status_if_allowed(
            streamer.context_balance_revision == update.expected_balance_revision,
            update.channel_points_enabled,
            update.balance,
            &update.active_multipliers,
            &update.community_goals,
        );
        if balance_applied {
            streamer.context_balance_revision = streamer.context_balance_revision.saturating_add(1);
        }
        let balance_delta = if balance_applied {
            streamer.channel_points.saturating_sub(previous_balance)
        } else {
            0
        };
        if !streamer.can_earn_channel_points() {
            return (Vec::new(), balance_delta);
        }
        if streamer.settings.community_goals
            && streamer
                .community_goals
                .values()
                .any(CommunityGoal::is_active)
        {
            return (
                vec![RuntimeEffect::ContributeCommunityGoals {
                    channel_id: update.channel_id.clone(),
                }],
                balance_delta,
            );
        }
        (Vec::new(), balance_delta)
    }

    pub fn apply_stream_update(
        &mut self,
        update: &StreamUpdate,
        now: OffsetDateTime,
    ) -> Option<Streamer> {
        let streamer = self.streamer_mut_by_channel_id(&update.channel_id)?;
        if streamer.stream_update_generation != update.expected_generation {
            return None;
        }
        let stream = streamer.stream.get_or_insert_with(Stream::default);
        let broadcast_changed = !stream.broadcast_id.is_empty() && stream.broadcast_id != update.id;
        let game_changed = stream.game_name() != update.game_name.trim();
        if stream.stream_up_at.is_none() || broadcast_changed {
            stream.stream_up_at = Some(now);
        }
        if broadcast_changed {
            // A WATCH reward from the previous broadcast cannot establish
            // progress for the new one. Clear the observation so the watcher
            // watchdog starts measuring this broadcast from its own evidence.
            streamer.last_server_confirmed_points_at = None;
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
            update.game_id.clone(),
            &update.tags,
            update.viewers_count,
            tm_twitch_drop_id(),
            now,
        );
        streamer.stream_update_generation = streamer.stream_update_generation.saturating_add(1);
        // Stream metadata is accepted only from a live fetch. Apply the
        // matching online transition under the same state lock so an offline
        // event cannot be followed by a stale second `set_presence(true)`
        // write from the caller.
        if !streamer.is_online || !streamer.presence_known {
            let short_restart = streamer.offline_at.is_some_and(|offline_at| {
                (now - offline_at).whole_seconds() <= STREAK_RESTART_CARRYOVER_SECONDS
            });
            streamer.presence_known = true;
            streamer.is_online = true;
            streamer.online_at = Some(now);
            streamer.offline_at = None;
            if !short_restart {
                if let Some(stream) = streamer.stream.as_mut() {
                    stream.watch_streak_missing = true;
                    stream.streak_carryover_until = None;
                }
            }
        }
        Some(streamer.clone())
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

    pub fn set_drop_campaign_eligibility_if_current(
        &mut self,
        channel_id: &str,
        expected_broadcast_id: &str,
        expected_game_id: Option<&str>,
        eligible: bool,
    ) -> bool {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return false;
        };
        let Some(stream) = streamer.stream.as_mut() else {
            return false;
        };
        if stream.broadcast_id != expected_broadcast_id
            || stream.game_id.as_deref() != expected_game_id
        {
            return false;
        }
        stream.drop_campaign_eligible = Some(eligible);
        true
    }

    pub fn mark_minute_watched(
        &mut self,
        channel_id: &str,
        expected_broadcast_id: &str,
        now: OffsetDateTime,
    ) {
        let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) else {
            return;
        };
        if !streamer.can_earn_channel_points() {
            return;
        }
        let Some(stream) = streamer.stream.as_mut() else {
            return;
        };
        if stream.broadcast_id != expected_broadcast_id {
            return;
        }
        stream.update_minute_watched(now, MAX_CONFIRMED_WATCH_INTERVAL_SECONDS);
    }

    pub fn reset_watch_progress(&mut self, channel_id: &str) -> bool {
        let Some(stream) = self
            .streamer_mut_by_channel_id(channel_id)
            .and_then(|streamer| streamer.stream.as_mut())
        else {
            return false;
        };
        stream.reset_watch_progress();
        true
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
                // A PredictionMade notification may race the mutation response, and a
                // response may be replayed by a caller after a reconnect. Keep a durable
                // in-memory marker alongside server point-event identities so the local
                // accounting cannot deduct one stake twice for the same prediction event.
                if remember_prediction_deduction(&mut streamer.processed_point_event_keys, event_id)
                {
                    apply_pubsub_gain(streamer, -decision.amount, "PREDICTION", 0);
                }
                event.streamer = streamer.clone();
            }
        }
        self.predictions.insert(event.event_id.clone(), event);
    }

    pub fn restore_prediction_placement(&mut self, event_id: &str, decision: &PredictionDecision) {
        let Some(mut event) = self.predictions.remove(event_id) else {
            return;
        };
        event.decision.clone_from(decision);
        event.bet_placed = true;
        event.bet_confirmed = true;
        if let Some(streamer) = self.streamer_mut_by_channel_id(&event.streamer.channel_id) {
            event.streamer = streamer.clone();
            // The persisted terminal journal entry means the stake was
            // already accounted for before this process started. Retain the
            // idempotency marker without applying a second local deduction.
            remember_prediction_deduction(&mut streamer.processed_point_event_keys, event_id);
            event.streamer = streamer.clone();
        }
        self.predictions.insert(event.event_id.clone(), event);
    }

    pub fn reserve_prediction_placement(
        &mut self,
        event_id: &str,
        decision: &PredictionDecision,
    ) -> bool {
        let Some(event) = self.predictions.get_mut(event_id) else {
            return false;
        };
        if event.bet_placed || event.bet_confirmed || !event.result_type.is_empty() {
            return false;
        }
        event.decision.clone_from(decision);
        // `bet_placed && !bet_confirmed` is the unresolved reservation state. It
        // makes concurrent evaluation effects idempotent before any network I/O.
        event.bet_placed = true;
        true
    }

    pub fn release_prediction_placement_reservation(&mut self, event_id: &str) -> bool {
        let Some(event) = self.predictions.get_mut(event_id) else {
            return false;
        };
        if !event.bet_placed || event.bet_confirmed || !event.result_type.is_empty() {
            return false;
        }
        event.bet_placed = false;
        true
    }

    pub fn stop_tracking_prediction(&mut self, event_id: &str, result_type: &str) {
        if result_type == "REJECTED"
            && self
                .predictions
                .get(event_id)
                .is_some_and(|event| event.bet_confirmed)
        {
            return;
        }
        if let Some(mut event) = self.predictions.remove(event_id) {
            self.pending_prediction_winners.remove(event_id);
            event.result_type = result_type.to_string();
            if event.bet_placed || result_type == "REJECTED" {
                self.remember_completed_prediction(event);
            } else {
                self.forget_prediction_deduction_marker(event_id);
            }
        }
    }

    pub fn mark_prediction_placement_unknown(
        &mut self,
        event_id: &str,
        decision: &PredictionDecision,
    ) {
        let Some(event) = self.predictions.get_mut(event_id) else {
            return;
        };
        event.decision.clone_from(decision);
        // A transport failure after the mutation was sent is an ambiguous outcome. Keep the
        // event active for a later PredictionMade/PredictionResult notification, but never
        // deduct the stake or issue a second mutation merely because this state is unresolved.
        event.bet_placed = true;
    }

    pub fn release_claim_bonus(&mut self, channel_id: &str, claim_id: &str) {
        if let Some(streamer) = self.streamer_mut_by_channel_id(channel_id) {
            streamer.processed_claim_ids.retain(|id| id != claim_id);
        }
    }

    pub fn release_prediction(&mut self, event_id: &str) {
        self.predictions.remove(event_id);
        self.pending_prediction_winners.remove(event_id);
        self.processed_prediction_ids.retain(|id| id != event_id);
        self.forget_prediction_deduction_marker(event_id);
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
            if let Some(evicted) = self.completed_predictions.pop_front() {
                self.forget_prediction_deduction_marker(&evicted.event_id);
            }
        }
        self.completed_predictions.push_back(event);
    }

    fn forget_prediction_deduction_marker(&mut self, event_id: &str) {
        let marker = prediction_deduction_marker(event_id);
        for streamer in &mut self.streamers {
            streamer
                .processed_point_event_keys
                .retain(|key| key != &marker);
        }
    }

    fn settle_confirmed_prediction(&mut self, event_id: &str) -> Option<RuntimeEffect> {
        let mut event = self.predictions.remove(event_id)?;
        if !event.bet_placed || event.decision.amount <= 0 || !event.result_type.is_empty() {
            self.predictions.insert(event.event_id.clone(), event);
            return None;
        }
        let winning_outcome_id = if event.status == "RESOLVED" {
            self.pending_prediction_winners.get(event_id).cloned()
        } else {
            None
        };
        let Some(effect) =
            build_prediction_settlement_effect(&mut event, winning_outcome_id.as_deref())
        else {
            self.predictions.insert(event.event_id.clone(), event);
            return None;
        };
        self.pending_prediction_winners.remove(event_id);
        self.remember_completed_prediction(event);
        Some(effect)
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

fn prediction_deduction_marker(event_id: &str) -> String {
    format!("prediction:{event_id}")
}

fn prediction_deduction_marker_exists(streamer: &Streamer, event_id: &str) -> bool {
    let marker = prediction_deduction_marker(event_id);
    streamer
        .processed_point_event_keys
        .iter()
        .any(|key| key == &marker)
}

fn remember_prediction_deduction(
    values: &mut std::collections::VecDeque<String>,
    event_id: &str,
) -> bool {
    let marker = prediction_deduction_marker(event_id);
    if values.iter().any(|existing| existing == &marker) {
        return false;
    }
    let protected_count = values
        .iter()
        .filter(|key| key.starts_with("prediction:"))
        .count();
    if protected_count >= MAX_PROTECTED_PREDICTION_MARKERS {
        return false;
    }
    while values
        .iter()
        .filter(|key| !key.starts_with("prediction:"))
        .count()
        >= MAX_PROCESSED_MUTATION_IDS
    {
        let Some(index) = values
            .iter()
            .position(|key| !key.starts_with("prediction:"))
        else {
            return false;
        };
        values.remove(index);
    }
    values.push_back(marker);
    true
}

fn remember_prediction_id(values: &mut std::collections::VecDeque<String>, value: &str) -> bool {
    const MAX_PROCESSED_PREDICTION_IDS: usize = MAX_ACTIVE_PREDICTIONS;
    if value.trim().is_empty() || values.iter().any(|existing| existing == value) {
        return false;
    }
    if values.len() == MAX_PROCESSED_PREDICTION_IDS {
        values.pop_front();
    }
    values.push_back(value.to_string());
    true
}

fn remember_mutation_id(values: &mut std::collections::VecDeque<String>, value: &str) -> bool {
    if value.trim().is_empty() || values.iter().any(|existing| existing == value) {
        return false;
    }
    while values
        .iter()
        .filter(|key| !key.starts_with("prediction:"))
        .count()
        >= MAX_PROCESSED_MUTATION_IDS
    {
        let Some(index) = values
            .iter()
            .position(|key| !key.starts_with("prediction:"))
        else {
            return false;
        };
        values.remove(index);
    }
    values.push_back(value.to_string());
    true
}

fn tm_twitch_drop_id() -> &'static str {
    "c2542d6d-cd10-4532-919b-3d19f30a768b"
}

#[cfg(test)]
mod mutation_id_tests {
    use super::{remember_mutation_id, MAX_PROCESSED_MUTATION_IDS};
    use std::collections::VecDeque;

    #[test]
    fn mutation_ids_reject_empty_and_exact_duplicates_only() {
        let mut values = VecDeque::new();

        assert!(!remember_mutation_id(&mut values, ""));
        assert!(!remember_mutation_id(&mut values, "   "));
        assert!(remember_mutation_id(&mut values, "claim-1"));
        assert!(!remember_mutation_id(&mut values, "claim-1"));
        assert!(remember_mutation_id(&mut values, "claim-10"));
        assert_eq!(
            values,
            VecDeque::from([String::from("claim-1"), String::from("claim-10")])
        );
    }

    #[test]
    fn mutation_ids_evict_only_the_oldest_entry_at_capacity() {
        let mut values = (0..MAX_PROCESSED_MUTATION_IDS)
            .map(|index| format!("id-{index}"))
            .collect::<VecDeque<_>>();

        assert!(remember_mutation_id(&mut values, "new-id"));
        assert_eq!(values.len(), MAX_PROCESSED_MUTATION_IDS);
        assert_eq!(values.front().map(String::as_str), Some("id-1"));
        assert_eq!(values.back().map(String::as_str), Some("new-id"));
    }
}
