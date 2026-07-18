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

#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    sender: mpsc::Sender<RuntimeCommand>,
    state_revision: watch::Receiver<u64>,
    metrics: Arc<RuntimeMetrics>,
}

const RUNTIME_QUEUE_CAPACITY: u64 = 64;

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
    fn record_enqueued(&self, available_capacity: usize) {
        let depth = RUNTIME_QUEUE_CAPACITY
            .saturating_sub(available_capacity as u64)
            .saturating_add(1)
            .min(RUNTIME_QUEUE_CAPACITY);
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

#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_runtime_session(session: RuntimeSession) -> RuntimeHandle {
    let (sender, mut receiver) = mpsc::channel(64);
    let (state_revision_tx, state_revision_rx) = watch::channel(0_u64);
    let metrics = Arc::new(RuntimeMetrics::default());
    let actor_metrics = Arc::clone(&metrics);
    tokio::spawn(async move {
        let RuntimeSession { summary, mut state } = session;
        let mut state_revision = 0_u64;
        while let Some(command) = receiver.recv().await {
            match command {
                RuntimeCommand::ApplyEvent {
                    event,
                    now,
                    enqueued_at,
                    respond_to,
                } => {
                    actor_metrics.record_processed(enqueued_at.elapsed());
                    let application = state.apply_event_with_outcome(&event, now);
                    if application.changed {
                        notify_state_change(&state_revision_tx, &mut state_revision);
                    }
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "ApplyEvent",
                        respond_to,
                        application,
                    ));
                }
                RuntimeCommand::SessionSummary {
                    anonymize,
                    now,
                    respond_to,
                } => {
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "SessionSummary",
                        respond_to,
                        state.session_summary(anonymize, now),
                    ));
                }
                RuntimeCommand::RuntimeSummary { respond_to } => {
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "RuntimeSummary",
                        respond_to,
                        summary.clone(),
                    ));
                }
                RuntimeCommand::StateSnapshot { respond_to } => {
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "StateSnapshot",
                        respond_to,
                        state.clone(),
                    ));
                }
                RuntimeCommand::ApplyContext { update, respond_to } => {
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "ApplyContext",
                        respond_to,
                        state.apply_context_update(&update),
                    ));
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::ApplyStreamUpdate { update, now } => {
                    state.apply_stream_update(&update, now);
                    notify_state_change(&state_revision_tx, &mut state_revision);
                }
                RuntimeCommand::SetDropCampaignEligibility {
                    channel_id,
                    eligible,
                } => {
                    state.set_drop_campaign_eligibility(&channel_id, eligible);
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
                RuntimeCommand::SetPresenceChecked {
                    channel_id,
                    online,
                    now,
                    respond_to,
                } => {
                    let changed = state.apply_presence(&channel_id, online, now);
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "SetPresenceChecked",
                        respond_to,
                        changed,
                    ));
                    if changed {
                        notify_state_change(&state_revision_tx, &mut state_revision);
                    }
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
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "Shutdown",
                        respond_to,
                        state.session_summary(anonymize, now),
                    ));
                    break;
                }
            }
        }
    });
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
        self.metrics.record_enqueued(self.sender.capacity());
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
