//! Durable, append-only record of every point reward the runtime accepts.
//!
//! Records are one compact JSON object per line in `earnings/YYYY-MM.jsonl`
//! under the data directory (UTC month). The runtime path only performs a
//! non-blocking `try_send`; a dedicated task owns the files, so a slow or
//! failing disk never delays mining. Lost records are counted and surfaced
//! through the status file instead of failing the miner.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::earnings::{today_total, DayTotal, EarningsTracker};
use crate::status::unix_now_infallible;

pub(crate) const LEDGER_DIR: &str = "earnings";
const LEDGER_VERSION: u8 = 1;
const QUEUE_CAPACITY: usize = 1024;
const WRITE_BATCH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerRecord {
    pub(crate) v: u8,
    pub(crate) ts: u64,
    pub(crate) channel_id: String,
    pub(crate) login: String,
    pub(crate) reason: String,
    pub(crate) points: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) balance: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) game: Option<String>,
}

impl LedgerRecord {
    pub(crate) fn new(
        ts: u64,
        channel_id: &str,
        login: &str,
        reason: &str,
        points: i64,
        balance: Option<i64>,
        game: Option<String>,
    ) -> Self {
        Self {
            v: LEDGER_VERSION,
            ts,
            channel_id: channel_id.to_string(),
            login: login.to_string(),
            reason: reason.trim().to_uppercase(),
            points,
            balance,
            game,
        }
    }

    fn encode(&self) -> serde_json::Result<Vec<u8>> {
        let mut line = serde_json::to_vec(self)?;
        line.push(b'\n');
        Ok(line)
    }

    /// Returns `None` for a torn, corrupt, or newer-format line.
    pub(crate) fn decode(line: &[u8]) -> Option<Self> {
        let record: Self = serde_json::from_slice(line.trim_ascii()).ok()?;
        (record.v == LEDGER_VERSION).then_some(record)
    }
}

/// UTC `(year, month)` of a Unix timestamp.
pub(crate) fn utc_month(ts: u64) -> Option<(i32, u8)> {
    let time = tm_runtime::RuntimeTime::from_unix_timestamp(i64::try_from(ts).ok()?).ok()?;
    Some((time.year(), u8::from(time.month())))
}

pub(crate) fn ledger_file_name(year: i32, month: u8) -> String {
    format!("{year:04}-{month:02}.jsonl")
}

fn ledger_path(work_dir: &Path, ts: u64) -> Option<PathBuf> {
    let (year, month) = utc_month(ts)?;
    Some(
        work_dir
            .join(LEDGER_DIR)
            .join(ledger_file_name(year, month)),
    )
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadSummary {
    /// Lines that could not be decoded, such as one torn by a crash.
    pub(crate) skipped_lines: u64,
}

/// Streams every record with `since <= ts < until` to `visit`, one line at a
/// time, reading only the monthly files that overlap the range.
pub(crate) fn read_range(
    work_dir: &Path,
    since: u64,
    until: u64,
    mut visit: impl FnMut(LedgerRecord),
) -> Result<ReadSummary> {
    let mut summary = ReadSummary::default();
    let (Some(first), Some(last)) = (utc_month(since), utc_month(until)) else {
        return Ok(summary);
    };
    let mut month = first;
    while month <= last {
        let path = work_dir
            .join(LEDGER_DIR)
            .join(ledger_file_name(month.0, month.1));
        match File::open(&path) {
            Ok(file) => read_file(file, since, until, &mut summary, &mut visit)
                .with_context(|| format!("read {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
        }
        month = if month.1 == 12 {
            (month.0 + 1, 1)
        } else {
            (month.0, month.1 + 1)
        };
    }
    Ok(summary)
}

fn read_file(
    file: File,
    since: u64,
    until: u64,
    summary: &mut ReadSummary,
    visit: &mut impl FnMut(LedgerRecord),
) -> std::io::Result<()> {
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        match LedgerRecord::decode(&line) {
            Some(record) if record.ts >= since && record.ts < until => visit(record),
            Some(_) => {}
            None => summary.skipped_lines += 1,
        }
    }
}

struct LedgerWriter {
    work_dir: PathBuf,
    open: Option<((i32, u8), File)>,
    failing: bool,
}

impl LedgerWriter {
    fn new(work_dir: PathBuf) -> Self {
        Self {
            work_dir,
            open: None,
            failing: false,
        }
    }

    /// Appends the batch and returns how many records could not be written.
    fn append_all(&mut self, batch: &[LedgerRecord]) -> u64 {
        let mut failures = 0;
        for record in batch {
            if let Err(error) = self.append(record) {
                // Reopen on the next record: the file or directory may have been replaced.
                self.open = None;
                failures += 1;
                if !self.failing {
                    self.failing = true;
                    tracing::warn!(
                        task = "ledger",
                        error_class = "write",
                        %error,
                        "earnings ledger write failed; rewards are not being recorded"
                    );
                }
            } else if self.failing {
                self.failing = false;
                tracing::info!(task = "ledger", "earnings ledger writes recovered");
            }
        }
        failures
    }

    fn append(&mut self, record: &LedgerRecord) -> Result<()> {
        let month = utc_month(record.ts).context("record timestamp out of range")?;
        if self.open.as_ref().map(|(open_month, _)| *open_month) != Some(month) {
            let path = ledger_path(&self.work_dir, record.ts).context("ledger path")?;
            self.open = Some((month, open_ledger_file(&path)?));
        }
        let Some((_, file)) = self.open.as_mut() else {
            anyhow::bail!("ledger file unavailable");
        };
        file.write_all(&record.encode()?)?;
        Ok(())
    }
}

fn open_ledger_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = open_private_append(path).with_context(|| format!("open {}", path.display()))?;
    // A crash can leave a torn last line. Start the next record on a fresh line
    // so it is not glued onto the fragment.
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(not(unix))]
pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

#[cfg(unix)]
fn open_private_append(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
}

/// Sending half of the ledger. Cloning is cheap; recording never blocks.
#[derive(Clone)]
pub(crate) struct LedgerHandle {
    sender: tokio::sync::mpsc::Sender<LedgerRecord>,
    tracker: EarningsTracker,
}

impl LedgerHandle {
    /// Starts the writer task. On stop it drains what is already queued.
    ///
    /// Today's total is read back from the ledger first, so the status block
    /// stays correct across restarts.
    pub(crate) fn start(
        work_dir: &Path,
        timezone: Option<chrono_tz::Tz>,
        stop: tokio::sync::watch::Receiver<bool>,
        tracker: EarningsTracker,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let today = today_total(work_dir, unix_now_infallible(), timezone).unwrap_or_else(|error| {
            tracing::warn!(task = "ledger", error_class = "read", %error, "could not read today's earnings from the ledger");
            DayTotal::default()
        });
        tracker.configure(timezone, today);
        let (sender, receiver) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
        let task = tokio::spawn(run_writer(
            work_dir.to_path_buf(),
            receiver,
            stop,
            tracker.clone(),
        ));
        (Self { sender, tracker }, task)
    }

    pub(crate) fn record(&self, record: LedgerRecord) {
        self.tracker.note_reward(&record);
        if self.sender.try_send(record).is_err() {
            self.tracker.note_dropped(1);
        }
    }
}

async fn run_writer(
    work_dir: PathBuf,
    mut receiver: tokio::sync::mpsc::Receiver<LedgerRecord>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    tracker: EarningsTracker,
) {
    let mut writer = LedgerWriter::new(work_dir.clone());
    let mut stopping = false;
    loop {
        if !stopping && *stop.borrow() {
            stopping = true;
            receiver.close();
        }
        let first = if stopping {
            receiver.try_recv().ok()
        } else {
            tokio::select! {
                record = receiver.recv() => record,
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        stopping = true;
                        receiver.close();
                    }
                    continue;
                }
            }
        };
        let Some(first) = first else {
            return;
        };
        let mut batch = vec![first];
        while batch.len() < WRITE_BATCH {
            match receiver.try_recv() {
                Ok(record) => batch.push(record),
                Err(_) => break,
            }
        }
        let count = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        let task = tokio::task::spawn_blocking(move || {
            let failures = writer.append_all(&batch);
            (writer, failures)
        });
        match task.await {
            Ok((returned, failures)) => {
                writer = returned;
                if failures > 0 {
                    tracker.note_write_failures(failures);
                }
            }
            Err(error) => {
                tracing::warn!(task = "ledger", error_class = "write", %error, "earnings ledger writer failed");
                tracker.note_write_failures(count);
                writer = LedgerWriter::new(work_dir.clone());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/ledger_tests.rs"]
mod tests;
