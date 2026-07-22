use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant as StdInstant;

use anyhow::{anyhow, Context, Result};
use reqwest::StatusCode;
use serde_json::json;
use tm_domain::{Game, Streamer};
use tm_observability::Event as DiscordEvent;
use tm_twitch::TwitchClient;

use crate::observability::{streamer_game_name, AppObservability};
use crate::status::HealthTracker;
use crate::utilities::{sleep_or_stop, time_now};
use crate::watching::{
    minute_watcher_resume_gap, CachedSpadeUrl, SpadeCacheEntry, SpadeResolveAction, WatchRotation,
};
use crate::{MINUTE_WATCHER_REQUEST_TIMEOUT, SPADE_URL_TTL, WATCH_SELECTION_REFRESH_CONCURRENCY};

const RENAME_RECOVERY_SUSPENSION_SECONDS: u64 = 5 * 60;

#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_minute_watcher_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    user_id: String,
    observability: AppObservability,
    health: HealthTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let spade_urls = tokio::sync::Mutex::new(HashMap::<String, SpadeCacheEntry>::new());
        let mut watch_rotation = WatchRotation::default();
        let mut stop = stop;
        let mut last_loop_at = time_now();
        'outer: loop {
            if *stop.borrow() {
                break;
            }

            let now = time_now();
            let loop_gap = minute_watcher_resume_gap(last_loop_at, now);
            last_loop_at = now;
            let snapshot = match runtime.state_snapshot().await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%error, "minute watcher snapshot failed");
                    break;
                }
            };
            if let Err(error) = refresh_watch_selection_metadata(
                &runtime,
                &twitch,
                &snapshot.streamers,
                &observability,
                now,
            )
            .await
            {
                health.failure("minute", "metadata-refresh");
                tracing::warn!(task = "minute", error_class = "metadata-refresh", %error, "watch selection metadata refresh failed");
            }
            let snapshot = match runtime.state_snapshot().await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%error, "minute watcher post-refresh snapshot failed");
                    break;
                }
            };
            let eligible_watch_logins = snapshot.watch_target_logins(now);
            let campaign_watch_logins = snapshot.campaign_watch_logins(now);
            let watch_logins = watch_rotation.select_with_campaigns(
                &eligible_watch_logins,
                &campaign_watch_logins,
                now,
            );
            if watch_logins.is_empty() {
                health.success("minute");
                if sleep_or_stop(&mut stop, std::time::Duration::from_secs(20)).await {
                    break;
                }
                continue;
            }
            if let Some(loop_gap) = loop_gap {
                spade_urls.lock().await.clear();
                let message =
                    observability.minute_watcher_resume_message(loop_gap, watch_logins.len());
                tracing::warn!("{message}");
            }

            let interval = tm_domain::watch_interval(watch_logins.len());
            for login in watch_logins {
                if *stop.borrow() {
                    break 'outer;
                }
                let snapshot = match runtime.state_snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        tracing::warn!(%error, "minute watcher refresh snapshot failed");
                        break 'outer;
                    }
                };
                let Some(streamer) = snapshot
                    .streamers
                    .iter()
                    .find(|streamer| streamer.username == login)
                    .cloned()
                else {
                    continue;
                };

                if !streamer.is_online || streamer.channel_id.trim().is_empty() {
                    continue;
                }

                match tokio::time::timeout(
                    MINUTE_WATCHER_REQUEST_TIMEOUT,
                    send_minute_watched_for_streamer(
                        &runtime,
                        &twitch,
                        &spade_urls,
                        &streamer,
                        &user_id,
                        &observability,
                    ),
                )
                .await
                {
                    Ok(Ok(())) => health.success("minute"),
                    Ok(Err(error)) => {
                        health.failure("minute", "watch-request");
                        tracing::warn!(task = "minute", error_class = "watch-request", streamer = %streamer.username, %error, "minute watched failed");
                    }
                    Err(_) => {
                        health.failure("minute", "watch-timeout");
                        tracing::warn!(
                            task = "minute",
                            error_class = "watch-timeout",
                            streamer = %streamer.username,
                            timeout_seconds = MINUTE_WATCHER_REQUEST_TIMEOUT.as_secs(),
                            "minute watched timed out"
                        );
                    }
                }

                if sleep_or_stop(&mut stop, interval).await {
                    break 'outer;
                }
            }
        }
    })
}

pub(crate) async fn refresh_watch_selection_metadata(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    streamers: &[Streamer],
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
) -> Result<()> {
    let mut refreshes = tokio::task::JoinSet::new();

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
        refreshes.spawn(async move {
            let previous_game = streamer_game_name(&streamer);
            let info = twitch
                .fetch_stream_info(&streamer.username)
                .await
                .with_context(|| format!("refresh stream info for {}", streamer.username))?;
            apply_live_stream_update(&runtime, &streamer, &info, &observability, now).await?;
            refresh_drop_campaign_eligibility(&runtime, &twitch, &streamer, &info).await?;
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
        .set_drop_campaign_eligibility(streamer.channel_id.clone(), !campaign_ids.is_empty())
        .await?;
    Ok(())
}

pub(crate) async fn send_minute_watched_for_streamer(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    spade_urls: &tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
    streamer: &Streamer,
    user_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let now = time_now();
    let previous_game = streamer_game_name(streamer);
    let (streamer, info) = match twitch.fetch_stream_info(&streamer.username).await {
        Ok(info) => (streamer.clone(), info),
        Err(error) => {
            let Some(recovered_streamer) = handle_minute_watched_info_error(
                runtime,
                twitch,
                streamer,
                observability,
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

    apply_live_stream_update(runtime, &streamer, &info, observability, now).await?;
    log_stream_presence_changes(
        observability,
        &streamer,
        previous_game.as_deref(),
        &info.game_name,
    );

    let mut stream = streamer.stream.clone().unwrap_or_default();
    stream.stream_up_at = Some(now);
    stream.update(
        &info.id,
        &info.title,
        Game::from_name(&info.game_name),
        &info.tags,
        info.viewers_count,
        tm_twitch::DROP_ID,
        now,
    );
    stream.payload = vec![build_minute_watched_event(&streamer, &info, user_id)];

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
