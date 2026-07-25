//! Single-writer mining state, effects, snapshots, and session summaries.
//!
//! One actor owns all mutable runtime state. Producers await a bounded command
//! queue instead of dropping events, reducers deduplicate external mutation
//! identifiers before emitting effects, and network mutations execute outside
//! this crate.

pub use tm_domain::OffsetDateTime as RuntimeTime;

mod actor;
mod effect;
mod error;
mod prediction;
mod pubsub;
mod state;
mod summary;
mod types;

pub use actor::{RuntimeHandle, RuntimeMetrics, RuntimeMetricsSnapshot};
pub use effect::RuntimeEffect;
pub use error::{Result, RuntimeError};
pub use summary::{apply_pubsub_gain, build_session_summary, update_history};
pub use tm_events::MinerEvent;
pub use types::{
    ContextUpdate, EventApplication, RuntimeSession, RuntimeState, RuntimeSummary, SessionSummary,
    StreamUpdate, StreamerSummary,
};

// Retained as the stable async bootstrap API even though construction currently needs no await.
#[allow(clippy::unused_async)]
pub async fn run(config: &tm_config::ConfigFile) -> RuntimeSession {
    bootstrap(config, tm_domain::OffsetDateTime::now_utc())
}

#[must_use]
pub fn bootstrap(
    config: &tm_config::ConfigFile,
    started_at: tm_domain::OffsetDateTime,
) -> RuntimeSession {
    RuntimeSession::from_state(RuntimeState::from_config(config, started_at))
}

#[must_use]
pub fn spawn_runtime(
    config: &tm_config::ConfigFile,
    started_at: tm_domain::OffsetDateTime,
) -> RuntimeHandle {
    actor::spawn_runtime_session(bootstrap(config, started_at))
}

#[must_use]
pub fn spawn_runtime_state(state: RuntimeState) -> RuntimeHandle {
    actor::spawn_runtime_session(RuntimeSession::from_state(state))
}

#[must_use]
pub fn spawn_runtime_now(config: &tm_config::ConfigFile) -> RuntimeHandle {
    spawn_runtime(config, tm_domain::OffsetDateTime::now_utc())
}

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
mod tests;
