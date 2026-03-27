use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tm_config::{build_base_streamer_settings, build_override_settings, ConfigFile};
use tm_domain::{
    format_channel_points, format_duration, normalize_game_list, normalize_streamer_list,
    parse_watch_priorities, pick_streamers_to_watch, should_join_chat, ActiveMultiplier,
    CommunityGoal, Game, OffsetDateTime, PredictionDecision, PredictionEvent, Stream, Streamer,
    WatchPriority,
};
use tm_pubsub::{
    CommunityGoalKind, PlaybackType, PredictionChannelKind, PredictionUserKind, PubSubEvent,
};
use tokio::sync::{mpsc, oneshot, watch};

pub use tm_domain::OffsetDateTime as RuntimeTime;

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

#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    sender: mpsc::Sender<RuntimeCommand>,
    state_revision: watch::Receiver<u64>,
}

#[derive(Debug)]
enum RuntimeCommand {
    ApplyPubSub {
        event: PubSubEvent,
        now: OffsetDateTime,
        respond_to: oneshot::Sender<Vec<RuntimeEffect>>,
    },
    SessionSummary {
        anonymize: bool,
        now: OffsetDateTime,
        respond_to: oneshot::Sender<SessionSummary>,
    },
    RuntimeSummary {
        respond_to: oneshot::Sender<RuntimeSummary>,
    },
    StateSnapshot {
        respond_to: oneshot::Sender<RuntimeState>,
    },
    ApplyContext {
        update: ContextUpdate,
    },
    ApplyStreamUpdate {
        update: StreamUpdate,
        now: OffsetDateTime,
    },
    SetPresence {
        channel_id: String,
        online: bool,
        now: OffsetDateTime,
    },
    MarkMinuteWatched {
        channel_id: String,
        now: OffsetDateTime,
    },
    RecordPredictionPlaced {
        event_id: String,
        decision: PredictionDecision,
        deduct_stake: bool,
    },
    StopTrackingPrediction {
        event_id: String,
        result_type: String,
    },
    Shutdown {
        anonymize: bool,
        now: OffsetDateTime,
        respond_to: oneshot::Sender<SessionSummary>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEffect {
    ClaimBonus {
        channel_id: String,
        claim_id: String,
    },
    ClaimMoment {
        channel_id: String,
        moment_id: String,
    },
    JoinRaid {
        channel_id: String,
        raid_id: String,
        target_login: String,
    },
    ContributeCommunityGoals {
        channel_id: String,
    },
    EvaluatePrediction {
        event_id: String,
    },
    PredictionSettled {
        event_id: String,
        streamer_username: String,
        title: String,
        decision_label: String,
        result_type: String,
        result_string: String,
    },
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

#[allow(clippy::unused_async)]
pub async fn run(config: &ConfigFile) -> Result<RuntimeSession> {
    Ok(bootstrap(config, OffsetDateTime::now_utc()))
}

#[must_use]
pub fn bootstrap(config: &ConfigFile, started_at: OffsetDateTime) -> RuntimeSession {
    RuntimeSession::from_state(RuntimeState::from_config(config, started_at))
}

#[must_use]
pub fn spawn_runtime(config: &ConfigFile, started_at: OffsetDateTime) -> RuntimeHandle {
    spawn_runtime_session(bootstrap(config, started_at))
}

#[must_use]
pub fn spawn_runtime_state(state: RuntimeState) -> RuntimeHandle {
    spawn_runtime_session(RuntimeSession::from_state(state))
}

fn spawn_runtime_session(session: RuntimeSession) -> RuntimeHandle {
    let (sender, mut receiver) = mpsc::channel(64);
    let (state_revision_tx, state_revision_rx) = watch::channel(0_u64);
    tokio::spawn(async move {
        let RuntimeSession { summary, mut state } = session;
        let mut state_revision = 0_u64;
        while let Some(command) = receiver.recv().await {
            match command {
                RuntimeCommand::ApplyPubSub {
                    event,
                    now,
                    respond_to,
                } => {
                    let _ = respond_to.send(state.apply_pubsub_event(&event, now));
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::SessionSummary {
                    anonymize,
                    now,
                    respond_to,
                } => {
                    let _ = respond_to.send(state.session_summary(anonymize, now));
                }
                RuntimeCommand::RuntimeSummary { respond_to } => {
                    let _ = respond_to.send(summary.clone());
                }
                RuntimeCommand::StateSnapshot { respond_to } => {
                    let _ = respond_to.send(state.clone());
                }
                RuntimeCommand::ApplyContext { update } => {
                    state.apply_context_update(&update);
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::ApplyStreamUpdate { update, now } => {
                    state.apply_stream_update(&update, now);
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::SetPresence {
                    channel_id,
                    online,
                    now,
                } => {
                    state.apply_presence(&channel_id, online, now);
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::MarkMinuteWatched { channel_id, now } => {
                    state.mark_minute_watched(&channel_id, now);
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::RecordPredictionPlaced {
                    event_id,
                    decision,
                    deduct_stake,
                } => {
                    state.record_prediction_placed(&event_id, &decision, deduct_stake);
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::StopTrackingPrediction {
                    event_id,
                    result_type,
                } => {
                    state.stop_tracking_prediction(&event_id, &result_type);
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::Shutdown {
                    anonymize,
                    now,
                    respond_to,
                } => {
                    let _ = respond_to.send(state.session_summary(anonymize, now));
                    break;
                }
            }
        }
    });
    RuntimeHandle {
        sender,
        state_revision: state_revision_rx,
    }
}

fn notify_state_change(sender: &watch::Sender<u64>, revision: &mut u64) {
    *revision = revision.saturating_add(1);
    let _ = sender.send(*revision);
}

#[must_use]
pub fn spawn_runtime_now(config: &ConfigFile) -> RuntimeHandle {
    spawn_runtime(config, OffsetDateTime::now_utc())
}

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

impl RuntimeHandle {
    #[must_use]
    pub fn subscribe_state_changes(&self) -> watch::Receiver<u64> {
        self.state_revision.clone()
    }

    pub async fn apply_pubsub_event(
        &self,
        event: PubSubEvent,
        now: OffsetDateTime,
    ) -> Result<Vec<RuntimeEffect>> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::ApplyPubSub {
                event,
                now,
                respond_to: send,
            })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))?;
        recv.await
            .map_err(|error| anyhow::anyhow!("pubsub effects dropped: {error}"))
    }

    pub async fn runtime_summary(&self) -> Result<RuntimeSummary> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::RuntimeSummary { respond_to: send })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))?;
        recv.await
            .map_err(|error| anyhow::anyhow!("runtime summary dropped: {error}"))
    }

    pub async fn session_summary(
        &self,
        anonymize: bool,
        now: OffsetDateTime,
    ) -> Result<SessionSummary> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::SessionSummary {
                anonymize,
                now,
                respond_to: send,
            })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))?;
        recv.await
            .map_err(|error| anyhow::anyhow!("session summary dropped: {error}"))
    }

    pub async fn shutdown(&self, anonymize: bool, now: OffsetDateTime) -> Result<SessionSummary> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::Shutdown {
                anonymize,
                now,
                respond_to: send,
            })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))?;
        recv.await
            .map_err(|error| anyhow::anyhow!("shutdown summary dropped: {error}"))
    }

    pub async fn state_snapshot(&self) -> Result<RuntimeState> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::StateSnapshot { respond_to: send })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))?;
        recv.await
            .map_err(|error| anyhow::anyhow!("state snapshot dropped: {error}"))
    }

    pub async fn apply_context_update(&self, update: ContextUpdate) -> Result<()> {
        self.sender
            .send(RuntimeCommand::ApplyContext { update })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))
    }

    pub async fn apply_stream_update(
        &self,
        update: StreamUpdate,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::ApplyStreamUpdate { update, now })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))
    }

    pub async fn set_presence(
        &self,
        channel_id: impl Into<String>,
        online: bool,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::SetPresence {
                channel_id: channel_id.into(),
                online,
                now,
            })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))
    }

    pub async fn mark_minute_watched(
        &self,
        channel_id: impl Into<String>,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::MarkMinuteWatched {
                channel_id: channel_id.into(),
                now,
            })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))
    }

    pub async fn record_prediction_placed(
        &self,
        event_id: impl Into<String>,
        decision: PredictionDecision,
        deduct_stake: bool,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::RecordPredictionPlaced {
                event_id: event_id.into(),
                decision,
                deduct_stake,
            })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))
    }

    pub async fn stop_tracking_prediction(
        &self,
        event_id: impl Into<String>,
        result_type: impl Into<String>,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::StopTrackingPrediction {
                event_id: event_id.into(),
                result_type: result_type.into(),
            })
            .await
            .map_err(|error| anyhow::anyhow!("runtime channel closed: {error}"))
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
    pub fn apply_pubsub_event(
        &mut self,
        event: &PubSubEvent,
        now: OffsetDateTime,
    ) -> Vec<RuntimeEffect> {
        match event {
            PubSubEvent::PointsEarned {
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
            PubSubEvent::ClaimAvailable {
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
            PubSubEvent::Playback { channel_id, kind } => {
                self.apply_presence(channel_id, *kind != PlaybackType::StreamDown, now);
                Vec::new()
            }
            PubSubEvent::Raid {
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
            PubSubEvent::Moment {
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
            PubSubEvent::PredictionChannel {
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
            PubSubEvent::PredictionUser {
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
            PubSubEvent::CommunityGoal {
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

    fn apply_presence(&mut self, channel_id: &str, online: bool, now: OffsetDateTime) {
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

    pub fn apply_context_update(&mut self, update: &ContextUpdate) {
        let Some(streamer) = self.streamer_mut_by_channel_id(&update.channel_id) else {
            return;
        };
        streamer.channel_points = update.balance.max(0);
        streamer
            .active_multipliers
            .clone_from(&update.active_multipliers);
        streamer.community_goals = update
            .community_goals
            .iter()
            .cloned()
            .map(|goal| (goal.id.clone(), goal))
            .collect();
        streamer.points_init = true;
    }

    pub fn apply_stream_update(&mut self, update: &StreamUpdate, now: OffsetDateTime) {
        let Some(streamer) = self.streamer_mut_by_channel_id(&update.channel_id) else {
            return;
        };
        let stream = streamer.stream.get_or_insert_with(Stream::default);
        stream.stream_up_at = Some(now);
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

fn build_prediction_settlement_effect(
    event: &mut PredictionEvent,
    winning_outcome_id: Option<&str>,
) -> Option<RuntimeEffect> {
    let settlement = match event.status.as_str() {
        "CANCELED" | "CANCELLED" => event.parse_result("REFUND", 0),
        "RESOLVED" => {
            let winning_outcome_id = winning_outcome_id?;
            if event.decision.outcome_id == winning_outcome_id {
                event.parse_result(
                    "WIN",
                    payout_for_outcome(&event.decision, &event.outcomes, winning_outcome_id),
                )
            } else {
                event.parse_result("LOSE", 0)
            }
        }
        _ => return None,
    };
    Some(RuntimeEffect::PredictionSettled {
        event_id: event.event_id.clone(),
        streamer_username: event.streamer.username.clone(),
        title: event.title.clone(),
        decision_label: settlement.decision_label,
        result_type: settlement.result_type,
        result_string: settlement.result_string,
    })
}

fn prediction_status_is_resolved(status: &str) -> bool {
    matches!(status, "RESOLVED" | "CANCELED" | "CANCELLED")
}

fn payout_for_outcome(
    decision: &PredictionDecision,
    outcomes: &[tm_domain::PredictionOutcome],
    winning_outcome_id: &str,
) -> i64 {
    if decision.amount <= 0 || decision.outcome_id != winning_outcome_id {
        return 0;
    }

    let total_points = outcomes
        .iter()
        .map(|outcome| outcome.total_points)
        .sum::<i64>();
    let winning_points = outcomes
        .iter()
        .find(|outcome| outcome.id == winning_outcome_id)
        .map(|outcome| outcome.total_points)
        .unwrap_or_default();
    if total_points <= 0 || winning_points <= 0 {
        return decision.amount;
    }

    let numerator = i128::from(decision.amount) * i128::from(total_points);
    let denominator = i128::from(winning_points);
    let payout = i64::try_from((numerator + (denominator / 2)) / denominator).unwrap_or(i64::MAX);
    payout.max(decision.amount)
}

pub fn apply_pubsub_gain(streamer: &mut Streamer, earned: i64, reason: &str, balance: i64) -> i64 {
    let previous = streamer.channel_points;
    let expected = previous + earned;
    let mut new_balance = expected;
    if earned == 0 && balance != 0 {
        new_balance = balance;
    }
    if new_balance < 0 {
        new_balance = 0;
    }
    if earned >= 0 && new_balance < previous {
        new_balance = previous;
    }

    streamer.channel_points = new_balance;
    streamer.points_init = true;

    let delta = if earned == 0 {
        streamer.channel_points - previous
    } else {
        earned
    };

    update_history(streamer, reason, earned);
    delta
}

pub fn update_history(streamer: &mut Streamer, reason: &str, amount: i64) {
    if reason.is_empty() {
        return;
    }
    let entry = streamer.history.entry(reason.to_string()).or_default();
    entry.count += 1;
    entry.amount += amount;
    if reason == "WATCH_STREAK" {
        if let Some(stream) = streamer.stream.as_mut() {
            stream.watch_streak_missing = false;
        }
    }
}

#[must_use]
pub fn build_session_summary(
    streamers: &[Streamer],
    initial_points: &[(&str, i64)],
    anonymize: bool,
    duration: std::time::Duration,
) -> SessionSummary {
    let initial_points: std::collections::HashMap<&str, i64> =
        initial_points.iter().copied().collect();
    let total_points_change: i64 = streamers
        .iter()
        .map(|streamer| {
            streamer.channel_points
                - initial_points
                    .get(streamer.username.as_str())
                    .copied()
                    .unwrap_or_default()
        })
        .sum();

    let total_points_line = if anonymize {
        "Total Points gained: [hidden]".to_string()
    } else {
        let sign = if total_points_change < 0 { "-" } else { "+" };
        format!("Total Points gained: {sign}{}", total_points_change.abs())
    };

    let streamers = streamers
        .iter()
        .filter_map(|streamer| {
            let initial = initial_points
                .get(streamer.username.as_str())
                .copied()
                .unwrap_or_default();
            let total = streamer.channel_points - initial;
            if total == 0 && streamer.history.is_empty() {
                return None;
            }

            let total_points_line = if anonymize {
                "Total Points [hidden]".to_string()
            } else {
                let sign = if total < 0 { "-" } else { "+" };
                format!("Total Points {sign}{}", total.abs())
            };
            let history_lines = streamer
                .history
                .iter()
                .map(|(reason, entry)| {
                    if anonymize {
                        format!("{reason} ({} times, [hidden])", entry.count)
                    } else {
                        format!("{reason} ({} times, {} gained)", entry.count, entry.amount)
                    }
                })
                .collect();

            Some(StreamerSummary {
                username: streamer.username.clone(),
                current_points: if anonymize {
                    "[hidden]".to_string()
                } else {
                    format_channel_points(streamer.channel_points)
                },
                total_points_line,
                history_lines,
            })
        })
        .collect();

    SessionSummary {
        duration: format_duration(duration),
        total_points_line,
        streamers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_domain::{
        CommunityGoal, HistoryEntry, IrcMode, PredictionDecision, PredictionEvent,
        PredictionOutcome, Stream, StreamerSettings,
    };

    fn ts(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    #[test]
    fn pubsub_gain_supports_prediction_stake_deduction() {
        let mut streamer = Streamer {
            username: "tester".into(),
            channel_points: 1_000_000,
            points_init: true,
            ..Streamer::default()
        };

        let delta = apply_pubsub_gain(&mut streamer, -250_000, "PREDICTION", 0);
        assert_eq!(delta, -250_000);
        assert_eq!(streamer.channel_points, 750_000);

        let delta = apply_pubsub_gain(&mut streamer, 256_827, "PREDICTION", 0);
        assert_eq!(delta, 256_827);
        assert_eq!(streamer.channel_points, 1_006_827);

        let entry = streamer.history.get("PREDICTION").unwrap();
        assert_eq!(entry.amount, 6_827);
        assert_eq!(entry.count, 2);
    }

    #[test]
    fn positive_pubsub_gains_do_not_move_balance_backward() {
        let mut streamer = Streamer {
            username: "tester".into(),
            channel_points: 1_000,
            points_init: true,
            ..Streamer::default()
        };

        let delta = apply_pubsub_gain(&mut streamer, 10, "WATCH", 900);
        assert_eq!(delta, 10);
        assert_eq!(streamer.channel_points, 1_010);
    }

    #[test]
    fn zero_earned_pubsub_message_can_adopt_absolute_balance() {
        let mut streamer = Streamer {
            username: "tester".into(),
            channel_points: 1_000,
            points_init: true,
            ..Streamer::default()
        };

        let delta = apply_pubsub_gain(&mut streamer, 0, "WATCH", 1_200);
        assert_eq!(delta, 200);
        assert_eq!(streamer.channel_points, 1_200);
    }

    #[test]
    fn watch_streak_history_clears_missing_state() {
        let mut streamer = Streamer {
            stream: Some(Stream {
                watch_streak_missing: true,
                ..Stream::default()
            }),
            settings: StreamerSettings::default(),
            ..Streamer::default()
        };

        update_history(&mut streamer, "WATCH_STREAK", 50);
        assert!(!streamer.stream.as_ref().unwrap().watch_streak_missing);
    }

    #[test]
    fn session_summary_hides_points_in_privacy_mode() {
        let streamer = Streamer {
            username: "tester".into(),
            channel_points: 2_000,
            history: std::collections::HashMap::from([(
                "WATCH".into(),
                HistoryEntry {
                    count: 2,
                    amount: 100,
                },
            )]),
            ..Streamer::default()
        };

        let summary = build_session_summary(
            &[streamer],
            &[("tester", 1_500)],
            true,
            std::time::Duration::from_secs(45),
        );

        assert_eq!(summary.duration, "45s");
        assert_eq!(summary.total_points_line, "Total Points gained: [hidden]");
        assert_eq!(summary.streamers[0].current_points, "[hidden]");
        assert_eq!(
            summary.streamers[0].total_points_line,
            "Total Points [hidden]"
        );
        assert_eq!(
            summary.streamers[0].history_lines[0],
            "WATCH (2 times, [hidden])"
        );
    }

    #[test]
    fn runtime_state_builds_from_config_with_overrides() {
        let config = ConfigFile {
            streamers: vec!["StreamerOne".into(), "streamertwo".into(), "ignored".into()],
            streamers_exclude: vec!["ignored".into()],
            watch_priority: vec!["POINTS_ASC".into(), "DROPS".into()],
            game_priority: vec!["Valorant".into()],
            streamer_overrides: HashMap::from([(
                "streamertwo".into(),
                tm_config::StreamerSettingsOverride {
                    claim_drops: Some(false),
                    chat_presence: Some("OFFLINE".into()),
                    ..tm_config::StreamerSettingsOverride::default()
                },
            )]),
            ..ConfigFile::default()
        };

        let state = RuntimeState::from_config(&config, ts(1000));
        assert!(!state.follower_mode);
        assert_eq!(state.streamers.len(), 2);
        assert_eq!(state.streamers[0].username, "streamerone");
        assert_eq!(state.streamers[1].username, "streamertwo");
        assert_eq!(
            state.watch_priorities,
            parse_watch_priorities(&config.watch_priority)
        );
        assert_eq!(state.game_priority, vec!["valorant"]);
        assert_eq!(state.streamers[1].settings.irc_mode, IrcMode::Offline);
        assert!(!state.streamers[1].settings.claim_drops);
    }

    #[test]
    fn bootstraps_runtime_session_and_captures_initial_points() {
        let config = ConfigFile {
            streamers: vec!["StreamerOne".into(), "ignored".into()],
            streamers_exclude: vec!["ignored".into()],
            ..ConfigFile::default()
        };

        let session = bootstrap(&config, ts(1_000));
        assert!(!session.summary.follower_mode);
        assert_eq!(session.summary.configured_streamers, 1);
        assert_eq!(session.state.streamers.len(), 1);
        assert_eq!(
            session.state.initial_points.get("streamerone"),
            Some(&session.state.streamers[0].channel_points)
        );
    }

    #[test]
    fn playback_presence_drives_watch_and_chat_targets() {
        let mut state = RuntimeState {
            started_at: ts(0),
            follower_mode: false,
            watch_priorities: vec![WatchPriority::Order],
            game_priority: Vec::new(),
            game_exclusions: Vec::new(),
            streamers: vec![Streamer {
                username: "tester".into(),
                channel_id: "123".into(),
                settings: StreamerSettings {
                    irc_mode: IrcMode::Online,
                    ..StreamerSettings::default()
                },
                stream: Some(Stream::default()),
                ..Streamer::default()
            }],
            initial_points: HashMap::new(),
            predictions: HashMap::new(),
        };

        state.apply_pubsub_event(
            &PubSubEvent::Playback {
                channel_id: "123".into(),
                kind: PlaybackType::StreamUp,
            },
            ts(100),
        );
        assert_eq!(state.desired_chat_logins(), vec!["tester"]);
        assert!(state.watch_target_logins(ts(120)).is_empty());
        assert_eq!(state.watch_target_logins(ts(131)), vec!["tester"]);

        state.apply_pubsub_event(
            &PubSubEvent::Playback {
                channel_id: "123".into(),
                kind: PlaybackType::StreamDown,
            },
            ts(200),
        );
        assert!(state.desired_chat_logins().is_empty());
        assert!(!state.streamers[0].is_online);
        assert_eq!(state.streamers[0].offline_at, Some(ts(200)));
        assert_eq!(
            state.streamers[0].stream.as_ref().unwrap().minute_watched,
            0.0
        );
    }

    #[test]
    fn raid_moment_goal_and_prediction_events_emit_effects() {
        let mut state = RuntimeState {
            started_at: ts(0),
            follower_mode: false,
            watch_priorities: vec![WatchPriority::Order],
            game_priority: Vec::new(),
            game_exclusions: Vec::new(),
            streamers: vec![Streamer {
                username: "tester".into(),
                channel_id: "123".into(),
                settings: StreamerSettings {
                    follow_raid: true,
                    claim_moments: true,
                    community_goals: true,
                    make_predictions: true,
                    ..StreamerSettings::default()
                },
                ..Streamer::default()
            }],
            initial_points: HashMap::new(),
            predictions: HashMap::new(),
        };

        let raid_effects = state.apply_pubsub_event(
            &PubSubEvent::Raid {
                channel_id: "123".into(),
                raid_id: "raid-1".into(),
                target_login: "target".into(),
            },
            ts(100),
        );
        assert_eq!(
            raid_effects,
            vec![RuntimeEffect::JoinRaid {
                channel_id: "123".into(),
                raid_id: "raid-1".into(),
                target_login: "target".into(),
            }]
        );
        assert!(state
            .apply_pubsub_event(
                &PubSubEvent::Raid {
                    channel_id: "123".into(),
                    raid_id: "raid-1".into(),
                    target_login: "target".into(),
                },
                ts(101),
            )
            .is_empty());

        let moment_effects = state.apply_pubsub_event(
            &PubSubEvent::Moment {
                channel_id: "123".into(),
                moment_id: "moment-1".into(),
            },
            ts(102),
        );
        assert_eq!(
            moment_effects,
            vec![RuntimeEffect::ClaimMoment {
                channel_id: "123".into(),
                moment_id: "moment-1".into(),
            }]
        );

        let goal_effects = state.apply_pubsub_event(
            &PubSubEvent::CommunityGoal {
                channel_id: "123".into(),
                kind: CommunityGoalKind::Created,
                goal: Some(CommunityGoal {
                    id: "goal-1".into(),
                    title: "Goal".into(),
                    is_in_stock: true,
                    points_contributed: 10,
                    amount_needed: 100,
                    per_stream_user_maximum_contribution: 50,
                    status: "ACTIVE".into(),
                }),
                goal_id: Some("goal-1".into()),
            },
            ts(103),
        );
        assert_eq!(
            goal_effects,
            vec![RuntimeEffect::ContributeCommunityGoals {
                channel_id: "123".into(),
            }]
        );
        assert!(state.streamers[0].community_goals.contains_key("goal-1"));

        let prediction_effects = state.apply_pubsub_event(
            &PubSubEvent::PredictionChannel {
                kind: PredictionChannelKind::EventCreated,
                event: Box::new(PredictionEvent {
                    streamer: state.streamers[0].clone(),
                    event_id: "event-1".into(),
                    title: "Prediction".into(),
                    status: "ACTIVE".into(),
                    created_at: ts(104),
                    window_seconds: 30.0,
                    outcomes: vec![
                        PredictionOutcome {
                            id: "a".into(),
                            title: "Alpha".into(),
                            color: "blue".into(),
                            total_users: 10,
                            total_points: 100,
                            top_points: 20,
                            percentage_users: 66.66666666666667,
                            odds: 1.5,
                            odds_percentage: 66.66666666666667,
                        },
                        PredictionOutcome {
                            id: "b".into(),
                            title: "Beta".into(),
                            color: "pink".into(),
                            total_users: 5,
                            total_points: 50,
                            top_points: 10,
                            percentage_users: 33.333333333333336,
                            odds: 3.0,
                            odds_percentage: 33.333333333333336,
                        },
                    ],
                    decision: PredictionDecision::default(),
                    bet_placed: false,
                    bet_confirmed: false,
                    result_type: String::new(),
                    result_string: String::new(),
                }),
                winning_outcome_id: None,
            },
            ts(104),
        );
        assert_eq!(
            prediction_effects,
            vec![RuntimeEffect::EvaluatePrediction {
                event_id: "event-1".into(),
            }]
        );
        assert!(state.predictions.contains_key("event-1"));

        let prediction_result = tm_pubsub::parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-user-v1.user","message":"{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"event-1\",\"result\":{\"type\":\"WIN\"}}}}"}}"#,
            &[],
        )
        .unwrap()
        .unwrap();
        let settled = state.apply_pubsub_event(&prediction_result, ts(105));
        assert_eq!(
            settled,
            vec![RuntimeEffect::PredictionSettled {
                event_id: "event-1".into(),
                streamer_username: "tester".into(),
                title: "Prediction".into(),
                decision_label: String::new(),
                result_type: "WIN".into(),
                result_string: "WIN, Gained: +0".into(),
            }]
        );
        assert!(!state.predictions.contains_key("event-1"));
    }

    #[test]
    fn runtime_session_summary_uses_captured_initial_points() {
        let mut state = RuntimeState {
            started_at: ts(10),
            follower_mode: false,
            watch_priorities: vec![WatchPriority::Order],
            game_priority: Vec::new(),
            game_exclusions: Vec::new(),
            streamers: vec![Streamer {
                username: "tester".into(),
                channel_points: 1_000,
                ..Streamer::default()
            }],
            initial_points: HashMap::new(),
            predictions: HashMap::new(),
        };

        state.capture_initial_points();
        state.streamers[0].channel_points = 1_250;
        update_history(&mut state.streamers[0], "WATCH", 250);

        let summary = state.session_summary(false, ts(70));
        assert_eq!(summary.duration, "01m 00s");
        assert_eq!(summary.total_points_line, "Total Points gained: +250");
        assert_eq!(summary.streamers[0].current_points, "1.25k");
    }

    #[tokio::test]
    async fn spawned_runtime_is_single_writer_for_pubsub_and_shutdown() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };
        let runtime = spawn_runtime(&config, ts(10));
        let summary = runtime.runtime_summary().await.unwrap();
        assert_eq!(summary.configured_streamers, 1);

        runtime
            .apply_pubsub_event(
                PubSubEvent::PointsEarned {
                    channel_id: String::new(),
                    earned: 100,
                    reason: "WATCH".into(),
                    balance: 100,
                },
                ts(20),
            )
            .await
            .unwrap();
        let summary = runtime.shutdown(false, ts(70)).await.unwrap();
        assert_eq!(summary.duration, "01m 00s");
    }

    #[tokio::test]
    async fn spawned_runtime_notifies_state_change_subscribers() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };
        let runtime = spawn_runtime(&config, ts(10));
        let mut changes = runtime.subscribe_state_changes();

        runtime.set_presence("100", true, ts(20)).await.unwrap();

        changes.changed().await.unwrap();
        assert_eq!(*changes.borrow(), 1);
    }
}
