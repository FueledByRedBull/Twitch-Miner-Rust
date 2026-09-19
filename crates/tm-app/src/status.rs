use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tm_runtime::{RuntimeMetrics, RuntimeMetricsSnapshot};
use tm_twitch::InventoryDrop;

use crate::build_info;

pub(crate) const STATUS_FILE_NAME: &str = "runtime-status.json";
const STATUS_SCHEMA_VERSION: u8 = 6;
const MAX_HEARTBEAT_AGE_SECONDS: u64 = 120;
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
const MAX_COUNTER_VALUE: u64 = 1_000_000_000;
const MAX_DROP_PROGRESS_ENTRIES: usize = 16;
const MAX_WATCH_SLOTS: usize = 2;
const MAX_WATCH_SLOT_AGE_SECONDS: u64 = 10 * 60;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TaskStatus {
    name: String,
    last_success_unix: u64,
    last_activity_unix: u64,
    consecutive_failures: u32,
    stale_after_seconds: u64,
    last_error_class: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RuntimeStatus {
    schema_version: u8,
    state: String,
    started_at_unix: u64,
    heartbeat_at_unix: u64,
    version: String,
    revision: String,
    target: String,
    tasks: Vec<TaskStatus>,
    counters: StatusCounters,
    watch_slots: Vec<WatchSlotStatus>,
    runtime_metrics: RuntimeMetricsSnapshot,
    eventsub: Option<tm_pubsub::EventSubSetupReport>,
    pubsub: Option<tm_pubsub::PubSubSetupReport>,
    #[serde(default)]
    prediction_journal: Option<crate::prediction_journal::JournalCapacity>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct StatusCounters {
    claims: u64,
    bets: u64,
    reconnects: u64,
    successful_refreshes: u64,
    last_error_class: Option<String>,
    #[serde(default)]
    drop_progress: Vec<DropProgressSnapshot>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DropProgressSnapshot {
    drop_key: String,
    reward_name: String,
    campaign_name: String,
    current_minutes_watched: i64,
    required_minutes_watched: i64,
    is_claimed: bool,
    observed_at_unix: u64,
    last_progress_increase_unix: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WatchProgress {
    #[default]
    MeasurementUnavailable,
    AwaitingFirstCredit,
    FirstCreditOverdue,
    Earning,
    Stalled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WatchSlotStatus {
    #[serde(default)]
    progress: WatchProgress,
    #[serde(default)]
    progress_age_seconds: Option<u64>,
    slot: usize,
    channel_index: Option<usize>,
    channel_key: Option<String>,
    broadcast_key: Option<String>,
    last_server_confirmed_points_unix: Option<u64>,
    last_context_observed_unix: Option<u64>,
    selected: bool,
    selection_reason: Option<String>,
    last_activity_unix: u64,
    last_accepted_watch_unix: Option<u64>,
    consecutive_failures: u32,
    last_error_class: Option<String>,
}

#[derive(Clone, Default)]
pub(crate) struct HealthTracker {
    tasks: Arc<Mutex<BTreeMap<String, TaskStatus>>>,
    counters: Arc<Mutex<StatusCounters>>,
    eventsub: Arc<Mutex<Option<tm_pubsub::EventSubSetupReport>>>,
    pubsub: Arc<Mutex<Option<tm_pubsub::PubSubSetupReport>>>,
    watch_slots: Arc<Mutex<Vec<WatchSlotStatus>>>,
    prediction_journal: Arc<Mutex<Option<crate::prediction_journal::PredictionPlacementJournal>>>,
}

impl HealthTracker {
    pub(crate) fn set_journal(
        &self,
        journal: crate::prediction_journal::PredictionPlacementJournal,
    ) {
        *self
            .prediction_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(journal);
    }

    pub(crate) fn register(&self, name: &'static str, stale_after: std::time::Duration) {
        let now = unix_now_infallible();
        self.lock_tasks().insert(
            name.to_string(),
            TaskStatus {
                name: name.to_string(),
                last_success_unix: now,
                last_activity_unix: now,
                consecutive_failures: 0,
                stale_after_seconds: stale_after.as_secs(),
                last_error_class: None,
            },
        );
    }

    pub(crate) fn success(&self, name: &'static str) {
        if let Some(task) = self.lock_tasks().get_mut(name) {
            let now = unix_now_infallible();
            task.last_success_unix = now;
            task.last_activity_unix = now;
            task.consecutive_failures = 0;
            task.last_error_class = None;
        }
    }

    pub(crate) fn activity(&self, name: &'static str) {
        if let Some(task) = self.lock_tasks().get_mut(name) {
            task.last_activity_unix = unix_now_infallible();
        }
    }

    pub(crate) fn failure(&self, name: &'static str, error_class: &'static str) {
        if let Some(task) = self.lock_tasks().get_mut(name) {
            task.last_activity_unix = unix_now_infallible();
            task.consecutive_failures = task.consecutive_failures.saturating_add(1);
            task.last_error_class = Some(error_class.to_string());
        }
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counters.last_error_class = Some(error_class.to_string());
        if matches!(name, "eventsub" | "pubsub")
            && matches!(
                error_class,
                "connection-closed"
                    | "connection-error"
                    | "connection-reset"
                    | "connection-task"
                    | "reconnect"
            )
        {
            counters.reconnects = counters.reconnects.saturating_add(1).min(MAX_COUNTER_VALUE);
        }
    }

    pub(crate) fn record_claim(&self) {
        self.increment(|counters| &mut counters.claims);
    }

    pub(crate) fn record_bet(&self) {
        self.increment(|counters| &mut counters.bets);
    }

    pub(crate) fn record_refresh(&self) {
        self.increment(|counters| &mut counters.successful_refreshes);
    }

    pub(crate) fn record_eventsub_setup(&self, report: tm_pubsub::EventSubSetupReport) {
        *self
            .eventsub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
    }

    pub(crate) fn record_pubsub_setup(&self, report: tm_pubsub::PubSubSetupReport) {
        *self
            .pubsub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
    }

    pub(crate) fn record_pubsub_acknowledgement(&self, topic_class: &str) -> bool {
        self.with_pubsub_capability(topic_class, |capability| {
            capability.acknowledged_topics = capability
                .acknowledged_topics
                .saturating_add(1)
                .min(capability.configured_topics);
            if capability.acknowledged_topics == capability.configured_topics {
                capability.failure_class = None;
            }
        });
        self.pubsub_ready()
    }

    pub(crate) fn record_pubsub_message(&self, topic_class: &str) {
        self.with_pubsub_capability(topic_class, |capability| {
            capability.last_message_unix = Some(unix_now_infallible());
        });
    }

    pub(crate) fn record_pubsub_failure(&self, topic_class: &str, failure_class: &str) {
        self.with_pubsub_capability(topic_class, |capability| {
            capability.failure_class = Some(failure_class.to_string());
        });
    }

    pub(crate) fn record_pubsub_disconnect(
        &self,
        topic_class: &str,
        acknowledged_topics: usize,
        failure_class: &str,
    ) {
        self.with_pubsub_capability(topic_class, |capability| {
            capability.acknowledged_topics = capability
                .acknowledged_topics
                .saturating_sub(acknowledged_topics);
            capability.reconnects = capability.reconnects.saturating_add(1);
            capability.failure_class = Some(failure_class.to_string());
        });
    }

    pub(crate) fn pubsub_ready(&self) -> bool {
        self.pubsub_snapshot().is_some_and(|report| {
            !report.capabilities.is_empty()
                && report.capabilities.iter().all(|capability| {
                    capability.acknowledged_topics == capability.configured_topics
                        && capability.failure_class.is_none()
                })
        })
    }

    fn with_pubsub_capability(
        &self,
        topic_class: &str,
        update: impl FnOnce(&mut tm_pubsub::PubSubCapabilityStatus),
    ) {
        let mut report = self
            .pubsub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(capability) = report.as_mut().and_then(|report| {
            report
                .capabilities
                .iter_mut()
                .find(|capability| capability.topic_class == topic_class)
        }) {
            update(capability);
        }
    }

    pub(crate) fn record_drop_progress(&self, drop: &InventoryDrop) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = unix_now_infallible();
        let drop_key = stable_drop_key(drop);
        let previous_progress_increase = counters
            .drop_progress
            .iter()
            .find(|existing| existing.drop_key == drop_key)
            .and_then(|existing| {
                (drop.current_minutes_watched > existing.current_minutes_watched)
                    .then_some(now)
                    .or(existing.last_progress_increase_unix)
            });
        let snapshot = DropProgressSnapshot {
            drop_key,
            reward_name: drop.reward_name.clone(),
            campaign_name: drop.campaign_name.clone(),
            current_minutes_watched: drop.current_minutes_watched.max(0),
            required_minutes_watched: drop.required_minutes_watched.max(0),
            is_claimed: drop.is_claimed,
            observed_at_unix: now,
            last_progress_increase_unix: previous_progress_increase,
        };
        if let Some(existing) = counters
            .drop_progress
            .iter_mut()
            .find(|existing| existing.drop_key == snapshot.drop_key)
        {
            let claimed = existing.is_claimed;
            *existing = snapshot;
            existing.is_claimed |= claimed;
        } else {
            if counters.drop_progress.len() >= MAX_DROP_PROGRESS_ENTRIES {
                counters.drop_progress.remove(0);
            }
            counters.drop_progress.push(snapshot);
        }
    }

    pub(crate) fn set_watch_selection(
        &self,
        selected: &[(usize, usize, &str, &str, &'static str)],
    ) {
        let mut slots = self
            .watch_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = slots.clone();
        for slot in slots.iter_mut() {
            slot.selected = false;
            slot.selection_reason = None;
            slot.progress = WatchProgress::MeasurementUnavailable;
            slot.progress_age_seconds = None;
        }
        for (slot, channel_index, channel_id, broadcast_id, reason) in selected {
            let Some(status) = watch_slot_mut(&mut slots, *slot) else {
                continue;
            };
            let channel_key = anonymized_key("channel", [*channel_id]);
            let broadcast_key = anonymized_key("broadcast", [*broadcast_id]);
            let channel_changed = status.channel_key.as_deref() != Some(channel_key.as_str());
            if channel_changed {
                if let Some(previous) = previous.iter().find(|previous| {
                    previous.channel_key.as_deref() == Some(channel_key.as_str())
                        && previous.broadcast_key.as_deref() == Some(broadcast_key.as_str())
                }) {
                    status.last_activity_unix = previous.last_activity_unix;
                    status.last_accepted_watch_unix = previous.last_accepted_watch_unix;
                    status.consecutive_failures = previous.consecutive_failures;
                    status
                        .last_error_class
                        .clone_from(&previous.last_error_class);
                    status.last_server_confirmed_points_unix =
                        previous.last_server_confirmed_points_unix;
                    status.last_context_observed_unix = previous.last_context_observed_unix;
                } else {
                    let now = unix_now_infallible();
                    status.last_activity_unix = now;
                    status.last_accepted_watch_unix = None;
                    status.last_server_confirmed_points_unix = None;
                    status.last_context_observed_unix = None;
                    status.consecutive_failures = 0;
                    status.last_error_class = None;
                }
            } else if status.broadcast_key.as_deref() != Some(broadcast_key.as_str()) {
                status.last_activity_unix = unix_now_infallible();
                status.last_accepted_watch_unix = None;
                status.last_server_confirmed_points_unix = None;
                status.last_context_observed_unix = None;
                status.consecutive_failures = 0;
                status.last_error_class = None;
            }
            status.channel_index = Some(*channel_index);
            status.channel_key = Some(channel_key);
            status.broadcast_key = Some(broadcast_key);
            status.selected = true;
            status.selection_reason = Some((*reason).to_string());
        }
    }

    pub(crate) fn watch_slot_measurement(
        &self,
        slot: usize,
        last_server_confirmed_points_at: Option<tm_domain::OffsetDateTime>,
        last_context_observed_at: Option<tm_domain::OffsetDateTime>,
    ) {
        let mut slots = self
            .watch_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(status) = watch_slot_mut(&mut slots, slot) {
            if let Some(timestamp) = last_server_confirmed_points_at {
                status.last_server_confirmed_points_unix =
                    u64::try_from(timestamp.unix_timestamp()).ok();
            }
            if let Some(timestamp) = last_context_observed_at {
                status.last_context_observed_unix = u64::try_from(timestamp.unix_timestamp()).ok();
            }
        }
    }

    pub(crate) fn watch_slot_progress(
        &self,
        slot: usize,
        progress: WatchProgress,
        age: Option<u64>,
    ) {
        let mut slots = self
            .watch_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(status) = watch_slot_mut(&mut slots, slot) {
            status.progress = progress;
            status.progress_age_seconds = age;
        }
    }

    pub(crate) fn watch_slot_activity(&self, slot: usize) {
        let mut slots = self
            .watch_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(status) = watch_slot_mut(&mut slots, slot) {
            status.last_activity_unix = unix_now_infallible();
        }
    }

    pub(crate) fn watch_slot_success(&self, slot: usize) {
        let mut slots = self
            .watch_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(status) = watch_slot_mut(&mut slots, slot) {
            let now = unix_now_infallible();
            status.last_activity_unix = now;
            status.last_accepted_watch_unix = Some(now);
            status.consecutive_failures = 0;
            status.last_error_class = None;
        }
    }

    pub(crate) fn watch_slot_failure(&self, slot: usize, error_class: &'static str) {
        let mut slots = self
            .watch_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(status) = watch_slot_mut(&mut slots, slot) {
            status.last_activity_unix = unix_now_infallible();
            status.consecutive_failures = status
                .consecutive_failures
                .saturating_add(1)
                .min(MAX_CONSECUTIVE_FAILURES);
            status.last_error_class = Some(error_class.to_string());
        }
    }

    fn increment(&self, selector: impl FnOnce(&mut StatusCounters) -> &mut u64) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value = selector(&mut counters);
        *value = value.saturating_add(1).min(MAX_COUNTER_VALUE);
    }

    fn snapshot(&self) -> Vec<TaskStatus> {
        self.lock_tasks().values().cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn task_consecutive_failures(&self, name: &str) -> Option<u32> {
        self.lock_tasks()
            .get(name)
            .map(|task| task.consecutive_failures)
    }

    #[cfg(test)]
    pub(crate) fn successful_refreshes(&self) -> u64 {
        self.counters_snapshot().successful_refreshes
    }

    fn counters_snapshot(&self) -> StatusCounters {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn eventsub_snapshot(&self) -> Option<tm_pubsub::EventSubSetupReport> {
        self.eventsub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn pubsub_snapshot(&self) -> Option<tm_pubsub::PubSubSetupReport> {
        self.pubsub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn watch_slots_snapshot(&self) -> Vec<WatchSlotStatus> {
        self.watch_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn lock_tasks(&self) -> MutexGuard<'_, BTreeMap<String, TaskStatus>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) struct StatusReporter {
    path: PathBuf,
    started_at_unix: u64,
    health: HealthTracker,
    metrics: Arc<RuntimeMetrics>,
}

impl StatusReporter {
    pub(crate) fn ready(
        work_dir: &Path,
        health: HealthTracker,
        metrics: Arc<RuntimeMetrics>,
    ) -> Result<Self> {
        let reporter = Self {
            path: work_dir.join(STATUS_FILE_NAME),
            started_at_unix: unix_now()?,
            health,
            metrics,
        };
        reporter.supervision_heartbeat()?;
        Ok(reporter)
    }

    #[cfg(test)]
    pub(crate) fn heartbeat(&self) -> Result<()> {
        let (status, now) = self.publish_heartbeat()?;
        validate_status(&status, now)
    }

    pub(crate) fn supervision_heartbeat(&self) -> Result<()> {
        let (status, now) = self.publish_heartbeat()?;
        validate_status_for_supervision(&status, now)
    }

    fn publish_heartbeat(&self) -> Result<(RuntimeStatus, u64)> {
        let now = unix_now()?;
        let status = RuntimeStatus {
            schema_version: STATUS_SCHEMA_VERSION,
            state: String::from("ready"),
            started_at_unix: self.started_at_unix,
            heartbeat_at_unix: now,
            version: String::from(build_info::VERSION),
            revision: String::from(build_info::GIT_REVISION),
            target: String::from(build_info::TARGET),
            tasks: self.health.snapshot(),
            counters: self.health.counters_snapshot(),
            watch_slots: self.health.watch_slots_snapshot(),
            runtime_metrics: self.metrics.snapshot(),
            eventsub: self.health.eventsub_snapshot(),
            pubsub: self.health.pubsub_snapshot(),
            prediction_journal: self
                .health
                .prediction_journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(crate::prediction_journal::PredictionPlacementJournal::capacity)
                .transpose()?,
        };
        atomic_json_write(&self.path, &status)?;
        Ok((status, now))
    }
}

pub(crate) fn check_health(work_dir: &Path) -> Result<()> {
    let path = work_dir.join(STATUS_FILE_NAME);
    let status = read_status(&path)?;
    validate_status(&status, unix_now()?)
}

pub(crate) fn print_status(work_dir: &Path) -> Result<()> {
    let status = read_status(&work_dir.join(STATUS_FILE_NAME))?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

#[derive(Serialize)]
struct SupportBundle {
    schema_version: u8,
    generated_at_unix: u64,
    version: &'static str,
    revision: &'static str,
    target: &'static str,
    runtime_status: Option<RuntimeStatus>,
    config_exists: bool,
    config_size_bytes: Option<u64>,
    cookie_file_count: usize,
    log_file_count: usize,
}

pub(crate) fn write_support_bundle(work_dir: &Path, destination: &Path) -> Result<()> {
    let config_path = work_dir.join("config.json");
    let bundle = SupportBundle {
        schema_version: 1,
        generated_at_unix: unix_now()?,
        version: build_info::VERSION,
        revision: build_info::GIT_REVISION,
        target: build_info::TARGET,
        runtime_status: read_status(&work_dir.join(STATUS_FILE_NAME)).ok(),
        config_exists: config_path.is_file(),
        config_size_bytes: fs::metadata(config_path).ok().map(|value| value.len()),
        cookie_file_count: count_files(&work_dir.join("cookies")),
        log_file_count: count_files(&work_dir.join("log")),
    };
    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    atomic_json_write(destination, &bundle)
}

fn read_status(path: &Path) -> Result<RuntimeStatus> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn validate_status(status: &RuntimeStatus, now: u64) -> Result<()> {
    validate_common_status(status, now, false)?;
    if let Some(pubsub) = &status.pubsub {
        for capability in &pubsub.capabilities {
            if capability.acknowledged_topics != capability.configured_topics
                || capability.failure_class.is_some()
            {
                return Err(anyhow!(
                    "pubsub capability {} is degraded ({}/{})",
                    capability.topic_class,
                    capability.acknowledged_topics,
                    capability.configured_topics
                ));
            }
        }
    }
    Ok(())
}

fn validate_status_for_supervision(status: &RuntimeStatus, now: u64) -> Result<()> {
    // Retry loops remain visible as degraded to --health, but active retries must not
    // terminate the process. Unexpected task exits are supervised separately.
    validate_common_status(status, now, true)
}

fn validate_common_status(status: &RuntimeStatus, now: u64, supervision: bool) -> Result<()> {
    if status.schema_version != STATUS_SCHEMA_VERSION || status.state != "ready" {
        return Err(anyhow!("runtime status is not ready"));
    }
    let age = now.saturating_sub(status.heartbeat_at_unix);
    if age > MAX_HEARTBEAT_AGE_SECONDS {
        return Err(anyhow!("runtime status is stale ({age}s old)"));
    }
    for task in &status.tasks {
        let task_timestamp = if supervision {
            task.last_activity_unix
        } else {
            task.last_success_unix
        };
        let task_age = now.saturating_sub(task_timestamp);
        if task_age > task.stale_after_seconds {
            return Err(anyhow!(
                "runtime task {} is {} ({task_age}s old)",
                task.name,
                if supervision { "inactive" } else { "stale" }
            ));
        }
        if !supervision && task.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            return Err(anyhow!(
                "runtime task {} has {} consecutive failures",
                task.name,
                task.consecutive_failures
            ));
        }
    }
    for slot in &status.watch_slots {
        if !slot.selected {
            continue;
        }
        let age = now.saturating_sub(slot.last_activity_unix);
        if age > MAX_WATCH_SLOT_AGE_SECONDS {
            return Err(anyhow!("watch slot {} is stale ({age}s old)", slot.slot));
        }
        if !supervision && slot.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            return Err(anyhow!(
                "watch slot {} has {} consecutive failures",
                slot.slot,
                slot.consecutive_failures
            ));
        }
    }
    Ok(())
}

fn count_files(path: &Path) -> usize {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .count()
}

fn watch_slot_mut(slots: &mut Vec<WatchSlotStatus>, slot: usize) -> Option<&mut WatchSlotStatus> {
    if slot >= MAX_WATCH_SLOTS {
        return None;
    }
    while slots.len() <= slot {
        let index = slots.len();
        slots.push(WatchSlotStatus {
            progress: WatchProgress::MeasurementUnavailable,
            progress_age_seconds: None,
            slot: index,
            channel_index: None,
            channel_key: None,
            broadcast_key: None,
            last_server_confirmed_points_unix: None,
            last_context_observed_unix: None,
            selected: false,
            selection_reason: None,
            last_activity_unix: unix_now_infallible(),
            last_accepted_watch_unix: None,
            consecutive_failures: 0,
            last_error_class: None,
        });
    }
    slots.get_mut(slot)
}

fn stable_drop_key(drop: &InventoryDrop) -> String {
    // The raw drop instance is required for the claim mutation but is viewer
    // scoped. Keep status output useful without publishing that identifier.
    anonymized_key(
        "drop",
        [
            &drop.campaign_name,
            &drop.reward_name,
            &drop.required_minutes_watched.to_string(),
            if drop.id.is_empty() {
                &drop.drop_instance_id
            } else {
                &drop.id
            },
            &drop.campaign_id,
        ],
    )
}

fn anonymized_key<const N: usize>(prefix: &str, parts: [&str; N]) -> String {
    let mut hash = 14_695_981_039_346_656_037_u64;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            hash ^= u64::from(b':');
            hash = hash.wrapping_mul(1_099_511_628_211);
        }
        for byte in part.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(1_099_511_628_211);
        }
    }
    format!("{prefix}-{hash:016x}")
}

fn atomic_json_write(path: &Path, value: &impl Serialize) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("runtime-status.json");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.write_all(&serde_json::to_vec_pretty(value)?)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        #[cfg(windows)]
        if path.is_file() {
            replace_windows_status_file(&temporary, path)
                .with_context(|| format!("replace {}", path.display()))?;
        } else {
            fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
        }
        #[cfg(not(windows))]
        fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_windows_status_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("runtime-status.json");
    let replacement_backup =
        path.with_file_name(format!(".{file_name}.{}.replace.tmp", std::process::id()));

    fs::rename(path, &replacement_backup)?;
    if let Err(error) = fs::rename(temporary, path) {
        let _ = fs::rename(&replacement_backup, path);
        return Err(error);
    }
    let _ = fs::remove_file(replacement_backup);
    Ok(())
}

fn unix_now() -> Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| anyhow!("system clock predates Unix epoch: {error}"))
}

fn unix_now_infallible() -> u64 {
    unix_now().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        atomic_json_write, check_health, validate_status, validate_status_for_supervision,
        write_support_bundle, HealthTracker, RuntimeMetrics, RuntimeMetricsSnapshot, RuntimeStatus,
        StatusCounters, StatusReporter, TaskStatus, MAX_DROP_PROGRESS_ENTRIES, STATUS_FILE_NAME,
        STATUS_SCHEMA_VERSION,
    };
    use tm_observability::{init_tracing, LoggerSettings, TracingInitOptions};
    use tm_twitch::InventoryDrop;

    #[test]
    fn ready_status_passes_health_check() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let health = HealthTracker::default();
        health.register("minute", std::time::Duration::from_secs(60));
        let reporter = StatusReporter::ready(
            directory.path(),
            health,
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        reporter.heartbeat()?;
        check_health(directory.path())?;
        assert!(directory.path().join(STATUS_FILE_NAME).is_file());
        Ok(())
    }

    #[test]
    fn missing_status_fails_health_check() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        assert!(check_health(directory.path()).is_err());
        Ok(())
    }

    #[test]
    fn journal_capacity_is_reported_without_identities_or_health_failure() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let journal =
            crate::prediction_journal::PredictionPlacementJournal::open(directory.path())?;
        let request = crate::prediction_journal::PredictionPlacementRequest {
            account_id: "private-account",
            channel_id: "private-channel",
            event_id: "private-event",
            choice: Some(0),
            outcome_id: "private-outcome",
            amount: 10,
            reserved_at_unix_seconds: 1,
        };
        assert!(journal.reserve(&request)?);
        let health = HealthTracker::default();
        health.set_journal(journal.clone());
        let reporter = StatusReporter::ready(
            directory.path(),
            health,
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        let (status, _) = reporter.publish_heartbeat()?;
        let serialized = serde_json::to_string(&status)?;
        assert!(!serialized.contains("private-"));
        let capacity = status
            .prediction_journal
            .ok_or_else(|| anyhow::anyhow!("missing journal capacity"))?;
        assert_eq!(capacity.unresolved_count, 1);
        assert_eq!(capacity.retained_count, 0);
        assert_eq!(
            capacity.bytes as u64,
            std::fs::metadata(directory.path().join("prediction-placements.json"))?.len()
        );
        let oversized = "x".repeat(256 * 1024);
        assert!(journal
            .reserve(&crate::prediction_journal::PredictionPlacementRequest {
                event_id: &oversized,
                ..request
            })
            .is_err());
        reporter.heartbeat()?;
        assert!(reporter
            .publish_heartbeat()?
            .0
            .prediction_journal
            .is_some_and(|capacity| capacity.capacity_blocked));
        journal.confirm(request.account_id, request.channel_id, request.event_id)?;
        reporter.heartbeat()?;
        let (status, _) = reporter.publish_heartbeat()?;
        assert_eq!(
            status
                .prediction_journal
                .map(|capacity| capacity.retained_count),
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn stale_or_repeatedly_failing_tasks_fail_health() {
        let mut status = RuntimeStatus {
            schema_version: STATUS_SCHEMA_VERSION,
            state: String::from("ready"),
            started_at_unix: 1,
            heartbeat_at_unix: 100,
            version: String::from("test"),
            revision: String::from("test"),
            target: String::from("test"),
            tasks: vec![TaskStatus {
                name: String::from("pubsub"),
                last_success_unix: 1,
                last_activity_unix: 100,
                consecutive_failures: 0,
                stale_after_seconds: 10,
                last_error_class: None,
            }],
            counters: StatusCounters::default(),
            watch_slots: Vec::new(),
            runtime_metrics: RuntimeMetricsSnapshot::default(),
            eventsub: None,
            pubsub: None,
            prediction_journal: None,
        };
        assert!(validate_status(&status, 100).is_err());
        assert!(validate_status_for_supervision(&status, 100).is_ok());
        status.tasks[0].last_success_unix = 100;
        status.tasks[0].consecutive_failures = 5;
        assert!(validate_status(&status, 100).is_err());
        assert!(validate_status_for_supervision(&status, 100).is_ok());
        status.tasks[0].last_activity_unix = 1;
        assert!(validate_status_for_supervision(&status, 100).is_err());
    }

    #[test]
    fn reporter_publishes_degraded_task_state_before_returning_an_error() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let health = HealthTracker::default();
        health.register("pubsub", std::time::Duration::from_secs(60));
        let reporter = StatusReporter::ready(
            directory.path(),
            health.clone(),
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        for _ in 0..5 {
            health.failure("pubsub", "connection-error");
        }

        assert!(reporter.heartbeat().is_err());
        reporter.supervision_heartbeat()?;
        assert!(check_health(directory.path()).is_err());
        Ok(())
    }

    #[test]
    fn activity_refreshes_supervision_without_hiding_failures() {
        let health = HealthTracker::default();
        health.register("eventsub", std::time::Duration::from_secs(60));
        health.failure("eventsub", "welcome-timeout");
        {
            let mut tasks = health.lock_tasks();
            match tasks.get_mut("eventsub") {
                Some(task) => task.last_activity_unix = 0,
                None => panic!("registered EventSub task must exist"),
            }
        }

        health.activity("eventsub");

        let Some(task) = health
            .snapshot()
            .into_iter()
            .find(|task| task.name == "eventsub")
        else {
            panic!("EventSub task must remain in the health snapshot");
        };
        assert!(task.last_activity_unix > 0);
        assert_eq!(task.consecutive_failures, 1);
        assert_eq!(task.last_error_class.as_deref(), Some("welcome-timeout"));
    }

    #[test]
    fn watch_slot_health_does_not_share_failures_between_slots() {
        let health = HealthTracker::default();
        health.set_watch_selection(&[
            (0, 0, "channel-a", "broadcast-a", "watch-order"),
            (1, 1, "channel-b", "broadcast-b", "fair-rotation"),
        ]);
        for _ in 0..4 {
            health.watch_slot_failure(1, "watch-timeout");
        }
        health.watch_slot_success(0);

        let slots = health.watch_slots_snapshot();
        assert_eq!(slots[0].consecutive_failures, 0);
        assert_eq!(slots[1].consecutive_failures, 4);
        assert_eq!(slots[1].last_error_class.as_deref(), Some("watch-timeout"));
    }

    #[test]
    fn watch_slot_health_resets_when_the_broadcast_changes() -> anyhow::Result<()> {
        let health = HealthTracker::default();
        health.set_watch_selection(&[(0, 0, "channel-a", "broadcast-a", "watch-order")]);
        health.watch_slot_success(0);
        health.watch_slot_failure(0, "watch-timeout");
        health.watch_slot_progress(0, super::WatchProgress::FirstCreditOverdue, Some(1_800));
        let serialized = serde_json::to_value(&health.watch_slots_snapshot()[0])?;
        assert_eq!(serialized["progress"], "first_credit_overdue");
        assert_eq!(serialized["progress_age_seconds"], 1_800);
        health.set_watch_selection(&[(0, 0, "channel-a", "broadcast-b", "watch-order")]);

        let slot = &health.watch_slots_snapshot()[0];
        assert!(slot.last_accepted_watch_unix.is_none());
        assert_eq!(slot.consecutive_failures, 0);
        assert!(slot.broadcast_key.as_deref().is_some());
        assert_eq!(slot.progress, super::WatchProgress::MeasurementUnavailable);
        assert_eq!(slot.progress_age_seconds, None);
        Ok(())
    }

    #[test]
    fn watch_slot_health_does_not_copy_a_new_broadcast_when_a_channel_moves_slots() {
        let health = HealthTracker::default();
        health.set_watch_selection(&[(0, 0, "channel-a", "broadcast-a", "watch-order")]);
        health.watch_slot_success(0);
        health.watch_slot_failure(0, "watch-timeout");

        health.set_watch_selection(&[(1, 0, "channel-a", "broadcast-b", "fair-rotation")]);

        let slots = health.watch_slots_snapshot();
        assert_eq!(slots[1].consecutive_failures, 0);
        assert!(slots[1].last_accepted_watch_unix.is_none());
        assert!(slots[1].last_server_confirmed_points_unix.is_none());
        assert!(slots[1].last_context_observed_unix.is_none());
    }

    #[test]
    fn drop_progress_is_identified_timestamped_and_bounded() {
        let health = HealthTracker::default();
        for index in 0..=MAX_DROP_PROGRESS_ENTRIES {
            health.record_drop_progress(&InventoryDrop {
                drop_instance_id: format!("drop-{index}"),
                reward_name: String::from("reward"),
                campaign_name: String::from("campaign"),
                current_minutes_watched: i64::try_from(index).unwrap_or(i64::MAX),
                required_minutes_watched: 60,
                is_claimed: false,
                ..InventoryDrop::default()
            });
        }

        let progress = health.counters_snapshot().drop_progress;
        assert_eq!(progress.len(), MAX_DROP_PROGRESS_ENTRIES);
        assert_eq!(progress[0].current_minutes_watched, 1);
        assert_ne!(progress[0].drop_key, "drop-1");
        assert!(progress.iter().all(|entry| entry.observed_at_unix > 0));
    }

    #[test]
    fn drop_identity_survives_claim_id_and_stale_inventory() {
        let health = HealthTracker::default();
        let mut drop = InventoryDrop {
            id: "reward-id".into(),
            campaign_id: "campaign-id".into(),
            current_minutes_watched: 10,
            required_minutes_watched: 30,
            ..Default::default()
        };
        health.record_drop_progress(&drop);
        drop.current_minutes_watched = 30;
        drop.drop_instance_id = "private-claim-id".into();
        drop.is_claimed = true;
        health.record_drop_progress(&drop);
        drop.is_claimed = false;
        health.record_drop_progress(&drop);
        let entries = health.counters_snapshot().drop_progress;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_claimed);
        assert!(entries[0].last_progress_increase_unix.is_some());
        assert!(!entries[0].drop_key.contains("private-claim-id"));
    }

    #[test]
    fn drop_progress_keeps_confirmed_increase_timestamp() {
        let health = HealthTracker::default();
        let mut drop = InventoryDrop {
            drop_instance_id: String::from("private-drop"),
            reward_name: String::from("reward"),
            campaign_name: String::from("campaign"),
            current_minutes_watched: 10,
            required_minutes_watched: 60,
            is_claimed: false,
            ..InventoryDrop::default()
        };
        health.record_drop_progress(&drop);
        assert!(health.counters_snapshot().drop_progress[0]
            .last_progress_increase_unix
            .is_none());
        drop.current_minutes_watched = 11;
        health.record_drop_progress(&drop);
        let first_increase =
            health.counters_snapshot().drop_progress[0].last_progress_increase_unix;
        assert!(first_increase.is_some());
        drop.current_minutes_watched = 9;
        health.record_drop_progress(&drop);
        assert_eq!(
            health.counters_snapshot().drop_progress[0].last_progress_increase_unix,
            first_increase
        );
    }

    #[test]
    fn status_counters_are_bounded_and_redacted() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let health = HealthTracker::default();
        health.record_claim();
        health.record_bet();
        health.record_refresh();
        health.failure("eventsub", "connection-reset");
        let reporter = StatusReporter::ready(
            directory.path(),
            health,
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        reporter.heartbeat()?;
        let status = std::fs::read_to_string(directory.path().join(STATUS_FILE_NAME))?;
        assert!(status.contains("\"claims\": 1"));
        assert!(status.contains("\"bets\": 1"));
        assert!(status.contains("connection-reset"));
        assert!(!status.contains("auth-token"));
        Ok(())
    }

    #[test]
    fn status_tracks_pubsub_capability_without_topic_identifiers() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let health = HealthTracker::default();
        health.record_pubsub_setup(tm_pubsub::PubSubSetupReport {
            connection_count: 1,
            total_topics: 1,
            capabilities: vec![tm_pubsub::PubSubCapabilityStatus {
                topic_class: String::from("prediction-channel"),
                configured_topics: 1,
                acknowledged_topics: 0,
                last_message_unix: None,
                reconnects: 0,
                failure_class: None,
            }],
        });
        assert!(health.record_pubsub_acknowledgement("prediction-channel"));
        health.record_pubsub_message("prediction-channel");
        health.record_pubsub_disconnect("prediction-channel", 1, "connection-reset");
        assert!(!health.pubsub_ready());
        let degraded_reporter = StatusReporter::ready(
            directory.path(),
            health.clone(),
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        assert!(degraded_reporter.heartbeat().is_err());
        degraded_reporter.supervision_heartbeat()?;
        let degraded = std::fs::read_to_string(directory.path().join(STATUS_FILE_NAME))?;
        assert!(degraded.contains("connection-reset"));
        assert!(degraded.contains("\"acknowledged_topics\": 0"));

        assert!(health.record_pubsub_acknowledgement("prediction-channel"));
        let reporter = StatusReporter::ready(
            directory.path(),
            health,
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        reporter.heartbeat()?;

        let status = std::fs::read_to_string(directory.path().join(STATUS_FILE_NAME))?;
        assert!(status.contains("prediction-channel"));
        assert!(status.contains("\"acknowledged_topics\": 1"));
        assert!(status.contains("\"last_message_unix\":"));
        assert!(status.contains("\"reconnects\": 1"));
        assert!(!status.contains("channel-456"));
        assert!(!status.contains("auth-token"));
        Ok(())
    }

    #[test]
    fn pubsub_failure_clears_only_after_every_disconnected_topic_is_reacknowledged() {
        let health = HealthTracker::default();
        health.record_pubsub_setup(tm_pubsub::PubSubSetupReport {
            connection_count: 1,
            total_topics: 2,
            capabilities: vec![tm_pubsub::PubSubCapabilityStatus {
                topic_class: String::from("prediction-channel"),
                configured_topics: 2,
                acknowledged_topics: 0,
                last_message_unix: None,
                reconnects: 0,
                failure_class: None,
            }],
        });

        assert!(!health.record_pubsub_acknowledgement("prediction-channel"));
        assert!(health.record_pubsub_acknowledgement("prediction-channel"));
        health.record_pubsub_disconnect("prediction-channel", 2, "connection-reset");
        assert!(!health.pubsub_ready());
        assert!(!health.record_pubsub_acknowledgement("prediction-channel"));
        assert!(health.record_pubsub_acknowledgement("prediction-channel"));
    }

    #[test]
    fn status_atomic_write_replaces_files_and_cleans_failed_temporary_files() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(STATUS_FILE_NAME);
        atomic_json_write(&path, &serde_json::json!({"old": true}))?;
        atomic_json_write(&path, &serde_json::json!({"new": true}))?;
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(value["new"], true);

        let target_directory = directory.path().join("directory-target.json");
        std::fs::create_dir_all(&target_directory)?;
        assert!(atomic_json_write(&target_directory, &serde_json::json!({})).is_err());
        let temporary = directory
            .path()
            .join(format!(".directory-target.json.{}.tmp", std::process::id()));
        assert!(!temporary.exists());
        Ok(())
    }

    #[test]
    fn privacy_canaries_do_not_escape_anonymized_outputs() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let markers = [
            "CANARY_USERNAME",
            "CANARY_CHANNEL_ID",
            "CANARY_BALANCE",
            "CANARY_AUTH_TOKEN",
            "CANARY_COOKIE",
            "CANARY_WEBHOOK",
            "CANARY_FILESYSTEM_PATH",
            "CANARY_CHAT_MESSAGE",
            "CANARY_RAW_PAYLOAD",
        ];
        init_tracing(&TracingInitOptions {
            settings: LoggerSettings {
                save: true,
                anonymize_logs: true,
                ..LoggerSettings::default()
            },
            base_dir: directory.path().to_path_buf(),
            username: markers[0].to_string(),
            timezone: None,
        })?;
        tracing::info!(
            operation = "privacy-canary",
            channel_id = markers[1],
            balance = markers[2],
            auth_token = markers[3],
            cookie = markers[4],
            webhook = markers[5],
            filesystem_path = markers[6],
            chat_message = markers[7],
            raw_payload = markers[8],
            "privacy canary"
        );
        std::fs::write(
            directory.path().join("config.json"),
            format!(r#"{{"private_config":"{}"}}"#, markers[3]),
        )?;
        std::fs::create_dir_all(directory.path().join("cookies"))?;
        std::fs::write(directory.path().join("cookies/fixture.json"), markers[4])?;
        std::fs::write(
            directory.path().join("log/fixture.log"),
            format!("Authorization: {}\n{}", markers[3], markers[5]),
        )?;
        let reporter = StatusReporter::ready(
            directory.path(),
            HealthTracker::default(),
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        reporter.heartbeat()?;
        let status_path = directory.path().join(STATUS_FILE_NAME);
        let log = std::fs::read_to_string(directory.path().join("log/miner.log"))?;
        let status = std::fs::read_to_string(&status_path)?;

        let destination = directory.path().join("support.json");
        write_support_bundle(directory.path(), &destination)?;
        let bundle = std::fs::read_to_string(destination)?;
        for marker in markers {
            assert!(!log.contains(marker));
            assert!(!status.contains(marker));
            assert!(!bundle.contains(marker));
        }
        assert!(!bundle.contains("Authorization"));
        assert!(!bundle.contains("raw_twitch_response"));
        assert!(bundle.contains("config_size_bytes"));
        assert!(bundle.contains("cookie_file_count"));
        assert!(bundle.contains("log_file_count"));
        Ok(())
    }
}
