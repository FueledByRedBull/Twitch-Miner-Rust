use anyhow::{Context, Result};
use tm_config::ConfigFile;
use tm_domain::{Game, Streamer};
use tm_observability::{Event as DiscordEvent, LoggerSettings};
use tm_twitch::TwitchClient;

use crate::context::{apply_context_to_streamer, contribute_streamer_community_goals};
use crate::observability::AppObservability;
use crate::streak_cache::StreakCache;

pub(crate) fn build_logger_settings(config: &ConfigFile) -> LoggerSettings {
    LoggerSettings {
        save: config.save_logs,
        emoji: config.emojis,
        smart: config.smart_logging,
        show_seconds: config.show_seconds,
        console_username: config.show_username_in_console,
        show_claimed_bonus: config.show_claimed_bonus_msg,
        debug: config.debug,
        debug_deep: config.debug_deep,
        anonymize_logs: config.privacy.anonymize_logs,
    }
}

pub(crate) fn build_canary_logger_settings(config: &ConfigFile) -> LoggerSettings {
    let mut settings = build_logger_settings(config);
    settings.save = false;
    settings
}

pub(crate) async fn bootstrap_runtime_state(
    config: &ConfigFile,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
    streak_cache: &mut StreakCache,
) -> Result<tm_runtime::RuntimeState> {
    let targets = load_targets(config, twitch).await?;
    let mut state = tm_runtime::RuntimeState::from_targets(config, &targets, started_at);
    tracing::info!(
        operation = "run",
        "{}",
        observability.loading_streamers_message(state.streamers.len())
    );

    for streamer in &mut state.streamers {
        bootstrap_streamer(
            streamer,
            twitch,
            user_id,
            started_at,
            observability,
            streak_cache,
        )
        .await?;
    }
    state.capture_initial_points();
    Ok(state)
}

pub(crate) async fn load_targets(
    config: &ConfigFile,
    twitch: &TwitchClient,
) -> Result<Vec<String>> {
    if !config.streamers.is_empty() {
        return Ok(config.streamers.clone());
    }
    twitch
        .fetch_followers(100, config.followers_order.as_str())
        .await
        .context("load followers")
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn bootstrap_streamer(
    streamer: &mut Streamer,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
    streak_cache: &mut StreakCache,
) -> Result<()> {
    streamer.channel_id = twitch
        .fetch_channel_id(&streamer.username)
        .await
        .with_context(|| format!("load channel id for {}", streamer.username))?;

    let mut context = twitch
        .fetch_channel_points_context(&streamer.username)
        .await
        .with_context(|| format!("load channel points context for {}", streamer.username))?;
    apply_context_to_streamer(streamer, &context);

    if let Some(claim_id) = context.claim_id.as_deref() {
        twitch
            .claim_bonus(&streamer.channel_id, claim_id, user_id)
            .await
            .with_context(|| format!("claim startup bonus for {}", streamer.username))?;
        if observability.show_claimed_bonus {
            let message = observability.bonus_claim_message(streamer, true);
            tracing::info!(operation = "claim_bonus", "{message}");
            observability.spawn_event(DiscordEvent::BonusClaim, message);
        }
        context = twitch
            .fetch_channel_points_context(&streamer.username)
            .await
            .with_context(|| format!("refresh claimed bonus context for {}", streamer.username))?;
        apply_context_to_streamer(streamer, &context);
    }

    if contribute_streamer_community_goals(twitch, streamer).await? {
        context = twitch
            .fetch_channel_points_context(&streamer.username)
            .await
            .with_context(|| format!("refresh channel points context for {}", streamer.username))?;
        apply_context_to_streamer(streamer, &context);
    }

    let is_live = twitch
        .is_stream_live(&streamer.channel_id)
        .await
        .with_context(|| format!("check live state for {}", streamer.username))?;
    streamer.presence_known = true;
    streamer.is_online = is_live;
    if is_live {
        streamer.online_at = Some(started_at);
        streamer.offline_at = None;
        let info = twitch
            .fetch_stream_info(&streamer.username)
            .await
            .with_context(|| format!("load stream info for {}", streamer.username))?;
        let stream = streamer
            .stream
            .get_or_insert_with(tm_domain::Stream::default);
        stream.stream_up_at = Some(started_at);
        stream.update(
            &info.id,
            &info.title,
            Game::from_name(&info.game_name),
            &info.tags,
            info.viewers_count,
            tm_twitch::DROP_ID,
            started_at,
        );
        if streamer.settings.watch_streak {
            match twitch
                .fetch_watch_streak_milestone(&streamer.channel_id)
                .await
            {
                Ok(Some(milestone)) => {
                    streak_cache.record_milestone(
                        &streamer.channel_id,
                        milestone.value,
                        milestone.achievement_timestamp,
                        milestone.expires_at,
                        started_at,
                    );
                    stream.watch_streak_count = milestone.value;
                    stream.watch_streak_resolved_at = Some(milestone.achievement_timestamp);
                    stream.watch_streak_expires_at = milestone.expires_at;
                    if info.created_at.is_some_and(|created_at| {
                        milestone.achievement_timestamp >= created_at
                            && milestone
                                .expires_at
                                .is_none_or(|expires_at| expires_at > started_at)
                    }) {
                        stream.watch_streak_missing = false;
                    }
                }
                Ok(None) => {
                    streak_cache.apply_to_stream(
                        &streamer.channel_id,
                        stream,
                        info.created_at,
                        started_at,
                    );
                }
                Err(error) => {
                    tracing::debug!(
                        failure_class = ?error.failure_class(),
                        "watch streak startup reconciliation unavailable"
                    );
                    streak_cache.apply_to_stream(
                        &streamer.channel_id,
                        stream,
                        info.created_at,
                        started_at,
                    );
                }
            }
        }
        tracing::info!(
            operation = "set_online",
            "{}",
            observability.online_message(streamer)
        );
    } else {
        streamer.online_at = None;
        streamer.offline_at = Some(started_at);
        tracing::info!(
            operation = "set_offline",
            "{}",
            observability.offline_message(streamer)
        );
    }

    Ok(())
}
