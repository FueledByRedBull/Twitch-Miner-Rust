use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant as StdInstant;

use anyhow::{anyhow, Context, Result};
use reqwest::StatusCode;
use serde_json::json;
use tm_domain::{OffsetDateTime, Stream, Streamer};
use tm_observability::Event as DiscordEvent;
use tm_twitch::TwitchClient;

use crate::observability::{streamer_game_name, AppObservability};
use crate::status::HealthTracker;
use crate::utilities::{sleep_or_stop, time_now};
use crate::watching::{
    minute_watcher_resume_gap, CachedSpadeUrl, SpadeCacheEntry, StreakCandidate, WatchRotation,
};
use crate::{MINUTE_WATCHER_REQUEST_TIMEOUT, SPADE_URL_TTL, WATCH_SELECTION_REFRESH_CONCURRENCY};

const RENAME_RECOVERY_SUSPENSION_SECONDS: u64 = 5 * 60;
// Comfortably above the 120-second batched refresh cadence, so this only fires
// when the refresh itself has stalled.
const MAX_WATCH_METADATA_AGE_SECONDS: i64 = 5 * 60;
const WATCH_REQUEST_FAILURE_THRESHOLD: u8 = 3;
const WATCH_CHANNEL_BACKOFF_SECONDS: u64 = 15 * 60;
// Two rotation windows give Twitch time to deliver a point event while still
// allowing the watcher to move on from a channel that is visibly stuck.
const WATCHDOG_STALL_SECONDS: u64 = 2 * 15 * 60;
const WATCHDOG_CONTEXT_FRESH_SECONDS: i64 = 15 * 60;

struct MinuteWatcherContext {
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    user_id: String,
    observability: AppObservability,
    health: HealthTracker,
    claim_coordinator: crate::drops::DropClaimCoordinator,
    spade_urls: tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
}

struct MetadataRefreshHandle {
    handle: tokio::task::JoinHandle<usize>,
}

impl MetadataRefreshHandle {
    fn new(handle: tokio::task::JoinHandle<usize>) -> Self {
        Self { handle }
    }

    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    fn abort(&self) {
        self.handle.abort();
    }

    async fn wait(mut self) -> std::result::Result<usize, tokio::task::JoinError> {
        std::pin::Pin::new(&mut self.handle).await
    }
}

impl Drop for MetadataRefreshHandle {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct MinuteWatcherState {
    watch_rotation: WatchRotation,
    selected_channel_ids: HashSet<String>,
    dispatch_order: Vec<String>,
    last_loop_at: tm_runtime::RuntimeTime,
    metadata_refresh: Option<MetadataRefreshHandle>,
    watch_failures: HashMap<String, WatchFailureState>,
    watchdog: WatchdogState,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct WatchFailureState {
    consecutive_requests: u8,
    backoff_until: Option<tm_runtime::RuntimeTime>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchAttemptOutcome {
    Success,
    RequestFailure,
    Timeout,
}

#[derive(Debug, Default)]
struct WatchdogState {
    records: HashMap<String, WatchdogRecord>,
}

#[derive(Debug)]
struct WatchdogRecord {
    broadcast_id: String,
    last_points_at: Option<OffsetDateTime>,
    stalled_since: Option<StdInstant>,
    awaiting_since: Option<StdInstant>,
    recovery_attempted: bool,
}

impl WatchdogState {
    #[allow(clippy::too_many_lines)]
    fn stalled_logins(
        &mut self,
        streamers: &[Streamer],
        selected_channel_ids: &HashSet<String>,
        watch_failures: &HashMap<String, WatchFailureState>,
        now: OffsetDateTime,
        measurement_available: bool,
        monotonic_now: StdInstant,
    ) -> Vec<(String, String)> {
        if !measurement_available {
            self.records.clear();
            return Vec::new();
        }
        let measurable_channel_ids = streamers
            .iter()
            .filter(|streamer| {
                selected_channel_ids.contains(&streamer.channel_id)
                    && streamer.is_online
                    && streamer.can_earn_channel_points()
                    && streamer
                        .stream
                        .as_ref()
                        .and_then(|stream| stream.last_update)
                        .is_some_and(|last_update| {
                            let age = (now - last_update).whole_seconds();
                            (0..=MAX_WATCH_METADATA_AGE_SECONDS).contains(&age)
                        })
                    && !watch_failures
                        .get(&streamer.username)
                        .is_some_and(|failure| {
                            failure.consecutive_requests > 0 || failure.backoff_until.is_some()
                        })
            })
            .map(|streamer| streamer.channel_id.as_str())
            .collect::<HashSet<_>>();
        self.records
            .retain(|channel_id, _| measurable_channel_ids.contains(channel_id.as_str()));

        let mut stalled = Vec::new();
        for streamer in streamers.iter().filter(|streamer| {
            selected_channel_ids.contains(&streamer.channel_id)
                && streamer.is_online
                && streamer.can_earn_channel_points()
        }) {
            let Some(stream) = streamer
                .stream
                .as_ref()
                .filter(|stream| !stream.broadcast_id.trim().is_empty())
            else {
                self.records.remove(&streamer.channel_id);
                continue;
            };
            let Some(stream_up_at) = stream.stream_up_at else {
                self.records.remove(&streamer.channel_id);
                continue;
            };
            let metadata_age = stream
                .last_update
                .map_or(i64::MAX, |last_update| (now - last_update).whole_seconds());
            if !(0..=MAX_WATCH_METADATA_AGE_SECONDS).contains(&metadata_age)
                || watch_failures
                    .get(&streamer.username)
                    .is_some_and(|failure| {
                        failure.consecutive_requests > 0 || failure.backoff_until.is_some()
                    })
            {
                self.records.remove(&streamer.channel_id);
                continue;
            }
            let points_at = streamer
                .last_server_confirmed_points_at
                .filter(|points_at| *points_at >= stream_up_at);
            let context_at = streamer.last_context_observed_at.filter(|observed_at| {
                let age = (now - *observed_at).whole_seconds();
                (0..=WATCHDOG_CONTEXT_FRESH_SECONDS).contains(&age)
            });
            let record = self
                .records
                .entry(streamer.channel_id.clone())
                .or_insert_with(|| WatchdogRecord {
                    broadcast_id: stream.broadcast_id.clone(),
                    last_points_at: None,
                    stalled_since: None,
                    awaiting_since: None,
                    recovery_attempted: false,
                });

            if record.broadcast_id != stream.broadcast_id {
                record.broadcast_id.clone_from(&stream.broadcast_id);
                record.last_points_at = None;
                record.stalled_since = None;
                record.awaiting_since = None;
                record.recovery_attempted = false;
            }

            // A confirmed point event proves progress for this broadcast. A
            // context observation only proves that measurement is alive; it
            // must not reset a no-progress timer by itself.
            // Channels with no confirmed point event for the current broadcast
            // stay unmeasured. The available drop payload has no per-channel
            // progress identity that could safely establish that baseline.
            if context_at.is_none() {
                record.awaiting_since = None;
                record.last_points_at = points_at;
                record.stalled_since = None;
                record.recovery_attempted = false;
                continue;
            }
            if points_at.is_none() {
                record.awaiting_since.get_or_insert(monotonic_now);
                record.last_points_at = None;
                record.stalled_since = None;
                record.recovery_attempted = false;
                continue;
            }
            record.awaiting_since = None;
            if record.last_points_at != points_at {
                record.last_points_at = points_at;
                record.stalled_since = Some(monotonic_now);
                record.recovery_attempted = false;
            } else if record.stalled_since.is_none() {
                record.stalled_since = Some(monotonic_now);
            }
            if !record.recovery_attempted
                && record.stalled_since.is_some_and(|started| {
                    monotonic_now
                        .checked_duration_since(started)
                        .is_some_and(|elapsed| {
                            elapsed >= std::time::Duration::from_secs(WATCHDOG_STALL_SECONDS)
                        })
                })
            {
                stalled.push((streamer.username.clone(), streamer.channel_id.clone()));
            }
        }
        stalled
    }

    fn progress(
        &self,
        channel_id: &str,
        broadcast_id: &str,
        now: StdInstant,
    ) -> (crate::status::WatchProgress, Option<u64>) {
        use crate::status::WatchProgress;
        let Some(record) = self
            .records
            .get(channel_id)
            .filter(|record| record.broadcast_id == broadcast_id)
        else {
            return (WatchProgress::MeasurementUnavailable, None);
        };
        let (since, waiting) = if let Some(since) = record.awaiting_since {
            (since, true)
        } else if let Some(since) = record.stalled_since {
            (since, false)
        } else {
            return (WatchProgress::MeasurementUnavailable, None);
        };
        let age = now.saturating_duration_since(since).as_secs();
        let progress = match (waiting, age >= WATCHDOG_STALL_SECONDS) {
            (true, false) => WatchProgress::AwaitingFirstCredit,
            (true, true) => WatchProgress::FirstCreditOverdue,
            (false, false) => WatchProgress::Earning,
            (false, true) => WatchProgress::Stalled,
        };
        (progress, Some(age))
    }

    fn mark_recovery_attempted(&mut self, channel_id: &str) {
        if let Some(record) = self.records.get_mut(channel_id) {
            record.recovery_attempted = true;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchAction {
    Continue,
    Stop,
}

pub(crate) fn spawn_minute_watcher_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    user_id: String,
    observability: AppObservability,
    health: HealthTracker,
    claim_coordinator: crate::drops::DropClaimCoordinator,
) -> tokio::task::JoinHandle<()> {
    let context = MinuteWatcherContext {
        runtime,
        twitch,
        user_id,
        observability,
        health,
        claim_coordinator,
        spade_urls: tokio::sync::Mutex::new(HashMap::new()),
    };
    tokio::spawn(run_minute_watcher_loop(stop, context))
}

async fn run_minute_watcher_loop(
    mut stop: tokio::sync::watch::Receiver<bool>,
    context: MinuteWatcherContext,
) {
    let mut state = MinuteWatcherState {
        watch_rotation: WatchRotation::default(),
        selected_channel_ids: HashSet::new(),
        dispatch_order: Vec::new(),
        last_loop_at: time_now(),
        metadata_refresh: None,
        watch_failures: HashMap::new(),
        watchdog: WatchdogState::default(),
    };
    while !*stop.borrow() {
        if run_minute_watcher_pass(&mut stop, &context, &mut state).await == WatchAction::Stop {
            break;
        }
    }
    stop_metadata_refresh(&mut state).await;
}

async fn run_minute_watcher_pass(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    context: &MinuteWatcherContext,
    state: &mut MinuteWatcherState,
) -> WatchAction {
    let now = time_now();
    context.health.activity("minute");
    let loop_gap = minute_watcher_resume_gap(state.last_loop_at, now);
    state.last_loop_at = now;
    let Some(watch_logins) = select_watch_logins(context, state, now).await else {
        return WatchAction::Stop;
    };
    let watch_logins = order_watch_requests(watch_logins, &mut state.dispatch_order);
    if watch_logins.is_empty() {
        context.health.success("minute");
        context.health.success("minute-watch");
        return if sleep_or_stop(stop, std::time::Duration::from_secs(20)).await {
            WatchAction::Stop
        } else {
            WatchAction::Continue
        };
    }
    if let Some(loop_gap) = loop_gap {
        context.spade_urls.lock().await.clear();
        let message = context
            .observability
            .minute_watcher_resume_message(loop_gap, watch_logins.len());
        tracing::warn!("{message}");
    }
    let interval = tm_domain::watch_interval(watch_logins.len());
    for (slot, login) in watch_logins {
        if *stop.borrow() {
            return WatchAction::Stop;
        }
        let (action, outcome) = watch_streamer_login(stop, context, &login, slot, interval).await;
        if let Some(outcome) = outcome {
            record_watch_attempt(&mut state.watch_failures, &login, outcome, time_now());
        }
        if action == WatchAction::Stop {
            return WatchAction::Stop;
        }
    }
    WatchAction::Continue
}

fn order_watch_requests(selected: Vec<String>, previous: &mut Vec<String>) -> Vec<(usize, String)> {
    // Keep logical health slots while moving new channels ahead of retained
    // ones. Preserve that order next pass to avoid a second pacing shift.
    let mut requests = selected.into_iter().enumerate().collect::<Vec<_>>();
    requests.sort_by_key(|(_, login)| {
        previous
            .iter()
            .position(|previous| previous == login)
            .map_or(0, |index| index + 1)
    });
    *previous = requests.iter().map(|(_, login)| login.clone()).collect();
    requests
}

#[allow(clippy::too_many_lines)]
async fn select_watch_logins(
    context: &MinuteWatcherContext,
    state: &mut MinuteWatcherState,
    now: tm_runtime::RuntimeTime,
) -> Option<Vec<String>> {
    reap_metadata_refresh(context, state).await;
    let snapshot = snapshot_or_log(&context.runtime, "minute watcher snapshot failed").await?;
    if state.metadata_refresh.is_none() {
        let runtime = context.runtime.clone();
        let twitch = Arc::clone(&context.twitch);
        let streamers = snapshot.streamers.clone();
        let observability = context.observability.clone();
        let health = context.health.clone();
        let claim_coordinator = context.claim_coordinator.clone();
        state.metadata_refresh = Some(MetadataRefreshHandle::new(tokio::spawn(async move {
            refresh_watch_selection_metadata_inner(
                &runtime,
                &twitch,
                &streamers,
                &observability,
                now,
                Some(health),
                claim_coordinator,
            )
            .await
        })));
        context.health.activity("minute-metadata");
    }
    let eligible = available_watch_logins(
        snapshot.watch_target_logins(now),
        &mut state.watch_failures,
        now,
    );
    let streak_candidates = eligible
        .iter()
        .filter_map(|login| {
            let streamer = snapshot
                .streamers
                .iter()
                .find(|streamer| &streamer.username == login)?;
            let stream = streamer.stream.as_ref()?;
            (tm_domain::should_prioritize_streak(streamer, now)
                && !stream.broadcast_id.trim().is_empty())
            .then(|| StreakCandidate {
                login: login.clone(),
                broadcast_id: stream.broadcast_id.clone(),
            })
        })
        .collect::<Vec<_>>();
    if let Some((login, channel_id)) = state
        .watchdog
        .stalled_logins(
            &snapshot.streamers,
            &state.selected_channel_ids,
            &state.watch_failures,
            now,
            context.health.pubsub_ready(),
            StdInstant::now(),
        )
        .into_iter()
        .next()
    {
        if state.watch_rotation.defer_stalled(&login) {
            state.watchdog.mark_recovery_attempted(&channel_id);
            tracing::warn!(
                task = "minute-watch",
                error_class = "progress-stalled",
                streamer = %login,
                recovery = "rotate-spare",
                "confirmed point progress stalled; rotating the watch slot"
            );
        }
    }
    let watch_logins = state.watch_rotation.select_with_campaigns(
        &eligible,
        &snapshot.campaign_watch_logins(now),
        &streak_candidates,
        now,
    );
    context.health.set_watch_selection(
        &watch_logins
            .iter()
            .enumerate()
            .filter_map(|(slot, login)| {
                let (channel_index, streamer) = snapshot
                    .streamers
                    .iter()
                    .enumerate()
                    .find(|(_, streamer)| &streamer.username == login)?;
                let broadcast_id = streamer
                    .stream
                    .as_ref()
                    .map_or("", |stream| stream.broadcast_id.as_str());
                Some((
                    slot,
                    channel_index,
                    streamer.channel_id.as_str(),
                    broadcast_id,
                    state.watch_rotation.selection_reason(login),
                ))
            })
            .collect::<Vec<_>>(),
    );
    for (slot, login) in watch_logins.iter().enumerate() {
        if let Some(streamer) = snapshot
            .streamers
            .iter()
            .find(|streamer| &streamer.username == login)
        {
            context.health.watch_slot_measurement(
                slot,
                streamer
                    .last_server_confirmed_points_at
                    .filter(|points_at| {
                        streamer
                            .stream
                            .as_ref()
                            .and_then(|stream| stream.stream_up_at)
                            .is_some_and(|start| *points_at >= start)
                    }),
                streamer.last_context_observed_at,
            );
            let (progress, age) = state.watchdog.progress(
                &streamer.channel_id,
                streamer
                    .stream
                    .as_ref()
                    .map_or("", |stream| stream.broadcast_id.as_str()),
                StdInstant::now(),
            );
            context.health.watch_slot_progress(slot, progress, age);
        }
    }
    let selected_channel_ids = watch_logins
        .iter()
        .filter_map(|login| {
            snapshot
                .streamers
                .iter()
                .find(|streamer| &streamer.username == login)
                .map(|streamer| streamer.channel_id.clone())
        })
        .collect::<HashSet<_>>();
    for released in released_watch_channel_ids(&state.selected_channel_ids, &selected_channel_ids) {
        if let Err(error) = context.runtime.reset_watch_progress(released).await {
            tracing::warn!(%error, "watch slot release could not reset streak progress");
            return None;
        }
    }
    state.selected_channel_ids = selected_channel_ids;
    Some(watch_logins)
}

pub(crate) fn released_watch_channel_ids(
    previous: &HashSet<String>,
    selected: &HashSet<String>,
) -> Vec<String> {
    previous.difference(selected).cloned().collect()
}

async fn reap_metadata_refresh(context: &MinuteWatcherContext, state: &mut MinuteWatcherState) {
    if !state
        .metadata_refresh
        .as_ref()
        .is_some_and(MetadataRefreshHandle::is_finished)
    {
        return;
    }
    let Some(handle) = state.metadata_refresh.take() else {
        return;
    };
    match handle.wait().await {
        Ok(0) => context.health.success("minute-metadata"),
        Ok(failures) => {
            context
                .health
                .failure("minute-metadata", "metadata-refresh");
            tracing::warn!(
                task = "minute-metadata",
                error_class = "metadata-refresh",
                failures,
                "watch selection metadata refresh failed"
            );
        }
        Err(error) => {
            context.health.failure("minute-metadata", "metadata-task");
            tracing::warn!(
                task = "minute-metadata",
                error_class = "metadata-task",
                %error,
                "watch selection metadata refresh task failed"
            );
        }
    }
}

async fn stop_metadata_refresh(state: &mut MinuteWatcherState) {
    if let Some(handle) = state.metadata_refresh.take() {
        handle.abort();
        let _ = handle.wait().await;
    }
}

async fn snapshot_or_log(
    runtime: &tm_runtime::RuntimeHandle,
    message: &'static str,
) -> Option<tm_runtime::RuntimeState> {
    match runtime.state_snapshot().await {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            tracing::warn!(%error, "{message}");
            None
        }
    }
}

async fn watch_streamer_login(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    context: &MinuteWatcherContext,
    login: &str,
    slot: usize,
    interval: std::time::Duration,
) -> (WatchAction, Option<WatchAttemptOutcome>) {
    let started = StdInstant::now();
    let Some(snapshot) =
        snapshot_or_log(&context.runtime, "minute watcher refresh snapshot failed").await
    else {
        context.health.watch_slot_activity(slot);
        return (WatchAction::Stop, None);
    };
    let Some(streamer) = snapshot
        .streamers
        .iter()
        .find(|streamer| streamer.username == login)
        .cloned()
    else {
        context.health.watch_slot_activity(slot);
        return (WatchAction::Continue, None);
    };
    if !streamer.is_online || streamer.channel_id.trim().is_empty() {
        context.health.watch_slot_activity(slot);
        return (WatchAction::Continue, None);
    }
    let outcome = record_minute_watch_result(
        context,
        &streamer,
        slot,
        tokio::time::timeout(
            MINUTE_WATCHER_REQUEST_TIMEOUT,
            send_minute_watched_for_streamer(
                &context.runtime,
                &context.twitch,
                &context.spade_urls,
                &streamer,
                &context.user_id,
                &context.observability,
            ),
        )
        .await,
    );
    // Request work consumes the interval; an overrun starts a fresh attempt,
    // without accumulating missed ticks to replay.
    if sleep_or_stop(stop, interval.saturating_sub(started.elapsed())).await {
        (WatchAction::Stop, Some(outcome))
    } else {
        (WatchAction::Continue, Some(outcome))
    }
}

fn record_minute_watch_result(
    context: &MinuteWatcherContext,
    streamer: &Streamer,
    slot: usize,
    result: std::result::Result<Result<()>, tokio::time::error::Elapsed>,
) -> WatchAttemptOutcome {
    match result {
        Ok(Ok(())) => {
            context.health.success("minute");
            context.health.success("minute-watch");
            context.health.watch_slot_success(slot);
            WatchAttemptOutcome::Success
        }
        Ok(Err(error)) => {
            context.health.failure("minute-watch", "watch-request");
            context.health.watch_slot_failure(slot, "watch-request");
            tracing::warn!(task = "minute-watch", error_class = "watch-request", streamer = %streamer.username, error = %format!("{error:#}"), "minute watched failed");
            WatchAttemptOutcome::RequestFailure
        }
        Err(_) => {
            context.health.failure("minute-watch", "watch-timeout");
            context.health.watch_slot_failure(slot, "watch-timeout");
            tracing::warn!(
                task = "minute-watch",
                error_class = "watch-timeout",
                streamer = %streamer.username,
                timeout_seconds = MINUTE_WATCHER_REQUEST_TIMEOUT.as_secs(),
                "minute watched timed out"
            );
            WatchAttemptOutcome::Timeout
        }
    }
}

pub(crate) fn record_watch_attempt(
    failures: &mut HashMap<String, WatchFailureState>,
    login: &str,
    outcome: WatchAttemptOutcome,
    now: tm_runtime::RuntimeTime,
) {
    if outcome == WatchAttemptOutcome::Success {
        failures.remove(login);
        return;
    }
    let failure = failures.entry(login.to_string()).or_default();
    failure.consecutive_requests = failure.consecutive_requests.saturating_add(1);
    if outcome == WatchAttemptOutcome::Timeout
        || failure.consecutive_requests >= WATCH_REQUEST_FAILURE_THRESHOLD
    {
        failure.backoff_until =
            Some(now + std::time::Duration::from_secs(WATCH_CHANNEL_BACKOFF_SECONDS));
    }
}

pub(crate) fn available_watch_logins(
    eligible: Vec<String>,
    failures: &mut HashMap<String, WatchFailureState>,
    now: tm_runtime::RuntimeTime,
) -> Vec<String> {
    failures.retain(|login, failure| {
        eligible.contains(login) && failure.backoff_until.is_none_or(|until| now < until)
    });
    eligible
        .into_iter()
        .filter(|login| {
            failures
                .get(login)
                .is_none_or(|failure| failure.backoff_until.is_none())
        })
        .collect()
}

#[cfg(test)]
pub(crate) async fn refresh_watch_selection_metadata(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    streamers: &[Streamer],
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
) -> usize {
    refresh_watch_selection_metadata_inner(
        runtime,
        twitch,
        streamers,
        observability,
        now,
        None,
        crate::drops::DropClaimCoordinator::default(),
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn refresh_watch_selection_metadata_inner(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    streamers: &[Streamer],
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
    health: Option<HealthTracker>,
    claim_coordinator: crate::drops::DropClaimCoordinator,
) -> usize {
    let mut failures = 0_usize;
    let mut refreshes = tokio::task::JoinSet::new();
    let should_claim_drops = streamers
        .iter()
        .any(|streamer| streamer.settings.claim_drops);
    let excluded_campaign_ids = Arc::new(
        if streamers.iter().any(|streamer| {
            streamer.is_online
                && (streamer.settings.farm_drops || should_claim_drops)
                && streamer
                    .stream
                    .as_ref()
                    .is_none_or(|stream| stream.update_required_at(now))
        }) {
            match twitch.fetch_inventory_snapshot_typed().await {
                Ok(snapshot) => {
                    if should_claim_drops {
                        if let Err(error) = crate::drops::claim_inventory_drops_with_coordinator(
                            twitch,
                            "prompt",
                            &snapshot.drops,
                            observability,
                            health.as_ref(),
                            &claim_coordinator,
                        )
                        .await
                        {
                            failures += 1;
                            tracing::warn!(
                                error_class = "prompt-drop-claim",
                                %error,
                                "prompt drop claim failed"
                            );
                        }
                    }
                    excluded_drop_campaign_ids(snapshot)
                }
                Err(error) => {
                    failures += 1;
                    if let Some(health) = health.as_ref() {
                        health.failure("drop", "inventory-or-claim");
                    }
                    tracing::warn!(
                        error_class = "campaign-inventory",
                        %error,
                        "drop campaign inventory refresh failed"
                    );
                    HashSet::new()
                }
            }
        } else {
            HashSet::new()
        },
    );
    for streamer in streamers.iter().filter(|streamer| {
        streamer.is_online
            && !streamer.channel_id.trim().is_empty()
            && streamer
                .stream
                .as_ref()
                .is_none_or(|stream| stream.update_required_at(now))
    }) {
        while refreshes.len() >= WATCH_SELECTION_REFRESH_CONCURRENCY {
            if let Some(result) = refreshes.join_next().await {
                failures += usize::from(log_watch_selection_refresh_result(result));
            }
        }

        let runtime = runtime.clone();
        let twitch = Arc::clone(twitch);
        let observability = observability.clone();
        let streamer = streamer.clone();
        let excluded_campaign_ids = Arc::clone(&excluded_campaign_ids);
        refreshes.spawn(async move {
            let previous_game = streamer_game_name(&streamer);
            let Some(mut expected_generation) = runtime
                .begin_stream_update(streamer.channel_id.clone())
                .await?
            else {
                return Ok::<_, anyhow::Error>(());
            };
            let (streamer, info) = match twitch.fetch_stream_info(&streamer.username).await {
                Ok(info) => (streamer, info),
                Err(error) => {
                    let Some(recovered_streamer) = handle_minute_watched_info_error(
                        &runtime,
                        &twitch,
                        &streamer,
                        &observability,
                        now,
                        error,
                    )
                    .await?
                    else {
                        return Ok(());
                    };
                    let Some(generation) = runtime
                        .begin_stream_update(recovered_streamer.channel_id.clone())
                        .await?
                    else {
                        return Ok(());
                    };
                    expected_generation = generation;
                    let info = twitch
                        .fetch_stream_info(&recovered_streamer.username)
                        .await
                        .with_context(|| {
                            format!(
                                "refresh stream info after channel rename for {}",
                                recovered_streamer.username
                            )
                        })?;
                    (recovered_streamer, info)
                }
            };
            apply_live_stream_update(
                &runtime,
                &streamer,
                &info,
                &observability,
                now,
                expected_generation,
            )
            .await?;
            refresh_drop_campaign_eligibility(
                &runtime,
                &twitch,
                &streamer,
                &info,
                &excluded_campaign_ids,
            )
            .await?;
            log_stream_presence_changes(
                &observability,
                &streamer,
                previous_game.as_deref(),
                &info.game_name,
            );
            Ok::<_, anyhow::Error>(())
        });
    }

    while let Some(result) = refreshes.join_next().await {
        failures += usize::from(log_watch_selection_refresh_result(result));
    }

    failures
}

fn excluded_drop_campaign_ids(snapshot: tm_twitch::InventorySnapshot) -> HashSet<String> {
    snapshot
        .completed_campaign_ids
        .into_iter()
        .chain(snapshot.subscription_only_campaign_ids)
        .collect()
}

fn log_watch_selection_refresh_result(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> bool {
    match result {
        Ok(Ok(())) => false,
        Ok(Err(error)) => {
            tracing::warn!(
                task = "minute",
                error_class = "metadata-refresh",
                %error,
                "watch selection refresh failed"
            );
            true
        }
        Err(error) => {
            tracing::warn!(
                task = "minute",
                error_class = "metadata-refresh",
                %error,
                "watch selection refresh task failed"
            );
            true
        }
    }
}

async fn refresh_drop_campaign_eligibility(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    info: &tm_twitch::StreamInfo,
    excluded_campaign_ids: &HashSet<String>,
) -> Result<()> {
    if !streamer.settings.farm_drops {
        return Ok(());
    }

    let has_game = !info.game_name.trim().is_empty()
        && info
            .game_id
            .as_deref()
            .is_some_and(|game_id| !game_id.trim().is_empty());
    if !has_game {
        runtime
            .set_drop_campaign_eligibility_if_current(
                streamer.channel_id.clone(),
                info.id.clone(),
                info.game_id.clone(),
                false,
            )
            .await?;
        return Ok(());
    }

    let campaign_ids = twitch
        .fetch_available_drop_campaigns_typed(&streamer.channel_id)
        .await
        .with_context(|| {
            format!(
                "refresh drop campaign eligibility for {}",
                streamer.username
            )
        })?;
    runtime
        .set_drop_campaign_eligibility_if_current(
            streamer.channel_id.clone(),
            info.id.clone(),
            info.game_id.clone(),
            has_unfinished_campaign(&campaign_ids, excluded_campaign_ids),
        )
        .await?;
    Ok(())
}

pub(crate) fn has_unfinished_campaign(
    available_campaign_ids: &[String],
    excluded_campaign_ids: &HashSet<String>,
) -> bool {
    available_campaign_ids
        .iter()
        .any(|campaign_id| !excluded_campaign_ids.contains(campaign_id))
}

pub(crate) async fn send_minute_watched_for_streamer(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    spade_urls: &tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
    streamer: &Streamer,
    user_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    if !streamer.can_earn_channel_points() && !streamer.can_watch_drop_campaign() {
        return Ok(());
    }
    let now = time_now();
    let streamer = match watch_metadata_defect(streamer, now) {
        None => streamer.clone(),
        Some(defect) => {
            recover_watch_metadata(runtime, twitch, streamer, observability, now, defect).await?
        }
    };
    if !streamer.can_earn_channel_points() && !streamer.can_watch_drop_campaign() {
        return Ok(());
    }
    let records_point_progress = streamer.can_earn_channel_points();
    let mut stream = streamer.stream.clone().unwrap_or_default();
    stream.payload = vec![build_minute_watched_event(&streamer, &stream, user_id)];

    twitch
        .prime_live_playback(&streamer.username)
        .await
        .with_context(|| format!("prime live playback for {}", streamer.username))?;

    let status = send_minute_watched_with_spade_cache(
        spade_urls,
        &streamer.username,
        |login| async move {
            twitch
                .fetch_spade_url(&login)
                .await
                .with_context(|| format!("resolve spade url for {login}"))
        },
        |spade_url| {
            let stream = stream.clone();
            async move {
                twitch
                    .send_minute_watched(&spade_url, &stream)
                    .await
                    .map_err(anyhow::Error::from)
            }
        },
    )
    .await?;
    if status == StatusCode::NO_CONTENT {
        if records_point_progress {
            runtime
                .mark_minute_watched(
                    streamer.channel_id.clone(),
                    stream.broadcast_id.clone(),
                    now,
                )
                .await?;
        }
        return Ok(());
    }

    Err(anyhow!(
        "minute watched returned unexpected status {status} for {}",
        streamer.username
    ))
}

/// Reports why the cached snapshot cannot back a minute-watched send, if it
/// cannot. The batched refresh normally keeps this `None`; a bounded age check
/// stops a stalled refresh from posting watch events against metadata that no
/// longer describes the broadcast.
pub(crate) fn watch_metadata_defect(
    streamer: &Streamer,
    now: tm_runtime::RuntimeTime,
) -> Option<&'static str> {
    let Some(stream) = streamer.stream.as_ref() else {
        return Some("missing stream metadata");
    };
    if stream.broadcast_id.trim().is_empty() {
        return Some("missing broadcast id");
    }
    if stream.last_update.is_some_and(|last_update| {
        let age = (now - last_update).whole_seconds();
        (0..=MAX_WATCH_METADATA_AGE_SECONDS).contains(&age)
    }) {
        None
    } else {
        Some("stale stream metadata")
    }
}

/// One bounded inline refresh for the rare pass whose batched refresh did not
/// land. Without it a single failed refresh would fail every watch tick for the
/// channel until the next batch succeeded.
async fn recover_watch_metadata(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
    defect: &'static str,
) -> Result<Streamer> {
    tracing::debug!(
        streamer = %streamer.username,
        defect,
        "refreshing stream metadata inline before minute watched"
    );
    let Some(expected_generation) = runtime
        .begin_stream_update(streamer.channel_id.clone())
        .await?
    else {
        return Ok(streamer.clone());
    };
    let info = twitch
        .fetch_stream_info(&streamer.username)
        .await
        .with_context(|| format!("{defect} for {}", streamer.username))?;
    let recovered = apply_live_stream_update(
        runtime,
        streamer,
        &info,
        observability,
        now,
        expected_generation,
    )
    .await?;
    if let Some(defect) = watch_metadata_defect(&recovered, now) {
        return Err(anyhow!("{defect} for {} after refresh", streamer.username));
    }
    Ok(recovered)
}

pub(crate) async fn handle_minute_watched_info_error(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
    error: tm_twitch::TwitchClientError,
) -> Result<Option<Streamer>> {
    let Some(generation) = runtime
        .begin_stream_update(streamer.channel_id.clone())
        .await?
    else {
        return Ok(None);
    };
    let is_live = twitch
        .is_stream_live(&streamer.channel_id)
        .await
        .with_context(|| format!("confirm live status after stream metadata error: {error}"))?;
    if is_live {
        if matches!(
            &error,
            tm_twitch::TwitchClientError::MissingField("data.user" | "data.user.stream")
        ) {
            if let Ok(login) = twitch.fetch_channel_login_by_id(&streamer.channel_id).await {
                if login != streamer.username {
                    runtime
                        .update_streamer_login(streamer.channel_id.clone(), login.clone())
                        .await?;
                    let streamer_name = observability.streamer_name(streamer);
                    tracing::warn!(
                        operation = "update_streamer_login",
                        "streamer login changed for {streamer_name}; runtime identity refreshed, update config before restart"
                    );
                    let mut recovered = streamer.clone();
                    recovered.username = login;
                    recovered.watch_suspended_until = None;
                    return Ok(Some(recovered));
                }
            }
            runtime
                .suspend_watching(
                    streamer.channel_id.clone(),
                    now + std::time::Duration::from_secs(RENAME_RECOVERY_SUSPENSION_SECONDS),
                )
                .await?;
            let streamer_name = observability.streamer_name(streamer);
            tracing::warn!(
                operation = "suspend_watching",
                suspension_seconds = RENAME_RECOVERY_SUSPENSION_SECONDS,
                "live channel identity for {streamer_name} could not be refreshed; releasing watch slot temporarily"
            );
        }
        return Err(error.into());
    }
    let changed = runtime
        .set_presence_if_current(&streamer.channel_id, false, generation, now)
        .await?;
    if changed && streamer.is_online {
        let message = observability.offline_message(streamer);
        tracing::info!(operation = "set_offline", "{message}");
        observability
            .send_event(DiscordEvent::StreamerOffline, &message)
            .await;
    }
    Ok(None)
}

pub(crate) async fn apply_live_stream_update(
    runtime: &tm_runtime::RuntimeHandle,
    streamer: &Streamer,
    info: &tm_twitch::StreamInfo,
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
    expected_generation: u64,
) -> Result<Streamer> {
    let Some(updated_streamer) = runtime
        .apply_stream_update(
            tm_runtime::StreamUpdate {
                channel_id: streamer.channel_id.clone(),
                id: info.id.clone(),
                title: info.title.clone(),
                game_name: info.game_name.clone(),
                game_id: info.game_id.clone(),
                viewers_count: info.viewers_count,
                tags: info.tags.clone(),
                expected_generation,
            },
            now,
        )
        .await?
    else {
        return runtime
            .state_snapshot()
            .await?
            .streamers
            .into_iter()
            .find(|current| current.channel_id == streamer.channel_id)
            .ok_or_else(|| {
                anyhow!(
                    "streamer {} disappeared during metadata refresh",
                    streamer.channel_id
                )
            });
    };
    if !streamer.is_online {
        let message = observability.online_message(&updated_streamer);
        tracing::info!(operation = "set_online", "{message}");
        observability
            .send_event(DiscordEvent::StreamerOnline, &message)
            .await;
    }
    Ok(updated_streamer)
}

pub(crate) fn log_stream_presence_changes(
    observability: &AppObservability,
    streamer: &Streamer,
    previous_game: Option<&str>,
    current_game: &str,
) {
    if let Some(message) =
        observability.game_change_message(streamer, previous_game.unwrap_or_default(), current_game)
    {
        tracing::info!(operation = "update_stream", "{message}");
    }
}

pub(crate) async fn resolve_spade_url<FetchSpade, FetchFuture, Error>(
    spade_urls: &tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
    streamer_username: &str,
    force_refresh: bool,
    fetch_spade: FetchSpade,
) -> std::result::Result<String, Error>
where
    FetchSpade: Fn(String) -> FetchFuture,
    FetchFuture: std::future::Future<Output = std::result::Result<String, Error>>,
{
    if !force_refresh {
        let cache = spade_urls.lock().await;
        if let Some(SpadeCacheEntry::Ready(entry)) = cache.get(streamer_username) {
            if entry.fetched_at.elapsed() < SPADE_URL_TTL {
                return Ok(entry.url.clone());
            }
        }
    }

    // Keep only completed values in the cache. There is one production caller,
    // so duplicate concurrent refreshes are cheaper and safer than storing an
    // owner that can be cancelled while its network request is pending.
    let resolved = fetch_spade(streamer_username.to_string()).await;
    if let Ok(url) = &resolved {
        spade_urls.lock().await.insert(
            streamer_username.to_string(),
            SpadeCacheEntry::Ready(CachedSpadeUrl {
                url: url.clone(),
                fetched_at: StdInstant::now(),
            }),
        );
    }
    resolved
}

pub(crate) async fn send_minute_watched_with_spade_cache<
    FetchSpade,
    FetchFuture,
    SendMinute,
    SendFuture,
    Error,
>(
    spade_urls: &tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
    streamer_username: &str,
    fetch_spade: FetchSpade,
    send_minute_watched: SendMinute,
) -> std::result::Result<StatusCode, Error>
where
    FetchSpade: Fn(String) -> FetchFuture,
    FetchFuture: std::future::Future<Output = std::result::Result<String, Error>>,
    SendMinute: Fn(String) -> SendFuture,
    SendFuture: std::future::Future<Output = std::result::Result<StatusCode, Error>>,
{
    let spade_url = resolve_spade_url(spade_urls, streamer_username, false, &fetch_spade).await?;
    if let Ok(StatusCode::NO_CONTENT) = send_minute_watched(spade_url.clone()).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        let refreshed =
            resolve_spade_url(spade_urls, streamer_username, true, &fetch_spade).await?;
        send_minute_watched(refreshed).await
    }
}

pub(crate) fn build_minute_watched_event(
    streamer: &Streamer,
    stream: &Stream,
    user_id: &str,
) -> serde_json::Value {
    let mut properties = serde_json::Map::from_iter([
        (String::from("channel_id"), json!(streamer.channel_id)),
        (String::from("broadcast_id"), json!(stream.broadcast_id)),
        (String::from("user_id"), json!(user_id)),
        (String::from("player"), json!("site")),
        (String::from("live"), json!(true)),
        (String::from("channel"), json!(streamer.username)),
    ]);
    let game_name = stream.game_name();
    if streamer.settings.farm_drops && !game_name.trim().is_empty() {
        properties.insert(String::from("game"), json!(game_name));
        if let Some(game_id) = stream
            .game_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            properties.insert(String::from("game_id"), json!(game_id));
        }
    }
    serde_json::Value::Object(serde_json::Map::from_iter([
        (String::from("event"), json!("minute-watched")),
        (
            String::from("properties"),
            serde_json::Value::Object(properties),
        ),
    ]))
}

#[cfg(test)]
#[path = "../tests/unit/watch_dispatch_tests.rs"]
mod watch_dispatch_tests;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::observability::AppObservabilitySettings;
    use tm_config::ConfigFile;
    use tm_domain::{Game, OffsetDateTime, Stream, Streamer};
    use tm_observability::DiscordClient;
    use tm_twitch::{TwitchClient, TwitchEndpoints};

    fn timestamp(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).expect("valid fixture timestamp")
    }

    #[tokio::test]
    async fn watch_cadence_includes_request_time_without_catch_up() {
        use std::io::{BufRead, BufReader, Read, Write};

        let interval = Duration::from_millis(500);
        let delays = [200, 200, 200, 700, 0];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            for delay in delays.into_iter().chain([0]) {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                }
                assert!(content_length <= 4096);
                reader.read_exact(&mut vec![0_u8; content_length]).unwrap();
                thread::sleep(Duration::from_millis(delay));
                let body = r#"{"data":{"streamPlaybackAccessToken":null}}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let mut state = tm_runtime::RuntimeState::from_targets(
            &ConfigFile::default(),
            &[String::from("alice")],
            time_now(),
        );
        state.streamers[0] = measured_streamer(true, "broadcast-1", None, None);
        state.streamers[0].stream.as_mut().unwrap().last_update = Some(time_now());
        let context = MinuteWatcherContext {
            runtime: tm_runtime::spawn_runtime_state(state),
            twitch: Arc::new(TwitchClient::with_client_and_endpoints(
                reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap(),
                "synthetic-token",
                "test-agent",
                TwitchEndpoints {
                    twitch_url: base.clone(),
                    gql_url: format!("{base}/gql"),
                    playback_url: format!("{base}/hls/"),
                },
            )),
            user_id: String::from("synthetic-viewer"),
            observability: AppObservability::new(
                None,
                DiscordClient::new(Duration::from_secs(1)).unwrap(),
                AppObservabilitySettings::default(),
            ),
            health: HealthTracker::default(),
            claim_coordinator: crate::drops::DropClaimCoordinator::default(),
            spade_urls: tokio::sync::Mutex::new(HashMap::new()),
        };
        let (sender, mut stop) = tokio::sync::watch::channel(false);
        let mut elapsed = Vec::new();
        for _ in delays {
            let started = Instant::now();
            let (action, outcome) =
                watch_streamer_login(&mut stop, &context, "alice", 0, interval).await;
            assert!(action == WatchAction::Continue);
            assert!(outcome == Some(WatchAttemptOutcome::RequestFailure));
            elapsed.push(started.elapsed());
        }
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            sender.send(true).unwrap();
        });
        let cancellation_started = Instant::now();
        let (action, _) = watch_streamer_login(&mut stop, &context, "alice", 0, interval).await;
        cancel.await.unwrap();
        server.join().unwrap();
        assert!(action == WatchAction::Stop);
        assert!(cancellation_started.elapsed() < interval);
        println!(
            "watch cadence elapsed milliseconds: {:?}",
            elapsed.iter().map(Duration::as_millis).collect::<Vec<_>>()
        );
        // Aggregate three attempts to tolerate scheduler noise while rejecting
        // the old extra 200 ms wait on every request.
        assert!(elapsed[..3].iter().sum::<Duration>() < Duration::from_millis(1_800));
        assert!(elapsed.iter().all(|duration| *duration >= interval));
        assert!(elapsed[3] >= Duration::from_millis(700));
        assert!(elapsed[3] < Duration::from_millis(1_000));
    }

    fn pending_inventory_server(
        released: Arc<AtomicBool>,
        request_seen: Arc<AtomicBool>,
    ) -> (TwitchEndpoints, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind pending inventory server");
        listener
            .set_nonblocking(true)
            .expect("set pending inventory server nonblocking");
        let address = listener
            .local_addr()
            .expect("pending inventory server address");
        let handle = thread::spawn(move || {
            let started = Instant::now();
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        request_seen.store(true, Ordering::Release);
                        while !released.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        drop(stream);
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if released.load(Ordering::Acquire)
                            || started.elapsed() >= Duration::from_secs(5)
                        {
                            return;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        let base = format!("http://{address}");
        (
            TwitchEndpoints {
                twitch_url: base.clone(),
                gql_url: format!("{base}/gql"),
                playback_url: format!("{base}/hls/"),
            },
            handle,
        )
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn measured_streamer(
        online: bool,
        broadcast_id: &str,
        points_at: Option<OffsetDateTime>,
        context_at: Option<OffsetDateTime>,
    ) -> Streamer {
        Streamer {
            username: String::from("alice"),
            channel_id: String::from("channel-alice"),
            is_online: online,
            channel_points_enabled: Some(true),
            stream: Some(Stream {
                broadcast_id: broadcast_id.to_string(),
                stream_up_at: Some(timestamp(100)),
                last_update: Some(timestamp(100)),
                ..Stream::default()
            }),
            last_server_confirmed_points_at: points_at,
            last_context_observed_at: context_at,
            ..Streamer::default()
        }
    }

    fn observe(
        watchdog: &mut WatchdogState,
        streamer: &Streamer,
        selected: &HashSet<String>,
        monotonic_now: Instant,
    ) -> Vec<(String, String)> {
        watchdog.stalled_logins(
            std::slice::from_ref(streamer),
            selected,
            &HashMap::new(),
            timestamp(200),
            true,
            monotonic_now,
        )
    }

    #[test]
    fn first_credit_visibility_never_triggers_rotation_and_resets_with_measurement() {
        use crate::status::WatchProgress;
        let selected = HashSet::from([String::from("channel-alice")]);
        let mut watchdog = WatchdogState::default();
        let mut streamer = measured_streamer(true, "broadcast-1", None, Some(timestamp(130)));
        let start = Instant::now();
        assert!(observe(&mut watchdog, &streamer, &selected, start).is_empty());
        assert_eq!(
            watchdog.progress("channel-alice", "broadcast-1", start),
            (WatchProgress::AwaitingFirstCredit, Some(0))
        );
        let later = start + Duration::from_secs(1_800);
        assert!(observe(&mut watchdog, &streamer, &selected, later).is_empty());
        assert_eq!(
            watchdog.progress("channel-alice", "broadcast-1", later),
            (WatchProgress::FirstCreditOverdue, Some(1_800))
        );
        assert_eq!(
            watchdog.progress("channel-alice", "broadcast-2", later),
            (WatchProgress::MeasurementUnavailable, None)
        );
        streamer.last_context_observed_at = None;
        assert!(observe(&mut watchdog, &streamer, &selected, later).is_empty());
        assert_eq!(
            watchdog.progress("channel-alice", "broadcast-1", later),
            (WatchProgress::MeasurementUnavailable, None)
        );
        streamer.last_context_observed_at = Some(timestamp(150));
        assert!(observe(&mut watchdog, &streamer, &selected, later).is_empty());
        assert_eq!(
            watchdog.progress("channel-alice", "broadcast-1", later),
            (WatchProgress::AwaitingFirstCredit, Some(0))
        );
        streamer.last_server_confirmed_points_at = Some(timestamp(160));
        assert!(observe(&mut watchdog, &streamer, &selected, later).is_empty());
        assert_eq!(
            watchdog.progress("channel-alice", "broadcast-1", later),
            (WatchProgress::Earning, Some(0))
        );
        let stalled = later + Duration::from_secs(1_800);
        assert!(!observe(&mut watchdog, &streamer, &selected, stalled).is_empty());
        assert_eq!(
            watchdog.progress("channel-alice", "broadcast-1", stalled),
            (WatchProgress::Stalled, Some(1_800))
        );
    }

    #[test]
    fn watchdog_requires_live_measurements_and_uses_monotonic_stall_time() {
        let selected = HashSet::from([String::from("channel-alice")]);
        let mut watchdog = WatchdogState::default();
        let mut streamer = measured_streamer(true, "broadcast-1", Some(timestamp(120)), None);
        let start = Instant::now();

        assert!(observe(&mut watchdog, &streamer, &selected, start).is_empty());
        streamer.last_context_observed_at = Some(timestamp(130));
        assert!(observe(&mut watchdog, &streamer, &selected, start).is_empty());
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(1_799)
        )
        .is_empty());
        assert_eq!(
            observe(
                &mut watchdog,
                &streamer,
                &selected,
                start + Duration::from_secs(1_800),
            ),
            vec![(String::from("alice"), String::from("channel-alice"))]
        );

        watchdog.mark_recovery_attempted("channel-alice");
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(3_600),
        )
        .is_empty());

        // A later confirmed point starts a new episode; a context update alone
        // must not make the stalled channel appear healthy.
        streamer.last_server_confirmed_points_at = Some(timestamp(140));
        streamer.last_context_observed_at = Some(timestamp(150));
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(3_601),
        )
        .is_empty());
        streamer.last_context_observed_at = Some(timestamp(160));
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(5_400),
        )
        .is_empty());
        assert!(!observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(5_401),
        )
        .is_empty());
    }

    #[test]
    fn watchdog_drops_offline_and_unmeasurable_channels_without_alarm() {
        let selected = HashSet::from([String::from("channel-alice")]);
        let mut watchdog = WatchdogState::default();
        let start = Instant::now();
        let mut streamer = measured_streamer(
            true,
            "broadcast-1",
            Some(timestamp(120)),
            Some(timestamp(130)),
        );
        assert!(observe(&mut watchdog, &streamer, &selected, start).is_empty());
        streamer.is_online = false;
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(1_800),
        )
        .is_empty());
        streamer.is_online = true;
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(3_600),
        )
        .is_empty());
        streamer.channel_points_enabled = Some(false);
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(5_400),
        )
        .is_empty());
    }

    #[test]
    fn watchdog_resets_after_watch_failures_and_requires_fresh_measurement() {
        let selected = HashSet::from([String::from("channel-alice")]);
        let mut watchdog = WatchdogState::default();
        let streamer = measured_streamer(
            true,
            "broadcast-1",
            Some(timestamp(120)),
            Some(timestamp(130)),
        );
        let start = Instant::now();
        let mut failures = HashMap::from([(
            String::from("alice"),
            super::WatchFailureState {
                consecutive_requests: 1,
                backoff_until: None,
            },
        )]);

        assert!(watchdog
            .stalled_logins(
                std::slice::from_ref(&streamer),
                &selected,
                &failures,
                timestamp(200),
                true,
                start + Duration::from_secs(1_800),
            )
            .is_empty());
        failures.clear();
        assert!(observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(1_800),
        )
        .is_empty());
        assert!(!observe(
            &mut watchdog,
            &streamer,
            &selected,
            start + Duration::from_secs(3_600),
        )
        .is_empty());
    }

    #[tokio::test]
    async fn stale_live_metadata_cannot_resurrect_an_offline_streamer() -> Result<()> {
        let now = timestamp(10_000);
        let mut runtime_state = tm_runtime::RuntimeState::from_targets(
            &ConfigFile::default(),
            &[String::from("alice")],
            now,
        );
        let streamer = runtime_state
            .streamers
            .first_mut()
            .ok_or_else(|| anyhow!("target streamer fixture missing"))?;
        streamer.channel_id = String::from("100");
        streamer.is_online = true;
        streamer.presence_known = true;
        streamer.stream = Some(Stream {
            broadcast_id: String::from("broadcast-1"),
            last_update: Some(now),
            ..Stream::default()
        });
        let request_streamer = streamer.clone();
        let runtime = tm_runtime::spawn_runtime_state(runtime_state);
        let expected_generation = runtime
            .begin_stream_update("100")
            .await?
            .ok_or_else(|| anyhow!("stream update generation missing"))?;
        runtime
            .set_presence("100", false, now + Duration::from_secs(1))
            .await?;

        let info = tm_twitch::StreamInfo {
            id: String::from("broadcast-1"),
            title: String::from("stale title"),
            game_name: String::from("Game"),
            game_id: Some(String::from("game-1")),
            viewers_count: 1,
            tags: Vec::new(),
            created_at: None,
        };
        let observability = AppObservability::new(
            None,
            DiscordClient::new(Duration::from_secs(1))?,
            AppObservabilitySettings::default(),
        );

        let updated = apply_live_stream_update(
            &runtime,
            &request_streamer,
            &info,
            &observability,
            now + Duration::from_secs(2),
            expected_generation,
        )
        .await?;

        assert!(!updated.is_online);
        let snapshot = runtime.state_snapshot().await?;
        let current = snapshot
            .streamers
            .first()
            .ok_or_else(|| anyhow!("target streamer disappeared"))?;
        assert!(!current.is_online);
        assert!(current.offline_at.is_some());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_metadata_does_not_block_watch_selection_and_is_reaped_on_stop() {
        let released = Arc::new(AtomicBool::new(false));
        let request_seen = Arc::new(AtomicBool::new(false));
        let (endpoints, server) =
            pending_inventory_server(Arc::clone(&released), Arc::clone(&request_seen));
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("build test HTTP client"),
            "token",
            "test-agent",
            endpoints,
        ));
        let now = timestamp(10_000);
        let mut runtime_state = tm_runtime::RuntimeState::from_targets(
            &ConfigFile::default(),
            &[String::from("alice")],
            now,
        );
        let streamer = runtime_state
            .streamers
            .first_mut()
            .expect("target streamer fixture");
        streamer.channel_id = String::from("100");
        streamer.is_online = true;
        streamer.presence_known = true;
        streamer.channel_points_enabled = Some(false);
        streamer.settings.farm_drops = true;
        streamer.stream = Some(Stream {
            broadcast_id: String::from("broadcast-1"),
            game: Some(Game::from_name("Game")),
            game_id: Some(String::from("game-1")),
            drop_campaign_eligible: Some(true),
            last_update: Some(now - Duration::from_secs(5 * 60)),
            ..Stream::default()
        });
        let runtime = tm_runtime::spawn_runtime_state(runtime_state);
        let context = MinuteWatcherContext {
            runtime,
            twitch,
            user_id: String::from("viewer"),
            observability: AppObservability::new(
                None,
                DiscordClient::new(Duration::from_secs(1)).expect("test Discord client"),
                AppObservabilitySettings::default(),
            ),
            health: HealthTracker::default(),
            claim_coordinator: crate::drops::DropClaimCoordinator::default(),
            spade_urls: tokio::sync::Mutex::new(HashMap::new()),
        };
        let mut state = MinuteWatcherState {
            watch_rotation: WatchRotation::default(),
            selected_channel_ids: HashSet::new(),
            dispatch_order: Vec::new(),
            last_loop_at: now,
            metadata_refresh: None,
            watch_failures: HashMap::new(),
            watchdog: WatchdogState::default(),
        };

        let selected = tokio::time::timeout(
            Duration::from_secs(1),
            select_watch_logins(&context, &mut state, now),
        )
        .await
        .expect("selection should not wait for metadata")
        .expect("runtime snapshot should remain available");
        assert_eq!(selected, vec![String::from("alice")]);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !request_seen.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("metadata inventory request should be pending");
        assert!(state
            .metadata_refresh
            .as_ref()
            .is_some_and(|handle| !handle.is_finished()));

        stop_metadata_refresh(&mut state).await;
        assert!(state.metadata_refresh.is_none());
        released.store(true, Ordering::Release);
        server.join().expect("pending metadata server should stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parent_abort_aborts_owned_metadata_refresh() {
        let child_started = Arc::new(AtomicBool::new(false));
        let child_dropped = Arc::new(AtomicBool::new(false));
        let parent = {
            let child_started = Arc::clone(&child_started);
            let child_dropped = Arc::clone(&child_dropped);
            tokio::spawn(async move {
                let child = tokio::spawn(async move {
                    let _probe = DropProbe(child_dropped);
                    child_started.store(true, Ordering::Release);
                    tokio::time::sleep(Duration::from_secs(3_600)).await;
                    0_usize
                });
                let _state = MinuteWatcherState {
                    watch_rotation: WatchRotation::default(),
                    selected_channel_ids: HashSet::new(),
                    dispatch_order: Vec::new(),
                    last_loop_at: timestamp(10_000),
                    metadata_refresh: Some(MetadataRefreshHandle::new(child)),
                    watch_failures: HashMap::new(),
                    watchdog: WatchdogState::default(),
                };
                std::future::pending::<()>().await;
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !child_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("metadata child should start");
        parent.abort();
        assert!(parent
            .await
            .expect_err("parent should be cancelled")
            .is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !child_dropped.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("parent cancellation should abort the metadata child");
    }
}
