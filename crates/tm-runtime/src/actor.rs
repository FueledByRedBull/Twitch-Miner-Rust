use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tm_domain::{OffsetDateTime, PredictionDecision};
use tm_events::MinerEvent;
use tokio::sync::{mpsc, oneshot, watch};

use crate::effect::RuntimeEffect;
use crate::error::{Result, RuntimeError};
use crate::types::{
    ContextUpdate, EventApplication, RuntimeSession, RuntimeState, RuntimeSummary, SessionSummary,
    StreamUpdate,
};

/// Cloneable command handle for the sole mutable [`RuntimeState`] owner.
///
/// Every operation awaits bounded queue capacity. Dropping a caller never
/// transfers state ownership or permits later producers to bypass ordering.
#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    sender: mpsc::Sender<RuntimeCommand>,
    state_revision: watch::Receiver<u64>,
    metrics: Arc<RuntimeMetrics>,
}

// A bounded queue applies backpressure instead of dropping transport events.
// The capacity is covered by the ignored release-mode sweep below and by the
// mixed replay benchmark; 64 absorbs realistic bursts without materially
// increasing tail latency or memory relative to smaller/larger candidates.
const RUNTIME_QUEUE_CAPACITY: usize = 64;

/// Lock-free measurements for the actor boundary.
///
/// Metrics are observational only and never participate in state decisions.
#[derive(Debug, Default)]
pub struct RuntimeMetrics {
    processed_events: AtomicU64,
    total_command_wait_micros: AtomicU64,
    max_queue_depth: AtomicU64,
    transport_events: AtomicU64,
    total_transport_latency_micros: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeMetricsSnapshot {
    pub processed_events: u64,
    pub total_command_wait_micros: u64,
    pub max_queue_depth: u64,
    pub transport_events: u64,
    pub total_transport_latency_micros: u64,
}

impl RuntimeMetrics {
    fn record_enqueued(&self, available_capacity: usize, queue_capacity: usize) {
        let capacity = u64::try_from(queue_capacity).unwrap_or(u64::MAX);
        let available = u64::try_from(available_capacity).unwrap_or(u64::MAX);
        let depth = capacity
            .saturating_sub(available)
            .saturating_add(1)
            .min(capacity);
        self.max_queue_depth.fetch_max(depth, Ordering::Relaxed);
    }

    fn record_processed(&self, wait: std::time::Duration) {
        self.processed_events.fetch_add(1, Ordering::Relaxed);
        let micros = wait.as_micros().try_into().unwrap_or(u64::MAX);
        self.total_command_wait_micros
            .fetch_add(micros, Ordering::Relaxed);
    }

    pub fn record_transport_latency(&self, latency: std::time::Duration) {
        self.transport_events.fetch_add(1, Ordering::Relaxed);
        let micros = latency.as_micros().try_into().unwrap_or(u64::MAX);
        self.total_transport_latency_micros
            .fetch_add(micros, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> RuntimeMetricsSnapshot {
        RuntimeMetricsSnapshot {
            processed_events: self.processed_events.load(Ordering::Relaxed),
            total_command_wait_micros: self.total_command_wait_micros.load(Ordering::Relaxed),
            max_queue_depth: self.max_queue_depth.load(Ordering::Relaxed),
            transport_events: self.transport_events.load(Ordering::Relaxed),
            total_transport_latency_micros: self
                .total_transport_latency_micros
                .load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
enum RuntimeCommand {
    ApplyEvent {
        event: MinerEvent,
        now: OffsetDateTime,
        enqueued_at: Instant,
        respond_to: oneshot::Sender<EventApplication>,
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
        respond_to: oneshot::Sender<Vec<RuntimeEffect>>,
    },
    ApplyStreamUpdate {
        update: StreamUpdate,
        now: OffsetDateTime,
    },
    SetDropCampaignEligibility {
        channel_id: String,
        eligible: bool,
    },
    UpdateStreamerLogin {
        channel_id: String,
        login: String,
        respond_to: oneshot::Sender<bool>,
    },
    SuspendWatching {
        channel_id: String,
        until: OffsetDateTime,
    },
    SetPresence {
        channel_id: String,
        online: bool,
        now: OffsetDateTime,
    },
    SetPresenceChecked {
        channel_id: String,
        online: bool,
        now: OffsetDateTime,
        respond_to: oneshot::Sender<bool>,
    },
    MarkMinuteWatched {
        channel_id: String,
        now: OffsetDateTime,
    },
    MarkWatchStreakRecovered {
        channel_id: String,
        streak_count: Option<u32>,
        resolved_at: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
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

struct RuntimeActor {
    summary: RuntimeSummary,
    state: RuntimeState,
    state_revision_tx: watch::Sender<u64>,
    state_revision: u64,
    metrics: Arc<RuntimeMetrics>,
}

impl RuntimeActor {
    async fn run(mut self, mut receiver: mpsc::Receiver<RuntimeCommand>) {
        while let Some(command) = receiver.recv().await {
            if !self.handle_command(command) {
                break;
            }
        }
    }

    fn handle_command(&mut self, command: RuntimeCommand) -> bool {
        match command {
            RuntimeCommand::ApplyEvent {
                event,
                now,
                enqueued_at,
                respond_to,
            } => self.apply_event(&event, now, enqueued_at, respond_to),
            RuntimeCommand::SessionSummary {
                anonymize,
                now,
                respond_to,
            } => Self::reply(
                "SessionSummary",
                respond_to,
                self.state.session_summary(anonymize, now),
            ),
            RuntimeCommand::RuntimeSummary { respond_to } => {
                Self::reply("RuntimeSummary", respond_to, self.summary.clone());
            }
            RuntimeCommand::StateSnapshot { respond_to } => {
                Self::reply("StateSnapshot", respond_to, self.state.clone());
            }
            RuntimeCommand::ApplyContext { update, respond_to } => {
                self.apply_context(&update, respond_to);
            }
            RuntimeCommand::ApplyStreamUpdate { update, now } => {
                self.apply_stream_update(&update, now);
            }
            RuntimeCommand::SetDropCampaignEligibility {
                channel_id,
                eligible,
            } => {
                self.set_drop_campaign_eligibility(&channel_id, eligible);
            }
            RuntimeCommand::UpdateStreamerLogin {
                channel_id,
                login,
                respond_to,
            } => self.update_streamer_login(&channel_id, &login, respond_to),
            RuntimeCommand::SuspendWatching { channel_id, until } => {
                if self.state.suspend_watching(&channel_id, until) {
                    self.notify_state_change();
                }
            }
            RuntimeCommand::SetPresence {
                channel_id,
                online,
                now,
            } => {
                self.set_presence(&channel_id, online, now);
            }
            RuntimeCommand::SetPresenceChecked {
                channel_id,
                online,
                now,
                respond_to,
            } => self.set_presence_checked(&channel_id, online, now, respond_to),
            RuntimeCommand::MarkMinuteWatched { channel_id, now } => {
                self.mark_minute_watched(&channel_id, now);
            }
            RuntimeCommand::MarkWatchStreakRecovered {
                channel_id,
                streak_count,
                resolved_at,
                expires_at,
            } => {
                self.mark_watch_streak_recovered(
                    &channel_id,
                    streak_count,
                    resolved_at,
                    expires_at,
                );
            }
            RuntimeCommand::RecordPredictionPlaced {
                event_id,
                decision,
                deduct_stake,
            } => {
                self.record_prediction_placed(&event_id, &decision, deduct_stake);
            }
            RuntimeCommand::StopTrackingPrediction {
                event_id,
                result_type,
            } => {
                self.stop_tracking_prediction(&event_id, &result_type);
            }
            RuntimeCommand::Shutdown {
                anonymize,
                now,
                respond_to,
            } => {
                Self::reply(
                    "Shutdown",
                    respond_to,
                    self.state.session_summary(anonymize, now),
                );
                return false;
            }
        }
        true
    }

    fn apply_event(
        &mut self,
        event: &MinerEvent,
        now: OffsetDateTime,
        enqueued_at: Instant,
        respond_to: oneshot::Sender<EventApplication>,
    ) {
        self.metrics.record_processed(enqueued_at.elapsed());
        let application = self.state.apply_event_with_outcome(event, now);
        if application.changed {
            self.notify_state_change();
        }
        Self::reply("ApplyEvent", respond_to, application);
    }

    fn update_streamer_login(
        &mut self,
        channel_id: &str,
        login: &str,
        respond_to: oneshot::Sender<bool>,
    ) {
        let changed = self.state.update_streamer_login(channel_id, login);
        if changed {
            self.notify_state_change();
        }
        Self::reply("UpdateStreamerLogin", respond_to, changed);
    }

    fn set_presence_checked(
        &mut self,
        channel_id: &str,
        online: bool,
        now: OffsetDateTime,
        respond_to: oneshot::Sender<bool>,
    ) {
        let changed = self.state.apply_presence(channel_id, online, now);
        Self::reply("SetPresenceChecked", respond_to, changed);
        if changed {
            self.notify_state_change();
        }
    }

    fn apply_context(
        &mut self,
        update: &ContextUpdate,
        respond_to: oneshot::Sender<Vec<RuntimeEffect>>,
    ) {
        let effects = self.state.apply_context_update(update);
        Self::reply("ApplyContext", respond_to, effects);
        self.notify_state_change();
    }

    fn apply_stream_update(&mut self, update: &StreamUpdate, now: OffsetDateTime) {
        self.state.apply_stream_update(update, now);
        self.notify_state_change();
    }

    fn set_drop_campaign_eligibility(&mut self, channel_id: &str, eligible: bool) {
        self.state
            .set_drop_campaign_eligibility(channel_id, eligible);
        self.notify_state_change();
    }

    fn set_presence(&mut self, channel_id: &str, online: bool, now: OffsetDateTime) {
        self.state.apply_presence(channel_id, online, now);
        self.notify_state_change();
    }

    fn mark_minute_watched(&mut self, channel_id: &str, now: OffsetDateTime) {
        self.state.mark_minute_watched(channel_id, now);
        self.notify_state_change();
    }

    fn mark_watch_streak_recovered(
        &mut self,
        channel_id: &str,
        streak_count: Option<u32>,
        resolved_at: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
    ) {
        if self
            .state
            .mark_watch_streak_recovered(channel_id, streak_count, resolved_at, expires_at)
        {
            self.notify_state_change();
        }
    }

    fn record_prediction_placed(
        &mut self,
        event_id: &str,
        decision: &PredictionDecision,
        deduct_stake: bool,
    ) {
        self.state
            .record_prediction_placed(event_id, decision, deduct_stake);
        self.notify_state_change();
    }

    fn stop_tracking_prediction(&mut self, event_id: &str, result_type: &str) {
        self.state.stop_tracking_prediction(event_id, result_type);
        self.notify_state_change();
    }

    fn notify_state_change(&mut self) {
        notify_state_change(&self.state_revision_tx, &mut self.state_revision);
    }

    fn reply<T>(command: &'static str, respond_to: oneshot::Sender<T>, value: T) {
        log_dropped_runtime_reply(&send_runtime_reply(command, respond_to, value));
    }
}

pub(crate) fn spawn_runtime_session(session: RuntimeSession) -> RuntimeHandle {
    spawn_runtime_session_with_capacity(session, RUNTIME_QUEUE_CAPACITY)
}

fn spawn_runtime_session_with_capacity(
    session: RuntimeSession,
    queue_capacity: usize,
) -> RuntimeHandle {
    let (sender, receiver) = mpsc::channel(queue_capacity);
    let (state_revision_tx, state_revision_rx) = watch::channel(0_u64);
    let metrics = Arc::new(RuntimeMetrics::default());
    let RuntimeSession { summary, state } = session;
    let actor = RuntimeActor {
        summary,
        state,
        state_revision_tx,
        state_revision: 0,
        metrics: Arc::clone(&metrics),
    };
    tokio::spawn(actor.run(receiver));
    RuntimeHandle {
        sender,
        state_revision: state_revision_rx,
        metrics,
    }
}

fn notify_state_change(sender: &watch::Sender<u64>, revision: &mut u64) {
    *revision = revision.saturating_add(1);
    let _ = sender.send(*revision);
}

fn send_runtime_reply<T>(
    command: &'static str,
    respond_to: oneshot::Sender<T>,
    value: T,
) -> Result<()> {
    respond_to
        .send(value)
        .map_err(|_| RuntimeError::CallerDropped { command })
}

fn log_dropped_runtime_reply(result: &Result<()>) {
    if let Err(RuntimeError::CallerDropped { command }) = result {
        tracing::warn!(command, "runtime reply receiver dropped");
    }
}

impl RuntimeHandle {
    #[must_use]
    pub fn subscribe_state_changes(&self) -> watch::Receiver<u64> {
        self.state_revision.clone()
    }

    pub async fn apply_event(
        &self,
        event: MinerEvent,
        now: OffsetDateTime,
    ) -> Result<Vec<RuntimeEffect>> {
        Ok(self.apply_event_with_outcome(event, now).await?.effects)
    }

    pub async fn apply_event_with_outcome(
        &self,
        event: MinerEvent,
        now: OffsetDateTime,
    ) -> Result<EventApplication> {
        let (send, recv) = oneshot::channel();
        let enqueued_at = Instant::now();
        self.metrics
            .record_enqueued(self.sender.capacity(), self.sender.max_capacity());
        self.sender
            .send(RuntimeCommand::ApplyEvent {
                event,
                now,
                enqueued_at,
                respond_to: send,
            })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "ApplyEvent",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "ApplyEvent",
        })
    }

    #[must_use]
    pub fn metrics(&self) -> RuntimeMetricsSnapshot {
        self.metrics.snapshot()
    }

    #[must_use]
    pub fn metrics_handle(&self) -> Arc<RuntimeMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Compatibility wrapper for callers that still use the former transport name.
    pub async fn apply_pubsub_event(
        &self,
        event: MinerEvent,
        now: OffsetDateTime,
    ) -> Result<Vec<RuntimeEffect>> {
        self.apply_event(event, now).await
    }

    pub async fn runtime_summary(&self) -> Result<RuntimeSummary> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::RuntimeSummary { respond_to: send })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "RuntimeSummary",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "RuntimeSummary",
        })
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
            .map_err(|_| RuntimeError::SendFailed {
                command: "SessionSummary",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "SessionSummary",
        })
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
            .map_err(|_| RuntimeError::SendFailed {
                command: "Shutdown",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "Shutdown",
        })
    }

    pub async fn state_snapshot(&self) -> Result<RuntimeState> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::StateSnapshot { respond_to: send })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "StateSnapshot",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "StateSnapshot",
        })
    }

    pub async fn apply_context_update(&self, update: ContextUpdate) -> Result<Vec<RuntimeEffect>> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::ApplyContext {
                update,
                respond_to: send,
            })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "ApplyContext",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "ApplyContext",
        })
    }

    pub async fn apply_stream_update(
        &self,
        update: StreamUpdate,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::ApplyStreamUpdate { update, now })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "ApplyStreamUpdate",
            })
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
            .map_err(|_| RuntimeError::SendFailed {
                command: "SetPresence",
            })
    }

    pub async fn set_drop_campaign_eligibility(
        &self,
        channel_id: impl Into<String>,
        eligible: bool,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::SetDropCampaignEligibility {
                channel_id: channel_id.into(),
                eligible,
            })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "SetDropCampaignEligibility",
            })
    }

    pub async fn update_streamer_login(
        &self,
        channel_id: impl Into<String>,
        login: impl Into<String>,
    ) -> Result<bool> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::UpdateStreamerLogin {
                channel_id: channel_id.into(),
                login: login.into(),
                respond_to: send,
            })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "UpdateStreamerLogin",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "UpdateStreamerLogin",
        })
    }

    pub async fn suspend_watching(
        &self,
        channel_id: impl Into<String>,
        until: OffsetDateTime,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::SuspendWatching {
                channel_id: channel_id.into(),
                until,
            })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "SuspendWatching",
            })
    }

    pub async fn set_presence_if_changed(
        &self,
        channel_id: impl Into<String>,
        online: bool,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let (send, recv) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::SetPresenceChecked {
                channel_id: channel_id.into(),
                online,
                now,
                respond_to: send,
            })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "SetPresenceChecked",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "SetPresenceChecked",
        })
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
            .map_err(|_| RuntimeError::SendFailed {
                command: "MarkMinuteWatched",
            })
    }

    pub async fn mark_watch_streak_recovered(
        &self,
        channel_id: impl Into<String>,
        streak_count: Option<u32>,
        resolved_at: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<()> {
        self.sender
            .send(RuntimeCommand::MarkWatchStreakRecovered {
                channel_id: channel_id.into(),
                streak_count,
                resolved_at,
                expires_at,
            })
            .await
            .map_err(|_| RuntimeError::SendFailed {
                command: "MarkWatchStreakRecovered",
            })
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
            .map_err(|_| RuntimeError::SendFailed {
                command: "RecordPredictionPlaced",
            })
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
            .map_err(|_| RuntimeError::SendFailed {
                command: "StopTrackingPrediction",
            })
    }
}

#[cfg(test)]
mod queue_profile {
    use super::*;
    use tm_config::ConfigFile;

    const PROFILE_STREAMERS: usize = 17;
    const PROFILE_EVENTS: usize = 5_000;

    fn profile_session() -> RuntimeSession {
        let targets = (0..PROFILE_STREAMERS)
            .map(|index| format!("streamer-{index}"))
            .collect::<Vec<_>>();
        let config = ConfigFile {
            streamers: targets.clone(),
            ..ConfigFile::default()
        };
        let mut state = RuntimeState::from_targets(&config, &targets, OffsetDateTime::UNIX_EPOCH);
        for (index, streamer) in state.streamers.iter_mut().enumerate() {
            streamer.channel_id = format!("channel-{index}");
        }
        RuntimeSession::from_state(state)
    }

    async fn measure_capacity(capacity: usize) -> (f64, u128, RuntimeMetricsSnapshot) {
        let runtime = spawn_runtime_session_with_capacity(profile_session(), capacity);
        let started = Instant::now();
        let mut tasks = Vec::with_capacity(PROFILE_EVENTS);
        for index in 0..PROFILE_EVENTS {
            let runtime = runtime.clone();
            tasks.push(tokio::spawn(async move {
                let event_started = Instant::now();
                runtime
                    .apply_event(
                        MinerEvent::PointsEarned {
                            channel_id: format!("channel-{}", index % PROFILE_STREAMERS),
                            earned: 10,
                            reason: String::from("WATCH"),
                            balance: 1_000,
                        },
                        OffsetDateTime::UNIX_EPOCH,
                    )
                    .await
                    .expect("profile event must be applied");
                event_started.elapsed().as_micros()
            }));
        }
        let mut latencies = Vec::with_capacity(PROFILE_EVENTS);
        for task in tasks {
            latencies.push(task.await.expect("profile task must complete"));
        }
        let elapsed = started.elapsed();
        latencies.sort_unstable();
        let p95 = latencies[latencies.len() * 95 / 100];
        let metrics = runtime.metrics();
        runtime
            .shutdown(true, OffsetDateTime::UNIX_EPOCH)
            .await
            .expect("profile runtime must stop");
        let event_count =
            f64::from(u32::try_from(PROFILE_EVENTS).expect("profile event count must fit in u32"));
        (event_count / elapsed.as_secs_f64(), p95, metrics)
    }

    #[tokio::test]
    #[ignore = "manual release-mode actor queue capacity sweep"]
    async fn actor_queue_capacity_sweep() {
        println!("capacity,throughput_commands_per_second,p95_micros,max_queue_depth");
        for capacity in [16, 32, 64, 128, 256] {
            let (throughput, p95, metrics) = measure_capacity(capacity).await;
            println!(
                "{capacity},{throughput:.0},{p95},{}",
                metrics.max_queue_depth
            );
        }
    }
}
