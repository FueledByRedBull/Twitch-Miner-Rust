//! Config hot reload.
//!
//! A supervised task rereads the active config file every 30 seconds and acts
//! only when its contents changed. The file is never written: it is read with
//! [`tm_config::preview_config`], validated like a startup load, and compared
//! with the running configuration. Live settings and channel list changes are
//! applied together through one runtime command; settings that are read once
//! at startup are reported and left alone.
//!
//! Everything that can fail, such as resolving a new channel, happens before
//! that command. A failed attempt changes nothing, keeps the running
//! configuration, and is retried at most every five minutes while the file is
//! unchanged.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tm_config::{
    build_base_streamer_settings, build_override_settings, diff_configs, merge_live_settings,
    preview_config, ConfigFile, ReloadClass, SettingChange,
};
use tm_domain::{
    normalize_game_list, normalize_streamer_list, parse_watch_priorities, Streamer,
    StreamerSettings,
};
use tm_observability::Event as DiscordEvent;
use tm_twitch::TwitchClient;

use crate::bootstrap::validate_app_config;
use crate::discovery;
use crate::identity_cache::IdentityCache;
use crate::observability::AppObservability;
use crate::startup::{bootstrap_streamer, load_targets};
use crate::status::{unix_now_infallible, HealthTracker};
use crate::streak_cache::StreakCache;
use crate::tasks::TransportController;
use crate::utilities::{sleep_or_stop, time_now};

pub(crate) const TASK_NAME: &str = "config-watch";
pub(crate) const CHECK_INTERVAL: Duration = Duration::from_secs(30);
// A reload that resolves many new channels can take minutes between checks.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(30 * 60);
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MAX_VALUE_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReloadResult {
    /// Live changes were applied; some settings may still need a restart.
    Applied,
    /// The file changed only settings that are read at startup.
    NeedsRestart,
    /// The file could not be used; the running configuration is unchanged.
    Rejected,
}

/// The `config_reload` block of the status file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConfigReloadStatus {
    #[serde(default)]
    pub(crate) last_applied_unix: Option<u64>,
    #[serde(default)]
    pub(crate) last_result: Option<ReloadResult>,
    #[serde(default)]
    pub(crate) last_error_class: Option<String>,
    /// Attempts that left the running configuration unchanged because the
    /// file or a new channel could not be used.
    #[serde(default)]
    pub(crate) rejected_attempts: u64,
    /// Settings that differ from the file but only take effect after a restart.
    #[serde(default)]
    pub(crate) needs_restart: Vec<String>,
    /// Channels removed from the config but still tracked because a prediction
    /// placement on them is not settled.
    #[serde(default)]
    pub(crate) deferred_removals: Vec<String>,
    #[serde(default)]
    pub(crate) deferred_removal_count: usize,
}

impl ConfigReloadStatus {
    /// Support files carry setting names but never channel names.
    pub(crate) fn without_channels(mut self) -> Self {
        self.deferred_removals.clear();
        self
    }
}

#[derive(Clone, Default)]
pub(crate) struct ReloadStatusTracker(Arc<Mutex<ConfigReloadStatus>>);

impl ReloadStatusTracker {
    pub(crate) fn status(&self) -> ConfigReloadStatus {
        self.update(|_| {})
    }

    fn update(&self, change: impl FnOnce(&mut ConfigReloadStatus)) -> ConfigReloadStatus {
        let mut status = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        change(&mut status);
        status.clone()
    }

    fn rejected(&self, error_class: &str) {
        self.update(|status| {
            status.rejected_attempts = status.rejected_attempts.saturating_add(1);
            status.last_result = Some(ReloadResult::Rejected);
            status.last_error_class = Some(error_class.to_string());
        });
    }
}

/// Resolves new channels the way startup loads each one: the identity cache, the
/// channel points context, a pending bonus claim, and the live state. A channel
/// that is already tracked, or repeated in `candidates`, is dropped. Any
/// failure fails the call, so a caller can apply the result or nothing. This is
/// the resolving half of adding channels; the other half is a `ConfigReload`
/// with `add` set, followed by [`TransportController::restart`].
pub(crate) async fn resolve_channels(
    context: &ConfigWatchContext,
    candidates: Vec<Streamer>,
    tracked: &[Streamer],
    identity_cache: &mut IdentityCache,
) -> anyhow::Result<Vec<Streamer>> {
    let mut resolved: Vec<Streamer> = Vec::new();
    if candidates.is_empty() {
        return Ok(resolved);
    }
    let mut streak_cache = StreakCache::load(&context.work_dir, time_now()).unwrap_or_default();
    for mut streamer in candidates {
        context.health.activity(TASK_NAME);
        bootstrap_streamer(
            &mut streamer,
            &context.twitch,
            context.user_id.as_deref(),
            time_now(),
            &context.observability,
            &mut streak_cache,
            identity_cache,
        )
        .await?;
        if !tracked
            .iter()
            .chain(resolved.iter())
            .any(|existing| existing.channel_id == streamer.channel_id)
        {
            resolved.push(streamer);
        }
    }
    Ok(resolved)
}

/// Which tasks the process started with. The Drop claim loop and the streak
/// recovery loop are spawned only when some channel needs them, so a change
/// that would turn either on or off waits for a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartupGates {
    claim_drops: bool,
    streak_recovery: bool,
}

impl StartupGates {
    pub(crate) fn from_streamers(streamers: &[Streamer]) -> Self {
        Self {
            claim_drops: streamers
                .iter()
                .any(|streamer| streamer.settings.claim_drops),
            streak_recovery: streamers.iter().any(recovers_streaks),
        }
    }

    /// Holds back the settings that would change a gate and returns their names.
    /// Retained channels keep their running values and new ones cannot enable
    /// what the process did not start.
    fn hold(
        self,
        running: &HashMap<String, StreamerSettings>,
        retained: &mut HashMap<String, StreamerSettings>,
        added: &mut [Streamer],
    ) -> Vec<&'static str> {
        let all = || {
            retained
                .values()
                .chain(added.iter().map(|streamer| &streamer.settings))
        };
        let claim_changes = all().any(|settings| settings.claim_drops) != self.claim_drops;
        let recovery_changes = all()
            .any(|settings| settings.watch_streak && settings.watch_streak_vod_recovery)
            != self.streak_recovery;
        let mut held = Vec::new();
        if claim_changes {
            for (channel_id, settings) in retained.iter_mut() {
                if let Some(current) = running.get(channel_id) {
                    settings.claim_drops = current.claim_drops;
                }
            }
            for streamer in added.iter_mut() {
                streamer.settings.claim_drops &= self.claim_drops;
            }
            held.push("claim_drops");
        }
        if recovery_changes {
            for (channel_id, settings) in retained.iter_mut() {
                if let Some(current) = running.get(channel_id) {
                    settings.watch_streak = current.watch_streak;
                    settings.watch_streak_vod_recovery = current.watch_streak_vod_recovery;
                }
            }
            for streamer in added.iter_mut() {
                streamer.settings.watch_streak_vod_recovery &= self.streak_recovery;
            }
            held.push("watch_streak_vod_recovery");
        }
        held
    }
}

fn recovers_streaks(streamer: &Streamer) -> bool {
    streamer.settings.watch_streak && streamer.settings.watch_streak_vod_recovery
}

/// What the watcher found in the file on one check.
#[derive(Debug, PartialEq, Eq)]
enum Observation {
    /// The same contents as last time and nothing is due.
    Unchanged,
    /// Different contents from the last check.
    Changed,
    /// The same contents, and a failed attempt is due for another try.
    RetryDue,
    /// The file could not be read. `first` is true once per distinct failure.
    Unreadable { first: bool },
}

#[derive(Default)]
struct FileObserver {
    seen: Option<Vec<u8>>,
    unreadable: Option<std::io::ErrorKind>,
    retry_at: Option<Instant>,
}

impl FileObserver {
    fn observe(&mut self, read: Result<Vec<u8>, std::io::ErrorKind>, now: Instant) -> Observation {
        let contents = match read {
            Ok(contents) => contents,
            Err(kind) => {
                let first = self.unreadable != Some(kind);
                self.unreadable = Some(kind);
                return Observation::Unreadable { first };
            }
        };
        self.unreadable = None;
        if self.seen.as_ref() == Some(&contents) {
            if self.retry_at.is_some_and(|due| now >= due) {
                self.retry_at = None;
                return Observation::RetryDue;
            }
            return Observation::Unchanged;
        }
        self.seen = Some(contents);
        self.retry_at = None;
        Observation::Changed
    }

    fn retry_later(&mut self, now: Instant) {
        self.retry_at = Some(now + RETRY_INTERVAL);
    }
}

/// A channel the runtime tracks and the logins the config may call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackedChannel {
    pub(crate) channel_id: String,
    pub(crate) names: Vec<String>,
    /// Found by streak discovery. Only discovery adds or removes such a channel,
    /// besides an exclusion, and a configured name takes it over.
    pub(crate) discovered: bool,
}

fn tracked_channels(
    streamers: &[Streamer],
    identity_cache: &IdentityCache,
    now: tm_runtime::RuntimeTime,
) -> Vec<TrackedChannel> {
    streamers
        .iter()
        .map(|streamer| {
            let mut names = vec![streamer.username.clone()];
            names.extend(identity_cache.configured_logins(&streamer.channel_id, now));
            TrackedChannel {
                channel_id: streamer.channel_id.clone(),
                names,
                discovered: streamer.discovered,
            }
        })
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelPlan {
    /// Logins to resolve and start tracking.
    pub(crate) resolve: Vec<String>,
    /// Channel ids to stop tracking.
    pub(crate) remove: Vec<String>,
}

pub(crate) fn channel_inputs_changed(running: &ConfigFile, next: &ConfigFile) -> bool {
    normalize_streamer_list(&running.streamers) != normalize_streamer_list(&next.streamers)
        || normalize_streamer_list(&running.streamers_exclude)
            != normalize_streamer_list(&next.streamers_exclude)
}

fn named_channels(config: &ConfigFile) -> Vec<String> {
    let excluded = normalize_streamer_list(&config.streamers_exclude);
    normalize_streamer_list(&config.streamers)
        .into_iter()
        .filter(|login| !excluded.contains(login))
        .collect()
}

/// Works out which channels to start and stop for a new channel list.
///
/// `desired` is the new effective list. Channels the running config named are
/// not resolved again, since one that failed at startup would otherwise block
/// every later reload. A tracked channel leaves only when this change drops it:
/// when the running config named it, when a named list replaces follower mode,
/// or when it is newly excluded. Channels found by Drop discovery stay put.
/// Channels found by streak discovery are never the config's to drop, except
/// for an exclusion, and a login the config names is resolved even when
/// discovery already tracks it, so the configured channel takes over.
pub(crate) fn plan_channels(
    running: &ConfigFile,
    next: &ConfigFile,
    desired: &[String],
    tracked: &[TrackedChannel],
) -> ChannelPlan {
    let previously_named = named_channels(running);
    let excluded = normalize_streamer_list(&next.streamers_exclude);
    let owned_by_config = |channel: &TrackedChannel| {
        if channel.discovered {
            false
        } else if running.streamers.is_empty() {
            !next.streamers.is_empty() && running.drop_discovery_games.is_empty()
        } else {
            channel
                .names
                .iter()
                .any(|name| previously_named.contains(name))
        }
    };
    let wanted = |channel: &TrackedChannel| channel.names.iter().any(|name| desired.contains(name));
    ChannelPlan {
        resolve: desired
            .iter()
            .filter(|login| {
                !previously_named.contains(login)
                    && !tracked
                        .iter()
                        .any(|channel| !channel.discovered && channel.names.contains(login))
            })
            .cloned()
            .collect(),
        remove: tracked
            .iter()
            .filter(|channel| {
                channel.names.iter().any(|name| excluded.contains(name))
                    || (owned_by_config(channel) && !wanted(channel))
            })
            .map(|channel| channel.channel_id.clone())
            .collect(),
    }
}

fn settings_for(
    names: &[String],
    base: &StreamerSettings,
    overrides: &HashMap<String, StreamerSettings>,
) -> StreamerSettings {
    names
        .iter()
        .find_map(|name| overrides.get(name))
        .cloned()
        .unwrap_or_else(|| base.clone())
}

/// How one check ended.
#[derive(Debug)]
enum Evaluation {
    /// The file parses but no setting differs from the running configuration.
    NoChange,
    Applied(Applied),
    /// Only settings that are read at startup differ.
    NeedsRestart(Vec<SettingChange>),
    /// The file or the change cannot be used and trying again would not help.
    Rejected {
        class: &'static str,
        detail: String,
    },
    /// An attempt failed in a way that may pass later.
    Failed {
        class: &'static str,
        detail: String,
    },
}

#[derive(Debug)]
struct Applied {
    live: Vec<SettingChange>,
    needs_restart: Vec<SettingChange>,
    held: Vec<&'static str>,
    added: Vec<String>,
    removed: Vec<String>,
    deferred: Vec<String>,
}

pub(crate) struct ConfigWatchContext {
    pub(crate) config_path: PathBuf,
    pub(crate) work_dir: PathBuf,
    pub(crate) running: ConfigFile,
    pub(crate) gates: StartupGates,
    pub(crate) user_id: Option<String>,
    pub(crate) runtime: tm_runtime::RuntimeHandle,
    pub(crate) twitch: Arc<TwitchClient>,
    pub(crate) observability: AppObservability,
    pub(crate) health: HealthTracker,
    pub(crate) transports: Arc<TransportController>,
}

struct ConfigWatcher {
    context: ConfigWatchContext,
    observer: FileObserver,
    status: ReloadStatusTracker,
    identity_cache: IdentityCache,
    /// Channels no longer configured that stay tracked until a prediction settles.
    deferred: Vec<String>,
    /// Whether the failed attempt for the current contents has been warned about.
    failure_logged: bool,
    /// When streak discovery next refreshes; `None` means at the next check.
    discovery_due: Option<Instant>,
    /// Whether the current run of failed discovery refreshes has been warned about.
    discovery_failure_logged: bool,
}

pub(crate) fn spawn_config_watch_loop(
    mut stop: tokio::sync::watch::Receiver<bool>,
    context: ConfigWatchContext,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let health = context.health.clone();
        let mut watcher = ConfigWatcher::new(context);
        loop {
            // A reload cut short by shutdown leaves the runtime and the
            // transports consistent: its one state change is a single command.
            tokio::select! {
                () = watcher.tick(Instant::now()) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
            health.success(TASK_NAME);
            if sleep_or_stop(&mut stop, CHECK_INTERVAL).await {
                return;
            }
        }
    })
}

impl ConfigWatcher {
    fn new(context: ConfigWatchContext) -> Self {
        let identity_cache = IdentityCache::load(&context.work_dir, time_now()).unwrap_or_default();
        let status = context.health.reload();
        Self {
            context,
            observer: FileObserver::default(),
            status,
            identity_cache,
            deferred: Vec::new(),
            failure_logged: false,
            discovery_due: None,
            discovery_failure_logged: false,
        }
    }

    async fn tick(&mut self, now: Instant) {
        let path = self.context.config_path.clone();
        let read = tokio::task::spawn_blocking(move || std::fs::read(path))
            .await
            .unwrap_or_else(|_| Err(std::io::ErrorKind::Other.into()))
            .map_err(|error| error.kind());
        match self.observer.observe(read, now) {
            Observation::Unchanged => {}
            Observation::Unreadable { first } => {
                if first {
                    self.status.rejected("unreadable");
                    tracing::warn!(
                        task = TASK_NAME,
                        error_class = "unreadable",
                        "config reload skipped: the config file is missing or unreadable; keeping the running configuration"
                    );
                }
            }
            observation @ (Observation::Changed | Observation::RetryDue) => {
                if observation == Observation::Changed {
                    self.failure_logged = false;
                }
                let evaluation = self.evaluate().await;
                self.report(evaluation, now);
            }
        }
        self.refresh_discovery(now).await;
        self.retry_deferred_removals().await;
    }

    /// Adds and drops the channels of `streak_discovery_games` as one change,
    /// through the same runtime command and transport restart as a config
    /// reload. A listing failure changes nothing; a channel that cannot be
    /// loaded is skipped and tried again at the retry interval.
    #[allow(clippy::too_many_lines)] // Keep list, plan, resolve and commit in order.
    async fn refresh_discovery(&mut self, now: Instant) {
        if self.discovery_due.is_some_and(|due| now < due) {
            return;
        }
        let running = &self.context.running;
        let games = normalize_game_list(&running.streak_discovery_games);
        // Configured channels, including ones that failed to load, stay configured.
        let mut excluded = normalize_streamer_list(&running.streamers_exclude);
        excluded.extend(normalize_streamer_list(&running.streamers));
        excluded.extend(normalize_streamer_list(std::slice::from_ref(
            &running.username,
        )));
        let found = if games.is_empty() {
            Vec::new()
        } else {
            match discovery::list_channels(&self.context.twitch, &games, &excluded).await {
                Ok(found) => found,
                Err(error) => {
                    if !std::mem::replace(&mut self.discovery_failure_logged, true) {
                        tracing::warn!(
                            task = TASK_NAME,
                            error_class = "streak-discovery",
                            %error,
                            "could not list live channels for streak discovery; keeping the current ones"
                        );
                    }
                    self.discovery_due = Some(now + RETRY_INTERVAL);
                    return;
                }
            }
        };
        let Ok(snapshot) = self.context.runtime.state_snapshot().await else {
            return;
        };
        let tracked = tracked_channels(&snapshot.streamers, &self.identity_cache, time_now());
        let discovered = snapshot
            .streamers
            .iter()
            .filter(|streamer| streamer.discovered)
            .map(|streamer| discovery::DiscoveredChannel {
                channel_id: streamer.channel_id.clone(),
                login: streamer.username.clone(),
                usable: streamer.is_online
                    && !discovery::plays_excluded_game(streamer, &snapshot.game_exclusions),
            })
            .collect::<Vec<_>>();
        let plan = discovery::plan_discovery(&found, &tracked, &discovered);
        // A channel listed again is wanted again, even if its removal was waiting.
        self.deferred.retain(|channel_id| {
            discovered.iter().all(|channel| {
                &channel.channel_id != channel_id || plan.remove.contains(channel_id)
            })
        });

        let settings = build_base_streamer_settings(&self.context.running).for_discovered();
        let mut identity_cache = self.identity_cache.clone();
        let mut resolved: Vec<Streamer> = Vec::new();
        let mut skipped = 0_usize;
        for login in &plan.resolve {
            let candidate = Streamer {
                username: login.clone(),
                discovered: true,
                settings: settings.clone(),
                ..Streamer::default()
            };
            match resolve_channels(
                &self.context,
                vec![candidate],
                &snapshot.streamers,
                &mut identity_cache,
            )
            .await
            {
                Ok(channels) => {
                    for streamer in channels {
                        if streamer.is_online
                            && !discovery::plays_excluded_game(&streamer, &snapshot.game_exclusions)
                            && !resolved
                                .iter()
                                .any(|added| added.channel_id == streamer.channel_id)
                        {
                            resolved.push(streamer);
                        }
                    }
                }
                Err(error) => {
                    skipped += 1;
                    tracing::debug!(task = TASK_NAME, %error, "a discovered channel could not be loaded");
                }
            }
        }
        if skipped > 0 && !std::mem::replace(&mut self.discovery_failure_logged, true) {
            tracing::warn!(
                task = TASK_NAME,
                error_class = "streak-discovery",
                "{skipped} discovered channel(s) could not be loaded and were skipped"
            );
        }
        self.discovery_due = Some(
            now + if skipped > 0 {
                RETRY_INTERVAL
            } else {
                discovery::REFRESH_INTERVAL
            },
        );
        if skipped == 0 {
            self.discovery_failure_logged = false;
        }
        if resolved.is_empty() && plan.remove.is_empty() {
            return;
        }

        self.keep_identity_cache(identity_cache);
        let reload = tm_runtime::ConfigReload {
            watch_priorities: snapshot.watch_priorities.clone(),
            game_priority: snapshot.game_priority.clone(),
            game_exclusions: snapshot.game_exclusions.clone(),
            follower_mode: snapshot.follower_mode,
            settings: HashMap::new(),
            add: resolved.clone(),
            remove: plan.remove,
        };
        let Ok(outcome) = self.context.runtime.apply_config_reload(reload).await else {
            return;
        };
        for channel_id in outcome.deferred {
            if !self.deferred.contains(&channel_id) {
                self.deferred.push(channel_id);
            }
        }
        if outcome.added.is_empty() && outcome.removed.is_empty() {
            return;
        }
        let label = |channel_ids: &[String]| {
            channel_ids
                .iter()
                .filter_map(|channel_id| {
                    snapshot
                        .streamers
                        .iter()
                        .chain(&resolved)
                        .find(|streamer| &streamer.channel_id == channel_id)
                })
                .map(|streamer| self.context.observability.streamer_name(streamer))
                .collect::<Vec<_>>()
        };
        let line = [("added", &outcome.added), ("removed", &outcome.removed)]
            .into_iter()
            .filter(|(_, channel_ids)| !channel_ids.is_empty())
            .map(|(verb, channel_ids)| format!("{verb} {}", label(channel_ids).join(", ")))
            .collect::<Vec<_>>()
            .join(" | ");
        tracing::info!(
            task = TASK_NAME,
            operation = "streak_discovery",
            "Streak discovery: {line}"
        );
        self.restart_transports().await;
    }

    async fn evaluate(&mut self) -> Evaluation {
        let preview = match preview_config(&self.context.config_path) {
            Ok(preview) => preview,
            Err(error) => {
                return Evaluation::Rejected {
                    class: "invalid-config",
                    detail: describe_config_error(&error),
                }
            }
        };
        if let Err(error) = validate_app_config(&preview.config) {
            return Evaluation::Rejected {
                class: "invalid-config",
                detail: describe_config_error(&error),
            };
        }
        let changes = match diff_configs(&self.context.running, &preview.config) {
            Ok(changes) => changes,
            Err(error) => {
                return Evaluation::Rejected {
                    class: "invalid-config",
                    detail: describe_config_error(&error),
                }
            }
        };
        if changes.is_empty() {
            return Evaluation::NoChange;
        }
        if changes
            .iter()
            .all(|change| change.class == ReloadClass::NeedsRestart)
        {
            return Evaluation::NeedsRestart(changes);
        }
        self.apply(preview.config, changes).await
    }

    #[allow(clippy::too_many_lines)] // Keep the ordered plan, resolve, commit steps together.
    async fn apply(&mut self, next: ConfigFile, changes: Vec<SettingChange>) -> Evaluation {
        let failed = |class, detail: &str| Evaluation::Failed {
            class,
            detail: detail.to_string(),
        };
        let Ok(snapshot) = self.context.runtime.state_snapshot().await else {
            return failed("runtime", "the runtime state is closed");
        };
        let tracked = tracked_channels(&snapshot.streamers, &self.identity_cache, time_now());
        let discovery_changed = changes
            .iter()
            .any(|change| change.name == "streak_discovery_games");

        let mut plan = ChannelPlan::default();
        let mut deferred = self.deferred.clone();
        if channel_inputs_changed(&self.context.running, &next) {
            let desired = match self.desired_channels(&next).await {
                Ok(desired) => desired,
                Err(error) => {
                    tracing::debug!(task = TASK_NAME, %error, "channel list could not be loaded");
                    return failed("followers", "the follower list could not be loaded");
                }
            };
            plan = plan_channels(&self.context.running, &next, &desired, &tracked);
            deferred.retain(|channel_id| {
                !tracked.iter().any(|channel| {
                    &channel.channel_id == channel_id
                        && channel.names.iter().any(|name| desired.contains(name))
                })
            });
        }
        let mut remove = plan.remove;
        for channel_id in deferred {
            if !remove.contains(&channel_id) {
                remove.push(channel_id);
            }
        }
        // Discovered channels only collect streaks, so they never count as a channel to mine.
        let remaining = snapshot
            .streamers
            .iter()
            .filter(|streamer| !streamer.discovered && !remove.contains(&streamer.channel_id))
            .count()
            + plan.resolve.len();
        if remaining == 0 {
            return Evaluation::Rejected {
                class: "no-channels",
                detail: String::from("the change would leave no channel to mine"),
            };
        }

        let base = build_base_streamer_settings(&next);
        let overrides = build_override_settings(&base, &next.streamer_overrides);
        let discovered_settings = base.for_discovered();
        let running_settings = snapshot
            .streamers
            .iter()
            .map(|streamer| (streamer.channel_id.clone(), streamer.settings.clone()))
            .collect::<HashMap<_, _>>();
        let mut retained = tracked
            .iter()
            .filter(|channel| !remove.contains(&channel.channel_id))
            .map(|channel| {
                let settings = if channel.discovered {
                    discovered_settings.clone()
                } else {
                    settings_for(&channel.names, &base, &overrides)
                };
                (channel.channel_id.clone(), settings)
            })
            .collect::<HashMap<_, _>>();
        let mut added = plan
            .resolve
            .iter()
            .map(|login| Streamer {
                username: login.clone(),
                settings: settings_for(std::slice::from_ref(login), &base, &overrides),
                ..Streamer::default()
            })
            .collect::<Vec<_>>();
        let held = self
            .context
            .gates
            .hold(&running_settings, &mut retained, &mut added);

        let mut identity_cache = self.identity_cache.clone();
        // A discovered channel does not count as tracked here: a configured name
        // for the same channel must be resolved so it can take that channel over.
        let configured = snapshot
            .streamers
            .iter()
            .filter(|streamer| !streamer.discovered)
            .cloned()
            .collect::<Vec<_>>();
        let resolved =
            match resolve_channels(&self.context, added, &configured, &mut identity_cache).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    // Fields are dropped from anonymized logs; the message names no channel.
                    tracing::warn!(task = TASK_NAME, %error, "a new channel could not be loaded");
                    return failed("channel-resolution", "a new channel could not be loaded");
                }
            };

        let reload = tm_runtime::ConfigReload {
            watch_priorities: parse_watch_priorities(&next.watch_priority),
            game_priority: normalize_game_list(&next.game_priority),
            game_exclusions: normalize_game_list(&next.game_exclude),
            follower_mode: next.streamers.is_empty(),
            settings: retained,
            add: resolved.clone(),
            remove,
        };
        let Ok(outcome) = self.context.runtime.apply_config_reload(reload).await else {
            return failed("runtime", "the runtime state is closed");
        };

        // Past the commit point nothing below can fail.
        self.context.observability.apply_live_config(&next);
        match merge_live_settings(&self.context.running, &next) {
            Ok(mut merged) => {
                if held.contains(&"claim_drops") {
                    merged.claim_drops = self.context.running.claim_drops;
                }
                if held.contains(&"watch_streak_vod_recovery") {
                    merged.watch_streak_vod_recovery =
                        self.context.running.watch_streak_vod_recovery;
                }
                self.context.running = merged;
            }
            Err(error) => {
                tracing::warn!(task = TASK_NAME, %error, "the running config view could not be updated");
            }
        }
        self.keep_identity_cache(identity_cache);
        self.deferred.clone_from(&outcome.deferred);
        if discovery_changed {
            self.discovery_due = None;
        }
        let label = |channel_id: &String| {
            snapshot
                .streamers
                .iter()
                .chain(resolved.iter())
                .find(|streamer| &streamer.channel_id == channel_id)
                .map(|streamer| self.context.observability.streamer_name(streamer))
                .unwrap_or_default()
        };
        let summary = Applied {
            added: outcome.added.iter().map(label).collect(),
            removed: outcome.removed.iter().map(label).collect(),
            deferred: outcome.deferred.iter().map(label).collect(),
            held,
            live: changes
                .iter()
                .filter(|change| change.class == ReloadClass::Live)
                .cloned()
                .collect(),
            needs_restart: changes
                .into_iter()
                .filter(|change| change.class == ReloadClass::NeedsRestart)
                .collect(),
        };
        if !outcome.added.is_empty()
            || !outcome.removed.is_empty()
            || self.pubsub_topics_changed(&snapshot.streamers).await
        {
            self.restart_transports().await;
        }
        Evaluation::Applied(summary)
    }

    fn keep_identity_cache(&mut self, mut identity_cache: IdentityCache) {
        if identity_cache
            .save(&self.context.work_dir, time_now())
            .is_err()
        {
            tracing::warn!(
                task = "identity-cache",
                error_class = "write",
                "startup identity cache write failed"
            );
        }
        self.identity_cache = identity_cache;
    }

    /// Channel settings decide some `PubSub` topics, so a change that keeps the
    /// channel list can still need new subscriptions.
    async fn pubsub_topics_changed(&self, before: &[Streamer]) -> bool {
        let Some(user_id) = self.context.user_id.as_deref() else {
            return false;
        };
        let Ok(snapshot) = self.context.runtime.state_snapshot().await else {
            return false;
        };
        tm_pubsub::build_topics(user_id, before).ok()
            != tm_pubsub::build_topics(user_id, &snapshot.streamers).ok()
    }

    /// The channels the new config asks for. Follower mode asks Twitch again,
    /// which happens only when the channel inputs changed.
    async fn desired_channels(&self, next: &ConfigFile) -> anyhow::Result<Vec<String>> {
        let mut source = next.clone();
        // Both are read at startup: discovery is not repeated, and the follower
        // order stays what the process started with.
        source.followers_order = self.context.running.followers_order;
        source.drop_discovery_games.clear();
        let excluded = normalize_streamer_list(&next.streamers_exclude);
        let targets = load_targets(&source, &self.context.twitch).await?;
        Ok(normalize_streamer_list(&targets)
            .into_iter()
            .filter(|login| !excluded.contains(login))
            .collect())
    }

    async fn restart_transports(&self) {
        let Ok(snapshot) = self.context.runtime.state_snapshot().await else {
            return;
        };
        self.context.transports.restart(&snapshot.streamers).await;
    }

    async fn retry_deferred_removals(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let Ok(snapshot) = self.context.runtime.state_snapshot().await else {
            return;
        };
        let reload = tm_runtime::ConfigReload {
            watch_priorities: snapshot.watch_priorities.clone(),
            game_priority: snapshot.game_priority.clone(),
            game_exclusions: snapshot.game_exclusions.clone(),
            follower_mode: snapshot.follower_mode,
            settings: HashMap::new(),
            add: Vec::new(),
            remove: self.deferred.clone(),
        };
        let Ok(outcome) = self.context.runtime.apply_config_reload(reload).await else {
            return;
        };
        self.deferred = outcome.deferred;
        let names = |channel_ids: &[String]| {
            channel_ids
                .iter()
                .filter_map(|channel_id| {
                    snapshot
                        .streamers
                        .iter()
                        .find(|streamer| &streamer.channel_id == channel_id)
                })
                .map(|streamer| self.context.observability.streamer_name(streamer))
                .collect::<Vec<_>>()
        };
        if !outcome.removed.is_empty() {
            let removed = names(&outcome.removed);
            self.record_deferred(&snapshot);
            self.restart_transports().await;
            tracing::info!(
                task = TASK_NAME,
                operation = "config_reload",
                "Config reload: removed {} after their predictions settled",
                removed.join(", ")
            );
        }
    }

    fn record_deferred(&self, snapshot: &tm_runtime::RuntimeState) {
        let names = self
            .deferred
            .iter()
            .filter_map(|channel_id| {
                snapshot
                    .streamers
                    .iter()
                    .find(|streamer| &streamer.channel_id == channel_id)
            })
            .map(|streamer| self.context.observability.streamer_name(streamer))
            .collect::<Vec<_>>();
        self.status.update(|status| {
            status.deferred_removal_count = self.deferred.len();
            status.deferred_removals = names;
        });
    }

    fn report(&mut self, evaluation: Evaluation, now: Instant) {
        match evaluation {
            Evaluation::NoChange => {
                self.status.update(|status| status.needs_restart.clear());
            }
            Evaluation::Applied(applied) => self.report_applied(&applied),
            Evaluation::NeedsRestart(changes) => {
                let names = changes.iter().map(|change| change.name.clone()).collect();
                self.status.update(|status| {
                    status.last_result = Some(ReloadResult::NeedsRestart);
                    status.last_error_class = None;
                    status.needs_restart = names;
                });
                let line = format!(
                    "Config reload: nothing applied; needs restart: {}",
                    render_changes(&changes)
                );
                tracing::warn!(task = TASK_NAME, operation = "config_reload", "{line}");
                self.notify(&line);
            }
            Evaluation::Rejected { class, detail } => {
                self.status.rejected(class);
                let line = format!(
                    "Config reload rejected ({class}): {detail}; keeping the running configuration"
                );
                tracing::warn!(task = TASK_NAME, error_class = class, "{line}");
                self.notify(&line);
            }
            Evaluation::Failed { class, detail } => {
                let first = !self.failure_logged;
                self.failure_logged = true;
                self.status.rejected(class);
                self.observer.retry_later(now);
                let line = format!(
                    "Config reload failed ({class}): {detail}; keeping the running configuration and retrying in {} minutes",
                    RETRY_INTERVAL.as_secs() / 60
                );
                if first {
                    tracing::warn!(task = TASK_NAME, error_class = class, "{line}");
                    self.notify(&line);
                } else {
                    tracing::debug!(task = TASK_NAME, error_class = class, "{line}");
                }
            }
        }
    }

    fn report_applied(&mut self, applied: &Applied) {
        let mut needs_restart = applied
            .needs_restart
            .iter()
            .map(|change| change.name.clone())
            .collect::<Vec<_>>();
        needs_restart.extend(applied.held.iter().map(ToString::to_string));
        self.status.update(|status| {
            status.last_applied_unix = Some(unix_now_infallible());
            status.last_result = Some(ReloadResult::Applied);
            status.last_error_class = None;
            status.needs_restart = needs_restart;
            status.deferred_removal_count = applied.deferred.len();
            status.deferred_removals.clone_from(&applied.deferred);
        });
        let line = format_applied(applied);
        tracing::info!(task = TASK_NAME, operation = "config_reload", "{line}");
        self.notify(&line);
    }

    fn notify(&self, line: &str) {
        self.context.observability.spawn_event(
            DiscordEvent::ConfigReload,
            self.context.observability.config_reload_message(line),
        );
    }
}

fn describe_config_error(error: &tm_config::ConfigError) -> String {
    match error {
        tm_config::ConfigError::Validation(message) => message.clone(),
        // The parser's own text can quote a value from the file.
        tm_config::ConfigError::InvalidConfig(error) => format!(
            "the file is not valid config JSON ({:?} at line {}, column {})",
            error.classify(),
            error.line(),
            error.column()
        ),
        tm_config::ConfigError::Io(_) => String::from("the file could not be read"),
    }
}

fn render_changes(changes: &[SettingChange]) -> String {
    changes
        .iter()
        .map(render_change)
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_change(change: &SettingChange) -> String {
    match &change.values {
        Some((before, after)) => format!(
            "{} ({} -> {})",
            change.name,
            truncate(before),
            truncate(after)
        ),
        None => change.name.clone(),
    }
}

fn truncate(value: &str) -> String {
    if value.chars().count() <= MAX_VALUE_CHARS {
        return value.to_string();
    }
    let mut shortened = value.chars().take(MAX_VALUE_CHARS).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn format_applied(applied: &Applied) -> String {
    let mut line = format!("Config reload applied: {}", render_changes(&applied.live));
    let mut restart = applied
        .needs_restart
        .iter()
        .map(render_change)
        .collect::<Vec<_>>();
    restart.extend(applied.held.iter().map(ToString::to_string));
    if !restart.is_empty() {
        let _ = write!(line, " | needs restart: {}", restart.join(", "));
    }
    for (label, channels) in [
        ("added", &applied.added),
        ("removed", &applied.removed),
        ("kept until their predictions settle", &applied.deferred),
    ] {
        if !channels.is_empty() {
            let _ = write!(line, " | channels {label}: {}", channels.join(", "));
        }
    }
    line
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/reload_tests.rs"]
mod tests;
