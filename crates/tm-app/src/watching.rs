use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use tm_runtime::RuntimeTime;

const MINUTE_WATCHER_RESUME_GAP: i64 = 10 * 60;

#[derive(Debug, Clone)]
pub(crate) struct CachedSpadeUrl {
    pub(crate) url: String,
    pub(crate) fetched_at: StdInstant,
}

#[derive(Debug, Clone)]
pub(crate) enum SpadeCacheEntry {
    Ready(CachedSpadeUrl),
    Refreshing(Arc<tokio::sync::Notify>),
}

pub(crate) enum SpadeResolveAction {
    Use(String),
    Wait(Arc<tokio::sync::Notify>),
    Fetch(Arc<tokio::sync::Notify>),
}

pub(crate) fn minute_watcher_resume_gap(
    previous: RuntimeTime,
    current: RuntimeTime,
) -> Option<Duration> {
    let gap = (current - previous).whole_seconds();
    (gap >= MINUTE_WATCHER_RESUME_GAP).then(|| Duration::from_secs(gap.cast_unsigned()))
}
