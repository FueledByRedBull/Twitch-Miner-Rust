//! Single-writer mining state, effects, snapshots, and session summaries.
//!
//! One mutex owns all mutable runtime state. Reducers deduplicate external
//! mutation identifiers before emitting effects, and network mutations execute
//! outside this crate.

pub use tm_domain::OffsetDateTime as RuntimeTime;

mod effect;
mod error;
mod handle;
mod prediction;
mod state;
mod summary;
mod types;

pub use effect::RuntimeEffect;
pub use error::{Result, RuntimeError};
pub use handle::{RuntimeHandle, RuntimeMetrics, RuntimeMetricsSnapshot};
pub use summary::{apply_pubsub_gain, build_session_summary, update_history};
pub use tm_domain::MinerEvent;
pub use types::{
    ContextUpdate, EventApplication, RuntimeSession, RuntimeState, RuntimeSummary, SessionSummary,
    StreamUpdate, StreamerSummary,
};

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
    handle::spawn_runtime_session(bootstrap(config, started_at))
}

#[must_use]
pub fn spawn_runtime_state(state: RuntimeState) -> RuntimeHandle {
    handle::spawn_runtime_session(RuntimeSession::from_state(state))
}

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
mod tests;
