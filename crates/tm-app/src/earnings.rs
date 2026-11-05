//! Earnings views over the ledger: the live tracker behind `--status`, and the
//! `--earnings` command's aggregation and rendering.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::ledger::{read_range, LedgerRecord};

/// Rewards kept in memory for the silent-loss alarm.
const RECENT_RETENTION_SECONDS: u64 = 3 * 60 * 60;
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// The `--status` earnings block. Older status files without it still load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EarningsStatus {
    /// Rewards accepted since this process started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_points: Option<i64>,
    /// Rewards accepted today, in the configured timezone, across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) today_points: Option<i64>,
    /// Rewards lost because the ledger queue was full or a write failed.
    #[serde(default)]
    pub(crate) ledger_dropped_writes: u64,
    /// Ledger writes that failed with an I/O error.
    #[serde(default)]
    pub(crate) ledger_write_failures: u64,
    #[serde(default)]
    pub(crate) earnings_stall: bool,
    #[serde(default)]
    pub(crate) drops_stall: bool,
}

impl EarningsStatus {
    /// Support files carry ledger health but never point totals.
    pub(crate) fn without_points(mut self) -> Self {
        self.session_points = None;
        self.today_points = None;
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DayTotal {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) points: i64,
}

#[derive(Default)]
struct TrackerState {
    timezone: Option<Tz>,
    session_points: i64,
    today: DayTotal,
    recent: VecDeque<(u64, i64)>,
    dropped_writes: u64,
    write_failures: u64,
    earnings_stall: bool,
    drops_stall: bool,
}

/// In-memory view of this session's rewards, shared by the ledger, the
/// silent-loss alarm and the status file. It never decides anything about
/// mining state.
#[derive(Clone, Default)]
pub(crate) struct EarningsTracker {
    state: Arc<Mutex<TrackerState>>,
}

impl EarningsTracker {
    pub(crate) fn configure(&self, timezone: Option<Tz>, today: DayTotal) {
        let mut state = self.lock();
        state.timezone = timezone;
        state.today = today;
    }

    pub(crate) fn note_reward(&self, record: &LedgerRecord) {
        let mut state = self.lock();
        state.session_points = state.session_points.saturating_add(record.points);
        if record.ts >= state.today.end {
            let bounds = tm_observability::local_day_bounds(
                UNIX_EPOCH + Duration::from_secs(record.ts),
                state.timezone,
            );
            if let Some((start, end)) = bounds {
                state.today = DayTotal {
                    start,
                    end,
                    points: 0,
                };
            }
        }
        if (state.today.start..state.today.end).contains(&record.ts) {
            state.today.points = state.today.points.saturating_add(record.points);
        }
        if matches!(record.reason.as_str(), "WATCH" | "CLAIM") {
            state.recent.push_back((record.ts, record.points));
            while state
                .recent
                .front()
                .is_some_and(|(ts, _)| ts + RECENT_RETENTION_SECONDS < record.ts)
            {
                state.recent.pop_front();
            }
        }
    }

    pub(crate) fn note_dropped(&self, count: u64) {
        let mut state = self.lock();
        state.dropped_writes = state.dropped_writes.saturating_add(count);
    }

    pub(crate) fn note_write_failures(&self, count: u64) {
        let mut state = self.lock();
        state.write_failures = state.write_failures.saturating_add(count);
        state.dropped_writes = state.dropped_writes.saturating_add(count);
    }

    /// WATCH and CLAIM points accepted at or after `since`.
    pub(crate) fn watch_claim_points_since(&self, since: u64) -> i64 {
        self.lock()
            .recent
            .iter()
            .filter(|(ts, _)| *ts >= since)
            .map(|(_, points)| *points)
            .sum()
    }

    pub(crate) fn set_alarms(&self, earnings_stall: bool, drops_stall: bool) {
        let mut state = self.lock();
        state.earnings_stall = earnings_stall;
        state.drops_stall = drops_stall;
    }

    pub(crate) fn status(&self, now: u64) -> EarningsStatus {
        let state = self.lock();
        let today = if (state.today.start..state.today.end).contains(&now) {
            state.today.points
        } else {
            0
        };
        EarningsStatus {
            session_points: Some(state.session_points),
            today_points: Some(today),
            ledger_dropped_writes: state.dropped_writes,
            ledger_write_failures: state.write_failures,
            earnings_stall: state.earnings_stall,
            drops_stall: state.drops_stall,
        }
    }

    fn lock(&self) -> MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Points already in the ledger for the local day containing `now`, so the
/// status block survives restarts.
pub(crate) fn today_total(work_dir: &Path, now: u64, timezone: Option<Tz>) -> Result<DayTotal> {
    let Some((start, end)) =
        tm_observability::local_day_bounds(UNIX_EPOCH + Duration::from_secs(now), timezone)
    else {
        return Ok(DayTotal::default());
    };
    let mut points = 0_i64;
    read_range(work_dir, start, end, |record| {
        points = points.saturating_add(record.points);
    })?;
    Ok(DayTotal { start, end, points })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum EarningsGrouping {
    Day,
    Channel,
    Reason,
}

impl EarningsGrouping {
    const fn label(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Channel => "channel",
            Self::Reason => "reason",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EarningsRow {
    pub(crate) key: String,
    pub(crate) points: i64,
    pub(crate) rewards: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EarningsSummary {
    pub(crate) since: u64,
    pub(crate) until: u64,
    pub(crate) first_reward: Option<u64>,
    pub(crate) total_points: i64,
    pub(crate) rewards: u64,
    pub(crate) skipped_lines: u64,
    pub(crate) rows: Vec<EarningsRow>,
}

impl EarningsSummary {
    /// Average per hour from the first reward in the window (or the window
    /// start), so a miner that ran for a day is not diluted across a week.
    pub(crate) fn points_per_hour(&self) -> f64 {
        let start = self
            .first_reward
            .map_or(self.since, |ts| ts.max(self.since));
        let seconds = self.until.saturating_sub(start).max(3_600);
        #[allow(clippy::cast_precision_loss)]
        let (points, seconds) = (self.total_points as f64, seconds as f64);
        points * 3_600.0 / seconds
    }
}

/// UTC calendar day, `YYYY-MM-DD`.
pub(crate) fn utc_day_label(ts: u64) -> String {
    i64::try_from(ts)
        .ok()
        .and_then(|ts| tm_runtime::RuntimeTime::from_unix_timestamp(ts).ok())
        .map_or_else(
            || String::from("unknown"),
            |time| {
                format!(
                    "{:04}-{:02}-{:02}",
                    time.year(),
                    u8::from(time.month()),
                    time.day()
                )
            },
        )
}

pub(crate) fn summarize(
    work_dir: &Path,
    since: u64,
    until: u64,
    by: EarningsGrouping,
) -> Result<EarningsSummary> {
    let mut groups = BTreeMap::<String, (i64, u64)>::new();
    let mut total_points = 0_i64;
    let mut rewards = 0_u64;
    let mut first_reward = None::<u64>;
    let read = read_range(work_dir, since, until, |record| {
        total_points = total_points.saturating_add(record.points);
        rewards += 1;
        first_reward = Some(first_reward.map_or(record.ts, |first| first.min(record.ts)));
        let key = match by {
            EarningsGrouping::Day => utc_day_label(record.ts),
            EarningsGrouping::Channel if record.login.is_empty() => record.channel_id,
            EarningsGrouping::Channel => record.login,
            EarningsGrouping::Reason => record.reason,
        };
        let entry = groups.entry(key).or_default();
        entry.0 = entry.0.saturating_add(record.points);
        entry.1 += 1;
    })?;
    let mut rows = groups
        .into_iter()
        .map(|(key, (points, rewards))| EarningsRow {
            key,
            points,
            rewards,
        })
        .collect::<Vec<_>>();
    if by != EarningsGrouping::Day {
        rows.sort_by(|left, right| {
            right
                .points
                .cmp(&left.points)
                .then_with(|| left.key.cmp(&right.key))
        });
    }
    Ok(EarningsSummary {
        since,
        until,
        first_reward,
        total_points,
        rewards,
        skipped_lines: read.skipped_lines,
        rows,
    })
}

fn render_text(summary: &EarningsSummary, days: u32, by: EarningsGrouping) -> String {
    let mut output = format!(
        "Earnings for the last {days} day(s), up to {} UTC\nTotal: {} points from {} rewards ({:.1} points/hour)\n",
        utc_day_label(summary.until),
        summary.total_points,
        summary.rewards,
        summary.points_per_hour()
    );
    if summary.rewards == 0 {
        output.push_str("No rewards recorded in this period.\n");
    } else {
        let heading = match by {
            EarningsGrouping::Day => "By day (UTC):",
            EarningsGrouping::Channel => "By channel:",
            EarningsGrouping::Reason => "By reason:",
        };
        output.push_str(heading);
        output.push('\n');
        let width = summary
            .rows
            .iter()
            .map(|row| row.key.chars().count())
            .max()
            .unwrap_or_default();
        for row in &summary.rows {
            let _ = writeln!(
                output,
                "  {:<width$}  {:>10}  ({} rewards)",
                row.key, row.points, row.rewards
            );
        }
    }
    if summary.skipped_lines > 0 {
        let _ = writeln!(
            output,
            "Skipped {} unreadable ledger line(s).",
            summary.skipped_lines
        );
    }
    output
}

fn render_json(summary: &EarningsSummary, days: u32, by: EarningsGrouping) -> serde_json::Value {
    serde_json::json!({
        "days": days,
        "by": by.label(),
        "from_unix": summary.since,
        "to_unix": summary.until,
        "total_points": summary.total_points,
        "rewards": summary.rewards,
        "points_per_hour": summary.points_per_hour(),
        "skipped_lines": summary.skipped_lines,
        "rows": summary
            .rows
            .iter()
            .map(|row| serde_json::json!({
                "key": row.key,
                "points": row.points,
                "rewards": row.rewards,
            }))
            .collect::<Vec<_>>(),
    })
}

pub(crate) fn earnings_output(
    work_dir: &Path,
    now: u64,
    days: u32,
    by: EarningsGrouping,
    json: bool,
) -> Result<String> {
    let since = now.saturating_sub(u64::from(days) * SECONDS_PER_DAY);
    let summary = summarize(work_dir, since, now.saturating_add(1), by)?;
    if json {
        Ok(serde_json::to_string_pretty(&render_json(
            &summary, days, by,
        ))?)
    } else {
        Ok(render_text(&summary, days, by))
    }
}

pub(crate) fn print_earnings(
    work_dir: &Path,
    days: u32,
    by: EarningsGrouping,
    json: bool,
) -> Result<()> {
    let now = crate::status::unix_now_infallible();
    println!(
        "{}",
        earnings_output(work_dir, now, days, by, json)?.trim_end()
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/earnings_tests.rs"]
mod tests;
