use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tm_runtime::{RuntimeMetrics, RuntimeMetricsSnapshot};

use crate::build_info;

pub(crate) const STATUS_FILE_NAME: &str = "runtime-status.json";
const STATUS_SCHEMA_VERSION: u8 = 5;
const MAX_HEARTBEAT_AGE_SECONDS: u64 = 120;
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
const MAX_COUNTER_VALUE: u64 = 1_000_000_000;
const MAX_DROP_PROGRESS_ENTRIES: usize = 16;

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
    runtime_metrics: RuntimeMetricsSnapshot,
    eventsub: Option<tm_pubsub::EventSubSetupReport>,
    pubsub: Option<tm_pubsub::PubSubSetupReport>,
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
    current_minutes_watched: i64,
    required_minutes_watched: i64,
    is_claimed: bool,
}

#[derive(Clone, Default)]
pub(crate) struct HealthTracker {
    tasks: Arc<Mutex<BTreeMap<String, TaskStatus>>>,
    counters: Arc<Mutex<StatusCounters>>,
    eventsub: Arc<Mutex<Option<tm_pubsub::EventSubSetupReport>>>,
    pubsub: Arc<Mutex<Option<tm_pubsub::PubSubSetupReport>>>,
}

impl HealthTracker {
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

    pub(crate) fn record_drop_progress(
        &self,
        current_minutes_watched: i64,
        required_minutes_watched: i64,
        is_claimed: bool,
    ) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if counters.drop_progress.len() < MAX_DROP_PROGRESS_ENTRIES {
            counters.drop_progress.push(DropProgressSnapshot {
                current_minutes_watched: current_minutes_watched.max(0),
                required_minutes_watched: required_minutes_watched.max(0),
                is_claimed,
            });
        }
    }

    pub(crate) fn clear_drop_progress(&self) {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drop_progress
            .clear();
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
            runtime_metrics: self.metrics.snapshot(),
            eventsub: self.health.eventsub_snapshot(),
            pubsub: self.health.pubsub_snapshot(),
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
        StatusCounters, StatusReporter, TaskStatus, STATUS_FILE_NAME, STATUS_SCHEMA_VERSION,
    };

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
            runtime_metrics: RuntimeMetricsSnapshot::default(),
            eventsub: None,
            pubsub: None,
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
    fn support_bundle_contains_metadata_not_file_contents() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let config_marker = "SHOULD_NOT_APPEAR_CONFIG";
        let cookie_marker = "SHOULD_NOT_APPEAR_COOKIE";
        let webhook_marker = "SHOULD_NOT_APPEAR_WEBHOOK";
        let response_marker = "SHOULD_NOT_APPEAR_RESPONSE";
        std::fs::write(
            directory.path().join("config.json"),
            format!(r#"{{"private_config":"{config_marker}"}}"#),
        )?;
        std::fs::create_dir_all(directory.path().join("cookies"))?;
        std::fs::write(directory.path().join("cookies/fixture.json"), cookie_marker)?;
        std::fs::create_dir_all(directory.path().join("log"))?;
        std::fs::write(
            directory.path().join("log/fixture.log"),
            format!(
                "Authorization: {cookie_marker}\nhttps://example.invalid/hooks/{webhook_marker}"
            ),
        )?;
        let reporter = StatusReporter::ready(
            directory.path(),
            HealthTracker::default(),
            std::sync::Arc::new(RuntimeMetrics::default()),
        )?;
        reporter.heartbeat()?;
        let status_path = directory.path().join(STATUS_FILE_NAME);
        let mut raw_status: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&status_path)?)?;
        raw_status["raw_twitch_response"] = serde_json::Value::String(response_marker.to_string());
        atomic_json_write(&status_path, &raw_status)?;

        let destination = directory.path().join("support.json");
        write_support_bundle(directory.path(), &destination)?;
        let bundle = std::fs::read_to_string(destination)?;
        for marker in [
            config_marker,
            cookie_marker,
            webhook_marker,
            response_marker,
            "Authorization",
            "raw_twitch_response",
        ] {
            assert!(!bundle.contains(marker));
        }
        assert!(bundle.contains("config_size_bytes"));
        assert!(bundle.contains("cookie_file_count"));
        assert!(bundle.contains("log_file_count"));
        Ok(())
    }
}
