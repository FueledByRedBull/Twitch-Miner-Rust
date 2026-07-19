use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tm_domain::{OffsetDateTime, Stream, Streamer};

use crate::status::HealthTracker;

pub(crate) const STREAK_CACHE_FILE_NAME: &str = "streak-cache.json";
const STREAK_CACHE_VERSION: u64 = 1;
const MAX_CACHE_ENTRIES: usize = 1_000;
const MAX_CACHE_BYTES: u64 = 256 * 1024;
const CACHE_MAX_AGE_SECONDS: i64 = 48 * 60 * 60;
const MAX_FUTURE_SKEW_SECONDS: i64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreakCacheEntry {
    channel_id: String,
    streak_count: Option<u32>,
    resolved_at: Option<OffsetDateTime>,
    expires_at: Option<OffsetDateTime>,
    updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreakCache {
    version: u64,
    entries: Vec<StreakCacheEntry>,
}

impl Default for StreakCache {
    fn default() -> Self {
        Self {
            version: STREAK_CACHE_VERSION,
            entries: Vec::new(),
        }
    }
}

impl StreakCache {
    pub(crate) fn load(work_dir: &Path, now: OffsetDateTime) -> Result<Self> {
        let path = cache_path(work_dir);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        if metadata.len() > MAX_CACHE_BYTES {
            return Err(anyhow!("streak cache exceeds {MAX_CACHE_BYTES} bytes"));
        }
        let mut cache: Self = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("decode {}", path.display()))?;
        if cache.version != STREAK_CACHE_VERSION {
            return Err(anyhow!(
                "unsupported streak cache version {}",
                cache.version
            ));
        }
        cache.prune(now);
        Ok(cache)
    }

    pub(crate) fn save(&mut self, work_dir: &Path, now: OffsetDateTime) -> Result<()> {
        self.prune(now);
        atomic_write(&cache_path(work_dir), &serde_json::to_vec(self)?)
    }

    pub(crate) fn record_milestone(
        &mut self,
        channel_id: &str,
        streak_count: Option<u32>,
        resolved_at: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            return;
        }
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.channel_id == channel_id)
        {
            entry.streak_count = streak_count;
            entry.resolved_at = Some(resolved_at);
            entry.expires_at = expires_at;
            entry.updated_at = now;
        } else {
            self.entries.push(StreakCacheEntry {
                channel_id: channel_id.to_string(),
                streak_count,
                resolved_at: Some(resolved_at),
                expires_at,
                updated_at: now,
            });
        }
        self.prune(now);
    }

    pub(crate) fn apply_to_stream(
        &self,
        channel_id: &str,
        stream: &mut Stream,
        stream_created_at: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> bool {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.channel_id == channel_id && entry_is_fresh(entry, now))
        else {
            return false;
        };
        stream.watch_streak_count = entry.streak_count;
        stream.watch_streak_expires_at = entry.expires_at;
        let resolved_for_current_stream = stream_created_at.is_some_and(|created_at| {
            entry
                .resolved_at
                .is_some_and(|resolved_at| resolved_at >= created_at)
                && entry.expires_at.is_none_or(|expires_at| expires_at > now)
        });
        if resolved_for_current_stream {
            stream.watch_streak_missing = false;
            stream.watch_streak_resolved_at = entry.resolved_at;
        }
        resolved_for_current_stream
    }

    fn reconcile_streamers(&mut self, streamers: &[Streamer], now: OffsetDateTime) -> bool {
        let mut changed = false;
        for streamer in streamers {
            let Some(stream) = streamer.stream.as_ref() else {
                continue;
            };
            let Some(resolved_at) = stream.watch_streak_resolved_at else {
                continue;
            };
            let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| entry.channel_id == streamer.channel_id)
            else {
                self.entries.push(StreakCacheEntry {
                    channel_id: streamer.channel_id.clone(),
                    streak_count: stream.watch_streak_count,
                    resolved_at: Some(resolved_at),
                    expires_at: stream.watch_streak_expires_at,
                    updated_at: now,
                });
                changed = true;
                continue;
            };
            if entry.streak_count != stream.watch_streak_count
                || entry.resolved_at != Some(resolved_at)
                || entry.expires_at != stream.watch_streak_expires_at
            {
                entry.streak_count = stream.watch_streak_count;
                entry.resolved_at = Some(resolved_at);
                entry.expires_at = stream.watch_streak_expires_at;
                entry.updated_at = now;
                changed = true;
            }
        }
        let before = self.entries.len();
        self.prune(now);
        changed || self.entries.len() != before
    }

    fn prune(&mut self, now: OffsetDateTime) {
        self.entries.retain(|entry| entry_is_fresh(entry, now));
        self.entries
            .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        self.entries.truncate(MAX_CACHE_ENTRIES);
    }
}

fn entry_is_fresh(entry: &StreakCacheEntry, now: OffsetDateTime) -> bool {
    let age_seconds = (now - entry.updated_at).whole_seconds();
    !entry.channel_id.trim().is_empty()
        && (-MAX_FUTURE_SKEW_SECONDS..=CACHE_MAX_AGE_SECONDS).contains(&age_seconds)
}

fn cache_path(work_dir: &Path) -> PathBuf {
    work_dir.join(STREAK_CACHE_FILE_NAME)
}

pub(crate) fn spawn_streak_cache_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    mut cache: StreakCache,
    work_dir: PathBuf,
    health: HealthTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        let mut revisions = runtime.subscribe_state_changes();
        let mut flush = tokio::time::interval(std::time::Duration::from_secs(30));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty = true;
        loop {
            tokio::select! {
                stop_result = stop.changed() => {
                    if stop_result.is_err() || *stop.borrow() {
                        break;
                    }
                }
                revision_result = revisions.changed() => {
                    if revision_result.is_err() {
                        break;
                    }
                    dirty = true;
                }
                _ = flush.tick() => {
                    if persist_runtime_cache(&runtime, &mut cache, &work_dir, dirty).await {
                        health.success("streak-cache");
                        dirty = false;
                    } else {
                        health.failure("streak-cache", "write");
                    }
                }
            }
        }
        let _ = persist_runtime_cache(&runtime, &mut cache, &work_dir, true).await;
    })
}

async fn persist_runtime_cache(
    runtime: &tm_runtime::RuntimeHandle,
    cache: &mut StreakCache,
    work_dir: &Path,
    dirty: bool,
) -> bool {
    if !dirty {
        return true;
    }
    let now = crate::utilities::time_now();
    let Ok(snapshot) = runtime.state_snapshot().await else {
        tracing::warn!(
            task = "streak-cache",
            error_class = "snapshot",
            "streak cache snapshot failed"
        );
        return false;
    };
    let changed = cache.reconcile_streamers(&snapshot.streamers, now);
    if !changed {
        return true;
    }
    if cache.save(work_dir, now).is_err() {
        tracing::warn!(
            task = "streak-cache",
            error_class = "write",
            "streak cache write failed"
        );
        return false;
    }
    true
}

fn atomic_write(path: &Path, payload: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(STREAK_CACHE_FILE_NAME);
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = open_private_file(&temporary)?;
        file.write_all(payload)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        match fs::rename(&temporary, path) {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(_) if path.is_file() => replace_windows_file(&temporary, path),
            Err(error) => Err(error),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("write {}", path.display()))
}

#[cfg(unix)]
fn open_private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
}

#[cfg(windows)]
fn replace_windows_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    let replacement = path.with_file_name(format!(
        ".{}.{}.replace.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(STREAK_CACHE_FILE_NAME),
        std::process::id()
    ));
    fs::rename(path, &replacement)?;
    if let Err(error) = fs::rename(temporary, path) {
        let _ = fs::rename(&replacement, path);
        return Err(error);
    }
    let _ = fs::remove_file(replacement);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(unix: i64) -> OffsetDateTime {
        match OffsetDateTime::from_unix_timestamp(unix) {
            Ok(value) => value,
            Err(error) => panic!("invalid fixture timestamp: {error}"),
        }
    }

    #[test]
    fn cache_applies_only_a_fresh_resolution_for_the_current_stream() {
        let mut cache = StreakCache::default();
        cache.record_milestone("100", Some(7), ts(200), Some(ts(500)), ts(200));
        let mut current = Stream::default();
        assert!(cache.apply_to_stream("100", &mut current, Some(ts(100)), ts(250)));
        assert!(!current.watch_streak_missing);
        assert_eq!(current.watch_streak_count, Some(7));

        let mut newer = Stream::default();
        assert!(!cache.apply_to_stream("100", &mut newer, Some(ts(300)), ts(350)));
        assert!(newer.watch_streak_missing);
        assert_eq!(newer.watch_streak_count, Some(7));

        let mut expired = Stream::default();
        assert!(!cache.apply_to_stream("100", &mut expired, Some(ts(100)), ts(501)));
    }

    #[test]
    fn cache_roundtrip_prunes_stale_and_bounds_entries() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let now = ts(200_000);
        let mut cache = StreakCache::default();
        cache.entries.push(StreakCacheEntry {
            channel_id: String::from("stale"),
            streak_count: Some(1),
            resolved_at: Some(ts(1)),
            expires_at: None,
            updated_at: now
                - std::time::Duration::from_secs((CACHE_MAX_AGE_SECONDS + 1).cast_unsigned()),
        });
        for index in 0..=MAX_CACHE_ENTRIES {
            cache.record_milestone(
                &format!("channel-{index}"),
                Some(u32::try_from(index)?),
                now,
                None,
                now,
            );
        }
        cache.save(dir.path(), now)?;

        let loaded = StreakCache::load(dir.path(), now)?;
        assert_eq!(loaded.version, STREAK_CACHE_VERSION);
        assert_eq!(loaded.entries.len(), MAX_CACHE_ENTRIES);
        assert!(loaded
            .entries
            .iter()
            .all(|entry| entry.channel_id != "stale"));
        Ok(())
    }
}
