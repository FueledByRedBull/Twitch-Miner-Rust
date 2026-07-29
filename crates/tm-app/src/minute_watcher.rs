use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant as StdInstant;

use anyhow::{anyhow, Context, Result};
use reqwest::StatusCode;
use serde_json::json;
use tm_domain::Streamer;
use tm_observability::Event as DiscordEvent;
use tm_twitch::TwitchClient;

use crate::observability::{streamer_game_name, AppObservability};
use crate::status::HealthTracker;
use crate::utilities::{sleep_or_stop, time_now};
use crate::watching::{
    minute_watcher_resume_gap, CachedSpadeUrl, SpadeCacheEntry, SpadeResolveAction,
    StreakCandidate, WatchRotation,
};
use crate::{MINUTE_WATCHER_REQUEST_TIMEOUT, SPADE_URL_TTL, WATCH_SELECTION_REFRESH_CONCURRENCY};

const RENAME_RECOVERY_SUSPENSION_SECONDS: u64 = 5 * 60;

struct MinuteWatcherContext {
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    user_id: String,
    observability: AppObservability,
    health: HealthTracker,
    spade_urls: tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
}

struct MinuteWatcherState {
    watch_rotation: WatchRotation,
    selected_channel_ids: HashSet<String>,
    last_loop_at: tm_runtime::RuntimeTime,
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
) -> tokio::task::JoinHandle<()> {
    let context = MinuteWatcherContext {
        runtime,
        twitch,
        user_id,
        observability,
        health,
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
        last_loop_at: time_now(),
    };
    while !*stop.borrow() {
        if run_minute_watcher_pass(&mut stop, &context, &mut state).await == WatchAction::Stop {
            break;
        }
    }
}

async fn run_minute_watcher_pass(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    context: &MinuteWatcherContext,
    state: &mut MinuteWatcherState,
) -> WatchAction {
    let now = time_now();
    let loop_gap = minute_watcher_resume_gap(state.last_loop_at, now);
    state.last_loop_at = now;
    let Some(watch_logins) = select_watch_logins(context, state, now).await else {
        return WatchAction::Stop;
    };
    if watch_logins.is_empty() {
        context.health.success("minute");
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
    for login in watch_logins {
        if *stop.borrow()
            || watch_streamer_login(stop, context, &login, interval).await == WatchAction::Stop
        {
            return WatchAction::Stop;
        }
    }
    WatchAction::Continue
}

async fn select_watch_logins(
    context: &MinuteWatcherContext,
    state: &mut MinuteWatcherState,
    now: tm_runtime::RuntimeTime,
) -> Option<Vec<String>> {
    let snapshot = snapshot_or_log(&context.runtime, "minute watcher snapshot failed").await?;
    if let Err(error) = refresh_watch_selection_metadata(
        &context.runtime,
        &context.twitch,
        &snapshot.streamers,
        &context.observability,
        now,
    )
    .await
    {
        context.health.failure("minute", "metadata-refresh");
        tracing::warn!(task = "minute", error_class = "metadata-refresh", %error, "watch selection metadata refresh failed");
    }
    let snapshot = snapshot_or_log(
        &context.runtime,
        "minute watcher post-refresh snapshot failed",
    )
    .await?;
    let eligible = snapshot.watch_target_logins(now);
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
    let watch_logins = state.watch_rotation.select_with_campaigns(
        &eligible,
        &snapshot.campaign_watch_logins(now),
        &streak_candidates,
        now,
    );
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
    interval: std::time::Duration,
) -> WatchAction {
    let Some(snapshot) =
        snapshot_or_log(&context.runtime, "minute watcher refresh snapshot failed").await
    else {
        return WatchAction::Stop;
    };
    let Some(streamer) = snapshot
        .streamers
        .iter()
        .find(|streamer| streamer.username == login)
        .cloned()
    else {
        return WatchAction::Continue;
    };
    if !streamer.is_online || streamer.channel_id.trim().is_empty() {
        return WatchAction::Continue;
    }
    record_minute_watch_result(
        context,
        &streamer,
        tokio::time::timeout(
            MINUTE_WATCHER_REQUEST_TIMEOUT,
            send_minute_watched_for_streamer(
                &context.runtime,
                &context.twitch,
                &context.spade_urls,
                &streamer,
                &context.user_id,
            ),
        )
        .await,
    );
    if sleep_or_stop(stop, interval).await {
        WatchAction::Stop
    } else {
        WatchAction::Continue
    }
}

fn record_minute_watch_result(
    context: &MinuteWatcherContext,
    streamer: &Streamer,
    result: std::result::Result<Result<()>, tokio::time::error::Elapsed>,
) {
    match result {
        Ok(Ok(())) => context.health.success("minute"),
        Ok(Err(error)) => {
            context.health.failure("minute", "watch-request");
            tracing::warn!(task = "minute", error_class = "watch-request", streamer = %streamer.username, %error, "minute watched failed");
        }
        Err(_) => {
            context.health.failure("minute", "watch-timeout");
            tracing::warn!(
                task = "minute",
                error_class = "watch-timeout",
                streamer = %streamer.username,
                timeout_seconds = MINUTE_WATCHER_REQUEST_TIMEOUT.as_secs(),
                "minute watched timed out"
            );
        }
    }
}

pub(crate) async fn refresh_watch_selection_metadata(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    streamers: &[Streamer],
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
) -> Result<()> {
    let mut refreshes = tokio::task::JoinSet::new();
    let completed_campaign_ids = Arc::new(
        if streamers.iter().any(|streamer| {
            streamer.is_online
                && streamer.settings.farm_drops
                && streamer
                    .stream
                    .as_ref()
                    .is_none_or(|stream| stream.update_required_at(now))
        }) {
            match twitch.fetch_inventory_snapshot_typed().await {
                Ok(snapshot) => snapshot
                    .completed_campaign_ids
                    .into_iter()
                    .collect::<HashSet<_>>(),
                Err(error) => {
                    tracing::warn!(
                        error_class = "campaign-inventory",
                        %error,
                        "drop campaign completion refresh failed"
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
                log_watch_selection_refresh_result(result);
            }
        }

        let runtime = runtime.clone();
        let twitch = Arc::clone(twitch);
        let observability = observability.clone();
        let streamer = streamer.clone();
        let completed_campaign_ids = Arc::clone(&completed_campaign_ids);
        refreshes.spawn(async move {
            let previous_game = streamer_game_name(&streamer);
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
            apply_live_stream_update(&runtime, &streamer, &info, &observability, now).await?;
            refresh_drop_campaign_eligibility(
                &runtime,
                &twitch,
                &streamer,
                &info,
                &completed_campaign_ids,
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
        log_watch_selection_refresh_result(result);
    }

    Ok(())
}

fn log_watch_selection_refresh_result(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "watch selection refresh failed"),
        Err(error) => tracing::warn!(%error, "watch selection refresh task failed"),
    }
}

async fn refresh_drop_campaign_eligibility(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    info: &tm_twitch::StreamInfo,
    completed_campaign_ids: &HashSet<String>,
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
            .set_drop_campaign_eligibility(streamer.channel_id.clone(), false)
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
        .set_drop_campaign_eligibility(
            streamer.channel_id.clone(),
            has_unfinished_campaign(&campaign_ids, completed_campaign_ids),
        )
        .await?;
    Ok(())
}

pub(crate) fn has_unfinished_campaign(
    available_campaign_ids: &[String],
    completed_campaign_ids: &HashSet<String>,
) -> bool {
    available_campaign_ids
        .iter()
        .any(|campaign_id| !completed_campaign_ids.contains(campaign_id))
}

pub(crate) async fn send_minute_watched_for_streamer(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    spade_urls: &tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
    streamer: &Streamer,
    user_id: &str,
) -> Result<()> {
    let now = time_now();
    let info = stream_info_from_snapshot(streamer)?;
    let mut stream = streamer.stream.clone().unwrap_or_default();
    stream.payload = vec![build_minute_watched_event(streamer, &info, user_id)];

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
        runtime
            .mark_minute_watched(streamer.channel_id.clone(), now)
            .await?;
        return Ok(());
    }

    Err(anyhow!(
        "minute watched returned unexpected status {status} for {}",
        streamer.username
    ))
}

fn stream_info_from_snapshot(streamer: &Streamer) -> Result<tm_twitch::StreamInfo> {
    let stream = streamer
        .stream
        .as_ref()
        .filter(|stream| !stream.broadcast_id.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "missing refreshed stream metadata for {}",
                streamer.username
            )
        })?;
    Ok(tm_twitch::StreamInfo {
        id: stream.broadcast_id.clone(),
        title: stream.title.clone(),
        game_name: stream.game_name().clone(),
        game_id: stream.game_id.clone(),
        viewers_count: stream.viewers_count,
        tags: stream.tags.clone(),
        created_at: stream.stream_up_at,
    })
}

pub(crate) async fn handle_minute_watched_info_error(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
    error: tm_twitch::TwitchClientError,
) -> Result<Option<Streamer>> {
    if twitch
        .is_stream_live(&streamer.channel_id)
        .await
        .unwrap_or(false)
    {
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
    runtime
        .set_presence(streamer.channel_id.clone(), false, now)
        .await?;
    if streamer.is_online {
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
) -> Result<()> {
    runtime
        .set_presence(streamer.channel_id.clone(), true, now)
        .await?;
    runtime
        .apply_stream_update(
            tm_runtime::StreamUpdate {
                channel_id: streamer.channel_id.clone(),
                id: info.id.clone(),
                title: info.title.clone(),
                game_name: info.game_name.clone(),
                game_id: info.game_id.clone(),
                viewers_count: info.viewers_count,
                tags: info.tags.clone(),
            },
            now,
        )
        .await?;
    if !streamer.is_online {
        let message = observability.online_message(streamer);
        tracing::info!(operation = "set_online", "{message}");
        observability
            .send_event(DiscordEvent::StreamerOnline, &message)
            .await;
    }
    Ok(())
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
    let mut force_refresh = force_refresh;
    loop {
        let action = {
            let mut cache = spade_urls.lock().await;
            match cache.get(streamer_username) {
                Some(SpadeCacheEntry::Ready(entry))
                    if !force_refresh && entry.fetched_at.elapsed() < SPADE_URL_TTL =>
                {
                    SpadeResolveAction::Use(entry.url.clone())
                }
                Some(SpadeCacheEntry::Refreshing(notify)) => {
                    SpadeResolveAction::Wait(Arc::clone(notify))
                }
                _ => {
                    let notify = Arc::new(tokio::sync::Notify::new());
                    cache.insert(
                        streamer_username.to_string(),
                        SpadeCacheEntry::Refreshing(Arc::clone(&notify)),
                    );
                    SpadeResolveAction::Fetch(notify)
                }
            }
        };

        match action {
            SpadeResolveAction::Use(url) => return Ok(url),
            SpadeResolveAction::Wait(notify) => {
                force_refresh = false;
                notify.notified().await;
            }
            SpadeResolveAction::Fetch(notify) => {
                let resolved = fetch_spade(streamer_username.to_string()).await;
                let mut cache = spade_urls.lock().await;
                match &resolved {
                    Ok(url) => {
                        cache.insert(
                            streamer_username.to_string(),
                            SpadeCacheEntry::Ready(CachedSpadeUrl {
                                url: url.clone(),
                                fetched_at: StdInstant::now(),
                            }),
                        );
                    }
                    Err(_) => {
                        cache.remove(streamer_username);
                    }
                }
                notify.notify_waiters();
                return resolved;
            }
        }
    }
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
    info: &tm_twitch::StreamInfo,
    user_id: &str,
) -> serde_json::Value {
    let mut properties = serde_json::Map::from_iter([
        (String::from("channel_id"), json!(streamer.channel_id)),
        (String::from("broadcast_id"), json!(info.id)),
        (String::from("user_id"), json!(user_id)),
        (String::from("player"), json!("site")),
        (String::from("live"), json!(true)),
        (String::from("channel"), json!(streamer.username)),
    ]);
    if streamer.settings.farm_drops && !info.game_name.trim().is_empty() {
        properties.insert(String::from("game"), json!(info.game_name));
        if let Some(game_id) = info
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
