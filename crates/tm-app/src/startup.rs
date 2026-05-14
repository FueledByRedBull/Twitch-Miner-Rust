#![allow(unused_imports)]
#![allow(clippy::wildcard_imports)]
use crate::*;

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

pub(crate) async fn run_auto_update_if_enabled(config: &ConfigFile) -> Result<bool> {
    if !config.auto_update {
        return Ok(false);
    }
    let args = env::args().skip(1).collect::<Vec<_>>();
    match tm_updater::run_auto_update(env!("CARGO_PKG_VERSION"), &args).await {
        Ok(tm_updater::AutoUpdateOutcome::UpToDate) => Ok(false),
        Ok(tm_updater::AutoUpdateOutcome::UpdateAvailableForDevRun { latest_version }) => {
            tracing::warn!(
                latest_version = %latest_version,
                "auto-update skipped for development run"
            );
            Ok(false)
        }
        Ok(tm_updater::AutoUpdateOutcome::UpdatedAndRestarting { latest_version }) => {
            tracing::info!(
                latest_version = %latest_version,
                "auto-update installed a newer version; restarting"
            );
            Ok(true)
        }
        Err(tm_updater::AutoUpdateError::Update(
            tm_updater::UpdateError::UnsupportedReleaseContract,
        )) => {
            tracing::warn!(
                repository = %tm_updater::PROJECT_REPOSITORY_URL,
                "auto-update skipped because no Rust binary release contract is configured"
            );
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn bootstrap_runtime_state(
    config: &ConfigFile,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
) -> Result<tm_runtime::RuntimeState> {
    let targets = load_targets(config, twitch).await?;
    let mut state = tm_runtime::RuntimeState::from_targets(config, &targets, started_at);
    tracing::info!(
        "{}",
        observability.loading_streamers_message(state.streamers.len())
    );

    for streamer in &mut state.streamers {
        bootstrap_streamer(streamer, twitch, user_id, started_at, observability).await?;
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
        .fetch_followers(100, "DESC")
        .await
        .context("load followers")
}

pub(crate) async fn bootstrap_streamer(
    streamer: &mut Streamer,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
) -> Result<()> {
    streamer.channel_id = twitch
        .fetch_channel_id(&streamer.username)
        .await
        .with_context(|| format!("load channel id for {}", streamer.username))?;

    let context = twitch
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
            tracing::info!("{message}");
            observability
                .send_event(DiscordEvent::BonusClaim, &message)
                .await;
        }
    }

    if contribute_streamer_community_goals(twitch, streamer).await? {
        let refreshed = twitch
            .fetch_channel_points_context(&streamer.username)
            .await
            .with_context(|| format!("refresh channel points context for {}", streamer.username))?;
        apply_context_to_streamer(streamer, &refreshed);
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
        tracing::info!("{}", observability.online_message(streamer));
    } else {
        streamer.online_at = None;
        streamer.offline_at = Some(started_at);
        tracing::info!("{}", observability.offline_message(streamer));
    }

    Ok(())
}
