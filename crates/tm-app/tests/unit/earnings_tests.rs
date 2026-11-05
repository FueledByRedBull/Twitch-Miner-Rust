use super::*;

use std::fs;

// 2026-11-01T00:00:00Z, 2026-11-02T00:00:00Z and noon on the 2nd.
const NOV_1: u64 = 1_793_491_200;
const NOV_2: u64 = 1_793_577_600;
const NOW: u64 = 1_793_620_800;
const HOUR: u64 = 3_600;

fn reward(ts: u64, login: &str, reason: &str, points: i64) -> LedgerRecord {
    LedgerRecord::new(ts, "id", login, reason, points, None, None)
}

fn write_month(dir: &Path, month: &str, records: &[LedgerRecord], tail: &str) {
    fs::create_dir_all(dir.join(crate::ledger::LEDGER_DIR)).unwrap();
    let mut text = String::new();
    for record in records {
        text.push_str(&serde_json::to_string(record).unwrap());
        text.push('\n');
    }
    text.push_str(tail);
    fs::write(
        dir.join(crate::ledger::LEDGER_DIR)
            .join(format!("{month}.jsonl")),
        text,
    )
    .unwrap();
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_month(
        dir.path(),
        "2026-10",
        &[reward(NOV_1 - HOUR, "alice", "WATCH", 10)],
        "",
    );
    write_month(
        dir.path(),
        "2026-11",
        &[
            reward(NOV_1 + HOUR, "alice", "WATCH", 10),
            reward(NOV_1 + 2 * HOUR, "bob", "CLAIM", 50),
            reward(NOV_2 + HOUR, "alice", "WATCH", 10),
            reward(NOV_2 + 2 * HOUR, "alice", "WATCH_STREAK", 300),
            reward(NOV_2 + 3 * HOUR, "bob", "WATCH", 10),
        ],
        "",
    );
    dir
}

fn rows(summary: &EarningsSummary) -> Vec<(&str, i64, u64)> {
    summary
        .rows
        .iter()
        .map(|row| (row.key.as_str(), row.points, row.rewards))
        .collect()
}

#[test]
fn groups_by_utc_day_oldest_first() {
    let dir = fixture();
    let summary = summarize(dir.path(), NOV_1 - 2 * HOUR, NOW, EarningsGrouping::Day).unwrap();
    assert_eq!(summary.total_points, 390);
    assert_eq!(summary.rewards, 6);
    assert_eq!(
        rows(&summary),
        vec![
            ("2026-10-31", 10, 1),
            ("2026-11-01", 60, 2),
            ("2026-11-02", 320, 3),
        ]
    );
}

#[test]
fn groups_by_channel_best_first() {
    let dir = fixture();
    let summary = summarize(dir.path(), NOV_1, NOW, EarningsGrouping::Channel).unwrap();
    assert_eq!(rows(&summary), vec![("alice", 320, 3), ("bob", 60, 2)]);
}

#[test]
fn groups_by_reason_best_first() {
    let dir = fixture();
    let summary = summarize(dir.path(), NOV_1, NOW, EarningsGrouping::Reason).unwrap();
    assert_eq!(
        rows(&summary),
        vec![("WATCH_STREAK", 300, 1), ("CLAIM", 50, 1), ("WATCH", 30, 3)]
    );
}

#[test]
fn window_excludes_older_rewards_and_ranks_ties_by_name() {
    let dir = tempfile::tempdir().unwrap();
    write_month(
        dir.path(),
        "2026-11",
        &[
            reward(NOV_2, "zed", "WATCH", 10),
            reward(NOV_2 + 1, "amy", "WATCH", 10),
            reward(NOV_1, "old", "WATCH", 999),
        ],
        "",
    );
    let summary = summarize(dir.path(), NOV_2, NOW, EarningsGrouping::Channel).unwrap();
    assert_eq!(rows(&summary), vec![("amy", 10, 1), ("zed", 10, 1)]);
}

#[test]
fn torn_last_line_is_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    write_month(
        dir.path(),
        "2026-11",
        &[reward(NOV_2 + HOUR, "alice", "WATCH", 10)],
        "{\"v\":1,\"ts\":1793",
    );
    let text = earnings_output(dir.path(), NOW, 1, EarningsGrouping::Day, false).unwrap();
    assert!(text.contains("Total: 10 points from 1 rewards"), "{text}");
    assert!(
        text.contains("Skipped 1 unreadable ledger line(s)."),
        "{text}"
    );
}

#[test]
fn rate_runs_from_the_first_reward_in_the_window() {
    let dir = fixture();
    // 320 points over the 12 hours from the first reward on Nov 2 01:00 to noon.
    let summary = summarize(dir.path(), NOV_2, NOW, EarningsGrouping::Day).unwrap();
    assert_eq!(summary.first_reward, Some(NOV_2 + HOUR));
    let expected = 320.0 * 3_600.0 / f64::from(u32::try_from(NOW - (NOV_2 + HOUR)).unwrap());
    assert!((summary.points_per_hour() - expected).abs() < 1e-9);

    let empty = summarize(dir.path(), NOW, NOW + 10, EarningsGrouping::Day).unwrap();
    assert!(empty.points_per_hour().abs() < f64::EPSILON);
}

#[test]
fn text_and_json_output_cover_each_mode() {
    let dir = fixture();
    let text = earnings_output(dir.path(), NOW, 1, EarningsGrouping::Reason, false).unwrap();
    assert!(text.contains("Earnings for the last 1 day(s), up to 2026-11-02 UTC"));
    assert!(text.contains("Total: 320 points from 3 rewards"), "{text}");
    assert!(text.contains("By reason:"));
    assert!(text.contains("WATCH_STREAK"));

    let json = earnings_output(dir.path(), NOW, 1, EarningsGrouping::Channel, true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["by"], "channel");
    assert_eq!(value["days"], 1);
    assert_eq!(value["total_points"], 320);
    assert_eq!(value["rows"][0]["key"], "alice");
    assert_eq!(value["rows"][0]["points"], 310);
    assert_eq!(value["to_unix"], NOW + 1);

    let none = earnings_output(
        dir.path(),
        NOW + 40 * 86_400,
        1,
        EarningsGrouping::Day,
        false,
    )
    .unwrap();
    assert!(none.contains("No rewards recorded in this period."));
}

#[test]
fn tracker_reports_session_today_and_ledger_health() {
    let tracker = EarningsTracker::default();
    tracker.configure(Some(chrono_tz::UTC), DayTotal::default());
    tracker.note_reward(&reward(NOV_2 + HOUR, "alice", "WATCH", 10));
    tracker.note_reward(&reward(NOV_2 + 2 * HOUR, "alice", "CLAIM", 50));
    tracker.note_dropped(2);
    tracker.note_write_failures(1);
    tracker.set_alarms(true, false);

    let status = tracker.status(NOV_2 + 3 * HOUR);
    assert_eq!(status.session_points, Some(60));
    assert_eq!(status.today_points, Some(60));
    assert_eq!(status.ledger_dropped_writes, 3);
    assert_eq!(status.ledger_write_failures, 1);
    assert!(status.earnings_stall && !status.drops_stall);

    // The next UTC day starts a new "today"; the session total keeps counting.
    tracker.note_reward(&reward(NOV_2 + 24 * HOUR + 5, "alice", "WATCH", 10));
    let status = tracker.status(NOV_2 + 24 * HOUR + 10);
    assert_eq!(status.session_points, Some(70));
    assert_eq!(status.today_points, Some(10));
    assert_eq!(tracker.status(NOV_2 + HOUR).today_points, Some(0));

    let redacted = status.without_points();
    assert_eq!(redacted.session_points, None);
    assert_eq!(redacted.today_points, None);
    assert_eq!(redacted.ledger_dropped_writes, 3);
}

#[test]
fn tracker_keeps_only_watch_and_claim_in_the_recent_window() {
    let tracker = EarningsTracker::default();
    tracker.configure(Some(chrono_tz::UTC), DayTotal::default());
    tracker.note_reward(&reward(NOV_2, "alice", "WATCH", 10));
    tracker.note_reward(&reward(NOV_2 + 60, "alice", "CLAIM", 50));
    tracker.note_reward(&reward(NOV_2 + 120, "alice", "WATCH_STREAK", 300));
    assert_eq!(tracker.watch_claim_points_since(NOV_2), 60);
    assert_eq!(tracker.watch_claim_points_since(NOV_2 + 61), 0);
    // Rewards older than the retention window are forgotten.
    tracker.note_reward(&reward(NOV_2 + 4 * HOUR, "alice", "WATCH", 10));
    assert_eq!(tracker.watch_claim_points_since(0), 10);
}

#[test]
fn status_without_an_earnings_block_still_loads() {
    let status: EarningsStatus = serde_json::from_str("{}").unwrap();
    assert_eq!(status, EarningsStatus::default());
}
