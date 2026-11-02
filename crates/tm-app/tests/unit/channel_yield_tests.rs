use std::path::Path;

use super::*;
use crate::ledger::{ledger_file_name, utc_month, LedgerRecord, LEDGER_DIR};

const DAY: u64 = 24 * 60 * 60;
// 2026-11-02T12:00:00Z
const NOW: u64 = 1_793_620_800;

fn write_ledger(work_dir: &Path, records: &[LedgerRecord]) {
    let directory = work_dir.join(LEDGER_DIR);
    std::fs::create_dir_all(&directory).unwrap();
    for record in records {
        let (year, month) = utc_month(record.ts).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(ledger_file_name(year, month)))
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            format!("{}\n", serde_json::to_string(record).unwrap()).as_bytes(),
        )
        .unwrap();
    }
}

fn reward(ts: u64, channel_id: &str, reason: &str, points: i64) -> LedgerRecord {
    LedgerRecord::new(ts, channel_id, channel_id, reason, points, None, None)
}

fn tiers(work_dir: &Path) -> HashMap<String, u8> {
    let top = top_tier(varying_sums(work_dir, NOW).unwrap());
    top.into_iter().map(|channel| (channel, 1)).collect()
}

#[test]
fn only_the_rewards_that_vary_count() {
    let directory = tempfile::tempdir().unwrap();
    write_ledger(
        directory.path(),
        &[
            reward(NOW - 10, "1", "WATCH", 10_000),
            reward(NOW - 10, "1", "CLAIM", 10_000),
            reward(NOW - 10, "2", "WATCH_STREAK", 450),
            reward(NOW - 10, "2", "RAID", 250),
            reward(NOW - 10, "3", "WATCH", 1),
        ],
    );

    let sums = varying_sums(directory.path(), NOW).unwrap();

    assert_eq!(sums, HashMap::from([(String::from("2"), 700)]));
}

#[test]
fn tier_one_is_the_top_quartile_of_channels_with_a_nonzero_sum() {
    let directory = tempfile::tempdir().unwrap();
    let mut records = (1..=8)
        .map(|channel| {
            reward(
                NOW - 100,
                &channel.to_string(),
                "WATCH_STREAK",
                channel * 10,
            )
        })
        .collect::<Vec<_>>();
    // Channels that only earn at the fixed rate are not part of the quartile.
    records.extend((20..60).map(|channel| reward(NOW - 100, &channel.to_string(), "WATCH", 500)));
    write_ledger(directory.path(), &records);

    let tiers = tiers(directory.path());

    let mut top = tiers.keys().cloned().collect::<Vec<_>>();
    top.sort();
    assert_eq!(top, ["7", "8"]);
}

#[test]
fn ties_and_tiny_ledgers_stay_coarse() {
    let sums = |values: &[i64]| {
        values
            .iter()
            .enumerate()
            .map(|(index, sum)| (index.to_string(), *sum))
            .collect::<HashMap<_, _>>()
    };
    let count = |values: &[i64]| top_tier(sums(values)).len();

    assert_eq!(count(&[]), 0);
    assert_eq!(count(&[5]), 1, "one channel is its own top quartile");
    assert_eq!(count(&[5, 5, 5, 5]), 4, "all equal means no distinction");
    assert_eq!(
        count(&[9, 9, 7, 1]),
        2,
        "a tie at the cut-off stays together"
    );
    assert_eq!(count(&[9, 3, 2, 1, 1]), 2, "five channels round up to two");
    assert_eq!(count(&[0, -4]), 0, "nothing earned means no tier");
}

#[test]
fn rewards_older_than_fourteen_days_are_ignored() {
    let directory = tempfile::tempdir().unwrap();
    write_ledger(
        directory.path(),
        &[
            reward(NOW - 15 * DAY, "1", "RAID", 5_000),
            reward(NOW - 13 * DAY, "2", "RAID", 100),
            reward(NOW - 13 * DAY, "3", "RAID", 100),
            reward(NOW - 13 * DAY, "3", "WATCH_STREAK", 50),
        ],
    );

    let sums = varying_sums(directory.path(), NOW).unwrap();

    assert_eq!(
        sums,
        HashMap::from([(String::from("2"), 100), (String::from("3"), 150)])
    );
}

#[test]
fn a_missing_or_empty_ledger_gives_every_channel_tier_zero() {
    let directory = tempfile::tempdir().unwrap();
    assert!(varying_sums(directory.path(), NOW).unwrap().is_empty());
    assert!(tiers(directory.path()).is_empty());

    write_ledger(directory.path(), &[]);
    std::fs::write(
        directory
            .path()
            .join(LEDGER_DIR)
            .join(ledger_file_name(2026, 11)),
        b"\n{torn",
    )
    .unwrap();
    assert!(tiers(directory.path()).is_empty());
    assert_eq!(YieldTiers::default().tier("1"), 0);
}

async fn settle(tiers: &mut YieldTiers, now: Instant) {
    tiers.refresh(now).await;
    while let Some(read) = tiers.pending.take() {
        tiers.accept(read.await);
    }
}

#[tokio::test]
async fn tiers_are_read_in_the_background_and_refreshed_at_most_hourly() {
    let directory = tempfile::tempdir().unwrap();
    let now = unix_now_infallible();
    write_ledger(
        directory.path(),
        &[reward(now - 5, "1", "WATCH_STREAK", 450)],
    );
    let start = Instant::now();
    let mut tiers = YieldTiers::new(directory.path().to_path_buf());

    assert_eq!(tiers.tier("1"), 0, "nothing is known before the first read");
    settle(&mut tiers, start).await;
    assert_eq!(tiers.tier("1"), 1);
    assert_eq!(tiers.tier("2"), 0);

    write_ledger(
        directory.path(),
        &[
            reward(now - 4, "2", "RAID", 5_000),
            reward(now - 4, "3", "RAID", 4_000),
            reward(now - 4, "4", "RAID", 3_000),
            reward(now - 4, "5", "RAID", 2_000),
        ],
    );
    settle(&mut tiers, start + Duration::from_secs(60 * 60 - 1)).await;
    assert_eq!(tiers.tier("2"), 0, "not due yet");
    settle(&mut tiers, start + REFRESH_INTERVAL).await;
    assert_eq!((tiers.tier("1"), tiers.tier("2")), (0, 1));
}

#[tokio::test]
async fn a_read_failure_keeps_the_previous_tiers() {
    let directory = tempfile::tempdir().unwrap();
    let now = unix_now_infallible();
    write_ledger(directory.path(), &[reward(now - 5, "1", "RAID", 450)]);
    let start = Instant::now();
    let mut tiers = YieldTiers::new(directory.path().to_path_buf());
    settle(&mut tiers, start).await;
    assert_eq!(tiers.tier("1"), 1);
    assert!(!tiers.failing);

    // A directory where the month's file should be cannot be read as a ledger.
    let (year, month) = utc_month(now).unwrap();
    let file = directory
        .path()
        .join(LEDGER_DIR)
        .join(ledger_file_name(year, month));
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();

    settle(&mut tiers, start + REFRESH_INTERVAL).await;
    assert_eq!(tiers.tier("1"), 1, "the previous tiers stay");
    assert!(tiers.failing);
    settle(&mut tiers, start + REFRESH_INTERVAL * 2).await;
    assert_eq!(tiers.tier("1"), 1);
    assert!(tiers.failing);

    std::fs::remove_dir(&file).unwrap();
    settle(&mut tiers, start + REFRESH_INTERVAL * 3).await;
    assert_eq!(tiers.tier("1"), 0, "an empty ledger now reads cleanly");
    assert!(!tiers.failing);
}
