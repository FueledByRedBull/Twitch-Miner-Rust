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
                    let confirmed = recover_streamer(
                        &mut stop,
                        &runtime,
                        twitch.as_ref(),
                        &user_id,
                        &streamer,
                        &observability,
                    ).await;
                    if confirmed {
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
            let eligible = streamer.can_earn_channel_points()
                && streamer.presence_known
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
    let started_at = time_now();
    let Some(stream) = streamer.stream.as_ref() else {
        return false;
    };
    let baseline = match twitch
        .fetch_watch_streak_milestone(&streamer.channel_id)
        .await
    {
        Ok(Some(milestone)) => milestone,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(operation = "streak_recovery", error_class = ?error.failure_class(), "Unable to load typed streak risk for {streamer_name}");
            return false;
        }
    };
    if !milestone_targets_broadcast(&baseline, &stream.broadcast_id, started_at) {
        return false;
    }
    tracing::info!(
        operation = "streak_recovery",
        "Starting bounded offline streak recovery for {streamer_name}"
    );
    let spade_url = match twitch.fetch_spade_url(&streamer.username).await {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(operation = "streak_recovery", error_class = ?error.failure_class(), "Unable to resolve offline playback endpoint for {streamer_name}");
            return false;
        }
    };
    let attempt = RecoveryAttempt {
        runtime,
        twitch,
        spade_url: &spade_url,
        user_id,
        streamer,
        baseline: &baseline,
        target_broadcast_id: &stream.broadcast_id,
        observability,
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
        return recover_with_vod(stop, &attempt, video).await;
    }
    let clips = match twitch.fetch_recent_clips(&streamer.username).await {
        Ok(clips) => clips,
        Err(error) => {
            tracing::warn!(operation = "streak_recovery", error_class = ?error.failure_class(), "Unable to load clips for {streamer_name}");
            return false;
        }
    };
    recover_with_clips(stop, &attempt, &clips).await
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

struct RecoveryAttempt<'a> {
    runtime: &'a tm_runtime::RuntimeHandle,
    twitch: &'a TwitchClient,
    spade_url: &'a str,
    user_id: &'a str,
    streamer: &'a Streamer,
    baseline: &'a WatchStreakMilestone,
    target_broadcast_id: &'a str,
    observability: &'a AppObservability,
}

async fn recover_with_vod(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    attempt: &RecoveryAttempt<'_>,
    video: &ArchivedVideo,
) -> bool {
    let mut accepted = 0_u64;
    for _ in 0..(MAX_RECOVERY_SECONDS / VOD_EVENT_INTERVAL_SECONDS) {
        if preempted(attempt.runtime, attempt.streamer).await {
            log_preempted(attempt.observability, attempt.streamer);
            return false;
        }
        let playback = Stream {
            payload: vec![vod_minute_event(
                attempt.streamer,
                attempt.user_id,
                &video.id,
            )],
            ..Stream::default()
        };
        if matches!(
            attempt
                .twitch
                .send_minute_watched(attempt.spade_url, &playback)
                .await,
            Ok(StatusCode::NO_CONTENT)
        ) {
            accepted += 1;
            log_progress(
                attempt.observability,
                attempt.streamer,
                "VOD",
                accepted,
                MAX_RECOVERY_SECONDS / VOD_EVENT_INTERVAL_SECONDS,
            );
            if reconcile_typed_recovery(
                attempt.runtime,
                attempt.twitch,
                attempt.streamer,
                attempt.baseline,
                attempt.target_broadcast_id,
                attempt.observability,
            )
            .await
            {
                return true;
            }
        }
        if !wait_or_stop(stop, VOD_EVENT_INTERVAL_SECONDS).await {
            return false;
        }
    }
    log_unconfirmed(attempt.observability, attempt.streamer, "VOD", accepted);
    false
}

async fn recover_with_clips(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    attempt: &RecoveryAttempt<'_>,
    clips: &[RecentClip],
) -> bool {
    let mut elapsed = 0_u64;
    let mut accepted = 0_u64;
    for clip in clips
        .iter()
        .filter(|clip| clip_matches_broadcast(clip, attempt.target_broadcast_id))
    {
        if elapsed >= MAX_RECOVERY_SECONDS {
            break;
        }
        let play_session_id = tm_twitch::generate_client_session_id();
        let mut playback = Stream {
            payload: vec![clip_play_event(
                attempt.streamer,
                attempt.user_id,
                clip,
                &play_session_id,
            )],
            ..Stream::default()
        };
        if matches!(
            attempt
                .twitch
                .send_minute_watched(attempt.spade_url, &playback)
                .await,
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
            if preempted(attempt.runtime, attempt.streamer).await {
                log_preempted(attempt.observability, attempt.streamer);
                return false;
            }
            if !wait_or_stop(stop, CLIP_EVENT_INTERVAL_SECONDS).await {
                return false;
            }
            elapsed += CLIP_EVENT_INTERVAL_SECONDS;
            playback.payload = vec![clip_progress_event(
                attempt.streamer,
                attempt.user_id,
                clip,
                &play_session_id,
                second,
            )];
            if matches!(
                attempt
                    .twitch
                    .send_minute_watched(attempt.spade_url, &playback)
                    .await,
                Ok(StatusCode::NO_CONTENT)
            ) {
                accepted += 1;
                log_progress(
                    attempt.observability,
                    attempt.streamer,
                    "clip",
                    elapsed,
                    MAX_RECOVERY_SECONDS,
                );
                if reconcile_typed_recovery(
                    attempt.runtime,
                    attempt.twitch,
                    attempt.streamer,
                    attempt.baseline,
                    attempt.target_broadcast_id,
                    attempt.observability,
                )
                .await
                {
                    return true;
                }
            }
            second += CLIP_EVENT_INTERVAL_SECONDS;
        }
    }
    log_unconfirmed(attempt.observability, attempt.streamer, "clip", accepted);
    false
}

async fn reconcile_typed_recovery(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    baseline: &WatchStreakMilestone,
    target_broadcast_id: &str,
    observability: &AppObservability,
) -> bool {
    let Ok(Some(milestone)) = twitch
        .fetch_watch_streak_milestone(&streamer.channel_id)
        .await
    else {
        return false;
    };
    if !targeted_recovery_confirmed(baseline, &milestone, target_broadcast_id) {
        return false;
    }
    if runtime
        .mark_watch_streak_recovered(
            streamer.channel_id.clone(),
            milestone.value,
            time_now(),
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
        "Typed missed-broadcast state confirmed offline streak recovery for {name}"
    );
    true
}

fn milestone_targets_broadcast(
    milestone: &WatchStreakMilestone,
    broadcast_id: &str,
    now: OffsetDateTime,
) -> bool {
    milestone.value.is_some()
        && milestone
            .expires_at
            .is_some_and(|expires_at| expires_at > now)
        && milestone
            .missed_broadcast_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == broadcast_id))
}

fn targeted_recovery_confirmed(
    baseline: &WatchStreakMilestone,
    milestone: &WatchStreakMilestone,
    broadcast_id: &str,
) -> bool {
    baseline.value.is_some()
        && milestone.value == baseline.value
        && baseline
            .missed_broadcast_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == broadcast_id))
        && match milestone.missed_broadcast_ids.as_ref() {
            Some(ids) => ids.iter().all(|id| id != broadcast_id),
            None => milestone.expires_at.is_none(),
        }
}

fn clip_matches_broadcast(clip: &RecentClip, broadcast_id: &str) -> bool {
    clip.broadcast_id.as_deref() == Some(broadcast_id)
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
    tracing::info!(operation = "streak_recovery", "Offline streak {source} playback finished for {name}: {accepted} events accepted; recovery remains unconfirmed without typed missed-broadcast clearance");
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
    fn selection_ignores_channels_with_points_disabled() {
        let now = ts(100_000);
        let mut disabled = candidate("disabled", "broadcast-1", 99_000, 2);
        disabled.channel_points_enabled = Some(false);
        assert!(select_recovery_candidate(&[disabled], &HashMap::new(), now).is_none());
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
            broadcast_id: Some(String::from("broadcast")),
        };
        let progress = clip_progress_event(&streamer, "viewer", &clip, "session", 5);
        assert_eq!(progress["event"], "n_second_play");
        assert_eq!(
            progress["properties"]["content_mode"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn recovery_requires_the_targeted_missed_broadcast_to_clear() {
        let now = ts(10_000);
        let baseline = WatchStreakMilestone {
            value: Some(4),
            achievement_timestamp: ts(9_900),
            expires_at: Some(ts(20_000)),
            missed_broadcast_ids: Some(vec![String::from("broadcast-1")]),
        };
        let confirmed = WatchStreakMilestone {
            value: Some(4),
            achievement_timestamp: ts(9_900),
            expires_at: None,
            missed_broadcast_ids: None,
        };
        assert!(milestone_targets_broadcast(&baseline, "broadcast-1", now));
        assert!(targeted_recovery_confirmed(
            &baseline,
            &confirmed,
            "broadcast-1"
        ));
        for remaining in [Vec::new(), vec![String::from("broadcast-2")]] {
            let active_confirmed = WatchStreakMilestone {
                expires_at: Some(ts(20_000)),
                missed_broadcast_ids: Some(remaining),
                ..confirmed.clone()
            };
            assert!(targeted_recovery_confirmed(
                &baseline,
                &active_confirmed,
                "broadcast-1"
            ));
        }

        for unconfirmed in [
            WatchStreakMilestone {
                value: Some(5),
                ..confirmed.clone()
            },
            WatchStreakMilestone {
                value: None,
                ..confirmed.clone()
            },
            WatchStreakMilestone {
                expires_at: Some(ts(20_000)),
                missed_broadcast_ids: Some(vec![String::from("broadcast-1")]),
                ..confirmed.clone()
            },
            WatchStreakMilestone {
                missed_broadcast_ids: Some(vec![String::from("broadcast-1")]),
                ..confirmed.clone()
            },
        ] {
            assert!(!targeted_recovery_confirmed(
                &baseline,
                &unconfirmed,
                "broadcast-1"
            ));
        }
        assert!(!targeted_recovery_confirmed(
            &baseline,
            &confirmed,
            "broadcast-2"
        ));
    }

    #[test]
    fn recovery_clips_must_match_the_target_broadcast() {
        let mut clip = RecentClip {
            id: String::from("id"),
            slug: String::from("slug"),
            url: String::from("https://clips.twitch.tv/slug"),
            duration_seconds: 10.0,
            broadcast_id: Some(String::from("broadcast-1")),
        };
        assert!(clip_matches_broadcast(&clip, "broadcast-1"));
        clip.broadcast_id = None;
        assert!(!clip_matches_broadcast(&clip, "broadcast-1"));
    }
}
