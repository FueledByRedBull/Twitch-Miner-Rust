use super::*;

use crate::ledger::LedgerRecord;
use crate::observability::AppObservabilitySettings;

// 2026-11-01T00:00:00Z, 2026-11-02T00:00:00Z and noon on the 2nd.
const NOV_1: u64 = 1_793_491_200;
const NOV_2: u64 = 1_793_577_600;
const NOON_NOV_2: u64 = 1_793_620_800;
const HOUR: u64 = 3_600;
const TICK: u64 = CHECK_INTERVAL.as_secs();

fn check(window_points: i64, slots: Option<u32>, history: Option<f64>) -> PointsCheck {
    PointsCheck {
        window_points,
        selected_slots: slots,
        history_per_hour: history,
    }
}

#[test]
fn baseline_needs_72_nonzero_hours_and_uses_the_median() {
    assert_eq!(median_hourly_points(vec![300; 71]), None);
    assert_eq!(median_hourly_points(vec![300; 72]), Some(300.0));
    // Hours without points never count toward the 72, nor drag the median down.
    let mut mostly_idle = vec![0; 500];
    mostly_idle.extend(vec![300; 71]);
    assert_eq!(median_hourly_points(mostly_idle), None);

    let mut odd = vec![100; 36];
    odd.extend([200]);
    odd.extend(vec![900; 36]);
    assert_eq!(median_hourly_points(odd), Some(200.0));
    let mut even = vec![100; 36];
    even.extend(vec![300; 36]);
    assert_eq!(median_hourly_points(even), Some(200.0));
}

#[test]
fn points_alarm_fires_below_forty_percent_of_history() {
    // 2 hours at 100/hour against a usual 630/hour.
    assert_eq!(
        points_alarm_step(false, &check(200, Some(2), Some(630.0))),
        Some(Transition::Fired)
    );
    // 45% of normal is low but not an alarm.
    assert_eq!(
        points_alarm_step(false, &check(567, Some(2), Some(630.0))),
        None
    );
    // History wins over the slot count.
    assert_eq!(
        points_alarm_step(false, &check(700, Some(2), Some(1_000.0))),
        Some(Transition::Fired)
    );
}

#[test]
fn points_alarm_judges_a_lone_slot_against_its_share_of_history() {
    // One unsubscribed channel at 320/hour against a 1,280/hour two-slot history.
    assert_eq!(
        points_alarm_step(false, &check(640, Some(1), Some(1_280.0))),
        None
    );
    // A real stall on the lone slot still fires, and recovery on it can clear.
    assert_eq!(
        points_alarm_step(false, &check(100, Some(1), Some(1_280.0))),
        Some(Transition::Fired)
    );
    assert_eq!(
        points_alarm_step(true, &check(800, Some(1), Some(1_280.0))),
        Some(Transition::Cleared)
    );
}

#[test]
fn points_alarm_clears_only_above_sixty_percent() {
    let baseline = Some(1_000.0);
    // Between 40% and 60% an active alarm neither clears nor re-fires.
    assert_eq!(
        points_alarm_step(true, &check(900, Some(2), baseline)),
        None
    );
    assert_eq!(
        points_alarm_step(true, &check(1_200, Some(2), baseline)),
        None
    );
    assert_eq!(
        points_alarm_step(true, &check(1_300, Some(2), baseline)),
        Some(Transition::Cleared)
    );
    // An inactive alarm in the same band stays quiet.
    assert_eq!(
        points_alarm_step(false, &check(1_000, Some(2), baseline)),
        None
    );
}

#[test]
fn points_alarm_uses_the_fixed_baseline_per_slot_without_history() {
    // Two slots: 630/hour, so the fire line is 252/hour (504 points in two hours).
    assert_eq!(
        points_alarm_step(false, &check(503, Some(2), None)),
        Some(Transition::Fired)
    );
    assert_eq!(points_alarm_step(false, &check(506, Some(2), None)), None);
    // One slot: 315/hour, so 252 points over two hours is 126/hour, the fire line.
    assert_eq!(points_alarm_step(false, &check(252, Some(1), None)), None);
    assert_eq!(
        points_alarm_step(false, &check(251, Some(1), None)),
        Some(Transition::Fired)
    );
    assert_eq!(
        points_alarm_step(true, &check(800, Some(2), None)),
        Some(Transition::Cleared)
    );
}

#[test]
fn points_alarm_is_silent_without_a_continuously_selected_slot() {
    assert_eq!(points_alarm_step(false, &check(0, None, Some(630.0))), None);
    assert_eq!(points_alarm_step(true, &check(10_000, None, None)), None);
}

#[test]
fn slot_samples_need_the_whole_window_with_a_slot_selected() {
    let mut samples = SlotSamples::default();
    let start = NOV_2;
    let mut now = start;
    // Nothing is known before the window has been observed in full.
    for _ in 0..(POINTS_WINDOW_SECONDS / TICK) {
        samples.record(now, 2);
        now += TICK;
        assert_eq!(samples.selected_for_whole_window(now - TICK), None);
    }
    samples.record(now, 2);
    assert_eq!(samples.selected_for_whole_window(now), Some(2));

    // The fewest slots over the window is what counts.
    now += TICK;
    samples.record(now, 1);
    now += TICK;
    samples.record(now, 2);
    assert_eq!(samples.selected_for_whole_window(now), Some(1));

    // A check with nothing selected blocks evaluation until it leaves the window.
    now += TICK;
    samples.record(now, 0);
    assert_eq!(samples.selected_for_whole_window(now), None);
    let blocked_until = now + POINTS_WINDOW_SECONDS + TICK;
    while now < blocked_until {
        now += TICK;
        samples.record(now, 2);
    }
    assert_eq!(samples.selected_for_whole_window(now), Some(2));
}

#[test]
fn slot_samples_restart_after_a_suspension() {
    let mut samples = SlotSamples::default();
    let mut now = NOV_2;
    for _ in 0..30 {
        samples.record(now, 2);
        now += TICK;
    }
    assert_eq!(samples.selected_for_whole_window(now), Some(2));
    // The machine slept for an hour: the old samples say nothing about the gap.
    now += HOUR;
    samples.record(now, 2);
    assert_eq!(samples.selected_for_whole_window(now), None);
}

#[test]
fn drops_alarm_fires_after_three_selected_hours_without_movement() {
    let mut state = DropWatch::default();
    assert_eq!(drops_alarm_step(&mut state, 0, true, Some(40)), None);
    let ticks = DROP_SELECTED_SECONDS / TICK;
    for tick in 1..=ticks {
        let step = drops_alarm_step(&mut state, TICK, true, Some(40));
        if tick < ticks {
            assert_eq!(step, None, "tick {tick}");
        } else {
            assert_eq!(step, Some(Transition::Fired));
        }
    }
    assert!(state.active);
    // It fires once, not on every later check.
    assert_eq!(drops_alarm_step(&mut state, TICK, true, Some(40)), None);
}

#[test]
fn drops_alarm_counts_only_selected_time_and_cumulates() {
    let mut state = DropWatch::default();
    assert_eq!(drops_alarm_step(&mut state, 0, false, Some(40)), None);
    // Two hours selected, a long stretch unselected, then another hour selected.
    for _ in 0..(2 * HOUR / TICK) {
        assert_eq!(drops_alarm_step(&mut state, TICK, true, Some(40)), None);
    }
    for _ in 0..50 {
        assert_eq!(drops_alarm_step(&mut state, TICK, false, Some(40)), None);
    }
    let mut fired = None;
    for _ in 0..(HOUR / TICK) {
        fired = drops_alarm_step(&mut state, TICK, true, Some(40));
    }
    assert_eq!(fired, Some(Transition::Fired));
}

#[test]
fn drops_alarm_resets_and_clears_on_any_movement() {
    let mut state = DropWatch::default();
    drops_alarm_step(&mut state, 0, true, Some(40));
    for _ in 0..(2 * HOUR / TICK) {
        drops_alarm_step(&mut state, TICK, true, Some(40));
    }
    // Progress just before the line starts the three hours over.
    assert_eq!(drops_alarm_step(&mut state, TICK, true, Some(41)), None);
    for _ in 0..(2 * HOUR / TICK) {
        assert_eq!(drops_alarm_step(&mut state, TICK, true, Some(41)), None);
    }

    let mut state = DropWatch::default();
    drops_alarm_step(&mut state, 0, true, Some(40));
    for _ in 0..(DROP_SELECTED_SECONDS / TICK) {
        drops_alarm_step(&mut state, TICK, true, Some(40));
    }
    assert!(state.active);
    assert_eq!(
        drops_alarm_step(&mut state, TICK, true, Some(41)),
        Some(Transition::Cleared)
    );
    assert!(!state.active);
    assert_eq!(drops_alarm_step(&mut state, TICK, true, Some(41)), None);
}

#[test]
fn drops_alarm_ignores_unknown_inventory_and_suspended_time() {
    let mut state = DropWatch::default();
    for _ in 0..100 {
        assert_eq!(drops_alarm_step(&mut state, TICK, true, None), None);
    }
    assert!(!state.active);

    // A single long gap adds at most two intervals, not three hours.
    drops_alarm_step(&mut state, 0, true, Some(5));
    assert_eq!(drops_alarm_step(&mut state, 5 * HOUR, true, Some(5)), None);
    assert!(!state.active);
}

#[test]
fn report_day_is_yesterday_and_only_due_once() {
    let utc = Some(chrono_tz::UTC);
    let due = report_day_due(NOON_NOV_2, utc, None).unwrap();
    assert_eq!(
        due,
        ReportDay {
            label: String::from("2026-11-01"),
            start: NOV_1,
            end: NOV_2,
        }
    );
    assert!(report_day_due(NOON_NOV_2, utc, Some("2026-11-01")).is_none());
    assert!(report_day_due(NOON_NOV_2, utc, Some("2026-11-02")).is_none());
    // Days missed while the miner was off are skipped; only yesterday is reported.
    assert_eq!(
        report_day_due(NOON_NOV_2, utc, Some("2026-10-20"))
            .unwrap()
            .label,
        "2026-11-01"
    );
    // Nothing new is due until local midnight passes.
    assert!(report_day_due(NOV_2 - 1, utc, Some("2026-10-31")).is_none());
    assert_eq!(
        report_day_due(NOV_2, utc, Some("2026-10-31"))
            .unwrap()
            .label,
        "2026-11-01"
    );
}

#[test]
fn report_day_follows_the_configured_timezone() {
    let athens: Tz = "Europe/Athens".parse().unwrap();
    // 2026-11-01T23:30:00Z is already Nov 2 01:30 in Athens (UTC+2), so Nov 1 is yesterday.
    let now = NOV_2 - 30 * 60;
    let day = report_day_due(now, Some(athens), None).unwrap();
    assert_eq!(day.label, "2026-11-01");
    assert_eq!(day.start, NOV_1 - 2 * HOUR);
    assert_eq!(day.end, NOV_2 - 2 * HOUR);
    // In UTC the same instant is still Nov 1, so yesterday is Oct 31.
    assert_eq!(
        report_day_due(now, Some(chrono_tz::UTC), None)
            .unwrap()
            .label,
        "2026-10-31"
    );
}

#[test]
fn report_marker_survives_a_restart_and_ignores_garbage() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_report_marker(dir.path()), None);
    write_report_marker(dir.path(), "2026-11-01").unwrap();
    // A new process reads the marker back and does not report the day again.
    let marker = read_report_marker(dir.path());
    assert_eq!(marker.as_deref(), Some("2026-11-01"));
    assert!(report_day_due(NOON_NOV_2, Some(chrono_tz::UTC), marker.as_deref()).is_none());
    // The next local day is due again.
    assert!(report_day_due(
        NOON_NOV_2 + 24 * HOUR,
        Some(chrono_tz::UTC),
        marker.as_deref()
    )
    .is_some());

    fs::write(marker_path(dir.path()), "2026-1").unwrap();
    assert_eq!(read_report_marker(dir.path()), None);
    fs::write(marker_path(dir.path()), "not-a-date").unwrap();
    assert_eq!(read_report_marker(dir.path()), None);
}

fn write_ledger(dir: &Path, records: &[LedgerRecord]) {
    fs::create_dir_all(dir.join(LEDGER_DIR)).unwrap();
    let mut text = String::new();
    for record in records {
        text.push_str(&serde_json::to_string(record).unwrap());
        text.push('\n');
    }
    fs::write(dir.join(LEDGER_DIR).join("2026-11.jsonl"), text).unwrap();
}

fn reward(ts: u64, login: &str, reason: &str, points: i64) -> LedgerRecord {
    LedgerRecord::new(ts, "id", login, reason, points, None, None)
}

#[test]
fn daily_report_totals_yesterday_by_reason_and_top_three_channels() {
    let dir = tempfile::tempdir().unwrap();
    write_ledger(
        dir.path(),
        &[
            reward(NOV_1 - 1, "before", "WATCH", 999),
            reward(NOV_1 + 10, "alice", "WATCH", 10),
            reward(NOV_1 + 20, "alice", "WATCH", 10),
            reward(NOV_1 + 30, "bob", "CLAIM", 50),
            reward(NOV_1 + 40, "carol", "WATCH_STREAK", 300),
            reward(NOV_1 + 50, "dave", "WATCH", 5),
            reward(NOV_2, "after", "WATCH", 999),
        ],
    );
    let day = report_day_due(NOON_NOV_2, Some(chrono_tz::UTC), None).unwrap();
    let report = build_daily_report(dir.path(), &day).unwrap();
    assert_eq!(report.label, "2026-11-01");
    assert_eq!(report.total_points, 375);
    assert_eq!(report.rewards, 5);
    assert_eq!(
        report.by_reason,
        vec![
            (String::from("WATCH_STREAK"), 300),
            (String::from("CLAIM"), 50),
            (String::from("WATCH"), 25),
        ]
    );
    assert_eq!(
        report.top_channels,
        vec![
            (String::from("carol"), 300),
            (String::from("bob"), 50),
            (String::from("alice"), 20),
        ]
    );
}

#[cfg(unix)]
#[test]
fn report_marker_creates_the_ledger_directory_privately() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    write_report_marker(dir.path(), "2026-11-01").unwrap();
    let mode = fs::metadata(dir.path().join(LEDGER_DIR))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

fn context_for(dir: &Path, anonymize: bool) -> EarningsWatchContext {
    EarningsWatchContext {
        observability: AppObservability::new(
            None,
            tm_observability::DiscordClient::new(Duration::from_secs(1)).unwrap(),
            AppObservabilitySettings {
                anonymize_logs: anonymize,
                ..AppObservabilitySettings::default()
            },
        ),
        health: HealthTracker::default(),
        work_dir: dir.to_path_buf(),
        timezone: Some(chrono_tz::UTC),
    }
}

#[tokio::test]
async fn daily_report_is_marked_once_and_empty_days_send_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write_ledger(dir.path(), &[reward(NOV_1 + 10, "alice", "WATCH", 10)]);
    let context = context_for(dir.path(), false);

    let mut marker = None;
    send_daily_report_if_due(&context, NOON_NOV_2, Some(120), &mut marker).await;
    assert_eq!(marker.as_deref(), Some("2026-11-01"));
    assert_eq!(
        read_report_marker(dir.path()).as_deref(),
        Some("2026-11-01")
    );

    // Later checks the same day, and a restart reading the marker, do nothing.
    let marker_file = marker_path(dir.path());
    fs::remove_file(&marker_file).unwrap();
    send_daily_report_if_due(&context, NOON_NOV_2 + HOUR, None, &mut marker).await;
    assert!(!marker_file.exists());

    // The next day has no rewards: it is marked, but nothing would be sent.
    send_daily_report_if_due(&context, NOON_NOV_2 + 24 * HOUR, None, &mut marker).await;
    assert_eq!(marker.as_deref(), Some("2026-11-02"));
}

#[test]
fn daily_report_message_anonymizes_names_but_keeps_totals() {
    let report = DailyReport {
        label: String::from("2026-11-01"),
        total_points: 375,
        rewards: 5,
        by_reason: vec![(String::from("WATCH"), 375)],
        top_channels: vec![
            (String::from("secretstreamer"), 300),
            (String::from("other"), 75),
        ],
    };
    let dir = tempfile::tempdir().unwrap();
    let hidden = context_for(dir.path(), true)
        .observability
        .daily_report_message(&report, Some(90));
    assert!(!hidden.contains("secretstreamer"), "{hidden}");
    assert!(hidden.contains("Streamer1 300"), "{hidden}");
    assert!(hidden.contains("375 points from 5 rewards"), "{hidden}");
    assert!(
        hidden.contains("Unclaimed Drop minutes in inventory: 90"),
        "{hidden}"
    );

    let shown = context_for(dir.path(), false)
        .observability
        .daily_report_message(&report, None);
    assert!(shown.contains("secretstreamer 300, other 75"), "{shown}");
    assert!(!shown.contains("Drop minutes"));
}

#[test]
fn baseline_is_loaded_from_ledger_hours_outside_the_judged_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut records = Vec::new();
    // 80 hours of 300 points, ending well before the two-hour window.
    for hour in 0..80 {
        let ts = NOV_1 + hour * HOUR;
        records.push(reward(ts, "alice", "WATCH", 250));
        records.push(reward(ts + 60, "alice", "CLAIM", 50));
    }
    // The recent stalled hours must not pull the baseline down, and other
    // reasons never count.
    let now = NOV_1 + 100 * HOUR;
    records.push(reward(now - 10, "alice", "WATCH", 1));
    records.push(reward(NOV_1 + 100, "alice", "WATCH_STREAK", 99_999));
    write_ledger(dir.path(), &records);

    assert_eq!(load_baseline(dir.path(), now).unwrap(), Some(300.0));
    // Too little history falls back to the fixed per-slot figure.
    assert_eq!(load_baseline(dir.path(), NOV_1 + 30 * HOUR).unwrap(), None);
}
