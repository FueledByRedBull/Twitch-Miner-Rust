use super::*;

use std::time::Duration;

use crate::observability::{AppObservability, AppObservabilitySettings};
use crate::status::{write_support_bundle, HealthTracker};

// 2026-10-31T23:59:59Z, the last second of October, and the first of November.
const LAST_OF_OCTOBER: u64 = 1_793_491_199;
const FIRST_OF_NOVEMBER: u64 = 1_793_491_200;

fn record(ts: u64, login: &str, reason: &str, points: i64) -> LedgerRecord {
    LedgerRecord::new(ts, "100", login, reason, points, None, None)
}

fn visit_all(dir: &Path, since: u64, until: u64) -> (Vec<LedgerRecord>, ReadSummary) {
    let mut records = Vec::new();
    let summary = read_range(dir, since, until, |record| records.push(record)).unwrap();
    (records, summary)
}

#[test]
fn record_line_has_the_documented_compact_shape() {
    let full = LedgerRecord::new(
        1_793_620_800,
        "100",
        "alice",
        " watch ",
        10,
        Some(12_345),
        None,
    );
    assert_eq!(
        String::from_utf8(full.encode().unwrap()).unwrap(),
        "{\"v\":1,\"ts\":1793620800,\"channel_id\":\"100\",\"login\":\"alice\",\"reason\":\"WATCH\",\"points\":10,\"balance\":12345}\n"
    );

    let with_game = LedgerRecord::new(1, "1", "a", "CLAIM", 50, None, Some(String::from("Chess")));
    let line = String::from_utf8(with_game.encode().unwrap()).unwrap();
    assert!(!line.contains("balance"));
    assert!(line.contains("\"game\":\"Chess\""));
    assert_eq!(LedgerRecord::decode(line.as_bytes()), Some(with_game));
}

#[test]
fn decode_rejects_torn_corrupt_and_future_lines() {
    let line = String::from_utf8(record(5, "alice", "WATCH", 10).encode().unwrap()).unwrap();
    assert!(LedgerRecord::decode(line.as_bytes()).is_some());
    assert!(LedgerRecord::decode(&line.as_bytes()[..line.len() / 2]).is_none());
    assert!(LedgerRecord::decode(b"not json").is_none());
    assert!(LedgerRecord::decode(b"{}").is_none());
    let future = line.replace("\"v\":1", "\"v\":2");
    assert!(LedgerRecord::decode(future.as_bytes()).is_none());
}

#[test]
fn readers_tolerate_a_torn_last_line_and_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        visit_all(dir.path(), 0, u64::MAX / 2).1,
        ReadSummary::default()
    );

    fs::create_dir_all(dir.path().join(LEDGER_DIR)).unwrap();
    let complete = String::from_utf8(
        record(FIRST_OF_NOVEMBER + 5, "alice", "WATCH", 10)
            .encode()
            .unwrap(),
    )
    .unwrap();
    let torn = &complete[..complete.len() - 12];
    fs::write(
        dir.path().join(LEDGER_DIR).join("2026-11.jsonl"),
        format!("{complete}\n{complete}{torn}"),
    )
    .unwrap();

    let (records, summary) = visit_all(dir.path(), FIRST_OF_NOVEMBER, FIRST_OF_NOVEMBER + 100);
    assert_eq!(records.len(), 2);
    assert_eq!(summary.skipped_lines, 1);
}

#[test]
fn writer_rotates_by_utc_month_and_readers_span_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = LedgerWriter::new(dir.path().to_path_buf());
    let batch = [
        record(LAST_OF_OCTOBER, "alice", "WATCH", 10),
        record(FIRST_OF_NOVEMBER, "alice", "CLAIM", 50),
        record(FIRST_OF_NOVEMBER + 60, "bob", "WATCH", 10),
    ];
    assert_eq!(writer.append_all(&batch), 0);

    let october = fs::read_to_string(dir.path().join(LEDGER_DIR).join("2026-10.jsonl")).unwrap();
    let november = fs::read_to_string(dir.path().join(LEDGER_DIR).join("2026-11.jsonl")).unwrap();
    assert_eq!(october.lines().count(), 1);
    assert_eq!(november.lines().count(), 2);

    let (records, _) = visit_all(dir.path(), LAST_OF_OCTOBER, FIRST_OF_NOVEMBER + 61);
    assert_eq!(records.len(), 3);
    // A range inside November never needs October's file.
    fs::remove_file(dir.path().join(LEDGER_DIR).join("2026-10.jsonl")).unwrap();
    let (records, _) = visit_all(dir.path(), FIRST_OF_NOVEMBER, FIRST_OF_NOVEMBER + 61);
    assert_eq!(records.len(), 2);
    assert_eq!(utc_month(LAST_OF_OCTOBER), Some((2026, 10)));
    assert_eq!(utc_month(FIRST_OF_NOVEMBER), Some((2026, 11)));
    assert_eq!(ledger_file_name(2026, 3), "2026-03.jsonl");
}

#[test]
fn reopening_after_a_torn_line_starts_the_next_record_on_a_new_line() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(LEDGER_DIR)).unwrap();
    let path = dir.path().join(LEDGER_DIR).join("2026-11.jsonl");
    fs::write(&path, "{\"v\":1,\"ts\":17").unwrap();

    let mut writer = LedgerWriter::new(dir.path().to_path_buf());
    assert_eq!(
        writer.append_all(&[record(FIRST_OF_NOVEMBER + 1, "alice", "WATCH", 10)]),
        0
    );

    let (records, summary) = visit_all(dir.path(), FIRST_OF_NOVEMBER, FIRST_OF_NOVEMBER + 10);
    assert_eq!(records.len(), 1);
    assert_eq!(summary.skipped_lines, 1);
}

#[cfg(unix)]
#[test]
fn ledger_files_and_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let mut writer = LedgerWriter::new(dir.path().to_path_buf());
    assert_eq!(
        writer.append_all(&[record(FIRST_OF_NOVEMBER, "alice", "WATCH", 10)]),
        0
    );
    let ledger_dir = dir.path().join(LEDGER_DIR);
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&ledger_dir), 0o700);
    assert_eq!(mode(&ledger_dir.join("2026-11.jsonl")), 0o600);
}

#[tokio::test]
async fn handle_records_through_the_writer_and_drains_on_stop() {
    let dir = tempfile::tempdir().unwrap();
    let tracker = EarningsTracker::default();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (handle, task) =
        LedgerHandle::start(dir.path(), Some(chrono_tz::UTC), stop_rx, tracker.clone());
    let now = unix_now_infallible();
    for offset in 0..5 {
        handle.record(record(now + offset, "alice", "WATCH", 10));
    }
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();

    let (records, summary) = visit_all(dir.path(), now, now + 10);
    assert_eq!(records.len(), 5);
    assert_eq!(summary.skipped_lines, 0);
    let status = tracker.status(now);
    assert_eq!(status.session_points, Some(50));
    assert_eq!(status.ledger_dropped_writes, 0);

    // A later start reads today's rewards back, so a restart keeps the total.
    let restarted = EarningsTracker::default();
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (_handle, _task) =
        LedgerHandle::start(dir.path(), Some(chrono_tz::UTC), stop_rx, restarted.clone());
    let status = restarted.status(now);
    assert_eq!(status.session_points, Some(0));
    assert_eq!(status.today_points, Some(50));
}

#[tokio::test]
async fn full_queue_counts_drops_without_blocking() {
    let tracker = EarningsTracker::default();
    let (sender, _receiver) = tokio::sync::mpsc::channel(1);
    let handle = LedgerHandle {
        sender,
        tracker: tracker.clone(),
    };
    for offset in 0..4 {
        handle.record(record(FIRST_OF_NOVEMBER + offset, "alice", "WATCH", 10));
    }
    let status = tracker.status(FIRST_OF_NOVEMBER);
    assert_eq!(status.ledger_dropped_writes, 3);
    assert_eq!(status.ledger_write_failures, 0);
    // The session total still counts rewards the ledger could not take.
    assert_eq!(status.session_points, Some(40));
}

#[tokio::test]
async fn write_failures_are_counted_and_do_not_stop_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the ledger directory belongs makes every write fail.
    fs::write(dir.path().join(LEDGER_DIR), "in the way").unwrap();
    let tracker = EarningsTracker::default();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (handle, task) =
        LedgerHandle::start(dir.path(), Some(chrono_tz::UTC), stop_rx, tracker.clone());
    handle.record(record(FIRST_OF_NOVEMBER, "alice", "WATCH", 10));
    handle.record(record(FIRST_OF_NOVEMBER + 1, "alice", "WATCH", 10));
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();

    let status = tracker.status(FIRST_OF_NOVEMBER);
    assert_eq!(status.ledger_write_failures, 2);
    assert_eq!(status.ledger_dropped_writes, 2);
}

fn observability_with_ledger(
    dir: &Path,
) -> (
    AppObservability,
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
) {
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let (ledger, task) = LedgerHandle::start(
        dir,
        Some(chrono_tz::UTC),
        stop_rx,
        HealthTracker::default().earnings(),
    );
    let observability = AppObservability::new(
        None,
        tm_observability::DiscordClient::new(Duration::from_secs(1)).unwrap(),
        AppObservabilitySettings {
            anonymize_logs: true,
            ..AppObservabilitySettings::default()
        },
    )
    .with_ledger(ledger);
    (observability, stop_tx, task)
}

#[tokio::test]
async fn accepted_rewards_are_recorded_with_the_real_login_even_when_anonymized() {
    let dir = tempfile::tempdir().unwrap();
    let (observability, stop_tx, task) = observability_with_ledger(dir.path());
    let mut state = tm_runtime::RuntimeState::from_targets(
        &tm_config::ConfigFile::default(),
        &[String::from("alice")],
        tm_domain::OffsetDateTime::UNIX_EPOCH,
    );
    state.streamers[0].channel_id = String::from("100");
    let runtime = tm_runtime::spawn_runtime_state(state);
    let event = tm_domain::MinerEvent::PointsEarned {
        channel_id: String::from("100"),
        earned: 12,
        reason: String::from("watch"),
        balance: 5_012,
        source_id: Some(String::from("source-1")),
    };
    let application = runtime
        .apply_event_with_outcome(event.clone(), crate::utilities::time_now())
        .await
        .unwrap();
    assert!(application.changed);
    crate::pubsub::log_pubsub_event(&runtime, &observability, &event)
        .await
        .unwrap();
    // A zero-point balance sync is not a reward.
    let sync = tm_domain::MinerEvent::PointsEarned {
        channel_id: String::from("100"),
        earned: 0,
        reason: String::from("WATCH"),
        balance: 5_012,
        source_id: None,
    };
    crate::pubsub::log_pubsub_event(&runtime, &observability, &sync)
        .await
        .unwrap();
    stop_tx.send(true).unwrap();
    task.await.unwrap();

    let now = unix_now_infallible();
    let (records, _) = visit_all(dir.path(), now - 600, now + 600);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].login, "alice");
    assert_eq!(records[0].channel_id, "100");
    assert_eq!(records[0].reason, "WATCH");
    assert_eq!(records[0].points, 12);
    assert_eq!(records[0].balance, Some(5_012));
}

#[test]
fn support_bundle_does_not_include_ledger_contents_or_point_totals() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = LedgerWriter::new(dir.path().to_path_buf());
    assert_eq!(
        writer.append_all(&[LedgerRecord::new(
            FIRST_OF_NOVEMBER,
            "CANARY_CHANNEL_ID",
            "CANARY_LOGIN",
            "WATCH",
            424_242,
            Some(737_373),
            Some(String::from("CANARY_GAME")),
        )]),
        0
    );
    let tracker = HealthTracker::default();
    tracker.earnings().note_reward(&record(
        unix_now_infallible(),
        "CANARY_LOGIN",
        "WATCH",
        424_242,
    ));
    let reporter = crate::status::StatusReporter::ready(
        dir.path(),
        tracker,
        std::sync::Arc::new(tm_runtime::RuntimeMetrics::default()),
    )
    .unwrap();
    reporter.heartbeat().unwrap();
    let destination = dir.path().join("support.json");
    write_support_bundle(dir.path(), &destination).unwrap();

    let bundle = fs::read_to_string(destination).unwrap();
    for private in ["CANARY", "424242", "737373"] {
        assert!(!bundle.contains(private), "bundle leaked {private}");
    }
    assert!(bundle.contains("ledger_dropped_writes"));
    // The running status file itself still shows the user their own totals.
    let status = fs::read_to_string(dir.path().join(crate::status::STATUS_FILE_NAME)).unwrap();
    assert!(status.contains("424242"));
    assert!(!status.contains("CANARY"));
}
