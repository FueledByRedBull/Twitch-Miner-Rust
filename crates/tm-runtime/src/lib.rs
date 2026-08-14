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
pub use handle::{spawn_runtime_state, RuntimeHandle, RuntimeMetrics, RuntimeMetricsSnapshot};
pub use summary::{apply_pubsub_gain, build_session_summary, update_history};
pub use tm_domain::MinerEvent;
pub use types::{
    ContextUpdate, EventApplication, RuntimeState, RuntimeSummary, SessionSummary, StreamUpdate,
    StreamerSummary,
};

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
mod tests;
