use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::build_info;

pub(crate) const STATUS_FILE_NAME: &str = "runtime-status.json";
const STATUS_SCHEMA_VERSION: u8 = 2;
const MAX_HEARTBEAT_AGE_SECONDS: u64 = 120;
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TaskStatus {
    name: String,
    last_success_unix: u64,
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
}

#[derive(Clone, Default)]
pub(crate) struct HealthTracker {
    tasks: Arc<Mutex<BTreeMap<String, TaskStatus>>>,
}

impl HealthTracker {
    pub(crate) fn register(&self, name: &'static str, stale_after: std::time::Duration) {
        self.lock_tasks().insert(
            name.to_string(),
            TaskStatus {
                name: name.to_string(),
                last_success_unix: unix_now_infallible(),
                consecutive_failures: 0,
                stale_after_seconds: stale_after.as_secs(),
                last_error_class: None,
            },
        );
    }

    pub(crate) fn success(&self, name: &'static str) {
        if let Some(task) = self.lock_tasks().get_mut(name) {
            task.last_success_unix = unix_now_infallible();
            task.consecutive_failures = 0;
            task.last_error_class = None;
        }
    }

    pub(crate) fn failure(&self, name: &'static str, error_class: &'static str) {
        if let Some(task) = self.lock_tasks().get_mut(name) {
            task.consecutive_failures = task.consecutive_failures.saturating_add(1);
            task.last_error_class = Some(error_class.to_string());
        }
    }

    fn snapshot(&self) -> Vec<TaskStatus> {
        self.lock_tasks().values().cloned().collect()
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
}

impl StatusReporter {
    pub(crate) fn ready(work_dir: &Path, health: HealthTracker) -> Result<Self> {
        let reporter = Self {
            path: work_dir.join(STATUS_FILE_NAME),
            started_at_unix: unix_now()?,
            health,
        };
        reporter.heartbeat()?;
        Ok(reporter)
    }

    pub(crate) fn heartbeat(&self) -> Result<()> {
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
        };
        atomic_json_write(&self.path, &status)?;
        validate_status(&status, now)
    }
}

pub(crate) fn check_health(work_dir: &Path) -> Result<()> {
    let path = work_dir.join(STATUS_FILE_NAME);
    let status = read_status(&path)?;
    validate_status(&status, unix_now()?)
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
    if status.schema_version != STATUS_SCHEMA_VERSION || status.state != "ready" {
        return Err(anyhow!("runtime status is not ready"));
    }
    let age = now.saturating_sub(status.heartbeat_at_unix);
    if age > MAX_HEARTBEAT_AGE_SECONDS {
        return Err(anyhow!("runtime status is stale ({age}s old)"));
    }
    for task in &status.tasks {
        let task_age = now.saturating_sub(task.last_success_unix);
        if task_age > task.stale_after_seconds {
            return Err(anyhow!(
                "runtime task {} is stale ({task_age}s old)",
                task.name
            ));
        }
        if task.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
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
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("replace {}", path.display()))?;
    }
    fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
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
        check_health, validate_status, write_support_bundle, HealthTracker, RuntimeStatus,
        StatusReporter, TaskStatus, STATUS_FILE_NAME, STATUS_SCHEMA_VERSION,
    };

    #[test]
    fn ready_status_passes_health_check() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let health = HealthTracker::default();
        health.register("minute", std::time::Duration::from_secs(60));
        let reporter = StatusReporter::ready(directory.path(), health)?;
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
                consecutive_failures: 0,
                stale_after_seconds: 10,
                last_error_class: None,
            }],
        };
        assert!(validate_status(&status, 100).is_err());
        status.tasks[0].last_success_unix = 100;
        status.tasks[0].consecutive_failures = 5;
        assert!(validate_status(&status, 100).is_err());
    }

    #[test]
    fn reporter_publishes_degraded_task_state_before_returning_an_error() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let health = HealthTracker::default();
        health.register("pubsub", std::time::Duration::from_secs(60));
        let reporter = StatusReporter::ready(directory.path(), health.clone())?;
        for _ in 0..5 {
            health.failure("pubsub", "connection-error");
        }

        assert!(reporter.heartbeat().is_err());
        assert!(check_health(directory.path()).is_err());
        Ok(())
    }

    #[test]
    fn support_bundle_contains_metadata_not_file_contents() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("config.json"),
            r#"{"secret":"value"}"#,
        )?;
        let destination = directory.path().join("support.json");
        write_support_bundle(directory.path(), &destination)?;
        let bundle = std::fs::read_to_string(destination)?;
        assert!(!bundle.contains("secret"));
        assert!(!bundle.contains("value"));
        assert!(bundle.contains("config_size_bytes"));
        Ok(())
    }
}
