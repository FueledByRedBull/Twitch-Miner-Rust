//! Silent-loss alarms and the daily report.
//!
//! The decisions are pure functions over injected numbers and time. The
//! supervised task only gathers samples from the health tracker and the
//! ledger, applies those functions, and sends the notifications.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use chrono_tz::Tz;

use crate::earnings::{summarize, EarningsGrouping};
use crate::ledger::{create_private_dir, read_range, LEDGER_DIR};
use crate::observability::AppObservability;
use crate::status::{unix_now_infallible, HealthTracker};
use crate::utilities::sleep_or_stop;

pub(crate) const TASK_NAME: &str = "earnings-watch";
pub(crate) const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

const POINTS_WINDOW_HOURS: f64 = 2.0;
const POINTS_WINDOW_SECONDS: u64 = 2 * 60 * 60;
const BASELINE_DAYS: u64 = 14;
const BASELINE_REFRESH_SECONDS: u64 = 60 * 60;
const MIN_BASELINE_HOURS: usize = 72;
const FALLBACK_POINTS_PER_SLOT_HOUR: f64 = 315.0;
const MAX_SELECTED_SLOTS: f64 = 2.0;
const FIRE_BELOW_FRACTION: f64 = 0.4;
const CLEAR_ABOVE_FRACTION: f64 = 0.6;
const DROP_SELECTED_SECONDS: u64 = 3 * 60 * 60;
// A tick this much later than expected means the process was suspended, so
// neither the watch window nor the selected-time counter can be trusted.
const MAX_SAMPLE_GAP_SECONDS: u64 = 2 * CHECK_INTERVAL.as_secs();
const DAILY_REPORT_MARKER: &str = "last-daily-report";
const TOP_CHANNELS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    Fired,
    Cleared,
}

/// Number of watch slots selected at each recent check.
#[derive(Debug, Default)]
pub(crate) struct SlotSamples {
    samples: VecDeque<(u64, u32)>,
}

impl SlotSamples {
    pub(crate) fn record(&mut self, now: u64, slots: u32) {
        if self
            .samples
            .back()
            .is_some_and(|(last, _)| now.saturating_sub(*last) > MAX_SAMPLE_GAP_SECONDS)
        {
            self.samples.clear();
        }
        self.samples.push_back((now, slots));
        while self
            .samples
            .front()
            .is_some_and(|(ts, _)| ts + POINTS_WINDOW_SECONDS + CHECK_INTERVAL.as_secs() < now)
        {
            self.samples.pop_front();
        }
    }

    /// The fewest slots selected at any check in the points window, or `None`
    /// when the samples do not span the whole window or a check saw none.
    pub(crate) fn selected_for_whole_window(&self, now: u64) -> Option<u32> {
        let (oldest, _) = self.samples.front()?;
        if now.saturating_sub(*oldest) < POINTS_WINDOW_SECONDS {
            return None;
        }
        self.samples
            .iter()
            .filter(|(ts, _)| ts + POINTS_WINDOW_SECONDS >= now)
            .map(|(_, slots)| *slots)
            .min()
            .filter(|slots| *slots > 0)
    }
}

/// The user's normal WATCH+CLAIM points per hour: the median over hours that
/// earned something, or `None` when there is too little history to trust.
pub(crate) fn median_hourly_points(mut hours: Vec<i64>) -> Option<f64> {
    hours.retain(|points| *points > 0);
    if hours.len() < MIN_BASELINE_HOURS {
        return None;
    }
    hours.sort_unstable();
    let middle = hours.len() / 2;
    let median = if hours.len().is_multiple_of(2) {
        f64::midpoint(
            points_as_f64(hours[middle - 1]),
            points_as_f64(hours[middle]),
        )
    } else {
        points_as_f64(hours[middle])
    };
    Some(median)
}

// Hourly point totals are far below 2^52, so the conversion is exact.
#[allow(clippy::cast_precision_loss)]
fn points_as_f64(points: i64) -> f64 {
    points as f64
}

pub(crate) struct PointsCheck {
    pub(crate) window_points: i64,
    /// Fewest slots selected across the whole window, if one was selected throughout.
    pub(crate) selected_slots: Option<u32>,
    pub(crate) history_per_hour: Option<f64>,
}

impl PointsCheck {
    pub(crate) fn baseline_per_hour(&self) -> Option<f64> {
        let slots = f64::from(self.selected_slots?);
        // History mostly reflects both slots, so a lone slot is judged against its share.
        Some(
            self.history_per_hour
                .map_or(FALLBACK_POINTS_PER_SLOT_HOUR * slots, |history| {
                    history * slots / MAX_SELECTED_SLOTS
                }),
        )
    }

    pub(crate) fn rate_per_hour(&self) -> f64 {
        points_as_f64(self.window_points) / POINTS_WINDOW_HOURS
    }
}

/// Fires below 40% of the baseline and clears above 60%, so a rate hovering
/// near the line does not flap. Without a fully selected window it changes nothing.
pub(crate) fn points_alarm_step(active: bool, check: &PointsCheck) -> Option<Transition> {
    let baseline = check.baseline_per_hour()?;
    let rate = check.rate_per_hour();
    if !active && rate < baseline * FIRE_BELOW_FRACTION {
        Some(Transition::Fired)
    } else if active && rate > baseline * CLEAR_ABOVE_FRACTION {
        Some(Transition::Cleared)
    } else {
        None
    }
}

#[derive(Debug, Default)]
pub(crate) struct DropWatch {
    selected_seconds: u64,
    last_minutes: Option<i64>,
    pub(crate) active: bool,
}

/// Fires once a Drop-target channel has been selected for three cumulative
/// hours while unclaimed Drop minutes never moved; any movement clears it.
/// A claimed or removed Drop changes the total too, which also counts as movement.
pub(crate) fn drops_alarm_step(
    state: &mut DropWatch,
    elapsed_seconds: u64,
    drop_target_selected: bool,
    unclaimed_minutes: Option<i64>,
) -> Option<Transition> {
    let minutes = unclaimed_minutes?;
    let moved = state.last_minutes.is_some_and(|last| last != minutes);
    state.last_minutes = Some(minutes);
    if moved {
        state.selected_seconds = 0;
        return std::mem::take(&mut state.active).then_some(Transition::Cleared);
    }
    if drop_target_selected {
        state.selected_seconds += elapsed_seconds.min(MAX_SAMPLE_GAP_SECONDS);
    }
    if !state.active && state.selected_seconds >= DROP_SELECTED_SECONDS {
        state.active = true;
        return Some(Transition::Fired);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReportDay {
    pub(crate) label: String,
    pub(crate) start: u64,
    pub(crate) end: u64,
}

/// Yesterday in the configured timezone, unless it was already reported.
/// Dates compare as text because `YYYY-MM-DD` sorts chronologically.
pub(crate) fn report_day_due(
    now: u64,
    timezone: Option<Tz>,
    marker: Option<&str>,
) -> Option<ReportDay> {
    let at = |unix: u64| UNIX_EPOCH + Duration::from_secs(unix);
    let (today_start, _) = tm_observability::local_day_bounds(at(now), timezone)?;
    let (start, end) =
        tm_observability::local_day_bounds(at(today_start.checked_sub(1)?), timezone)?;
    let label = tm_observability::format_timestamp(at(start), timezone, "%Y-%m-%d");
    marker
        .is_none_or(|reported| reported < label.as_str())
        .then_some(ReportDay { label, start, end })
}

fn marker_path(work_dir: &Path) -> PathBuf {
    work_dir.join(LEDGER_DIR).join(DAILY_REPORT_MARKER)
}

pub(crate) fn read_report_marker(work_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(marker_path(work_dir)).ok()?;
    let label = text.trim();
    let valid = label.len() == 10
        && label.bytes().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7) == (byte == b'-') && (byte == b'-' || byte.is_ascii_digit())
        });
    valid.then(|| label.to_string())
}

pub(crate) fn write_report_marker(work_dir: &Path, label: &str) -> std::io::Result<()> {
    let path = marker_path(work_dir);
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    fs::write(path, format!("{label}\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DailyReport {
    pub(crate) label: String,
    pub(crate) total_points: i64,
    pub(crate) rewards: u64,
    pub(crate) by_reason: Vec<(String, i64)>,
    /// Login (or channel id) and points, best first.
    pub(crate) top_channels: Vec<(String, i64)>,
}

pub(crate) fn build_daily_report(work_dir: &Path, day: &ReportDay) -> Result<DailyReport> {
    let by_reason = summarize(work_dir, day.start, day.end, EarningsGrouping::Reason)?;
    let by_channel = summarize(work_dir, day.start, day.end, EarningsGrouping::Channel)?;
    Ok(DailyReport {
        label: day.label.clone(),
        total_points: by_reason.total_points,
        rewards: by_reason.rewards,
        by_reason: by_reason
            .rows
            .into_iter()
            .map(|row| (row.key, row.points))
            .collect(),
        top_channels: by_channel
            .rows
            .into_iter()
            .take(TOP_CHANNELS)
            .map(|row| (row.key, row.points))
            .collect(),
    })
}

fn load_baseline(work_dir: &Path, now: u64) -> Result<Option<f64>> {
    // Leave out the window being judged so a stall cannot lower its own baseline.
    let until = now.saturating_sub(POINTS_WINDOW_SECONDS);
    let since = until.saturating_sub(BASELINE_DAYS * 24 * 60 * 60);
    let mut hours = BTreeMap::<u64, i64>::new();
    read_range(work_dir, since, until, |record| {
        if matches!(record.reason.as_str(), "WATCH" | "CLAIM") {
            let hour = hours.entry(record.ts / 3_600).or_default();
            *hour = hour.saturating_add(record.points);
        }
    })?;
    Ok(median_hourly_points(hours.into_values().collect()))
}

async fn refresh_baseline(work_dir: &Path, now: u64, previous: Option<f64>) -> Option<f64> {
    let work_dir = work_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || load_baseline(&work_dir, now)).await {
        Ok(Ok(baseline)) => baseline,
        Ok(Err(error)) => {
            tracing::warn!(task = TASK_NAME, error_class = "ledger-read", %error, "earnings baseline unavailable");
            previous
        }
        Err(error) => {
            tracing::warn!(task = TASK_NAME, error_class = "ledger-read", %error, "earnings baseline task failed");
            previous
        }
    }
}

pub(crate) struct EarningsWatchContext {
    pub(crate) observability: AppObservability,
    pub(crate) health: HealthTracker,
    pub(crate) work_dir: PathBuf,
    pub(crate) timezone: Option<Tz>,
}

pub(crate) fn spawn_earnings_watch_loop(
    mut stop: tokio::sync::watch::Receiver<bool>,
    context: EarningsWatchContext,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let tracker = context.health.earnings();
        let mut slots = SlotSamples::default();
        let mut drops = DropWatch::default();
        let mut points_active = false;
        let mut baseline = None;
        let mut baseline_at = None::<u64>;
        let mut last_tick = unix_now_infallible();
        let mut marker = read_report_marker(&context.work_dir);
        loop {
            let now = unix_now_infallible();
            if baseline_at.is_none_or(|at| now.saturating_sub(at) >= BASELINE_REFRESH_SECONDS) {
                baseline = refresh_baseline(&context.work_dir, now, baseline).await;
                baseline_at = Some(now);
            }

            let selected = context.health.selected_watch_slots();
            slots.record(now, selected);
            let check = PointsCheck {
                window_points: tracker
                    .watch_claim_points_since(now.saturating_sub(POINTS_WINDOW_SECONDS)),
                selected_slots: slots.selected_for_whole_window(now),
                history_per_hour: baseline,
            };
            if let Some(transition) = points_alarm_step(points_active, &check) {
                points_active = transition == Transition::Fired;
                notify_points(
                    &context.observability,
                    transition,
                    &check,
                    baseline.is_some(),
                );
            }

            let minutes = context.health.unclaimed_drop_minutes();
            let elapsed = now.saturating_sub(last_tick);
            if let Some(transition) = drops_alarm_step(
                &mut drops,
                elapsed,
                context.health.drop_target_selected(),
                minutes,
            ) {
                notify_drops(&context.observability, transition);
            }
            tracker.set_alarms(points_active, drops.active);
            last_tick = now;

            send_daily_report_if_due(&context, now, minutes, &mut marker).await;
            context.health.success(TASK_NAME);
            if sleep_or_stop(&mut stop, CHECK_INTERVAL).await {
                return;
            }
        }
    })
}

fn notify_points(
    observability: &AppObservability,
    transition: Transition,
    check: &PointsCheck,
    from_history: bool,
) {
    let baseline = check.baseline_per_hour().unwrap_or_default();
    let rate = check.rate_per_hour();
    match transition {
        Transition::Fired => {
            tracing::warn!(
                task = TASK_NAME,
                error_class = "earnings-stall",
                "points earnings fell below 40% of the usual rate"
            );
            observability.spawn_event(
                tm_observability::Event::EarningsStall,
                observability.earnings_stall_message(
                    check.window_points,
                    rate,
                    baseline,
                    from_history,
                ),
            );
        }
        Transition::Cleared => {
            tracing::info!(task = TASK_NAME, "points earnings recovered");
            observability.spawn_event(
                tm_observability::Event::EarningsStall,
                observability.earnings_recovered_message(check.window_points, rate, baseline),
            );
        }
    }
}

fn notify_drops(observability: &AppObservability, transition: Transition) {
    match transition {
        Transition::Fired => {
            tracing::warn!(
                task = TASK_NAME,
                error_class = "drops-stall",
                "no Drop minutes advanced while a Drop channel was watched for three hours"
            );
            observability.spawn_event(
                tm_observability::Event::DropsStall,
                observability.drops_stall_message(),
            );
        }
        Transition::Cleared => {
            tracing::info!(task = TASK_NAME, "Drop minutes are advancing again");
            observability.spawn_event(
                tm_observability::Event::DropsStall,
                observability.drops_recovered_message(),
            );
        }
    }
}

async fn send_daily_report_if_due(
    context: &EarningsWatchContext,
    now: u64,
    drop_minutes: Option<i64>,
    marker: &mut Option<String>,
) {
    let Some(day) = report_day_due(now, context.timezone, marker.as_deref()) else {
        return;
    };
    let work_dir = context.work_dir.clone();
    let report_day = day.clone();
    let report = match tokio::task::spawn_blocking(move || {
        build_daily_report(&work_dir, &report_day)
    })
    .await
    {
        Ok(Ok(report)) => report,
        Ok(Err(error)) => {
            tracing::warn!(task = TASK_NAME, error_class = "ledger-read", %error, "daily report unavailable; retrying at the next check");
            return;
        }
        Err(error) => {
            tracing::warn!(task = TASK_NAME, error_class = "ledger-read", %error, "daily report task failed; retrying at the next check");
            return;
        }
    };
    // Record the day before delivering so a restart never repeats it, and keep
    // it in memory in case the marker cannot be written.
    *marker = Some(day.label.clone());
    if let Err(error) = write_report_marker(&context.work_dir, &day.label) {
        tracing::warn!(task = TASK_NAME, error_class = "marker-write", %error, "daily report marker could not be saved");
    }
    if report.rewards == 0 {
        return;
    }
    let message = context
        .observability
        .daily_report_message(&report, drop_minutes);
    tracing::info!(operation = "daily_report", "{message}");
    context
        .observability
        .spawn_event(tm_observability::Event::DailyReport, message);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/alarms_tests.rs"]
mod tests;
