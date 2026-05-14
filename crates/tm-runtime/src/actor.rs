use tm_domain::{OffsetDateTime, PredictionDecision};
use tm_pubsub::PubSubEvent;
use tokio::sync::{mpsc, oneshot, watch};

use crate::effect::RuntimeEffect;
use crate::error::{Result, RuntimeError};
use crate::types::{
    ContextUpdate, RuntimeSession, RuntimeState, RuntimeSummary, SessionSummary, StreamUpdate,
};

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
        respond_to: oneshot::Sender<Vec<RuntimeEffect>>,
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

#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_runtime_session(session: RuntimeSession) -> RuntimeHandle {
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
                    log_dropped_runtime_reply(&send_runtime_reply(
                        "ApplyPubSub",
                        respond_to,
                        state.apply_pubsub_event(&event, now),
                    ));
                    notify_state_change(&state_revision_tx, &mut state_revision);
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
            .map_err(|_| RuntimeError::SendFailed {
                command: "ApplyPubSub",
            })?;
        recv.await.map_err(|_| RuntimeError::ActorClosed {
            command: "ApplyPubSub",
        })
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
