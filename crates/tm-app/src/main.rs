#![warn(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use reqwest::StatusCode;
use serde_json::json;
use tm_config::{resolve_app_paths_from_env, ConfigFile};
use tm_domain::{Game, PredictionDecision, Streamer};
use tm_observability::{
    build_discord_request, event_from_bet_result, event_from_gain_reason, init_tracing, DiscordClient,
    Event as DiscordEvent, LoggerSettings, TracingInitOptions,
};
use tm_irc::ChatClient;
use tm_pubsub::{build_topic_batches, PubSubClient, PubSubConnectionEvent};
use tm_twitch::{InventoryDrop, TwitchClient, generate_device_id};

mod bootstrap;
mod effects;
mod observability;
mod prediction;
mod tasks;
mod watching;

use observability::{
    build_observability, log_session_summary, log_startup, AppObservability, TracingChatLogger,
    streamer_game_name,
};
use bootstrap::{
    build_http_client, has_override, load_config_with_fallback, load_or_login_session,
    log_timezone_validation, normalized_username, prepare_work_dir, validate_timezone_override,
    LoadedConfig, DEFAULT_USER_AGENT,
};
use effects::runtime_streamer_by_channel_id;
use prediction::prediction_wait_duration;
use tasks::{BackgroundTaskParams, BackgroundTasks};
use watching::{CachedSpadeUrl, SpadeCacheEntry, SpadeResolveAction, minute_watcher_resume_gap};

const DEFAULT_CONSOLE_TITLE: &str = "Klaro's Twitch Miner";
const CONTEXT_REFRESH_CONCURRENCY: usize = 8;
const WATCH_SELECTION_REFRESH_CONCURRENCY: usize = 4;
const PENDING_CLAIMS_INTERVAL: Duration = Duration::from_secs(5 * 60 * 60);
const SPADE_URL_TTL: Duration = Duration::from_secs(15 * 60);
const MINUTE_WATCHER_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const SHUTDOWN_TASK_GRACE_PERIOD: Duration = Duration::from_secs(5);
const SESSION_SUMMARY_INDENT: usize = 25;

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long = "data-dir")]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let has_override = has_override(&cli);
    let requested_paths = resolve_app_paths_from_env(cli.config, cli.data_dir)?;
    set_console_title(DEFAULT_CONSOLE_TITLE);
    clear_console();

    let LoadedConfig {
        config,
        active_paths,
    } = load_config_with_fallback(&requested_paths, has_override)?;
    prepare_work_dir(&active_paths)?;
    let timezone_validation = validate_timezone_override(config.timezone.as_deref());

    init_tracing(&TracingInitOptions {
        settings: build_logger_settings(&config),
        base_dir: active_paths.work_dir.clone(),
        username: config.username.clone(),
        timezone: config.timezone.clone(),
    })?;
    log_timezone_validation(timezone_validation.as_ref());

    if run_auto_update_if_enabled(&config).await? {
        return Ok(());
    }

    let observability = build_observability(&config)?;
    tracing::info!(
        "{} | v{}",
        tm_updater::PROJECT_DISPLAY_NAME,
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!("{}", tm_updater::PROJECT_REPOSITORY_URL);

    let session_id = new_session_id();
    tracing::info!("{}", observability.start_session_message(&session_id));
    let started_at = time_now();
    let http_client = build_http_client(config.disable_ssl_cert_verification)?;
    let session =
        load_or_login_session(&config, &active_paths.work_dir, http_client.clone()).await?;
    let auth_token = session
        .auth_token()
        .ok_or_else(|| anyhow!("missing auth token after login"))?
        .to_string();
    let user_id = session.user_id().map(str::to_string);
    let twitch_cookie_header = session.cookie_header_for_host("twitch.tv");
    let twitch = Arc::new(TwitchClient::with_client_and_cookie_header(
        http_client,
        &auth_token,
        DEFAULT_USER_AGENT,
        twitch_cookie_header,
    ));
    let bootstrap_started = StdInstant::now();
    let state = bootstrap_runtime_state(
        &config,
        &twitch,
        user_id.as_deref(),
        started_at,
        &observability,
    )
    .await?;
    claim_startup_drops_if_enabled(&config, &state.streamers, &twitch, &observability).await?;
    tracing::info!(
        "{}",
        observability.loaded_streamers_message(state.streamers.len(), bootstrap_started.elapsed())
    );
    let runtime = tm_runtime::spawn_runtime_state(state);
    let initial_streamers = runtime.state_snapshot().await?.streamers;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let tasks = spawn_background_tasks(BackgroundTaskParams {
        config: &config,
        stop_rx,
        runtime: &runtime,
        twitch: &twitch,
        auth_token: &auth_token,
        user_id: user_id.as_ref(),
        initial_streamers: &initial_streamers,
        observability: &observability,
    })?;
    let summary = runtime.runtime_summary().await?;

    log_startup(
        &requested_paths,
        &active_paths,
        &summary,
        &session_id,
        &observability,
    )
    .await;

    wait_for_shutdown_signal().await?;
    tracing::info!(session_id = %session_id, "shutdown requested");
    shutdown_background_tasks(stop_tx, tasks).await;
    tracing::info!(session_id = %session_id, "background tasks stopped");
    let summary = runtime
        .shutdown(config.privacy.anonymize_logs, time_now())
        .await?;
    send_discord_event(
        observability.discord.as_ref(),
        &observability.discord_client,
        DiscordEvent::Shutdown,
        &format!("Ending session: '{session_id}'"),
    )
    .await;
    log_session_summary(&summary);
    Ok(())
}

fn build_logger_settings(config: &ConfigFile) -> LoggerSettings {
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

async fn run_auto_update_if_enabled(config: &ConfigFile) -> Result<bool> {
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

fn spawn_background_tasks(params: BackgroundTaskParams<'_>) -> Result<BackgroundTasks> {
    let username = normalized_username(&params.config.username)?;
    let pubsub = params.user_id.map(|user_id| {
        spawn_pubsub_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            params.auth_token.to_string(),
            user_id.clone(),
            username.clone(),
            params.initial_streamers.to_vec(),
            user_id.clone(),
            params.observability.clone(),
        )
    });
    let context = params.user_id.map(|user_id| {
        spawn_context_refresh_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
        )
    });
    let pending_claims = params.user_id.map(|user_id| {
        spawn_pending_claim_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
        )
    });
    let minute = params.user_id.map(|user_id| {
        spawn_minute_watcher_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
        )
    });
    let drop = params
        .initial_streamers
        .iter()
        .any(|streamer| streamer.settings.claim_drops)
        .then(|| {
            spawn_drop_claim_loop(
                params.stop_rx.clone(),
                Arc::clone(params.twitch),
                params.observability.clone(),
            )
        });
    let chat = Some(spawn_chat_manager_loop(
        params.stop_rx,
        params.runtime.clone(),
        params.auth_token.to_string(),
        username,
        params.config.disable_at_in_nickname,
        params.observability.clone(),
    ));
    Ok(BackgroundTasks {
        pubsub,
        context,
        pending_claims,
        minute,
        drop,
        chat,
    })
}

async fn shutdown_background_tasks(
    stop_tx: tokio::sync::watch::Sender<bool>,
    tasks: BackgroundTasks,
) {
    let _ = stop_tx.send(true);
    if let Some(task) = tasks.pubsub {
        await_shutdown_task("pubsub", task).await;
    }
    if let Some(task) = tasks.context {
        await_shutdown_task("context", task).await;
    }
    if let Some(task) = tasks.pending_claims {
        await_shutdown_task("pending-claims", task).await;
    }
    if let Some(task) = tasks.minute {
        await_shutdown_task("minute", task).await;
    }
    if let Some(task) = tasks.drop {
        await_shutdown_task("drop", task).await;
    }
    if let Some(task) = tasks.chat {
        await_shutdown_task("chat", task).await;
    }
}

async fn await_shutdown_task(name: &str, mut task: tokio::task::JoinHandle<()>) {
    match tokio::time::timeout(SHUTDOWN_TASK_GRACE_PERIOD, &mut task).await {
        Ok(Ok(())) => {
            tracing::info!(task = name, "shutdown task stopped");
        }
        Ok(Err(error)) => {
            tracing::warn!(task = name, %error, "shutdown task failed while stopping");
        }
        Err(_) => {
            tracing::warn!(
                task = name,
                timeout_seconds = SHUTDOWN_TASK_GRACE_PERIOD.as_secs(),
                "shutdown task exceeded grace period; aborting"
            );
            task.abort();
            let _ = task.await;
        }
    }
}

async fn bootstrap_runtime_state(
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

async fn load_targets(config: &ConfigFile, twitch: &TwitchClient) -> Result<Vec<String>> {
    if !config.streamers.is_empty() {
        return Ok(config.streamers.clone());
    }
    twitch
        .fetch_followers(100, "DESC")
        .await
        .context("load followers")
}

async fn bootstrap_streamer(
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

async fn claim_startup_drops_if_enabled(
    config: &ConfigFile,
    streamers: &[Streamer],
    twitch: &TwitchClient,
    observability: &AppObservability,
) -> Result<()> {
    if !config.claim_drops_startup
        || !streamers
            .iter()
            .any(|streamer| streamer.settings.claim_drops)
    {
        return Ok(());
    }

    claim_available_drops(twitch, "startup", observability).await?;

    Ok(())
}

fn drop_is_claimable(drop: &InventoryDrop) -> bool {
    !drop.is_claimed
        && !drop.drop_instance_id.trim().is_empty()
        && drop.current_minutes_watched >= drop.required_minutes_watched
}


fn spawn_drop_claim_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    twitch: Arc<TwitchClient>,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop = stop;
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) =
                        claim_available_drops(twitch.as_ref(), "periodic", &observability).await
                    {
                        tracing::warn!(%error, "periodic drop claim failed");
                    }
                }
            }
        }
    })
}

async fn claim_available_drops(
    twitch: &TwitchClient,
    mode: &str,
    observability: &AppObservability,
) -> Result<()> {
    let drops = twitch
        .fetch_claimable_drops()
        .await
        .with_context(|| format!("load {mode} drops inventory"))?;
    for drop in drops.into_iter().filter(drop_is_claimable) {
        twitch
            .claim_drop(&drop.drop_instance_id)
            .await
            .with_context(|| format!("claim drop {}", drop.drop_instance_id))?;
        let message = observability.drop_claim_message(mode, &drop);
        tracing::info!("{message}");
        observability
            .send_event(DiscordEvent::DropClaim, &message)
            .await;
    }
    Ok(())
}

fn apply_context_to_streamer(streamer: &mut Streamer, context: &tm_twitch::ChannelPointsContext) {
    streamer.apply_channel_points_context(
        context.balance,
        &context.active_multipliers,
        &context.community_goals,
    );
}

async fn contribute_streamer_community_goals(
    twitch: &TwitchClient,
    streamer: &Streamer,
) -> Result<bool> {
    if !streamer.settings.community_goals {
        return Ok(false);
    }
    let contributions = load_goal_contributions(twitch, &streamer.username).await?;
    let mut available_points = streamer.channel_points;
    let mut contributed = false;
    for goal in streamer.community_goals.values() {
        if !goal.is_active() {
            continue;
        }
        let user_points = contributions.get(&goal.id).copied().unwrap_or_default();
        let amount = tm_twitch::community_goal_contribution_amount(goal, user_points, available_points);
        if amount <= 0 {
            continue;
        }
        twitch
            .contribute_community_goal(amount, &streamer.channel_id, &goal.id)
            .await?;
        available_points -= amount;
        contributed = true;
        tracing::info!(
            streamer = %streamer.username,
            goal_id = %goal.id,
            title = %goal.title,
            amount,
            "contributed to community goal"
        );
    }
    Ok(contributed)
}

#[allow(clippy::too_many_arguments)]
fn spawn_pubsub_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    auth_token: String,
    user_id: String,
    username: String,
    tracked_streamers: Vec<Streamer>,
    persistent_user_id: String,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
        let topic_batches = match build_topic_batches(&user_id, &tracked_streamers) {
            Ok(batches) => batches,
            Err(error) => {
                tracing::warn!(%error, "pubsub topic build failed");
                return;
            }
        };
        let mut event_stop = stop.clone();
        let event_runtime = runtime.clone();
        let event_twitch = Arc::clone(&twitch);
        let event_observability = observability.clone();
        let event_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = event_stop.changed() => {
                        if changed.is_err() || *event_stop.borrow() {
                            break;
                        }
                    }
                    message = receiver.recv() => {
                        let Some(message) = message else {
                            break;
                        };
                        match message {
                            PubSubConnectionEvent::Event(event) => {
                                let log_event = (*event).clone();
                                match event_runtime.apply_pubsub_event(*event, time_now()).await {
                                    Ok(effects) => {
                                        if let Err(error) = log_pubsub_event(
                                            &event_runtime,
                                            &event_observability,
                                            &log_event,
                                        )
                                        .await
                                        {
                                            tracing::warn!(%error, "pubsub log handling failed");
                                        }
                                        if let Err(error) = execute_runtime_effects(
                                            &event_runtime,
                                            &event_twitch,
                                            &persistent_user_id,
                                            effects,
                                            &event_observability,
                                        )
                                        .await
                                        {
                                            tracing::warn!(%error, "runtime effect execution failed");
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, "pubsub event application failed");
                                    }
                                }
                            }
                            PubSubConnectionEvent::ResponseError { error, nonce } => {
                                let message = nonce.map_or_else(
                                    || format!("PubSub response error: {error}"),
                                    |nonce| format!("PubSub response error: {error} (nonce {nonce})"),
                                );
                                tracing::warn!("{message}");
                            }
                        }
                    }
                }
            }
        });

        let mut connections = Vec::with_capacity(topic_batches.len());
        for (index, topics) in topic_batches.into_iter().enumerate() {
            connections.push(spawn_pubsub_connection_loop(
                stop.clone(),
                sender.clone(),
                auth_token.clone(),
                username.clone(),
                tracked_streamers.clone(),
                topics,
                index + 1,
            ));
        }

        for connection in connections {
            let _ = connection.await;
        }

        drop(sender);
        let _ = event_task.await;
    })
}

fn spawn_pubsub_connection_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    sender: tokio::sync::mpsc::Sender<PubSubConnectionEvent>,
    auth_token: String,
    username: String,
    tracked_streamers: Vec<Streamer>,
    topics: Vec<String>,
    connection_index: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        loop {
            if *stop.borrow() {
                break;
            }

            let client = PubSubClient::default();
            let connect = tokio::spawn({
                let auth_token = auth_token.clone();
                let username = username.clone();
                let tracked_streamers = tracked_streamers.clone();
                let topics = topics.clone();
                let sender = sender.clone();
                async move {
                    client
                        .connect_topics_and_listen(
                            &topics,
                            &auth_token,
                            Some(&username),
                            &tracked_streamers,
                            sender,
                        )
                        .await
                }
            });
            tokio::pin!(connect);

            let should_stop = tokio::select! {
                changed = stop.changed() => {
                    connect.as_mut().abort();
                    let _ = connect.await;
                    changed.is_err() || *stop.borrow()
                }
                result = &mut connect => {
                    let reconnect_delay = pubsub_reconnect_delay(&result, connection_index, topics.len());
                    match result {
                        Ok(Ok(())) => {
                            tracing::warn!(
                                "PubSub[{connection_index}] connection closed; reconnecting ({} topic(s))",
                                topics.len()
                            );
                        }
                        Ok(Err(tm_pubsub::PubSubError::ReconnectRequested)) => {}
                        Ok(Err(error)) => {
                            tracing::error!(
                                "PubSub[{connection_index}] connection error: {error}"
                            );
                        }
                        Err(error) if error.is_cancelled() => {
                            return;
                        }
                        Err(error) => {
                            tracing::error!("PubSub[{connection_index}] task failed: {error}");
                        }
                    }
                    if let Some(delay) = reconnect_delay {
                        tokio::select! {
                            changed = stop.changed() => changed.is_err() || *stop.borrow(),
                            () = tokio::time::sleep(delay) => false,
                        }
                    } else {
                        false
                    }
                }
            };

            if should_stop {
                break;
            }
        }
    })
}

fn pubsub_reconnect_delay(
    result: &std::result::Result<std::result::Result<(), tm_pubsub::PubSubError>, tokio::task::JoinError>,
    connection_index: usize,
    topic_count: usize,
) -> Option<Duration> {
    match result {
        Ok(Err(tm_pubsub::PubSubError::ReconnectRequested)) => {
            tracing::warn!(
                "PubSub[{connection_index}] reconnect requested; waiting 60 seconds ({topic_count} topic(s))"
            );
            Some(Duration::from_secs(60))
        }
        Ok(Ok(())) => Some(Duration::from_secs(5)),
        Err(error) if error.is_cancelled() => None,
        Ok(Err(_)) | Err(_) => Some(Duration::from_secs(10)),
    }
}

async fn execute_runtime_effects(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    effects: Vec<tm_runtime::RuntimeEffect>,
    observability: &AppObservability,
) -> Result<()> {
    for effect in effects {
        execute_runtime_effect(runtime, twitch, persistent_user_id, effect, observability).await?;
    }

    Ok(())
}

async fn execute_runtime_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    effect: tm_runtime::RuntimeEffect,
    observability: &AppObservability,
) -> Result<()> {
    match effect {
        tm_runtime::RuntimeEffect::ClaimBonus {
            channel_id,
            claim_id,
        } => {
            handle_claim_bonus_effect(
                runtime,
                twitch.as_ref(),
                persistent_user_id,
                &channel_id,
                &claim_id,
                observability,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::ClaimMoment {
            channel_id,
            moment_id,
        } => {
            handle_claim_moment_effect(
                runtime,
                twitch.as_ref(),
                &channel_id,
                &moment_id,
                observability,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::JoinRaid {
            channel_id,
            raid_id,
            target_login,
        } => {
            handle_join_raid_effect(
                runtime,
                twitch.as_ref(),
                &channel_id,
                &raid_id,
                &target_login,
                observability,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::ContributeCommunityGoals { channel_id } => {
            handle_community_goal_effect(
                runtime,
                twitch.as_ref(),
                persistent_user_id,
                &channel_id,
                observability,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::EvaluatePrediction { event_id } => {
            spawn_prediction_evaluation(runtime, twitch, &event_id, observability);
        }
        tm_runtime::RuntimeEffect::PredictionSettled {
            event_id,
            streamer_username,
            title,
            decision_label,
            result_type,
            result_string,
        } => {
            handle_prediction_settled_effect(
                &event_id,
                &streamer_username,
                &title,
                &decision_label,
                &result_type,
                &result_string,
                observability,
            )
            .await;
        }
    }

    Ok(())
}

async fn handle_claim_bonus_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    persistent_user_id: &str,
    channel_id: &str,
    claim_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    twitch
        .claim_bonus(channel_id, claim_id, Some(persistent_user_id))
        .await?;
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    if observability.show_claimed_bonus {
        let message = observability.bonus_claim_message(&streamer, false);
        tracing::info!("{message}");
        observability
            .send_event(DiscordEvent::BonusClaim, &message)
            .await;
    }
    Ok(())
}

async fn handle_claim_moment_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    channel_id: &str,
    moment_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    twitch.claim_moment(moment_id).await?;
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    let message = format!(
        "Claimed moment for {}",
        observability.streamer_label(&streamer)
    );
    tracing::info!("{message}");
    observability
        .send_event(DiscordEvent::MomentClaim, &message)
        .await;
    Ok(())
}

async fn handle_join_raid_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    channel_id: &str,
    raid_id: &str,
    target_login: &str,
    observability: &AppObservability,
) -> Result<()> {
    twitch.join_raid(raid_id).await?;
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    let message =
        observability.join_raid_message(&observability.streamer_name(&streamer), target_login);
    tracing::info!("{message}");
    observability
        .send_event(DiscordEvent::JoinRaid, &message)
        .await;
    Ok(())
}

async fn handle_community_goal_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    persistent_user_id: &str,
    channel_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    if contribute_streamer_community_goals(twitch, &streamer).await? {
        refresh_streamer_context_without_goal_effects(
            runtime,
            twitch,
            &streamer,
            Some(persistent_user_id),
            observability,
        )
        .await?;
    }
    Ok(())
}

fn spawn_prediction_evaluation(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    event_id: &str,
    observability: &AppObservability,
) {
    let runtime = runtime.clone();
    let twitch = Arc::clone(twitch);
    let observability = observability.clone();
    let event_id = event_id.to_string();
    tokio::spawn(async move {
        if let Err(error) =
            evaluate_prediction_after_delay(&runtime, &twitch, &event_id, &observability).await
        {
            tracing::warn!(event_id = %event_id, %error, "prediction evaluation failed");
        }
    });
}

async fn handle_prediction_settled_effect(
    event_id: &str,
    streamer_username: &str,
    title: &str,
    decision_label: &str,
    result_type: &str,
    result_string: &str,
    observability: &AppObservability,
) {
    let message = format!("Prediction settled for {streamer_username}: {title} - {result_string}");
    tracing::info!(
        decision = %decision_label,
        event_id = %event_id,
        result_type = %result_type,
        "{message}"
    );
    if let Some(event) = event_from_bet_result(result_type) {
        observability.send_event(event, &message).await;
    }
}

async fn evaluate_prediction_after_delay(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let Some(wait) = prediction_wait_for_event(runtime, event_id).await? else {
        return Ok(());
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    evaluate_prediction(runtime, twitch, event_id, observability).await
}

async fn prediction_wait_for_event(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
) -> Result<Option<Duration>> {
    let snapshot = runtime.state_snapshot().await?;
    Ok(snapshot
        .predictions
        .get(event_id)
        .cloned()
        .map(|event| prediction_wait_duration(&event, time_now())))
}

async fn evaluate_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let snapshot = runtime.state_snapshot().await?;
    let Some(mut event) = snapshot.predictions.get(event_id).cloned() else {
        return Ok(());
    };
    if event.bet_placed || !event.result_type.is_empty() {
        return Ok(());
    }
    let Some(streamer) = snapshot
        .streamers
        .iter()
        .find(|streamer| streamer.channel_id == event.streamer.channel_id)
        .cloned()
    else {
        runtime.stop_tracking_prediction(event_id, "ERROR").await?;
        return Ok(());
    };

    if maybe_skip_prediction_for_status(runtime, event_id, &event, &streamer, observability).await?
    {
        return Ok(());
    }

    if maybe_skip_prediction_for_balance(runtime, event_id, &streamer, observability).await? {
        return Ok(());
    }

    event.streamer = streamer.clone();
    let decision = event.decide(streamer.channel_points);
    if decision.outcome_id.is_empty() {
        skip_prediction(
            runtime,
            event_id,
            format!(
                "skip prediction: no outcome selected for {}",
                observability.streamer_name(&streamer)
            ),
        )
        .await?;
        return Ok(());
    }

    let (skip, compared, reason) = event.should_skip_by_filter();
    if skip {
        let filter_reason = if reason.is_empty() {
            format!("filter_condition not satisfied (current {compared})")
        } else {
            reason
        };
        skip_prediction(
            runtime,
            event_id,
            format!(
                "skip prediction for {}: {}",
                observability.streamer_name(&streamer),
                filter_reason
            ),
        )
        .await?;
        return Ok(());
    }

    if decision.amount < 10 {
        skip_prediction(
            runtime,
            event_id,
            format!(
                "skip prediction: below Twitch minimum for {}",
                observability.streamer_name(&streamer)
            ),
        )
        .await?;
        return Ok(());
    }

    place_prediction(
        runtime,
        twitch,
        event_id,
        &event,
        &decision,
        &streamer,
        observability,
    )
    .await
}

async fn maybe_skip_prediction_for_status(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    streamer: &Streamer,
    observability: &AppObservability,
) -> Result<bool> {
    if event.status == "ACTIVE" {
        return Ok(false);
    }
    tracing::info!(
        event_id = %event_id,
        status = %event.status,
        "skip prediction: event status is not active for {}",
        observability.streamer_name(streamer)
    );
    runtime
        .stop_tracking_prediction(event_id, "SKIPPED")
        .await?;
    Ok(true)
}

async fn maybe_skip_prediction_for_balance(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
    streamer: &Streamer,
    observability: &AppObservability,
) -> Result<bool> {
    let Some(minimum_points) = streamer.settings.bet.minimum_points else {
        return Ok(false);
    };
    if streamer.channel_points > i64::from(minimum_points) {
        return Ok(false);
    }
    tracing::info!(
        event_id = %event_id,
        balance = streamer.channel_points,
        minimum_points,
        "skip prediction: balance below minimum_points for {}",
        observability.streamer_name(streamer)
    );
    runtime
        .stop_tracking_prediction(event_id, "SKIPPED")
        .await?;
    Ok(true)
}

async fn skip_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
    message: String,
) -> Result<()> {
    tracing::info!(event_id = %event_id, "{message}");
    runtime.stop_tracking_prediction(event_id, "SKIPPED").await?;
    Ok(())
}

async fn place_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    decision: &PredictionDecision,
    streamer: &Streamer,
    observability: &AppObservability,
) -> Result<()> {
    match twitch
        .make_prediction(&event.event_id, &decision.outcome_id, decision.amount)
        .await
    {
        Ok(()) => {
            let deduct_stake = streamer.settings.bet.deduct_stake_on_place.unwrap_or(true);
            runtime
                .record_prediction_placed(&event.event_id, decision.clone(), deduct_stake)
                .await?;
            let message = format!(
                "Placed prediction for {}: {} on {}",
                observability.streamer_name(streamer),
                decision.amount,
                event.decision_label()
            );
            tracing::info!(event_id = %event.event_id, "{message}");
            observability
                .send_event(DiscordEvent::BetGeneral, &message)
                .await;
            Ok(())
        }
        Err(error) => {
            runtime.stop_tracking_prediction(event_id, "ERROR").await?;
            observability
                .send_event(
                    DiscordEvent::BetFailed,
                    &format!(
                        "Prediction failed for {}: {error}",
                        observability.streamer_name(streamer)
                    ),
                )
                .await;
            Err(error.into())
        }
    }
}

async fn log_pubsub_event(
    runtime: &tm_runtime::RuntimeHandle,
    observability: &AppObservability,
    event: &tm_pubsub::PubSubEvent,
) -> Result<()> {
    match event {
        tm_pubsub::PubSubEvent::PointsEarned {
            channel_id,
            earned,
            reason,
            ..
        } => {
            let snapshot = runtime.state_snapshot().await?;
            let Some(streamer) = snapshot
                .streamers
                .iter()
                .find(|streamer| streamer.channel_id == *channel_id)
            else {
                return Ok(());
            };
            let message = observability.points_earned_message(streamer, *earned, reason);
            tracing::info!("{message}");
            if let Some(event) = event_from_gain_reason(reason) {
                observability.send_event(event, &message).await;
            }
        }
        tm_pubsub::PubSubEvent::Playback { channel_id, kind } => {
            let snapshot = runtime.state_snapshot().await?;
            let Some(streamer) = snapshot
                .streamers
                .iter()
                .find(|streamer| streamer.channel_id == *channel_id)
            else {
                return Ok(());
            };
            match kind {
                tm_pubsub::PlaybackType::StreamUp => {
                    let message = observability.online_message(streamer);
                    tracing::info!("{message}");
                    observability
                        .send_event(DiscordEvent::StreamerOnline, &message)
                        .await;
                }
                tm_pubsub::PlaybackType::StreamDown => {
                    let message = observability.offline_message(streamer);
                    tracing::info!("{message}");
                    observability
                        .send_event(DiscordEvent::StreamerOffline, &message)
                        .await;
                }
                tm_pubsub::PlaybackType::Viewcount => {}
            }
        }
        _ => {}
    }
    Ok(())
}

fn spawn_context_refresh_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    persistent_user_id: String,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(20 * 60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop = stop;
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) = refresh_snapshot_streamers(
                        &runtime,
                        &twitch,
                        &persistent_user_id,
                        &observability,
                    )
                    .await
                    {
                        tracing::warn!(%error, "context refresh snapshot failed");
                    }
                }
            }
        }
    })
}

fn spawn_pending_claim_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    persistent_user_id: String,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + PENDING_CLAIMS_INTERVAL,
            PENDING_CLAIMS_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop = stop;

        if let Err(error) = refresh_snapshot_streamers(
            &runtime,
            &twitch,
            &persistent_user_id,
            &observability,
        )
        .await
        {
            tracing::warn!(%error, "pending bonus sweep failed");
        }

        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) = refresh_snapshot_streamers(
                        &runtime,
                        &twitch,
                        &persistent_user_id,
                        &observability,
                    )
                    .await
                    {
                        tracing::warn!(%error, "pending bonus sweep failed");
                    }
                }
            }
        }
    })
}

async fn refresh_snapshot_streamers(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let snapshot = runtime.state_snapshot().await?;
    let mut refreshes = tokio::task::JoinSet::new();

    for streamer in snapshot.streamers {
        while refreshes.len() >= CONTEXT_REFRESH_CONCURRENCY {
            log_context_refresh_result(refreshes.join_next().await);
        }
        let runtime = runtime.clone();
        let twitch = Arc::clone(twitch);
        let persistent_user_id = persistent_user_id.to_string();
        let observability = observability.clone();
        refreshes.spawn(async move {
            let username = streamer.username.clone();
            let result = match refresh_streamer_context(
                &runtime,
                twitch.as_ref(),
                &streamer,
                Some(&persistent_user_id),
                &observability,
            )
            .await {
                Ok(effects) => {
                execute_runtime_effects(
                    &runtime,
                    &twitch,
                    &persistent_user_id,
                    effects,
                    &observability,
                )
                .await
                }
                Err(error) => Err(error),
            };
            (username, result)
        });
    }

    while !refreshes.is_empty() {
        log_context_refresh_result(refreshes.join_next().await);
    }

    Ok(())
}

fn log_context_refresh_result(
    result: Option<std::result::Result<(String, Result<()>), tokio::task::JoinError>>,
) {
    match result {
        Some(Ok((username, Err(error)))) => {
            tracing::warn!(streamer = %username, %error, "context refresh failed");
        }
        Some(Err(error)) => {
            tracing::warn!(%error, "context refresh task failed");
        }
        _ => {}
    }
}

async fn refresh_streamer_context(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    persistent_user_id: Option<&str>,
    observability: &AppObservability,
) -> Result<Vec<tm_runtime::RuntimeEffect>> {
    let context = twitch
        .fetch_channel_points_context(&streamer.username)
        .await
        .with_context(|| format!("fetch context for {}", streamer.username))?;
    let effects = runtime
        .apply_context_update(tm_runtime::ContextUpdate {
            channel_id: streamer.channel_id.clone(),
            balance: context.balance,
            active_multipliers: context.active_multipliers,
            community_goals: context.community_goals,
        })
        .await?;
    if let Some(claim_id) = context.claim_id.as_deref() {
        twitch
            .claim_bonus(&streamer.channel_id, claim_id, persistent_user_id)
            .await
            .with_context(|| format!("claim refreshed bonus for {}", streamer.username))?;
        if observability.show_claimed_bonus {
            let message = observability.bonus_claim_message(streamer, false);
            tracing::info!("{message}");
            observability
                .send_event(DiscordEvent::BonusClaim, &message)
                .await;
        }
    }
    Ok(effects)
}

async fn refresh_streamer_context_without_goal_effects(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    persistent_user_id: Option<&str>,
    observability: &AppObservability,
) -> Result<()> {
    let _ = refresh_streamer_context(runtime, twitch, streamer, persistent_user_id, observability)
        .await?;
    Ok(())
}

async fn load_goal_contributions(
    twitch: &TwitchClient,
    username: &str,
) -> Result<HashMap<String, i64>> {
    let response = twitch.fetch_user_points_contribution(username).await?;
    let contributions = tm_twitch::parse_user_points_contributions(&response)
        .into_iter()
        .collect();
    Ok(contributions)
}

#[allow(clippy::too_many_lines)]
fn spawn_minute_watcher_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    user_id: String,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let spade_urls = tokio::sync::Mutex::new(HashMap::<String, SpadeCacheEntry>::new());
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
                tracing::warn!(%error, "watch selection metadata refresh failed");
            }
            let snapshot = match runtime.state_snapshot().await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%error, "minute watcher post-refresh snapshot failed");
                    break;
                }
            };
            let watch_logins = snapshot.watch_target_logins(now);
            if watch_logins.is_empty() {
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
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(streamer = %streamer.username, %error, "minute watched failed");
                    }
                    Err(_) => {
                        tracing::warn!(
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

async fn refresh_watch_selection_metadata(
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
            let _ = refreshes.join_next().await;
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
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "watch selection refresh failed"),
            Err(error) => tracing::warn!(%error, "watch selection refresh task failed"),
        }
    }

    Ok(())
}

async fn send_minute_watched_for_streamer(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    spade_urls: &tokio::sync::Mutex<HashMap<String, SpadeCacheEntry>>,
    streamer: &Streamer,
    user_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let now = time_now();
    let previous_game = streamer_game_name(streamer);
    let info = match twitch.fetch_stream_info(&streamer.username).await {
        Ok(info) => info,
        Err(error) => {
            return handle_minute_watched_info_error(
                runtime,
                twitch,
                streamer,
                observability,
                now,
                error,
            )
            .await;
        }
    };

    apply_live_stream_update(runtime, streamer, &info, observability, now).await?;
    log_stream_presence_changes(
        observability,
        streamer,
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
    stream.payload = vec![build_minute_watched_event(streamer, &info, user_id)];

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

async fn handle_minute_watched_info_error(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    observability: &AppObservability,
    now: tm_runtime::RuntimeTime,
    error: tm_twitch::TwitchClientError,
) -> Result<()> {
    if twitch
        .is_stream_live(&streamer.channel_id)
        .await
        .unwrap_or(false)
    {
        return Err(error.into());
    }
    runtime
        .set_presence(streamer.channel_id.clone(), false, now)
        .await?;
    if streamer.is_online {
        let message = observability.offline_message(streamer);
        tracing::info!("{message}");
        observability
            .send_event(DiscordEvent::StreamerOffline, &message)
            .await;
    }
    Ok(())
}

async fn apply_live_stream_update(
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
        tracing::info!("{message}");
        observability
            .send_event(DiscordEvent::StreamerOnline, &message)
            .await;
    }
    Ok(())
}

fn log_stream_presence_changes(
    observability: &AppObservability,
    streamer: &Streamer,
    previous_game: Option<&str>,
    current_game: &str,
) {
    if let Some(message) =
        observability.game_change_message(streamer, previous_game.unwrap_or_default(), current_game)
    {
        tracing::info!("{message}");
    }
}

async fn resolve_spade_url<FetchSpade, FetchFuture, Error>(
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

async fn send_minute_watched_with_spade_cache<
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

fn build_minute_watched_event(
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
    if streamer.settings.claim_drops && !info.game_name.trim().is_empty() {
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

async fn sleep_or_stop(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    duration: std::time::Duration,
) -> bool {
    tokio::select! {
        changed = stop.changed() => {
            changed.is_err() || *stop.borrow()
        }
        () = tokio::time::sleep(duration) => false,
    }
}

fn spawn_chat_manager_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    auth_token: String,
    username: String,
    disable_at_in_nickname: bool,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut watchers: HashMap<
            String,
            (
                tokio::sync::watch::Sender<bool>,
                tokio::task::JoinHandle<()>,
            ),
        > = HashMap::new();
        let mut stop = stop;
        let mut state_changes = runtime.subscribe_state_changes();

        if let Err(error) = reconcile_chat_watchers(
            &runtime,
            &mut watchers,
            &auth_token,
            &username,
            disable_at_in_nickname,
            &observability,
        )
        .await
        {
            tracing::warn!(%error, "chat manager snapshot failed");
            return;
        }

        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                changed = state_changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    if let Err(error) = reconcile_chat_watchers(
                        &runtime,
                        &mut watchers,
                        &auth_token,
                        &username,
                        disable_at_in_nickname,
                        &observability,
                    )
                    .await
                    {
                        tracing::warn!(%error, "chat manager snapshot failed");
                        break;
                    }
                }
            }
        }

        for (_, (watcher_stop, task)) in watchers {
            let _ = watcher_stop.send(true);
            let _ = task.await;
        }
    })
}

async fn reconcile_chat_watchers(
    runtime: &tm_runtime::RuntimeHandle,
    watchers: &mut HashMap<
        String,
        (
            tokio::sync::watch::Sender<bool>,
            tokio::task::JoinHandle<()>,
        ),
    >,
    auth_token: &str,
    username: &str,
    disable_at_in_nickname: bool,
    observability: &AppObservability,
) -> Result<()> {
    let snapshot = runtime.state_snapshot().await?;
    let labels = snapshot
        .streamers
        .iter()
        .map(|streamer| {
            (
                streamer.username.to_lowercase(),
                observability.streamer_name(streamer),
            )
        })
        .collect::<HashMap<_, _>>();
    let desired = snapshot.desired_chat_logins();
    let desired: std::collections::HashSet<_> = desired.into_iter().collect();

    let existing = watchers.keys().cloned().collect::<Vec<_>>();
    for login in existing {
        if desired.contains(&login) {
            continue;
        }
        if let Some((watcher_stop, task)) = watchers.remove(&login) {
            let _ = watcher_stop.send(true);
            let _ = task.await;
            let message = observability.chat_presence_message(
                false,
                labels
                    .get(&login.to_lowercase())
                    .map_or(login.as_str(), String::as_str),
            );
            tracing::info!("{message}");
        }
    }

    for login in desired {
        if watchers.contains_key(&login) {
            continue;
        }
        let (watcher_stop, watcher_rx) = tokio::sync::watch::channel(false);
        let task = spawn_chat_watcher_loop(
            watcher_rx,
            username.to_string(),
            auth_token.to_string(),
            login.clone(),
            disable_at_in_nickname,
            observability.clone(),
        );
        watchers.insert(login.clone(), (watcher_stop, task));
        let message = observability.chat_presence_message(
            true,
            labels
                .get(&login.to_lowercase())
                .map_or(login.as_str(), String::as_str),
        );
        tracing::info!("{message}");
    }

    Ok(())
}

fn spawn_chat_watcher_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    username: String,
    auth_token: String,
    channel: String,
    disable_at_in_nickname: bool,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        loop {
            if *stop.borrow() {
                break;
            }

            let mut client = ChatClient::new(
                &username,
                &auth_token,
                &channel,
                TracingChatLogger {
                    observability: observability.clone(),
                },
                disable_at_in_nickname,
            );
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                result = client.connect_and_run() => {
                    if let Err(error) = result {
                        tracing::warn!(channel = %channel, %error, "irc watcher disconnected");
                    }
                }
            }

            if sleep_or_stop(&mut stop, std::time::Duration::from_secs(5)).await {
                break;
            }
        }
    })
}

fn clear_console() {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "cls"]);
        command
    } else {
        Command::new("clear")
    };
    let _ = command.status();
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}

fn new_session_id() -> String {
    format!("session-{}", generate_device_id())
}

fn time_now() -> tm_runtime::RuntimeTime {
    tm_runtime::RuntimeTime::now_utc()
}

async fn send_discord_event(
    webhook: Option<&tm_observability::DiscordWebhook>,
    client: &DiscordClient,
    event: DiscordEvent,
    message: &str,
) {
    let Some(webhook) = webhook else {
        return;
    };
    let Some(request) = build_discord_request(webhook, message, Some(event)) else {
        return;
    };
    if let Err(error) = client.send(&request).await {
        tracing::warn!(event = ?event, %error, "discord notification failed");
    }
}

fn set_console_title(title: &str) {
    if !cfg!(windows) || title.trim().is_empty() {
        return;
    }
    let _ = Command::new("cmd")
        .args(["/C", &format!("title {title}")])
        .status();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use crate::bootstrap::{
        env_has_value, load_config_with_fallback_using, load_or_login_session_with_auth_client,
        should_fallback_to_user_config, TimezoneValidation, READ_ONLY_FILE_SYSTEM_ERROR,
    };
    use crate::observability::format_resume_gap;
    use tm_auth::{AuthEndpoints, TwitchAuthClient};
    use tm_config::{AppPaths, ConfigError};
    use tm_domain::{
        BetSettings, DelayMode, OffsetDateTime, PredictionDecision, PredictionEvent,
        PredictionOutcome,
    };
    use tm_twitch::TwitchEndpoints;

    fn ts(seconds: i64) -> tm_runtime::RuntimeTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn unique_temp_dir() -> PathBuf {
        env::temp_dir().join(format!(
            "tm-app-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn fixture_json(name: &str) -> String {
        fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures")
                .join(name),
        )
        .unwrap()
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut header_end = None;
        let mut content_length = 0_usize;

        loop {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if header_end.is_none() {
                header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                if let Some(position) = header_end {
                    let headers = String::from_utf8_lossy(&buffer[..position + 4]);
                    content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("Content-Length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or_default();
                    if buffer.len() >= position + 4 + content_length {
                        break;
                    }
                }
            } else if let Some(position) = header_end {
                if buffer.len() >= position + 4 + content_length {
                    break;
                }
            }
        }

        String::from_utf8(buffer).unwrap()
    }

    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn empty_http_response(status: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
    }

    fn spawn_auth_server() -> (AuthEndpoints, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                requests.push(request);
                let response = match index {
                    0 => http_response(
                        "200 OK",
                        r#"{"device_code":"device-code","user_code":"ABCD","interval":0,"expires_in":60}"#,
                    ),
                    1 => http_response(
                        "400 Bad Request",
                        r#"{"status":400,"message":"authorization_pending"}"#,
                    ),
                    2 => http_response("200 OK", r#"{"access_token":"token-123"}"#),
                    3 => http_response("200 OK", r#"{"data":{"user":{"id":"user-123"}}}"#),
                    _ => unreachable!(),
                };
                stream.write_all(&response).unwrap();
            }
            requests
        });

        (
            AuthEndpoints {
                device_code_url: format!("http://{address}/oauth2/device"),
                token_url: format!("http://{address}/oauth2/token"),
                gql_url: format!("http://{address}/gql"),
            },
            handle,
        )
    }

    fn spawn_twitch_server(
        expected_requests: usize,
    ) -> (
        TwitchEndpoints,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                recorded.lock().unwrap().push(request.clone());
                let response = if request.starts_with("GET / ") {
                    http_response(
                        "200 OK",
                        r#"<!doctype html><script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#,
                    )
                } else if request.contains(r#""operationName":"ChannelFollows""#) {
                    http_response(
                        "200 OK",
                        r#"{"data":{"user":{"follows":{"edges":[{"node":{"login":"alice"},"cursor":"cursor-1"},{"node":{"login":"bob"},"cursor":"cursor-2"}],"pageInfo":{"hasNextPage":false}}}}}"#,
                    )
                } else if request.contains(r#""operationName":"GetIDFromLogin""#) {
                    http_response("200 OK", r#"{"data":{"user":{"id":"100"}}}"#)
                } else if request.contains(r#""operationName":"ChannelPointsContext""#) {
                    http_response(
                        "200 OK",
                        &fixture_json("twitch.channel_points_context.json"),
                    )
                } else if request.contains(r#""operationName":"ClaimCommunityPoints""#) {
                    http_response(
                        "200 OK",
                        r#"{"data":{"claimCommunityPoints":{"balance":1550}}}"#,
                    )
                } else if request.contains(r#""operationName":"WithIsStreamLiveQuery""#) {
                    http_response(
                        "200 OK",
                        r#"{"data":{"user":{"stream":{"id":"stream-1"}}}}"#,
                    )
                } else if request
                    .contains(r#""operationName":"VideoPlayerStreamInfoOverlayChannel""#)
                {
                    http_response("200 OK", &fixture_json("twitch.stream_info.json"))
                } else {
                    panic!("unexpected request: {request}");
                };
                stream.write_all(&response).unwrap();
            }
        });

        (
            TwitchEndpoints {
                twitch_url: format!("http://{address}"),
                gql_url: format!("http://{address}/gql"),
            },
            requests,
            handle,
        )
    }

    fn spawn_status_server(
        statuses: Vec<&'static str>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                recorded.lock().unwrap().push(request);
                stream.write_all(&empty_http_response(status)).unwrap();
            }
        });
        (format!("http://{address}/spade"), requests, handle)
    }

    fn spawn_json_response_server(
        responses: Vec<String>,
    ) -> (
        TwitchEndpoints,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let mut responses = std::collections::VecDeque::from(responses);
            while !responses.is_empty() {
                let wait_started = std::time::Instant::now();
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if wait_started.elapsed() >= Duration::from_secs(5) {
                                return;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept json test request failed: {error}"),
                    }
                };
                let request = read_http_request(&mut stream);
                recorded.lock().unwrap().push(request);
                let latest_request = recorded.lock().unwrap().last().cloned().unwrap_or_default();
                let response = if latest_request.starts_with("GET / ") {
                    http_response(
                        "200 OK",
                        r#"<!doctype html><script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#,
                    )
                } else {
                    let body = responses.pop_front().unwrap();
                    http_response("200 OK", &body)
                };
                stream.write_all(&response).unwrap();
            }
        });
        (
            TwitchEndpoints {
                twitch_url: format!("http://{address}"),
                gql_url: format!("http://{address}/gql"),
            },
            requests,
            handle,
        )
    }

    fn test_observability() -> AppObservability {
        AppObservability::new(
            None,
            DiscordClient::new(Duration::from_secs(1)).unwrap(),
            false,
            false,
            false,
            true,
        )
    }

    #[test]
    fn env_has_value_ignores_missing_and_blank_values() {
        let key = format!(
            "TM_APP_TEST_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        env::remove_var(&key);
        assert!(!env_has_value(&key));
        env::set_var(&key, "   ");
        assert!(!env_has_value(&key));
        env::set_var(&key, "value");
        assert!(env_has_value(&key));
        env::remove_var(&key);
    }

    #[test]
    fn cli_override_detection_matches_path_and_env_inputs() {
        let cli = Cli {
            config: None,
            data_dir: None,
        };
        assert!(!has_override(&cli));

        let cli = Cli {
            config: Some(PathBuf::from("config.json")),
            data_dir: None,
        };
        assert!(has_override(&cli));
    }

    #[test]
    fn should_fallback_to_user_config_matches_go_permission_cases() {
        let permission = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        assert!(should_fallback_to_user_config(&permission));

        let read_only = io::Error::from_raw_os_error(READ_ONLY_FILE_SYSTEM_ERROR);
        assert!(should_fallback_to_user_config(&read_only));

        let missing = io::Error::new(io::ErrorKind::NotFound, "missing");
        assert!(!should_fallback_to_user_config(&missing));
    }

    #[test]
    fn config_fallback_switches_active_work_dir_and_config_path() {
        let requested_dir = unique_temp_dir();
        let fallback_dir = unique_temp_dir();
        fs::create_dir_all(&requested_dir).unwrap();

        let requested_paths = AppPaths {
            work_dir: requested_dir.clone(),
            config_path: requested_dir.join("config.json"),
        };
        let loaded = load_config_with_fallback_using(
            &requested_paths,
            false,
            || Some(fallback_dir.clone()),
            |path| {
                if path == requested_paths.config_path {
                    return Err(ConfigError::Io(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "read only",
                    )));
                }
                if path == fallback_dir.join("config.json") {
                    return Ok(ConfigFile {
                        username: String::from("tester"),
                        ..ConfigFile::default()
                    });
                }
                panic!("unexpected config path: {}", path.display());
            },
        )
        .unwrap();

        assert_eq!(loaded.active_paths.work_dir, fallback_dir);
        assert_eq!(loaded.active_paths.config_path, fallback_dir.join("config.json"));
        assert_eq!(loaded.config.username, "tester");

        fs::remove_dir_all(&requested_dir).unwrap();
        fs::remove_dir_all(&fallback_dir).unwrap();
    }

    #[test]
    fn new_session_id_is_stable_shape() {
        let session_id = new_session_id();
        assert!(session_id.contains('-'));
        assert!(!session_id.ends_with('-'));
    }

    #[test]
    fn normalized_username_rejects_default_placeholder() {
        assert!(normalized_username("your-twitch-username").is_err());
        assert_eq!(normalized_username(" Alice ").unwrap(), "alice");
    }

    #[test]
    fn drop_is_claimable_requires_unclaimed_completed_drop() {
        let claimable = InventoryDrop {
            drop_instance_id: "drop-1".into(),
            reward_name: "Reward".into(),
            campaign_name: "Campaign".into(),
            current_minutes_watched: 60,
            required_minutes_watched: 60,
            is_claimed: false,
        };
        assert!(drop_is_claimable(&claimable));

        let claimed = InventoryDrop {
            is_claimed: true,
            ..claimable.clone()
        };
        assert!(!drop_is_claimable(&claimed));

        let incomplete = InventoryDrop {
            current_minutes_watched: 59,
            ..claimable
        };
        assert!(!drop_is_claimable(&incomplete));
    }

    #[test]
    fn streamer_game_name_prefers_display_name() {
        let streamer = Streamer {
            stream: Some(tm_domain::Stream {
                game: Some(Game {
                    display_name: Some(String::from("VALORANT")),
                    name: Some(String::from("valorant")),
                }),
                ..tm_domain::Stream::default()
            }),
            ..Streamer::default()
        };
        assert_eq!(
            streamer_game_name(&streamer),
            Some(String::from("VALORANT"))
        );
    }

    #[test]
    fn observability_online_message_includes_game_when_enabled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            channel_points: 1_250,
            stream: Some(tm_domain::Stream {
                game: Some(Game {
                    display_name: Some(String::from("VALORANT")),
                    name: Some(String::from("valorant")),
                }),
                ..tm_domain::Stream::default()
            }),
            ..Streamer::default()
        };

        assert_eq!(
            observability.online_message(&streamer),
            "🥳 alice (1.25k points) is Online! | Playing: VALORANT"
        );
    }

    #[test]
    fn observability_game_change_requires_enabled_distinct_games() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            ..Streamer::default()
        };

        assert_eq!(
            observability.game_change_message(&streamer, "Just Chatting", "VALORANT"),
            Some(String::from("🎮 alice now playing: VALORANT!"))
        );
        assert_eq!(
            observability.game_change_message(&streamer, "VALORANT", "valorant"),
            None
        );
    }

    #[test]
    fn observability_points_message_matches_sample_shape() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            false,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            channel_points: 1_250,
            stream: Some(tm_domain::Stream {
                game: Some(Game {
                    display_name: Some(String::from("VALORANT")),
                    name: Some(String::from("valorant")),
                }),
                ..tm_domain::Stream::default()
            }),
            ..Streamer::default()
        };

        assert_eq!(
            observability.points_earned_message(&streamer, 10, "watch"),
            "🚀 +10 → alice (1.25k points) - Reason: WATCH | Game: VALORANT"
        );
    }

    #[test]
    fn observability_claim_messages_are_styled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            channel_points: 1_250,
            ..Streamer::default()
        };

        assert_eq!(
            observability.bonus_claim_message(&streamer, false),
            "🎁 Claimed bonus → alice (1.25k points)"
        );
        assert_eq!(
            observability.bonus_claim_message(&streamer, true),
            "🎁 Claimed startup bonus → alice (1.25k points)"
        );
    }

    #[test]
    fn observability_startup_messages_are_styled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );

        assert_eq!(
            observability.start_session_message("session-123"),
            "🟢 Start session: 'session-123'"
        );
        assert_eq!(
            observability.loading_streamers_message(16),
            "⏳ Loading data for 16 streamer(s). Please wait..."
        );
        assert_eq!(
            observability.loaded_streamers_message(16, Duration::from_millis(20_500)),
            "✅ 16 Streamer loaded! (20.5 seconds)"
        );
    }

    #[test]
    fn observability_drop_claim_message_is_styled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let drop = InventoryDrop {
            drop_instance_id: String::from("drop-1"),
            reward_name: String::from("60 min."),
            campaign_name: String::from("Crimson Desert Drops #2"),
            current_minutes_watched: 61,
            required_minutes_watched: 60,
            is_claimed: false,
        };

        assert_eq!(
            observability.drop_claim_message("periodic", &drop),
            "🎁 Claimed drop → 60 min. | Campaign: Crimson Desert Drops #2 | Progress: 61/60 (101%) | Mode: PERIODIC"
        );
    }

    #[test]
    fn minute_watcher_resume_gap_uses_threshold() {
        assert_eq!(minute_watcher_resume_gap(ts(0), ts(599)), None);
        assert_eq!(
            minute_watcher_resume_gap(ts(0), ts(600)),
            Some(Duration::from_secs(600))
        );
        assert_eq!(format_resume_gap(Duration::from_secs(6_123)), "1h 42m 3s");
    }

    #[test]
    fn pubsub_reconnect_delay_distinguishes_requested_and_generic_retries() {
        let reconnect_requested = Ok(Err(tm_pubsub::PubSubError::ReconnectRequested));
        let generic_failure = Ok(Err(tm_pubsub::PubSubError::PongTimeout));
        let clean_close = Ok(Ok(()));

        assert_eq!(
            pubsub_reconnect_delay(&reconnect_requested, 0, 1),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            pubsub_reconnect_delay(&generic_failure, 0, 1),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            pubsub_reconnect_delay(&clean_close, 0, 1),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn timezone_validation_accepts_iana_names() {
        assert_eq!(
            validate_timezone_override(Some("Europe/Athens")),
            Some(TimezoneValidation::Valid(String::from("Europe/Athens")))
        );
        assert_eq!(
            validate_timezone_override(Some("not/a-timezone")),
            Some(TimezoneValidation::Invalid(String::from("not/a-timezone")))
        );
        assert_eq!(validate_timezone_override(Some("auto")), None);
    }

    #[test]
    fn prediction_wait_duration_uses_streamer_delay_settings() {
        let streamer = Streamer {
            settings: tm_domain::StreamerSettings {
                bet: BetSettings {
                    delay_mode: DelayMode::FromEnd,
                    delay: Some(15.0),
                    ..BetSettings::default()
                },
                ..tm_domain::StreamerSettings::default()
            },
            ..Streamer::default()
        };
        let event = PredictionEvent {
            streamer,
            event_id: String::from("event-1"),
            title: String::from("Prediction"),
            status: String::from("ACTIVE"),
            created_at: ts(0),
            window_seconds: 100.0,
            outcomes: vec![PredictionOutcome::default()],
            decision: PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };

        assert_eq!(
            prediction_wait_duration(&event, ts(10)),
            Duration::from_secs(75)
        );
    }

    #[tokio::test]
    async fn load_targets_uses_mocked_followers_in_follower_mode() {
        let (endpoints, requests, server) = spawn_twitch_server(2);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );
        let config = ConfigFile::default();

        let targets = load_targets(&config, &twitch).await.unwrap();
        assert_eq!(targets, vec![String::from("alice"), String::from("bob")]);

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests.iter().any(|request| request.starts_with("GET / ")));
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""operationName":"ChannelFollows""#)));
    }

    #[tokio::test]
    async fn bootstrap_runtime_state_claims_startup_bonus_in_manual_mode() {
        let (endpoints, requests, server) = spawn_twitch_server(6);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );
        let observability = test_observability();
        let config = ConfigFile {
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };

        let state =
            bootstrap_runtime_state(&config, &twitch, Some("user-1"), ts(0), &observability)
                .await
                .unwrap();

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(state.streamers.len(), 1);
        assert_eq!(state.streamers[0].channel_id, "100");
        assert_eq!(state.streamers[0].channel_points, 1234);
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""operationName":"ClaimCommunityPoints""#)));
    }

    #[tokio::test]
    async fn mocked_login_and_bootstrap_flow_rehydrates_session_into_twitch_client() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();

        let (auth_endpoints, auth_server) = spawn_auth_server();
        let auth_client = TwitchAuthClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            auth_endpoints,
        );
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };

        let session = load_or_login_session_with_auth_client(&config, &temp_dir, &auth_client)
            .await
            .unwrap();
        assert_eq!(session.auth_token(), Some("token-123"));
        assert_eq!(session.user_id(), Some("user-123"));

        let (twitch_endpoints, requests, twitch_server) = spawn_twitch_server(6);
        let twitch = TwitchClient::with_client_and_cookie_header_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            session.auth_token().unwrap(),
            "ua",
            session.cookie_header_for_host("twitch.tv"),
            twitch_endpoints,
        );

        let state = bootstrap_runtime_state(
            &config,
            &twitch,
            session.user_id(),
            ts(0),
            &test_observability(),
        )
        .await
        .unwrap();

        auth_server.join().unwrap();
        twitch_server.join().unwrap();
        assert_eq!(state.streamers[0].username, "alice");
        assert_eq!(
            session.cookie_header_for_host("gql.twitch.tv").as_deref(),
            Some("auth-token=token-123; persistent=user-123")
        );
        assert!(requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(r#""operationName":"ChannelPointsContext""#)));

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[tokio::test]
    async fn refresh_snapshot_streamers_updates_runtime_context() {
        let (endpoints, requests, server) = spawn_twitch_server(3);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);

        refresh_snapshot_streamers(&runtime, &twitch, "user-1", &test_observability())
            .await
            .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        server.join().unwrap();
        assert_eq!(snapshot.streamers[0].channel_points, 1234);
        assert!(requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(r#""operationName":"ClaimCommunityPoints""#)));
    }

    #[tokio::test]
    async fn pending_claim_loop_runs_immediate_sweep_and_stops_promptly() {
        let (endpoints, requests, server) = spawn_json_response_server(vec![
            fixture_json("twitch.channel_points_context.json"),
            serde_json::json!({
                "data": {
                    "claimCommunityPoints": {
                        "balance": 1550
                    }
                }
            })
            .to_string(),
        ]);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = spawn_pending_claim_loop(
            stop_rx,
            runtime.clone(),
            twitch,
            String::from("user-1"),
            test_observability(),
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.state_snapshot().await.unwrap().streamers[0].channel_points == 1234 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""operationName":"ClaimCommunityPoints""#)));
    }

    #[tokio::test]
    async fn pending_claim_loop_skips_bonus_claim_when_none_is_available() {
        let (endpoints, requests, server) = spawn_json_response_server(vec![
            serde_json::json!({
                "data": {
                    "community": {
                        "channel": {
                            "self": {
                                "communityPoints": {
                                    "balance": 1234,
                                    "availableClaim": null,
                                    "activeMultipliers": []
                                }
                            },
                            "communityPointsSettings": {
                                "goals": []
                            }
                        }
                    }
                }
            })
            .to_string(),
        ]);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = spawn_pending_claim_loop(
            stop_rx,
            runtime.clone(),
            twitch,
            String::from("user-1"),
            test_observability(),
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.state_snapshot().await.unwrap().streamers[0].channel_points == 1234 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(!requests
            .iter()
            .any(|request| request.contains(r#""operationName":"ClaimCommunityPoints""#)));
    }

    #[tokio::test]
    async fn refresh_watch_selection_metadata_updates_candidate_choice() {
        let (endpoints, requests, server) =
            spawn_json_response_server(vec![fixture_json("twitch.stream_info.json")]);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![
                String::from("alice"),
                String::from("bob"),
                String::from("carol"),
            ],
            watch_priority: vec![String::from("ORDER")],
            game_exclude: vec![String::from("game name")],
            ..ConfigFile::default()
        };
        let now = ts(300);
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![
            Streamer {
                username: String::from("alice"),
                channel_id: String::from("100"),
                is_online: true,
                presence_known: true,
                online_at: Some(ts(0)),
                stream: Some(tm_domain::Stream {
                    game: Some(Game::from_name("Chess")),
                    last_update: Some(ts(0)),
                    ..tm_domain::Stream::default()
                }),
                ..Streamer::default()
            },
            Streamer {
                username: String::from("bob"),
                channel_id: String::from("200"),
                is_online: true,
                presence_known: true,
                online_at: Some(ts(0)),
                stream: Some(tm_domain::Stream {
                    game: Some(Game::from_name("Chess")),
                    last_update: Some(now),
                    ..tm_domain::Stream::default()
                }),
                ..Streamer::default()
            },
            Streamer {
                username: String::from("carol"),
                channel_id: String::from("300"),
                is_online: true,
                presence_known: true,
                online_at: Some(ts(0)),
                stream: Some(tm_domain::Stream {
                    game: Some(Game::from_name("Chess")),
                    last_update: Some(now),
                    ..tm_domain::Stream::default()
                }),
                ..Streamer::default()
            },
        ];
        let runtime = tm_runtime::spawn_runtime_state(state);

        assert_eq!(
            runtime.state_snapshot().await.unwrap().watch_target_logins(now),
            vec![String::from("alice"), String::from("bob")]
        );

        let stale_streamer = runtime.state_snapshot().await.unwrap().streamers[0].clone();
        refresh_watch_selection_metadata(
            &runtime,
            &twitch,
            &[stale_streamer],
            &test_observability(),
            now,
        )
        .await
        .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        server.join().unwrap();
        assert_eq!(
            snapshot.watch_target_logins(now),
            vec![String::from("bob"), String::from("carol")]
        );
        assert_eq!(
            snapshot.streamers[0]
                .stream
                .as_ref()
                .and_then(|stream| stream.game.as_ref())
                .and_then(|game| game.display_name.clone()),
            Some(String::from("Game Name"))
        );
        assert!(requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(r#""operationName":"VideoPlayerStreamInfoOverlayChannel""#)));
    }

    #[tokio::test]
    async fn claim_available_drops_rejects_invalid_claim_status() {
        let (endpoints, requests, server) = spawn_json_response_server(vec![
            serde_json::json!({
                "data": {
                    "currentUser": {
                        "inventory": {
                            "dropCampaignsInProgress": [{
                                "name": "Campaign",
                                "timeBasedDrops": [{
                                    "name": "Reward",
                                    "requiredMinutesWatched": 60,
                                    "self": {
                                        "dropInstanceID": "drop-1",
                                        "currentMinutesWatched": 60,
                                        "isClaimed": false
                                    }
                                }]
                            }]
                        }
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "data": {
                    "claimDropRewards": {
                        "status": "INELIGIBLE"
                    }
                }
            })
            .to_string(),
        ]);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );

        let error = claim_available_drops(&twitch, "periodic", &test_observability())
            .await
            .unwrap_err();

        server.join().unwrap();
        assert!(
            error.chain().any(|cause| cause
                .to_string()
                .contains("unexpected drop claim status INELIGIBLE")),
            "{error:?}"
        );
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn send_minute_watched_for_streamer_updates_presence_and_watch_progress() {
        let (endpoints, _requests, twitch_server) = spawn_twitch_server(2);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );
        let (spade_url, _spade_requests, spade_server) =
            spawn_status_server(vec!["204 No Content"]);
        let spade_urls = tokio::sync::Mutex::new(HashMap::from([(
            String::from("alice"),
            SpadeCacheEntry::Ready(CachedSpadeUrl {
                url: spade_url,
                fetched_at: StdInstant::now(),
            }),
        )]));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);

        let mut snapshot = runtime.state_snapshot().await.unwrap();
        let streamer = snapshot.streamers.remove(0);
        send_minute_watched_for_streamer(
            &runtime,
            &twitch,
            &spade_urls,
            &streamer,
            "user-1",
            &test_observability(),
        )
        .await
        .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        twitch_server.join().unwrap();
        spade_server.join().unwrap();
        assert!(snapshot.streamers[0].is_online);
        assert!(snapshot.streamers[0]
            .stream
            .as_ref()
            .and_then(|stream| stream.last_minute_update)
            .is_some());
    }

    #[tokio::test]
    async fn spade_cache_retries_with_fresh_url_after_failure() {
        let spade_urls = tokio::sync::Mutex::new(HashMap::new());
        let fetches = Arc::new(AtomicUsize::new(0));
        let sent_urls = Arc::new(Mutex::new(Vec::<String>::new()));

        let status = send_minute_watched_with_spade_cache(
            &spade_urls,
            "alice",
            {
                let fetches = Arc::clone(&fetches);
                move |_login| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        let next = fetches.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok::<_, std::io::Error>(format!("https://spade-{next}.example"))
                    }
                }
            },
            {
                let sent_urls = Arc::clone(&sent_urls);
                move |spade_url| {
                    let sent_urls = Arc::clone(&sent_urls);
                    let spade_url = spade_url.clone();
                    async move {
                        sent_urls.lock().unwrap().push(spade_url);
                        if sent_urls.lock().unwrap().len() == 1 {
                            Ok(StatusCode::BAD_REQUEST)
                        } else {
                            Ok(StatusCode::NO_CONTENT)
                        }
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert_eq!(
            sent_urls.lock().unwrap().as_slice(),
            ["https://spade-1.example", "https://spade-2.example"]
        );
    }

    #[tokio::test]
    async fn spade_cache_uses_single_inflight_fetch_per_streamer() {
        let spade_urls = tokio::sync::Mutex::new(HashMap::new());
        let fetches = Arc::new(AtomicUsize::new(0));

        let (first, second) = tokio::join!(
            resolve_spade_url(&spade_urls, "alice", false, {
                let fetches = Arc::clone(&fetches);
                move |_login| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        fetches.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(String::from("https://spade.example"))
                    }
                }
            }),
            resolve_spade_url(&spade_urls, "alice", false, {
                let fetches = Arc::clone(&fetches);
                move |_login| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        fetches.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(String::from("https://spade.example"))
                    }
                }
            })
        );

        assert_eq!(first.unwrap(), "https://spade.example");
        assert_eq!(second.unwrap(), "https://spade.example");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn record_prediction_placed_can_skip_balance_deduction() {
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            channel_points: 1_000,
            ..Streamer::default()
        }];
        state.predictions.insert(
            String::from("event-1"),
            PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: String::from("event-1"),
                title: String::from("Prediction"),
                status: String::from("ACTIVE"),
                created_at: ts(0),
                window_seconds: 30.0,
                outcomes: vec![PredictionOutcome {
                    id: String::from("a"),
                    title: String::from("Alpha"),
                    color: String::from("blue"),
                    ..PredictionOutcome::default()
                }],
                decision: PredictionDecision::default(),
                bet_placed: false,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        let runtime = tm_runtime::spawn_runtime_state(state);

        runtime
            .record_prediction_placed(
                "event-1",
                PredictionDecision {
                    choice: Some(0),
                    outcome_id: String::from("a"),
                    amount: 250,
                },
                false,
            )
            .await
            .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        assert_eq!(snapshot.streamers[0].channel_points, 1_000);
        assert_eq!(snapshot.predictions["event-1"].decision.amount, 250);
    }
}


