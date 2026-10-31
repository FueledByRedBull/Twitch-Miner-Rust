use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use tm_domain::{Stream, Streamer};
use tm_twitch::{ArchivedVideo, RecentClip, TwitchClient};

use crate::observability::AppObservability;
use crate::streak_recovery::{
    clip_play_event, clip_progress_event, vod_minute_event, wait_or_stop,
};
use crate::utilities::time_now;

const SCAN_INTERVAL_SECONDS: u64 = 6 * 60 * 60;
/// Spaces the per-channel reads so a long followed list is not a request burst.
const CHANNEL_SPACING_SECONDS: u64 = 5;
const MAX_VOD_MINUTES: u64 = 8;
const MAX_CLIP_SECONDS: u64 = 30;
const CLIP_EVENT_INTERVAL_SECONDS: u64 = 5;
const MIN_VOD_SECONDS: u32 = 5 * 60;

/// Every few hours, visits each channel whose Weekly Visit Reward still needs
/// today's visit, one at a time, by playing a VOD or a clip until Twitch confirms.
pub(crate) fn spawn_weekly_visit_loop(
    mut stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    user_id: String,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let streamers = match runtime.state_snapshot().await {
                Ok(snapshot) => snapshot.streamers,
                Err(error) => {
                    tracing::warn!(operation = "weekly_visit", %error, "state snapshot failed");
                    Vec::new()
                }
            };
            for streamer in &streamers {
                if streamer.channel_id.trim().is_empty() || !streamer.can_earn_channel_points() {
                    continue;
                }
                if !wait_or_stop(&mut stop, CHANNEL_SPACING_SECONDS).await {
                    return;
                }
                if needs_visit(&twitch, streamer).await
                    && !visit(&mut stop, &twitch, &user_id, streamer, &observability).await
                    && *stop.borrow()
                {
                    return;
                }
            }
            if !wait_or_stop(&mut stop, SCAN_INTERVAL_SECONDS).await {
                return;
            }
        }
    })
}

async fn needs_visit(twitch: &TwitchClient, streamer: &Streamer) -> bool {
    match twitch
        .fetch_weekly_visit_rewards_typed(&streamer.channel_id)
        .await
    {
        Ok(rewards) => rewards.is_some_and(|rewards| rewards.needs_visit(time_now())),
        Err(error) => {
            // Twitch reports an error once a channel's last reward tier is earned.
            tracing::debug!(operation = "weekly_visit", error_class = ?error.failure_class(), "weekly reward read failed");
            false
        }
    }
}

async fn visit(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    twitch: &TwitchClient,
    user_id: &str,
    streamer: &Streamer,
    observability: &AppObservability,
) -> bool {
    let spade_url = match twitch.fetch_spade_url(&streamer.username).await {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(operation = "weekly_visit", error_class = ?error.failure_class(), "spade lookup failed");
            return false;
        }
    };
    let videos = twitch
        .fetch_recent_archived_videos(&streamer.username)
        .await
        .unwrap_or_default();
    let confirmed = if let Some(video) = visit_video(&videos) {
        visit_with_vod(stop, twitch, &spade_url, user_id, streamer, video).await
    } else {
        let clips = twitch
            .fetch_recent_clips(&streamer.username)
            .await
            .unwrap_or_default();
        match clips.first() {
            Some(clip) => visit_with_clip(stop, twitch, &spade_url, user_id, streamer, clip).await,
            None => false,
        }
    };
    if confirmed {
        tracing::info!(
            operation = "weekly_visit",
            "Weekly visit recorded for {}",
            observability.streamer_name(streamer)
        );
    }
    confirmed
}

fn visit_video(videos: &[ArchivedVideo]) -> Option<&ArchivedVideo> {
    videos
        .iter()
        .find(|video| video.length_seconds >= MIN_VOD_SECONDS)
}

async fn send_event(twitch: &TwitchClient, spade_url: &str, event: serde_json::Value) -> bool {
    let playback = Stream {
        payload: vec![event],
        ..Stream::default()
    };
    matches!(
        twitch.send_minute_watched(spade_url, &playback).await,
        Ok(StatusCode::NO_CONTENT)
    )
}

async fn visit_with_vod(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    twitch: &TwitchClient,
    spade_url: &str,
    user_id: &str,
    streamer: &Streamer,
    video: &ArchivedVideo,
) -> bool {
    for _ in 0..MAX_VOD_MINUTES {
        if !send_event(
            twitch,
            spade_url,
            vod_minute_event(streamer, user_id, &video.id),
        )
        .await
            || !wait_or_stop(stop, 60).await
        {
            return false;
        }
        if !needs_visit(twitch, streamer).await {
            return true;
        }
    }
    false
}

async fn visit_with_clip(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    twitch: &TwitchClient,
    spade_url: &str,
    user_id: &str,
    streamer: &Streamer,
    clip: &RecentClip,
) -> bool {
    let session = tm_twitch::generate_client_session_id();
    if !send_event(
        twitch,
        spade_url,
        clip_play_event(streamer, user_id, clip, &session),
    )
    .await
    {
        return false;
    }
    let seconds = Duration::from_secs_f64(clip.duration_seconds.clamp(0.0, 30.0))
        .as_secs()
        .min(MAX_CLIP_SECONDS);
    let mut second = CLIP_EVENT_INTERVAL_SECONDS;
    while second <= seconds {
        if !wait_or_stop(stop, CLIP_EVENT_INTERVAL_SECONDS).await
            || !send_event(
                twitch,
                spade_url,
                clip_progress_event(streamer, user_id, clip, &session, second),
            )
            .await
        {
            return false;
        }
        second += CLIP_EVENT_INTERVAL_SECONDS;
    }
    !needs_visit(twitch, streamer).await
}
