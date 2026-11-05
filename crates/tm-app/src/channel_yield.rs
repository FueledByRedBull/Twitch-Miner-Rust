//! Per-channel yield tiers read back from the earnings ledger.
//!
//! This is the one place the miner reads the ledger to influence mining, and
//! the influence is the last tie-break in [`tm_domain::ChannelScore`]: it
//! chooses between waiting channels whose streak, Drops and multiplier all tie.
//! Without a ledger, or when it cannot be read, every channel is tier 0, which
//! is the same as the score without the field.
//!
//! WATCH and CLAIM points arrive at a fixed rate whichever channels are
//! watched, so only the rewards that vary (streaks, raids and so on) say which
//! channels pay more. The tiers are deliberately coarse so real ties remain.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::ledger::read_range;
use crate::status::unix_now_infallible;

const WINDOW_SECONDS: u64 = 14 * 24 * 60 * 60;
const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Channel ids in the top tier. Every other channel is tier 0.
type TopTier = HashSet<String>;

/// Sums, per channel id, the points of every reward other than WATCH and CLAIM
/// over the 14 days before `now`.
fn varying_sums(work_dir: &std::path::Path, now: u64) -> Result<HashMap<String, i64>> {
    let mut sums = HashMap::<String, i64>::new();
    read_range(
        work_dir,
        now.saturating_sub(WINDOW_SECONDS),
        now.saturating_add(1),
        |record| {
            if !matches!(record.reason.as_str(), "WATCH" | "CLAIM") && !record.channel_id.is_empty()
            {
                let sum = sums.entry(record.channel_id).or_default();
                *sum = sum.saturating_add(record.points);
            }
        },
    )?;
    Ok(sums)
}

/// Tier 1 is the top quartile of the channels with a positive sum, rounded up
/// and including channels that tie with the last one in it.
fn top_tier(sums: HashMap<String, i64>) -> TopTier {
    let mut positive = sums
        .values()
        .copied()
        .filter(|sum| *sum > 0)
        .collect::<Vec<_>>();
    positive.sort_unstable_by(|left, right| right.cmp(left));
    let Some(threshold) = positive
        .len()
        .div_ceil(4)
        .checked_sub(1)
        .and_then(|last| positive.get(last).copied())
    else {
        return TopTier::new();
    };
    sums.into_iter()
        .filter(|(_, sum)| *sum >= threshold)
        .map(|(channel_id, _)| channel_id)
        .collect()
}

/// The tiers for the minute watcher, refreshed at most hourly in the background.
#[derive(Default)]
pub(crate) struct YieldTiers {
    /// Without a data directory nothing is read and every channel is tier 0.
    work_dir: Option<PathBuf>,
    top: TopTier,
    next_refresh: Option<Instant>,
    pending: Option<tokio::task::JoinHandle<Result<TopTier>>>,
    failing: bool,
}

impl YieldTiers {
    pub(crate) fn new(work_dir: PathBuf) -> Self {
        Self {
            work_dir: Some(work_dir),
            ..Self::default()
        }
    }

    /// 1 for a channel in the top tier, else 0.
    pub(crate) fn tier(&self, channel_id: &str) -> u8 {
        u8::from(self.top.contains(channel_id))
    }

    /// Takes the result of a finished read and starts the next one when due.
    /// The read runs on a blocking thread, so this never waits for the disk.
    pub(crate) async fn refresh(&mut self, now: Instant) {
        if self
            .pending
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(read) = self.pending.take() {
                self.accept(read.await);
            }
        }
        let Some(work_dir) = self.work_dir.clone() else {
            return;
        };
        if self.pending.is_none() && self.next_refresh.is_none_or(|due| now >= due) {
            self.next_refresh = Some(now + REFRESH_INTERVAL);
            self.pending = Some(tokio::task::spawn_blocking(move || {
                varying_sums(&work_dir, unix_now_infallible()).map(top_tier)
            }));
        }
    }

    /// A failed read keeps the previous tiers and warns once until a read works.
    fn accept(&mut self, read: Result<Result<TopTier>, tokio::task::JoinError>) {
        match read.map_err(anyhow::Error::from).and_then(|read| read) {
            Ok(top) => {
                self.top = top;
                self.failing = false;
            }
            Err(error) => {
                if !std::mem::replace(&mut self.failing, true) {
                    tracing::warn!(
                        task = "minute-watch",
                        error_class = "ledger-read",
                        %error,
                        "could not read the earnings ledger for channel ranking; keeping the previous tiers"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/channel_yield_tests.rs"]
mod tests;
