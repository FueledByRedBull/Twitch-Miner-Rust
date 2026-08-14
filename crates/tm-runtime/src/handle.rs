use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tm_domain::{MinerEvent, OffsetDateTime, PredictionDecision};
use tokio::sync::{watch, Mutex, MutexGuard};

use crate::effect::RuntimeEffect;
use crate::error::{Result, RuntimeError};
use crate::types::{
    ContextUpdate, EventApplication, RuntimeState, RuntimeSummary, SessionSummary, StreamUpdate,
};

/// Cloneable handle for the sole mutable [`RuntimeState`] owner.
///
/// Each operation takes the same mutex, so state transitions and their effects
/// remain serialized. The lock is held only while reducing state; callers
/// perform network work after this future resolves.
#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    // ponytail: one global state lock; split ownership only if measured contention matters.
    state: Arc<Mutex<RuntimeState>>,
    summary: RuntimeSummary,
    state_revision_tx: watch::Sender<u64>,
    state_revision: watch::Receiver<u64>,
    revision: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    metrics: Arc<RuntimeMetrics>,
}

/// Lock-free measurements for the runtime boundary.
///
/// Metrics are observational only and never participate in state decisions.
#[derive(Debug, Default)]
pub struct RuntimeMetrics {
    processed_events: AtomicU64,
    total_command_wait_micros: AtomicU64,
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
    fn record_command_wait(&self, wait: std::time::Duration) {
        let micros = wait.as_micros().try_into().unwrap_or(u64::MAX);
        self.total_command_wait_micros
            .fetch_add(micros, Ordering::Relaxed);
    }

    fn record_processed(&self, wait: std::time::Duration) {
        self.processed_events.fetch_add(1, Ordering::Relaxed);
        self.record_command_wait(wait);
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
            // Kept in the snapshot for status-file compatibility; mutex
            // serialization has no queue depth to measure.
            max_queue_depth: 0,
            transport_events: self.transport_events.load(Ordering::Relaxed),
            total_transport_latency_micros: self
                .total_transport_latency_micros
                .load(Ordering::Relaxed),
        }
    }
}

#[must_use]
pub fn spawn_runtime_state(mut state: RuntimeState) -> RuntimeHandle {
    let (state_revision_tx, state_revision) = watch::channel(0_u64);
    let metrics = Arc::new(RuntimeMetrics::default());
    state.capture_initial_points();
    let summary = RuntimeSummary {
        configured_streamers: state.streamers.len(),
        follower_mode: state.follower_mode,
    };
    RuntimeHandle {
        state: Arc::new(Mutex::new(state)),
        summary,
        state_revision_tx,
        state_revision,
        revision: Arc::new(AtomicU64::new(0)),
        closed: Arc::new(AtomicBool::new(false)),
        metrics,
    }
}

impl RuntimeHandle {
    async fn lock_open(&self, command: &'static str) -> Result<MutexGuard<'_, RuntimeState>> {
        let state = self.state.lock().await;
        if self.closed.load(Ordering::Acquire) {
            Err(RuntimeError::RuntimeClosed { command })
        } else {
            Ok(state)
        }
    }

    fn notify_state_change(&self) {
        let revision = self
            .revision
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(1))
            })
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let _ = self.state_revision_tx.send(revision);
    }

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
        let started = Instant::now();
        let mut state = self.lock_open("ApplyEvent").await?;
        self.metrics.record_processed(started.elapsed());
        let application = state.apply_event_with_outcome(&event, now);
        if application.changed {
            self.notify_state_change();
        }
        Ok(application)
    }

    #[must_use]
    pub fn metrics(&self) -> RuntimeMetricsSnapshot {
        self.metrics.snapshot()
    }

    #[must_use]
    pub fn metrics_handle(&self) -> Arc<RuntimeMetrics> {
        Arc::clone(&self.metrics)
    }

    pub async fn runtime_summary(&self) -> Result<RuntimeSummary> {
        let started = Instant::now();
        let _state = self.lock_open("RuntimeSummary").await?;
        self.metrics.record_command_wait(started.elapsed());
        Ok(self.summary.clone())
    }

    pub async fn session_summary(
        &self,
        anonymize: bool,
        now: OffsetDateTime,
    ) -> Result<SessionSummary> {
        let started = Instant::now();
        let state = self.lock_open("SessionSummary").await?;
        self.metrics.record_command_wait(started.elapsed());
        Ok(state.session_summary(anonymize, now))
    }

    pub async fn shutdown(&self, anonymize: bool, now: OffsetDateTime) -> Result<SessionSummary> {
        let started = Instant::now();
        let state = self.lock_open("Shutdown").await?;
        self.metrics.record_command_wait(started.elapsed());
        let summary = state.session_summary(anonymize, now);
        self.closed.store(true, Ordering::Release);
        Ok(summary)
    }

    pub async fn state_snapshot(&self) -> Result<RuntimeState> {
        let started = Instant::now();
        let state = self.lock_open("StateSnapshot").await?;
        self.metrics.record_command_wait(started.elapsed());
        Ok(state.clone())
    }

    pub async fn apply_context_update(&self, update: ContextUpdate) -> Result<Vec<RuntimeEffect>> {
        let started = Instant::now();
        let mut state = self.lock_open("ApplyContext").await?;
        self.metrics.record_command_wait(started.elapsed());
        let effects = state.apply_context_update(&update);
        self.notify_state_change();
        Ok(effects)
    }

    pub async fn apply_stream_update(
        &self,
        update: StreamUpdate,
        now: OffsetDateTime,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("ApplyStreamUpdate").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.apply_stream_update(&update, now);
        self.notify_state_change();
        Ok(())
    }

    pub async fn set_presence(
        &self,
        channel_id: impl Into<String>,
        online: bool,
        now: OffsetDateTime,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("SetPresence").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.apply_presence(&channel_id.into(), online, now);
        self.notify_state_change();
        Ok(())
    }

    pub async fn set_drop_campaign_eligibility(
        &self,
        channel_id: impl Into<String>,
        eligible: bool,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("SetDropCampaignEligibility").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.set_drop_campaign_eligibility(&channel_id.into(), eligible);
        self.notify_state_change();
        Ok(())
    }

    pub async fn update_streamer_login(
        &self,
        channel_id: impl Into<String>,
        login: impl Into<String>,
    ) -> Result<bool> {
        let started = Instant::now();
        let mut state = self.lock_open("UpdateStreamerLogin").await?;
        self.metrics.record_command_wait(started.elapsed());
        let changed = state.update_streamer_login(&channel_id.into(), &login.into());
        if changed {
            self.notify_state_change();
        }
        Ok(changed)
    }

    pub async fn suspend_watching(
        &self,
        channel_id: impl Into<String>,
        until: OffsetDateTime,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("SuspendWatching").await?;
        self.metrics.record_command_wait(started.elapsed());
        if state.suspend_watching(&channel_id.into(), until) {
            self.notify_state_change();
        }
        Ok(())
    }

    pub async fn set_presence_if_changed(
        &self,
        channel_id: impl Into<String>,
        online: bool,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let started = Instant::now();
        let mut state = self.lock_open("SetPresenceChecked").await?;
        self.metrics.record_command_wait(started.elapsed());
        let changed = state.apply_presence(&channel_id.into(), online, now);
        if changed {
            self.notify_state_change();
        }
        Ok(changed)
    }

    pub async fn mark_minute_watched(
        &self,
        channel_id: impl Into<String>,
        now: OffsetDateTime,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("MarkMinuteWatched").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.mark_minute_watched(&channel_id.into(), now);
        self.notify_state_change();
        Ok(())
    }

    pub async fn reset_watch_progress(&self, channel_id: impl Into<String>) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("ResetWatchProgress").await?;
        self.metrics.record_command_wait(started.elapsed());
        if state.reset_watch_progress(&channel_id.into()) {
            self.notify_state_change();
        }
        Ok(())
    }

    pub async fn mark_watch_streak_recovered(
        &self,
        channel_id: impl Into<String>,
        streak_count: Option<u32>,
        resolved_at: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("MarkWatchStreakRecovered").await?;
        self.metrics.record_command_wait(started.elapsed());
        if state.mark_watch_streak_recovered(
            &channel_id.into(),
            streak_count,
            resolved_at,
            expires_at,
        ) {
            self.notify_state_change();
        }
        Ok(())
    }

    pub async fn record_prediction_placed(
        &self,
        event_id: impl Into<String>,
        decision: PredictionDecision,
        deduct_stake: bool,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("RecordPredictionPlaced").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.record_prediction_placed(&event_id.into(), &decision, deduct_stake);
        self.notify_state_change();
        Ok(())
    }

    pub async fn stop_tracking_prediction(
        &self,
        event_id: impl Into<String>,
        result_type: impl Into<String>,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("StopTrackingPrediction").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.stop_tracking_prediction(&event_id.into(), &result_type.into());
        self.notify_state_change();
        Ok(())
    }

    pub async fn release_claim_bonus(
        &self,
        channel_id: impl Into<String>,
        claim_id: impl Into<String>,
    ) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("ReleaseClaimBonus").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.release_claim_bonus(&channel_id.into(), &claim_id.into());
        self.notify_state_change();
        Ok(())
    }

    pub async fn release_prediction(&self, event_id: impl Into<String>) -> Result<()> {
        let started = Instant::now();
        let mut state = self.lock_open("ReleasePrediction").await?;
        self.metrics.record_command_wait(started.elapsed());
        state.release_prediction(&event_id.into());
        self.notify_state_change();
        Ok(())
    }
}
