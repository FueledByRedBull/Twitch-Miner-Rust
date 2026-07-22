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
    pinned_campaign: Option<String>,
    active_since: Option<RuntimeTime>,
}

impl WatchRotation {
    pub(crate) fn select_with_campaigns(
        &mut self,
        ordered_eligible: &[String],
        campaign_logins: &[String],
        now: RuntimeTime,
    ) -> Vec<String> {
        let pinned_campaign = campaign_logins
            .iter()
            .find(|login| ordered_eligible.iter().any(|eligible| eligible == *login))
            .cloned();
        let campaign_changed = self.pinned_campaign != pinned_campaign;
        self.pinned_campaign = pinned_campaign;

        // Twitch advances Drop progress on only one channel. Pin the first
        // ranked campaign and keep competing campaigns out of the spare slot.
        self.queue.retain(|login| {
            !campaign_logins.contains(login)
                && ordered_eligible.iter().any(|eligible| eligible == login)
        });
        for login in ordered_eligible {
            if !campaign_logins.contains(login) && !self.queue.iter().any(|queued| queued == login)
            {
                self.queue.push_back(login.clone());
            }
        }

        if self.queue.is_empty() {
            self.active_since = None;
            return self.pinned_campaign.iter().cloned().collect();
        }

        if campaign_changed || self.active_since.is_none() {
            self.active_since = Some(now);
        }

        let rotating_slots = MAX_CONCURRENT_WATCHERS - usize::from(self.pinned_campaign.is_some());
        if self.queue.len() > rotating_slots
            && self
                .active_since
                .is_some_and(|started| (now - started).whole_seconds() >= WATCH_ROTATION_SECONDS)
        {
            for _ in 0..rotating_slots {
                if let Some(login) = self.queue.pop_front() {
                    self.queue.push_back(login);
                }
            }
            self.active_since = Some(now);
        }

        self.pinned_campaign
            .iter()
            .cloned()
            .chain(self.queue.iter().take(rotating_slots).cloned())
            .collect()
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
            rotation.select_with_campaigns(&eligible, &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], ts(899)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], ts(900)),
            logins(&["charlie", "delta"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], ts(1_800)),
            logins(&["echo", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], ts(2_700)),
            logins(&["bravo", "charlie"])
        );
    }

    #[test]
    fn removes_ineligible_channels_and_refills_without_waiting() {
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select_with_campaigns(&logins(&["alpha", "bravo", "charlie"]), &[], ts(0),),
            logins(&["alpha", "bravo"])
        );

        assert_eq!(
            rotation.select_with_campaigns(&logins(&["bravo", "charlie"]), &[], ts(30),),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&logins(&["bravo", "charlie", "delta"]), &[], ts(899),),
            logins(&["bravo", "charlie"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&logins(&["bravo", "charlie", "delta"]), &[], ts(900),),
            logins(&["delta", "bravo"])
        );
    }

    #[test]
    fn returns_every_available_channel_when_at_or_below_the_limit() {
        let mut rotation = WatchRotation::default();
        assert_eq!(
            rotation.select_with_campaigns(&logins(&["alpha"]), &[], ts(0)),
            logins(&["alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&logins(&["alpha", "bravo"]), &[], ts(1)),
            logins(&["alpha", "bravo"])
        );
        assert!(rotation.select_with_campaigns(&[], &[], ts(2)).is_empty());
    }

    #[test]
    fn campaign_preempts_immediately_and_keeps_the_other_slot_rotating() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta", "echo"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], ts(0)),
            logins(&["alpha", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta"]), ts(100)),
            logins(&["delta", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta"]), ts(999)),
            logins(&["delta", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta"]), ts(1_000)),
            logins(&["delta", "bravo"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &[], ts(1_001)),
            logins(&["bravo", "charlie"])
        );
    }

    #[test]
    fn campaign_change_preempts_the_previous_campaign() {
        let eligible = logins(&["alpha", "bravo", "charlie", "delta"]);
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["delta", "charlie"]), ts(0),),
            logins(&["delta", "alpha"])
        );
        assert_eq!(
            rotation.select_with_campaigns(&eligible, &logins(&["charlie"]), ts(1)),
            logins(&["charlie", "alpha"])
        );
    }

    #[test]
    fn newly_available_spare_slot_starts_at_the_front_of_the_queue() {
        let mut rotation = WatchRotation::default();

        assert_eq!(
            rotation.select_with_campaigns(&logins(&["delta"]), &logins(&["delta"]), ts(0),),
            logins(&["delta"])
        );
        assert_eq!(
            rotation.select_with_campaigns(
                &logins(&["alpha", "bravo", "delta"]),
                &logins(&["delta"]),
                ts(1_000),
            ),
            logins(&["delta", "alpha"])
        );
    }
}
