use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tm_auth::normalize_username;
use tm_domain::OffsetDateTime;

use crate::streak_cache::atomic_write;

pub(crate) const IDENTITY_CACHE_FILE_NAME: &str = "startup-identity.json";

const IDENTITY_CACHE_VERSION: u64 = 1;
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CACHE_BYTES: u64 = 64 * 1024;
const CACHE_MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
const MAX_FUTURE_SKEW_SECONDS: i64 = 5 * 60;
const MAX_CHANNEL_ID_LENGTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IdentityCacheEntry {
    pub(crate) configured_login: String,
    pub(crate) channel_id: String,
    pub(crate) verified_login: String,
    pub(crate) verified_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct IdentityCache {
    version: u64,
    entries: Vec<IdentityCacheEntry>,
}

impl Default for IdentityCache {
    fn default() -> Self {
        Self {
            version: IDENTITY_CACHE_VERSION,
            entries: Vec::new(),
        }
    }
}

impl IdentityCache {
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
            return Err(anyhow!(
                "startup identity cache exceeds {MAX_CACHE_BYTES} bytes"
            ));
        }
        let file = fs::File::open(&path).with_context(|| format!("read {}", path.display()))?;
        let mut bytes = Vec::new();
        file.take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {}", path.display()))?;
        let max_cache_bytes = usize::try_from(MAX_CACHE_BYTES)
            .map_err(|_| anyhow!("startup identity cache size does not fit this platform"))?;
        if bytes.len() > max_cache_bytes {
            return Err(anyhow!(
                "startup identity cache exceeds {MAX_CACHE_BYTES} bytes"
            ));
        }
        let mut cache: Self =
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
        if cache.version != IDENTITY_CACHE_VERSION {
            return Err(anyhow!(
                "unsupported startup identity cache version {}",
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

    pub(crate) fn lookup(
        &self,
        configured_login: &str,
        now: OffsetDateTime,
    ) -> Option<IdentityCacheEntry> {
        let configured_login = normalize_username(configured_login).ok()?;
        self.entries
            .iter()
            .find(|entry| entry.configured_login == configured_login && entry_is_fresh(entry, now))
            .cloned()
    }

    pub(crate) fn record(
        &mut self,
        configured_login: &str,
        channel_id: &str,
        verified_login: &str,
        verified_at: OffsetDateTime,
    ) {
        let Ok(configured_login) = normalize_username(configured_login) else {
            return;
        };
        let Ok(verified_login) = normalize_username(verified_login) else {
            return;
        };
        let channel_id = channel_id.trim();
        let entry = IdentityCacheEntry {
            configured_login,
            channel_id: channel_id.to_string(),
            verified_login,
            verified_at,
        };
        if !entry_is_fresh(&entry, verified_at) {
            return;
        }

        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|existing| existing.configured_login == entry.configured_login)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
        self.prune(verified_at);
    }

    fn prune(&mut self, now: OffsetDateTime) {
        self.entries.retain(|entry| entry_is_fresh(entry, now));
        self.entries
            .sort_by(|left, right| right.verified_at.cmp(&left.verified_at));

        let mut seen_logins = HashSet::with_capacity(self.entries.len());
        self.entries
            .retain(|entry| seen_logins.insert(entry.configured_login.clone()));
        self.entries.truncate(MAX_CACHE_ENTRIES);
    }
}

fn entry_is_fresh(entry: &IdentityCacheEntry, now: OffsetDateTime) -> bool {
    let age_seconds = (now - entry.verified_at).whole_seconds();
    valid_login(&entry.configured_login)
        && valid_login(&entry.verified_login)
        && valid_channel_id(&entry.channel_id)
        && (-MAX_FUTURE_SKEW_SECONDS..=CACHE_MAX_AGE_SECONDS).contains(&age_seconds)
}

fn valid_login(login: &str) -> bool {
    normalize_username(login).is_ok_and(|normalized| normalized == login)
}

fn valid_channel_id(channel_id: &str) -> bool {
    !channel_id.is_empty()
        && channel_id.len() <= MAX_CHANNEL_ID_LENGTH
        && channel_id.bytes().all(|byte| byte.is_ascii_digit())
}

fn cache_path(work_dir: &Path) -> PathBuf {
    work_dir.join(IDENTITY_CACHE_FILE_NAME)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ts(unix: i64) -> OffsetDateTime {
        match OffsetDateTime::from_unix_timestamp(unix) {
            Ok(value) => value,
            Err(error) => panic!("invalid fixture timestamp: {error}"),
        }
    }

    fn entry(index: usize, verified_at: OffsetDateTime) -> IdentityCacheEntry {
        IdentityCacheEntry {
            configured_login: format!("user_{index}"),
            channel_id: (100_000 + index).to_string(),
            verified_login: format!("user_{index}"),
            verified_at,
        }
    }

    #[test]
    fn lookup_returns_only_a_fresh_verified_identity() {
        let now = ts(200_000);
        let mut cache = IdentityCache::default();
        cache.record(" Alice ", "123", "Renamed", now);

        let resolved = cache.lookup("alice", now + std::time::Duration::from_secs(1));
        assert_eq!(
            resolved.map(|entry| entry.channel_id),
            Some(String::from("123"))
        );
        assert_eq!(
            cache
                .lookup("alice", now + std::time::Duration::from_secs(1))
                .map(|entry| entry.verified_login),
            Some(String::from("renamed"))
        );
        assert!(cache
            .lookup("alice", now + std::time::Duration::from_secs(1))
            .is_some());
        assert!(cache
            .lookup(
                "alice",
                now + std::time::Duration::from_secs((CACHE_MAX_AGE_SECONDS + 1) as u64)
            )
            .is_none());
        assert!(cache.lookup("unknown", now).is_none());
    }

    #[test]
    fn load_prunes_old_future_invalid_and_duplicate_entries() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let now = ts(200_000);
        let mut cache = IdentityCache::default();
        cache.entries.push(entry(1, now));
        cache.entries.push(entry(
            2,
            now - std::time::Duration::from_secs((CACHE_MAX_AGE_SECONDS + 1) as u64),
        ));
        cache.entries.push(entry(
            3,
            now + std::time::Duration::from_secs((MAX_FUTURE_SKEW_SECONDS + 1) as u64),
        ));
        cache.entries.push(IdentityCacheEntry {
            configured_login: String::from("invalid-login"),
            channel_id: String::from("123"),
            verified_login: String::from("invalid-login"),
            verified_at: now,
        });
        cache
            .entries
            .push(entry(1, now - std::time::Duration::from_secs(1)));
        for index in 4..=MAX_CACHE_ENTRIES + 8 {
            cache.entries.push(entry(index, now));
        }
        cache.save(directory.path(), now)?;

        let loaded = IdentityCache::load(directory.path(), now)?;
        assert_eq!(loaded.entries.len(), MAX_CACHE_ENTRIES);
        assert_eq!(loaded.lookup("user_1", now).unwrap().verified_at, now);
        assert_eq!(
            loaded
                .entries
                .iter()
                .filter(|entry| entry.configured_login == "user_1")
                .count(),
            1
        );
        assert!(loaded
            .entries
            .iter()
            .all(|entry| entry.configured_login != "invalid-login"));
        assert!(loaded
            .entries
            .iter()
            .all(|entry| entry.configured_login != "user_2"));
        assert!(loaded
            .entries
            .iter()
            .all(|entry| entry.configured_login != "user_3"));
        Ok(())
    }

    #[test]
    fn load_rejects_oversized_cache_before_decoding() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = cache_path(directory.path());
        let oversized_len = usize::try_from(MAX_CACHE_BYTES + 1)
            .map_err(|_| anyhow!("startup identity cache size does not fit this platform"))?;
        fs::write(&path, vec![b'{'; oversized_len])?;

        let Err(error) = IdentityCache::load(directory.path(), ts(200_000)) else {
            return Err(anyhow!("oversized cache unexpectedly loaded"));
        };
        assert!(error.to_string().contains("exceeds"));
        Ok(())
    }

    #[test]
    fn save_replaces_the_file_without_leaving_a_temporary_record() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let now = ts(200_000);
        let mut cache = IdentityCache::default();
        cache.record("alice", "123", "alice", now);
        cache.save(directory.path(), now)?;
        cache.record("alice", "456", "alice_renamed", now);
        cache.save(directory.path(), now)?;

        let loaded = IdentityCache::load(directory.path(), now)?;
        assert_eq!(
            loaded.lookup("alice", now).map(|entry| entry.channel_id),
            Some(String::from("456"))
        );
        let temporary_prefix = format!(".{IDENTITY_CACHE_FILE_NAME}.");
        assert!(!fs::read_dir(directory.path())?.any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| name.starts_with(&temporary_prefix))
        }));
        Ok(())
    }
}
