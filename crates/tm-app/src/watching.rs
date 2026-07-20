use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use tm_runtime::RuntimeTime;

const MINUTE_WATCHER_RESUME_GAP: i64 = 10 * 60;
const MAX_CONCURRENT_WATCHERS: usize = 2;
const WATCH_ROTATION_SECONDS: i64 = 15 * 60;

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

#[derive(Debug, Default)]
pub(crate) struct WatchRotation {
    queue: VecDeque<String>,
    active_since: Option<RuntimeTime>,
}

impl WatchRotation {
    pub(crate) fn select(&mut self, ordered_eligible: &[String], now: RuntimeTime) -> Vec<String> {
        self.queue
            .retain(|login| ordered_eligible.iter().any(|eligible| eligible == login));
        for login in ordered_eligible {
            if !self.queue.iter().any(|queued| queued == login) {
                self.queue.push_back(login.clone());
            }
        }

        if self.queue.is_empty() {
            self.active_since = None;
            return Vec::new();
        }

        if self.active_since.is_none() {
            self.active_since = Some(now);
        }

        if self.queue.len() > MAX_CONCURRENT_WATCHERS
            && self
                .active_since
                .is_some_and(|started| (now - started).whole_seconds() >= WATCH_ROTATION_SECONDS)
        {
            for _ in 0..MAX_CONCURRENT_WATCHERS {
                if let Some(login) = self.queue.pop_front() {
                    self.queue.push_back(login);
                }
            }
            self.active_since = Some(now);
        }

        self.active().cloned().collect()
    }

    fn active(&self) -> impl Iterator<Item = &String> {
        self.queue.iter().take(MAX_CONCURRENT_WATCHERS)
    }
}

pub(crate) fn minute_watcher_resume_gap(
    previous: RuntimeTime,
    current: RuntimeTime,
) -> Option<Duration> {
    let gap = (current - previous).whole_seconds();
    (gap >= MINUTE_WATCHER_RESUME_GAP).then(|| Duration::from_secs(gap.cast_unsigned()))
}

#[cfg(test)]
mod tests {
    use super::WatchRotation;
    use tm_runtime::RuntimeTime;

    fn ts(seconds: u64) -> RuntimeTime {
        RuntimeTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds)
    }

    fn logins(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn rotates_two_creditable_slots_every_fifteen_minutes() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta", "echo"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select(&eligible, ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select(&eligible, ts(899)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select(&eligible, ts(900)),
            logins(&["charlie", "delta"])
        );
        assert_eq!(
            rotation.select(&eligible, ts(1_800)),
            logins(&["echo", "alpha"])
        );
        assert_eq!(
            rotation.select(&eligible, ts(2_700)),
            logins(&["bravo", "charlie"])
        );
    }

    #[test]
    fn removes_ineligible_channels_and_refills_without_waiting() {
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select(&logins(&["alpha", "bravo", "charlie"]), ts(0)),
            logins(&["alpha", "bravo"])
        );

        assert_eq!(
            rotation.select(&logins(&["bravo", "charlie"]), ts(30)),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select(&logins(&["bravo", "charlie", "delta"]), ts(899)),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select(&logins(&["bravo", "charlie", "delta"]), ts(900)),
            logins(&["delta", "bravo"])
        );
    }

    #[test]
    fn returns_every_available_channel_when_at_or_below_the_limit() {
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select(&logins(&["alpha"]), ts(0)),
            logins(&["alpha"])
        );
        assert_eq!(
            rotation.select(&logins(&["alpha", "bravo"]), ts(1)),
            logins(&["alpha", "bravo"])
        );
        assert!(rotation.select(&[], ts(2)).is_empty());
    }
}
