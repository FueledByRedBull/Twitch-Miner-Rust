//! Durable guard for prediction placement requests.
//!
//! The mutation client generates a transaction ID, but the retry/idempotency
//! contract for reusing it has not been verified. A request can therefore be
//! accepted while its response is lost, and retrying it after a restart could
//! spend the stake twice. This journal records the event identity before the
//! network request and keeps the record until a response or an authoritative
//! rejection is observed.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const JOURNAL_FILE_NAME: &str = "prediction-placements.json";
const JOURNAL_SCHEMA_VERSION: u8 = 1;
const MAX_PENDING_PLACEMENTS: usize = 128;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024;
const TERMINAL_TOMBSTONE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingPlacement {
    account_id: String,
    channel_id: String,
    event_id: String,
    choice: Option<usize>,
    outcome_id: String,
    amount: i64,
    reserved_at_unix_seconds: i64,
    resolved_at_unix_seconds: Option<i64>,
    status: PlacementStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlacementStatus {
    Pending,
    Unknown,
    Confirmed,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PredictionPlacementStatus {
    Pending,
    Unknown,
    Confirmed,
    Rejected,
}

#[derive(Debug, Clone)]
pub(crate) struct PredictionPlacementReservation {
    pub(crate) choice: Option<usize>,
    pub(crate) outcome_id: String,
    pub(crate) amount: i64,
    pub(crate) status: PredictionPlacementStatus,
}

pub(crate) struct PredictionPlacementRequest<'a> {
    pub(crate) account_id: &'a str,
    pub(crate) channel_id: &'a str,
    pub(crate) event_id: &'a str,
    pub(crate) choice: Option<usize>,
    pub(crate) outcome_id: &'a str,
    pub(crate) amount: i64,
    pub(crate) reserved_at_unix_seconds: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalSnapshot {
    schema_version: u8,
    pending: BTreeMap<String, PendingPlacement>,
}

struct JournalState {
    path: Option<PathBuf>,
    pending: BTreeMap<String, PendingPlacement>,
}

/// Shared process-local journal for prediction placement reservations.
///
/// The memory-only constructor is used by unit tests and by callers that do
/// not own the application's work directory.  The production constructor
/// loads the existing file and fails closed when it is malformed, since
/// silently discarding pending reservations could cause a duplicate spend.
#[derive(Clone)]
pub(crate) struct PredictionPlacementJournal {
    state: Arc<Mutex<JournalState>>,
}

impl PredictionPlacementJournal {
    pub(crate) fn memory() -> Self {
        Self {
            state: Arc::new(Mutex::new(JournalState {
                path: None,
                pending: BTreeMap::new(),
            })),
        }
    }

    pub(crate) fn open(work_dir: &Path) -> Result<Self> {
        let path = work_dir.join(JOURNAL_FILE_NAME);
        let pending = match read_bounded(&path)? {
            Some(bytes) => load_snapshot(&path, &bytes)?.pending,
            None => BTreeMap::new(),
        };
        validate_pending(&pending)?;
        Ok(Self {
            state: Arc::new(Mutex::new(JournalState {
                path: Some(path),
                pending,
            })),
        })
    }

    /// Return the persisted decision for this account/channel/event when it
    /// is protected by an unresolved reservation or a recent terminal
    /// tombstone. Unknown and pre-request reservations both suppress another
    /// mutation.
    pub(crate) fn lookup(
        &self,
        account_id: &str,
        channel_id: &str,
        event_id: &str,
    ) -> Result<Option<PredictionPlacementReservation>> {
        let mut state = self.lock()?;
        prune_terminal(&mut state, unix_now_seconds())?;
        Ok(state
            .pending
            .get(&journal_key(account_id, channel_id, event_id))
            .map(|entry| PredictionPlacementReservation {
                choice: entry.choice,
                outcome_id: entry.outcome_id.clone(),
                amount: entry.amount,
                status: match entry.status {
                    PlacementStatus::Pending => PredictionPlacementStatus::Pending,
                    PlacementStatus::Unknown => PredictionPlacementStatus::Unknown,
                    PlacementStatus::Confirmed => PredictionPlacementStatus::Confirmed,
                    PlacementStatus::Rejected => PredictionPlacementStatus::Rejected,
                },
            }))
    }

    #[cfg(test)]
    fn contains(&self, account_id: &str, channel_id: &str, event_id: &str) -> Result<bool> {
        Ok(self.lookup(account_id, channel_id, event_id)?.is_some())
    }

    /// Persist a reservation, returning `false` when one already exists.
    pub(crate) fn reserve(&self, request: &PredictionPlacementRequest<'_>) -> Result<bool> {
        validate_reservation_input(
            request.account_id,
            request.channel_id,
            request.event_id,
            request.outcome_id,
            request.amount,
        )?;
        let key = journal_key(request.account_id, request.channel_id, request.event_id);
        let mut state = self.lock()?;
        prune_terminal(&mut state, request.reserved_at_unix_seconds)?;
        if state.pending.contains_key(&key) {
            return Ok(false);
        }
        if state.pending.len() >= MAX_PENDING_PLACEMENTS {
            return Err(anyhow!(
                "prediction placement journal reached its {MAX_PENDING_PLACEMENTS} entry limit"
            ));
        }
        state.pending.insert(
            key.clone(),
            PendingPlacement {
                account_id: request.account_id.to_string(),
                channel_id: request.channel_id.to_string(),
                event_id: request.event_id.to_string(),
                choice: request.choice,
                outcome_id: request.outcome_id.to_string(),
                amount: request.amount,
                reserved_at_unix_seconds: request.reserved_at_unix_seconds,
                resolved_at_unix_seconds: None,
                status: PlacementStatus::Pending,
            },
        );
        if let Err(error) = persist_locked(&state) {
            state.pending.remove(&key);
            return Err(error);
        }
        Ok(true)
    }

    /// Mark the request as response-unknown while retaining the reservation
    /// across process restarts.
    pub(crate) fn mark_unknown(
        &self,
        account_id: &str,
        channel_id: &str,
        event_id: &str,
    ) -> Result<()> {
        let key = journal_key(account_id, channel_id, event_id);
        let mut state = self.lock()?;
        let Some(entry) = state.pending.get_mut(&key) else {
            return Ok(());
        };
        if matches!(
            entry.status,
            PlacementStatus::Confirmed | PlacementStatus::Rejected
        ) {
            // A late transport error must not downgrade an authoritative viewer
            // confirmation or rejection back to an unresolved reservation.
            return Ok(());
        }
        let previous = entry.status;
        entry.status = PlacementStatus::Unknown;
        if let Err(error) = persist_locked(&state) {
            if let Some(entry) = state.pending.get_mut(&key) {
                entry.status = previous;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Keep a bounded terminal tombstone after a successful mutation. This
    /// prevents a replayed active event after restart from spending again.
    pub(crate) fn confirm(&self, account_id: &str, channel_id: &str, event_id: &str) -> Result<()> {
        self.resolve(account_id, channel_id, event_id, PlacementStatus::Confirmed)
    }

    /// Keep a bounded terminal tombstone after an authoritative rejection.
    pub(crate) fn reject(&self, account_id: &str, channel_id: &str, event_id: &str) -> Result<()> {
        self.resolve(account_id, channel_id, event_id, PlacementStatus::Rejected)
    }

    fn resolve(
        &self,
        account_id: &str,
        channel_id: &str,
        event_id: &str,
        status: PlacementStatus,
    ) -> Result<()> {
        let key = journal_key(account_id, channel_id, event_id);
        let mut state = self.lock()?;
        let Some(entry) = state.pending.get_mut(&key) else {
            return Ok(());
        };
        // A viewer confirmation is authoritative evidence that the mutation
        // was accepted. A late typed mutation rejection can therefore never
        // downgrade a confirmed placement. A later confirmation also upgrades
        // a previously recorded rejection when the two observations race.
        if matches!(entry.status, PlacementStatus::Confirmed)
            || matches!(status, PlacementStatus::Rejected)
                && matches!(entry.status, PlacementStatus::Rejected)
        {
            return Ok(());
        }
        let previous = (entry.status, entry.resolved_at_unix_seconds);
        entry.status = status;
        entry.resolved_at_unix_seconds = Some(unix_now_seconds());
        if let Err(error) = persist_locked(&state) {
            if let Some(entry) = state.pending.get_mut(&key) {
                entry.status = previous.0;
                entry.resolved_at_unix_seconds = previous.1;
            }
            return Err(error);
        }
        Ok(())
    }

    #[cfg(test)]
    fn pending_len(&self) -> Result<usize> {
        Ok(self.lock()?.pending.len())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, JournalState>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("prediction placement journal lock poisoned"))
    }
}

fn journal_key(account_id: &str, channel_id: &str, event_id: &str) -> String {
    format!("{account_id}\u{0}{channel_id}\u{0}{event_id}")
}

fn validate_reservation_input(
    account_id: &str,
    channel_id: &str,
    event_id: &str,
    outcome_id: &str,
    amount: i64,
) -> Result<()> {
    if account_id.trim().is_empty()
        || channel_id.trim().is_empty()
        || event_id.trim().is_empty()
        || outcome_id.trim().is_empty()
        || amount <= 0
    {
        return Err(anyhow!("invalid prediction placement reservation"));
    }
    Ok(())
}

fn load_snapshot(path: &Path, bytes: &[u8]) -> Result<JournalSnapshot> {
    let snapshot: JournalSnapshot = serde_json::from_slice(bytes)
        .with_context(|| format!("parse prediction placement journal {}", path.display()))?;
    if snapshot.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(anyhow!(
            "unsupported prediction placement journal schema {} in {}",
            snapshot.schema_version,
            path.display()
        ));
    }
    Ok(snapshot)
}

fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_JOURNAL_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_JOURNAL_BYTES {
        return Err(anyhow!(
            "prediction placement journal exceeds {MAX_JOURNAL_BYTES} bytes"
        ));
    }
    Ok(Some(bytes))
}

fn validate_pending(pending: &BTreeMap<String, PendingPlacement>) -> Result<()> {
    if pending.len() > MAX_PENDING_PLACEMENTS {
        return Err(anyhow!(
            "prediction placement journal contains {} entries; maximum is {}",
            pending.len(),
            MAX_PENDING_PLACEMENTS
        ));
    }
    for (key, entry) in pending {
        if entry.account_id.trim().is_empty()
            || entry.channel_id.trim().is_empty()
            || entry.event_id.trim().is_empty()
            || entry.outcome_id.trim().is_empty()
            || entry.amount <= 0
            || *key != journal_key(&entry.account_id, &entry.channel_id, &entry.event_id)
        {
            return Err(anyhow!("invalid prediction placement journal entry"));
        }
    }
    Ok(())
}

fn prune_terminal(state: &mut JournalState, now_unix_seconds: i64) -> Result<()> {
    let cutoff = now_unix_seconds.saturating_sub(TERMINAL_TOMBSTONE_TTL_SECONDS);
    let entries = std::mem::take(&mut state.pending);
    let mut removed = BTreeMap::new();
    let mut retained = BTreeMap::new();
    for (key, entry) in entries {
        let keep = matches!(
            entry.status,
            PlacementStatus::Pending | PlacementStatus::Unknown
        ) || entry
            .resolved_at_unix_seconds
            .unwrap_or(entry.reserved_at_unix_seconds)
            >= cutoff;
        if keep {
            retained.insert(key, entry);
        } else {
            removed.insert(key, entry);
        }
    }
    state.pending = retained;
    if !removed.is_empty() {
        if let Err(error) = persist_locked(state) {
            state.pending.extend(removed);
            return Err(error);
        }
    }
    Ok(())
}

fn unix_now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

fn persist_locked(state: &JournalState) -> Result<()> {
    let Some(path) = state.path.as_deref() else {
        return Ok(());
    };
    if state.pending.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", path.display()));
            }
        }
    }
    let snapshot = JournalSnapshot {
        schema_version: JOURNAL_SCHEMA_VERSION,
        pending: state.pending.clone(),
    };
    atomic_json_write(path, &snapshot)
}

fn atomic_json_write(path: &Path, value: &impl Serialize) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(JOURNAL_FILE_NAME);
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = create_private_file(&temporary)
            .with_context(|| format!("write {}", temporary.display()))?;
        let bytes = serde_json::to_vec(value)
            .with_context(|| format!("serialize {}", temporary.display()))?;
        if bytes.len() >= usize::try_from(MAX_JOURNAL_BYTES).unwrap_or(usize::MAX) {
            return Err(anyhow!(
                "prediction placement journal exceeds {MAX_JOURNAL_BYTES} bytes"
            ));
        }
        file.write_all(&bytes)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        drop(file);
        publish_temporary(&temporary, path)
            .with_context(|| format!("publish {}", path.display()))?;
        #[cfg(unix)]
        sync_parent_directory(path)?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn publish_temporary(temporary: &Path, path: &Path) -> std::io::Result<()> {
    // Keep the destination present throughout publication. std::fs::rename
    // replaces it atomically on the supported targets; moving the old file
    // away first would create a crash window in which a restart could mistake
    // the missing journal for an empty one.
    fs::rename(temporary, path)
}

fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        let mut permissions = file.metadata()?.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
    }
    Ok(file)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::{PredictionPlacementJournal, PredictionPlacementStatus, JOURNAL_FILE_NAME};

    #[test]
    fn reservation_is_durable_and_blocks_until_completed() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let journal = PredictionPlacementJournal::open(directory.path())?;
        let now = super::unix_now_seconds();
        let request = super::PredictionPlacementRequest {
            account_id: "account",
            channel_id: "channel",
            event_id: "event",
            choice: Some(0),
            outcome_id: "outcome",
            amount: 10,
            reserved_at_unix_seconds: now,
        };
        assert!(journal.reserve(&request)?);
        assert!(!journal.reserve(&request)?);
        assert!(journal.contains("account", "channel", "event")?);
        journal.mark_unknown("account", "channel", "event")?;
        let reloaded = PredictionPlacementJournal::open(directory.path())?;
        assert!(reloaded.contains("account", "channel", "event")?);
        assert_eq!(reloaded.pending_len()?, 1);
        reloaded.confirm("account", "channel", "event")?;
        assert!(reloaded.contains("account", "channel", "event")?);
        assert_eq!(
            reloaded
                .lookup("account", "channel", "event")?
                .map(|entry| entry.choice),
            Some(Some(0))
        );
        assert!(directory.path().join(JOURNAL_FILE_NAME).exists());
        Ok(())
    }

    #[test]
    fn authoritative_confirmation_wins_over_late_rejection() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let journal = PredictionPlacementJournal::open(directory.path())?;
        let request = super::PredictionPlacementRequest {
            account_id: "account",
            channel_id: "channel",
            event_id: "event",
            choice: Some(0),
            outcome_id: "outcome",
            amount: 10,
            reserved_at_unix_seconds: super::unix_now_seconds(),
        };
        assert!(journal.reserve(&request)?);
        journal.confirm("account", "channel", "event")?;
        journal.reject("account", "channel", "event")?;
        assert_eq!(
            journal
                .lookup("account", "channel", "event")?
                .map(|entry| entry.status),
            Some(PredictionPlacementStatus::Confirmed)
        );

        let other = super::PredictionPlacementRequest {
            event_id: "other-event",
            reserved_at_unix_seconds: super::unix_now_seconds(),
            ..request
        };
        assert!(journal.reserve(&other)?);
        journal.reject("account", "channel", "other-event")?;
        journal.confirm("account", "channel", "other-event")?;
        assert_eq!(
            journal
                .lookup("account", "channel", "other-event")?
                .map(|entry| entry.status),
            Some(PredictionPlacementStatus::Confirmed)
        );
        Ok(())
    }

    #[test]
    fn resolution_persistence_failure_keeps_durable_pending_state() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let journal = PredictionPlacementJournal::open(directory.path())?;
        let request = super::PredictionPlacementRequest {
            account_id: "account",
            channel_id: "channel",
            event_id: "event",
            choice: Some(0),
            outcome_id: "outcome",
            amount: 10,
            reserved_at_unix_seconds: super::unix_now_seconds(),
        };
        assert!(journal.reserve(&request)?);
        let original_path = directory.path().join(JOURNAL_FILE_NAME);
        let broken_path = directory.path().join("missing").join(JOURNAL_FILE_NAME);
        journal
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("journal lock poisoned"))?
            .path = Some(broken_path);

        assert!(journal.mark_unknown("account", "channel", "event").is_err());
        assert_eq!(
            journal
                .lookup("account", "channel", "event")?
                .map(|entry| entry.status),
            Some(PredictionPlacementStatus::Pending)
        );
        assert_eq!(
            PredictionPlacementJournal::open(directory.path())?
                .lookup("account", "channel", "event")?
                .map(|entry| entry.status),
            Some(PredictionPlacementStatus::Pending)
        );

        let journal = PredictionPlacementJournal::open(directory.path())?;
        let second = super::PredictionPlacementRequest {
            event_id: "rejected-event",
            reserved_at_unix_seconds: super::unix_now_seconds(),
            ..request
        };
        assert!(journal.reserve(&second)?);
        journal
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("journal lock poisoned"))?
            .path = Some(
            directory
                .path()
                .join("also-missing")
                .join(JOURNAL_FILE_NAME),
        );
        assert!(journal
            .reject("account", "channel", "rejected-event")
            .is_err());
        assert_eq!(
            journal
                .lookup("account", "channel", "rejected-event")?
                .map(|entry| entry.status),
            Some(PredictionPlacementStatus::Pending)
        );
        assert!(original_path.exists());
        Ok(())
    }

    #[test]
    fn oversized_serialized_journal_keeps_previous_file() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let journal = PredictionPlacementJournal::open(directory.path())?;
        let now = super::unix_now_seconds();
        let first = super::PredictionPlacementRequest {
            account_id: "account",
            channel_id: "channel",
            event_id: "first-event",
            choice: Some(0),
            outcome_id: "outcome",
            amount: 10,
            reserved_at_unix_seconds: now,
        };
        assert!(journal.reserve(&first)?);
        let oversized_outcome =
            "x".repeat(usize::try_from(super::MAX_JOURNAL_BYTES).unwrap_or(usize::MAX));
        let oversized = super::PredictionPlacementRequest {
            event_id: "oversized-event",
            outcome_id: &oversized_outcome,
            ..first
        };
        assert!(journal.reserve(&oversized).is_err());
        assert_eq!(journal.pending_len()?, 1);
        let reloaded = PredictionPlacementJournal::open(directory.path())?;
        assert!(reloaded.contains("account", "channel", "first-event")?);
        assert!(!reloaded.contains("account", "channel", "oversized-event")?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn journal_file_is_private_on_unix() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let journal = PredictionPlacementJournal::open(directory.path())?;
        let request = super::PredictionPlacementRequest {
            account_id: "account",
            channel_id: "channel",
            event_id: "event",
            choice: Some(0),
            outcome_id: "outcome",
            amount: 10,
            reserved_at_unix_seconds: super::unix_now_seconds(),
        };
        assert!(journal.reserve(&request)?);
        let mode = std::fs::metadata(directory.path().join(JOURNAL_FILE_NAME))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn malformed_journal_fails_closed() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(JOURNAL_FILE_NAME);
        for bytes in [b"{malformed".as_slice(), b"{}".as_slice()] {
            std::fs::write(&path, bytes)?;
            assert!(PredictionPlacementJournal::open(directory.path()).is_err());
        }
        std::fs::write(&path, br#"{"schema_version":1}"#)?;
        assert!(PredictionPlacementJournal::open(directory.path()).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn authoritative_confirmation_resolves_unknown_after_restart() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let journal = PredictionPlacementJournal::open(directory.path())?;
        let event_id = String::from("prediction-confirmed-after-restart");
        let request = super::PredictionPlacementRequest {
            account_id: "account",
            channel_id: "channel",
            event_id: &event_id,
            choice: Some(0),
            outcome_id: "outcome",
            amount: 10,
            reserved_at_unix_seconds: super::unix_now_seconds(),
        };
        assert!(journal.reserve(&request)?);
        journal.mark_unknown("account", "channel", &event_id)?;

        let config = tm_config::ConfigFile {
            streamers: vec![String::from("tester")],
            ..tm_config::ConfigFile::default()
        };
        let mut state =
            tm_runtime::RuntimeState::from_config(&config, tm_domain::OffsetDateTime::UNIX_EPOCH);
        state.streamers[0].channel_id = String::from("channel");
        state.predictions.insert(
            event_id.clone(),
            tm_domain::PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: event_id.clone(),
                title: String::from("Fixture prediction"),
                status: String::from("ACTIVE"),
                created_at: tm_domain::OffsetDateTime::UNIX_EPOCH,
                window_seconds: 30.0,
                outcomes: Vec::new(),
                decision: tm_domain::PredictionDecision {
                    choice: Some(0),
                    outcome_id: "outcome".into(),
                    amount: 10,
                },
                bet_placed: true,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        let runtime = tm_runtime::spawn_runtime_state(state);
        let confirmation = tm_domain::MinerEvent::PredictionUser {
            event_id: event_id.clone(),
            kind: tm_domain::PredictionUserKind::PredictionMade,
            result: None,
        };
        runtime
            .apply_event(confirmation.clone(), tm_domain::OffsetDateTime::UNIX_EPOCH)
            .await?;
        crate::runtime_effects::reconcile_prediction_journal(
            &runtime,
            &journal,
            "account",
            &confirmation,
        )
        .await?;

        let reopened = PredictionPlacementJournal::open(directory.path())?;
        assert_eq!(
            reopened
                .lookup("account", "channel", &event_id)?
                .map(|entry| entry.status),
            Some(PredictionPlacementStatus::Confirmed)
        );
        Ok(())
    }
}
