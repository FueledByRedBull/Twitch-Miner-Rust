use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use tm_domain::{OffsetDateTime, Stream, Streamer};
use tm_twitch::{ArchivedVideo, RecentClip, TwitchClient, WatchStreakMilestone};

use crate::observability::AppObservability;
use crate::status::HealthTracker;
use crate::utilities::time_now;

const RECOVERY_WINDOW_SECONDS: i64 = 23 * 60 * 60 + 30 * 60;
const RETRY_COOLDOWN_SECONDS: u64 = 15 * 60;
const MIN_VOD_SECONDS: u32 = 5 * 60;
const MAX_RECOVERY_SECONDS: u64 = 8 * 60;
const VOD_EVENT_INTERVAL_SECONDS: u64 = 60;
const CLIP_EVENT_INTERVAL_SECONDS: u64 = 5;

pub(crate) fn spawn_streak_recovery_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    user_id: String,
    observability: AppObservability,
    health: HealthTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut retry_after = HashMap::<String, OffsetDateTime>::new();
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    let now = time_now();
                    retry_after.retain(|_, retry| *retry + Duration::from_secs(RECOVERY_WINDOW_SECONDS.cast_unsigned()) > now);
                    let Ok(snapshot) = runtime.state_snapshot().await else {
                        health.failure("streak-recovery", "snapshot");
                        continue;
                    };
                    let Some(streamer) = select_recovery_candidate(&snapshot.streamers, &retry_after, now) else {
                        health.success("streak-recovery");
                        continue;
                    };
                    let key = streamer.stream.as_ref().map(|stream| stream.broadcast_id.clone()).unwrap_or_default();
                    retry_after.insert(key, now + Duration::from_secs(RETRY_COOLDOWN_SECONDS));
                    let playback_attempted = recover_streamer(
                        &mut stop,
                        &runtime,
                        twitch.as_ref(),
                        &user_id,
                        &streamer,
                        &observability,
                    ).await;
                    if playback_attempted {
                        retry_after.insert(
                            streamer.stream.as_ref().map(|stream| stream.broadcast_id.clone()).unwrap_or_default(),
                            now + Duration::from_secs(RECOVERY_WINDOW_SECONDS.cast_unsigned()),
                        );
                    }
                    health.success("streak-recovery");
                }
            }
        }
    })
}

fn select_recovery_candidate(
    streamers: &[Streamer],
    retry_after: &HashMap<String, OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<Streamer> {
    let mut candidates = streamers
        .iter()
        .enumerate()
        .filter_map(|(index, streamer)| {
            let stream = streamer.stream.as_ref()?;
            let offline_at = streamer.offline_at?;
            let age = (now - offline_at).whole_seconds();
            let eligible = streamer.presence_known
                && !streamer.is_online
                && streamer.settings.watch_streak
                && streamer.settings.watch_streak_vod_recovery
                && stream.watch_streak_missing
                && stream.watch_streak_count.is_some()
                && stream
                    .watch_streak_expires_at
                    .is_none_or(|expires_at| expires_at > now)
                && !stream.broadcast_id.trim().is_empty()
                && (0..=RECOVERY_WINDOW_SECONDS).contains(&age)
                && retry_after
                    .get(&stream.broadcast_id)
                    .is_none_or(|retry| *retry <= now);
            eligible.then_some((
                offline_at,
                std::cmp::Reverse(stream.watch_streak_count),
                index,
                streamer,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| (candidate.0, candidate.1, candidate.2));
    candidates.first().map(|candidate| candidate.3.clone())
}

async fn recover_streamer(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    user_id: &str,
    streamer: &Streamer,
    observability: &AppObservability,
) -> bool {
    let streamer_name = observability.streamer_name(streamer);
    tracing::info!(
        operation = "streak_recovery",
        "Starting bounded offline streak recovery for {streamer_name}"
    );
    let started_at = time_now();
    let baseline = twitch
        .fetch_watch_streak_milestone(&streamer.channel_id)
        .await
        .ok()
        .flatten();
    let spade_url = match twitch.fetch_spade_url(&streamer.username).await {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(operation = "streak_recovery", error_class = ?error.failure_class(), "Unable to resolve offline playback endpoint for {streamer_name}");
            return false;
        }
    };
    let Some(stream) = streamer.stream.as_ref() else {
        return false;
    };
    let videos = match twitch
        .fetch_recent_archived_videos(&streamer.username)
        .await
    {
        Ok(videos) => videos,
        Err(error) => {
            tracing::warn!(operation = "streak_recovery", error_class = ?error.failure_class(), "Unable to load archived videos for {streamer_name}");
            Vec::new()
        }
    };
    if let Some(video) = exact_recovery_video(&videos, &stream.broadcast_id) {
        return recover_with_vod(
            stop,
            runtime,
            twitch,
            &spade_url,
            user_id,
            streamer,
            video,
            baseline.as_ref(),
            started_at,
            observability,
        )
        .await;
    }
    let clips = match twitch.fetch_recent_clips(&streamer.username).await {
        Ok(clips) => clips,
        Err(error) => {
            tracing::warn!(operation = "streak_recovery", error_class = ?error.failure_class(), "Unable to load clips for {streamer_name}");
            return false;
        }
    };
    recover_with_clips(
        stop,
        runtime,
        twitch,
        &spade_url,
        user_id,
        streamer,
        &clips,
        baseline.as_ref(),
        started_at,
        observability,
    )
    .await
}

fn exact_recovery_video<'a>(
    videos: &'a [ArchivedVideo],
    broadcast_id: &str,
) -> Option<&'a ArchivedVideo> {
    videos.iter().find(|video| {
        video.length_seconds >= MIN_VOD_SECONDS
            && video.broadcast_id.as_deref() == Some(broadcast_id)
    })
}

#[allow(clippy::too_many_arguments)]
async fn recover_with_vod(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    spade_url: &str,
    user_id: &str,
    streamer: &Streamer,
    video: &ArchivedVideo,
    baseline: Option<&WatchStreakMilestone>,
    started_at: OffsetDateTime,
    observability: &AppObservability,
) -> bool {
    let mut accepted = 0_u64;
    for _ in 0..(MAX_RECOVERY_SECONDS / VOD_EVENT_INTERVAL_SECONDS) {
        if preempted(runtime, streamer).await {
            log_preempted(observability, streamer);
            return true;
        }
        let playback = Stream {
            payload: vec![vod_minute_event(streamer, user_id, &video.id)],
            ..Stream::default()
        };
        if matches!(
            twitch.send_minute_watched(spade_url, &playback).await,
            Ok(StatusCode::NO_CONTENT)
        ) {
            accepted += 1;
            log_progress(
                observability,
                streamer,
                "VOD",
                accepted,
                MAX_RECOVERY_SECONDS / VOD_EVENT_INTERVAL_SECONDS,
            );
            if reconcile_typed_recovery(
                runtime,
                twitch,
                streamer,
                baseline,
                started_at,
                observability,
            )
            .await
            {
                return true;
            }
        }
        if !wait_or_stop(stop, VOD_EVENT_INTERVAL_SECONDS).await {
            return true;
        }
    }
    log_unconfirmed(observability, streamer, "VOD", accepted);
    accepted > 0
}

#[allow(clippy::too_many_arguments)]
async fn recover_with_clips(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    spade_url: &str,
    user_id: &str,
    streamer: &Streamer,
    clips: &[RecentClip],
    baseline: Option<&WatchStreakMilestone>,
    started_at: OffsetDateTime,
    observability: &AppObservability,
) -> bool {
    let mut elapsed = 0_u64;
    let mut accepted = 0_u64;
    for clip in clips {
        if elapsed >= MAX_RECOVERY_SECONDS {
            break;
        }
        let play_session_id = tm_twitch::generate_client_session_id();
        let mut playback = Stream {
            payload: vec![clip_play_event(streamer, user_id, clip, &play_session_id)],
            ..Stream::default()
        };
        if matches!(
            twitch.send_minute_watched(spade_url, &playback).await,
            Ok(StatusCode::NO_CONTENT)
        ) {
            accepted += 1;
        }
        let clip_seconds =
            Duration::from_secs_f64(clip.duration_seconds.clamp(0.0, 30.0)).as_secs();
        let mut second = CLIP_EVENT_INTERVAL_SECONDS;
        while second <= clip_seconds {
            if elapsed >= MAX_RECOVERY_SECONDS {
                break;
            }
            if preempted(runtime, streamer).await {
                log_preempted(observability, streamer);
                return true;
            }
            if !wait_or_stop(stop, CLIP_EVENT_INTERVAL_SECONDS).await {
                return true;
            }
            elapsed += CLIP_EVENT_INTERVAL_SECONDS;
            playback.payload = vec![clip_progress_event(
                streamer,
                user_id,
                clip,
                &play_session_id,
                second,
            )];
            if matches!(
                twitch.send_minute_watched(spade_url, &playback).await,
                Ok(StatusCode::NO_CONTENT)
            ) {
                accepted += 1;
                log_progress(
                    observability,
                    streamer,
                    "clip",
                    elapsed,
                    MAX_RECOVERY_SECONDS,
                );
                if reconcile_typed_recovery(
                    runtime,
                    twitch,
                    streamer,
                    baseline,
                    started_at,
                    observability,
                )
                .await
                {
                    return true;
                }
            }
            second += CLIP_EVENT_INTERVAL_SECONDS;
        }
    }
    log_unconfirmed(observability, streamer, "clip", accepted);
    accepted > 0
}

async fn reconcile_typed_recovery(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    baseline: Option<&WatchStreakMilestone>,
    started_at: OffsetDateTime,
    observability: &AppObservability,
) -> bool {
    let Ok(Some(milestone)) = twitch
        .fetch_watch_streak_milestone(&streamer.channel_id)
        .await
    else {
        return false;
    };
    let changed = baseline
        .is_none_or(|before| milestone.achievement_timestamp > before.achievement_timestamp);
    if !changed || milestone.achievement_timestamp < started_at - Duration::from_secs(5 * 60) {
        return false;
    }
    if runtime
        .mark_watch_streak_recovered(
            streamer.channel_id.clone(),
            milestone.value,
            milestone.achievement_timestamp,
            milestone.expires_at,
        )
        .await
        .is_err()
    {
        return false;
    }
    let name = observability.streamer_name(streamer);
    tracing::info!(
        operation = "streak_recovery",
        "Typed milestone confirmed offline streak recovery for {name}"
    );
    true
}

async fn preempted(runtime: &tm_runtime::RuntimeHandle, streamer: &Streamer) -> bool {
    runtime
        .state_snapshot()
        .await
        .ok()
        .and_then(|state| {
            state
                .streamers
                .into_iter()
                .find(|current| current.channel_id == streamer.channel_id)
        })
        .is_none_or(|current| {
            current.is_online
                || !current
                    .stream
                    .is_some_and(|stream| stream.watch_streak_missing)
        })
}

async fn wait_or_stop(stop: &mut tokio::sync::watch::Receiver<bool>, seconds: u64) -> bool {
    tokio::select! {
        changed = stop.changed() => changed.is_ok() && !*stop.borrow(),
        () = tokio::time::sleep(Duration::from_secs(seconds)) => true,
    }
}

fn vod_minute_event(streamer: &Streamer, user_id: &str, vod_id: &str) -> serde_json::Value {
    serde_json::json!({ "event": "minute-watched", "properties": {
        "channel_id": streamer.channel_id, "broadcast_id": null, "player": "site",
        "user_id": user_id, "live": false, "channel": streamer.username,
        "vod_id": vod_id, "content_mode": "video"
    }})
}

fn clip_play_event(
    streamer: &Streamer,
    user_id: &str,
    clip: &RecentClip,
    session: &str,
) -> serde_json::Value {
    serde_json::json!({ "event": "video-play", "properties": {
        "location": "vod", "url": clip.url, "channel_id": streamer.channel_id,
        "vod_type": "clip", "vod_id": clip.id, "content_mode": "clip", "live": false,
        "minutes_logged": 0, "play_session_id": session, "player": "site", "user_id": user_id,
        "vod_timestamp": 0, "clip_slug": clip.slug
    }})
}

fn clip_progress_event(
    streamer: &Streamer,
    user_id: &str,
    clip: &RecentClip,
    session: &str,
    second: u64,
) -> serde_json::Value {
    serde_json::json!({ "event": "n_second_play", "properties": {
        "location": "vod", "platform": "web", "url": clip.url,
        "channel_id": streamer.channel_id, "vod_type": "clip", "vod_id": clip.id,
        "live": false, "minutes_logged": 0, "play_session_id": session, "player": "site",
        "seconds_after_play": second, "vod_timestamp": Duration::from_secs(second).as_secs_f64() - 0.1,
        "clip_slug": clip.slug, "user_id": user_id
    }})
}

fn log_progress(
    observability: &AppObservability,
    streamer: &Streamer,
    source: &str,
    current: u64,
    total: u64,
) {
    let name = observability.streamer_name(streamer);
    tracing::info!(
        operation = "streak_recovery",
        "Offline streak {source} progress for {name}: {current}/{total} accepted"
    );
}

fn log_preempted(observability: &AppObservability, streamer: &Streamer) {
    let name = observability.streamer_name(streamer);
    tracing::info!(
        operation = "streak_recovery",
        "Offline streak recovery preempted by live or resolved state for {name}"
    );
}

fn log_unconfirmed(
    observability: &AppObservability,
    streamer: &Streamer,
    source: &str,
    accepted: u64,
) {
    let name = observability.streamer_name(streamer);
    tracing::info!(operation = "streak_recovery", "Offline streak {source} playback finished for {name}: {accepted} events accepted; recovery remains unconfirmed without a typed milestone");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(value: i64) -> OffsetDateTime {
        match OffsetDateTime::from_unix_timestamp(value) {
            Ok(value) => value,
            Err(error) => panic!("invalid fixture timestamp: {error}"),
        }
    }

    fn candidate(login: &str, broadcast: &str, offline_at: i64, count: u32) -> Streamer {
        Streamer {
            username: login.into(),
            channel_id: format!("id-{login}"),
            presence_known: true,
            offline_at: Some(ts(offline_at)),
            settings: tm_domain::StreamerSettings {
                watch_streak: true,
                watch_streak_vod_recovery: true,
                ..Default::default()
            },
            stream: Some(Stream {
                broadcast_id: broadcast.into(),
                watch_streak_count: Some(count),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn selection_is_bounded_deterministic_and_respects_cooldown() {
        let now = ts(100_000);
        let older = candidate("older", "broadcast-1", 99_000, 2);
        let newer = candidate("newer", "broadcast-2", 99_500, 9);
        assert_eq!(
            select_recovery_candidate(&[newer.clone(), older.clone()], &HashMap::new(), now)
                .map(|streamer| streamer.username),
            Some(String::from("older"))
        );
        let cooldown = HashMap::from([(String::from("broadcast-1"), ts(101_000))]);
        assert_eq!(
            select_recovery_candidate(&[newer, older], &cooldown, now)
                .map(|streamer| streamer.username),
            Some(String::from("newer"))
        );
    }

    #[test]
    fn exact_video_match_requires_five_minutes() {
        let videos = vec![
            ArchivedVideo {
                id: "short".into(),
                broadcast_id: Some("wanted".into()),
                length_seconds: 299,
            },
            ArchivedVideo {
                id: "other".into(),
                broadcast_id: Some("other".into()),
                length_seconds: 600,
            },
            ArchivedVideo {
                id: "right".into(),
                broadcast_id: Some("wanted".into()),
                length_seconds: 600,
            },
        ];
        assert_eq!(
            exact_recovery_video(&videos, "wanted").map(|video| video.id.as_str()),
            Some("right")
        );
    }

    #[test]
    fn playback_payloads_are_offline_and_typed_by_content() {
        let streamer = candidate("name", "broadcast", 1, 2);
        let vod = vod_minute_event(&streamer, "viewer", "vod");
        assert_eq!(
            vod.pointer("/properties/live"),
            Some(&serde_json::json!(false))
        );
        let clip = RecentClip {
            id: "id".into(),
            slug: "slug".into(),
            url: "https://clips.twitch.tv/slug".into(),
            duration_seconds: 10.0,
        };
        let progress = clip_progress_event(&streamer, "viewer", &clip, "session", 5);
        assert_eq!(progress["event"], "n_second_play");
        assert_eq!(
            progress["properties"]["content_mode"],
            serde_json::Value::Null
        );
    }
}
