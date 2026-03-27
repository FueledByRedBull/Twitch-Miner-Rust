use std::collections::HashMap;
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant as StdInstant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use reqwest::StatusCode;
use serde_json::json;
use tm_auth::{AuthSession, AuthSessionError, TwitchAuthClient};
use tm_config::{
    default_user_config_dir, load_or_create_config, resolve_app_paths_from_env, validate_config,
    AppPaths, ConfigError, ConfigFile,
};
use tm_domain::{
    format_channel_points, format_drop_progress, progress_percent, Game, PredictionDecision,
    Streamer,
};
use tm_irc::{ChatClient, ChatEventKind, ChatLogger};
use tm_observability::{
    build_discord_request, event_from_bet_result, event_from_gain_reason, init_tracing,
    new_discord_webhook, Anonymizer, DiscordClient, DiscordSettings, Event as DiscordEvent,
    LoggerSettings, TracingInitOptions,
};
use tm_pubsub::{build_topic_batches, PubSubClient, PubSubConnectionEvent};
use tm_twitch::{generate_device_id, InventoryDrop, TwitchClient};

const DEFAULT_CONSOLE_TITLE: &str = "Klaro's Twitch Miner";
const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";
const READ_ONLY_FILE_SYSTEM_ERROR: i32 = 30;
const CONTEXT_REFRESH_CONCURRENCY: usize = 8;
const SPADE_URL_TTL: Duration = Duration::from_secs(15 * 60);
const SESSION_SUMMARY_INDENT: usize = 25;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TimezoneValidation {
    Valid(String),
    Invalid(String),
}

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long = "data-dir")]
    data_dir: Option<PathBuf>,
}

struct BackgroundTasks {
    pubsub: Option<tokio::task::JoinHandle<()>>,
    context: Option<tokio::task::JoinHandle<()>>,
    minute: Option<tokio::task::JoinHandle<()>>,
    drop: Option<tokio::task::JoinHandle<()>>,
    chat: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
struct CachedSpadeUrl {
    url: String,
    fetched_at: StdInstant,
}

#[derive(Debug, Clone)]
enum SpadeCacheEntry {
    Ready(CachedSpadeUrl),
    Refreshing(Arc<tokio::sync::Notify>),
}

enum SpadeResolveAction {
    Use(String),
    Wait(Arc<tokio::sync::Notify>),
    Fetch(Arc<tokio::sync::Notify>),
}

struct BackgroundTaskParams<'a> {
    config: &'a ConfigFile,
    stop_rx: tokio::sync::watch::Receiver<bool>,
    runtime: &'a tm_runtime::RuntimeHandle,
    twitch: &'a Arc<TwitchClient>,
    auth_token: &'a str,
    user_id: Option<&'a String>,
    initial_streamers: &'a [Streamer],
    observability: &'a AppObservability,
}

#[derive(Clone)]
struct AppObservability {
    discord: Option<tm_observability::DiscordWebhook>,
    discord_client: DiscordClient,
    anonymizer: Arc<Mutex<Anonymizer>>,
    emoji: bool,
    show_claimed_bonus: bool,
    show_game: bool,
}

impl AppObservability {
    #[allow(clippy::fn_params_excessive_bools)]
    fn new(
        discord: Option<tm_observability::DiscordWebhook>,
        discord_client: DiscordClient,
        anonymize_logs: bool,
        emoji: bool,
        show_claimed_bonus: bool,
        show_game: bool,
    ) -> Self {
        Self {
            discord,
            discord_client,
            anonymizer: Arc::new(Mutex::new(Anonymizer::new(anonymize_logs))),
            emoji,
            show_claimed_bonus,
            show_game,
        }
    }

    fn streamer_name(&self, streamer: &Streamer) -> String {
        let mut anonymizer = self.anonymizer.lock().expect("anonymizer lock poisoned");
        anonymizer.streamer_name(streamer)
    }

    fn channel_points(&self, streamer: &Streamer) -> String {
        let mut anonymizer = self.anonymizer.lock().expect("anonymizer lock poisoned");
        format_channel_points(anonymizer.pseudo_channel_points(streamer))
    }

    fn streamer_label(&self, streamer: &Streamer) -> String {
        format!(
            "{} ({} points)",
            self.streamer_name(streamer),
            self.channel_points(streamer)
        )
    }

    fn decorate(&self, emoji: &str, message: String) -> String {
        if self.emoji {
            format!("{emoji} {message}")
        } else {
            message
        }
    }

    fn online_message(&self, streamer: &Streamer) -> String {
        self.decorate(
            "🥳",
            format!("{} is Online!", self.streamer_label(streamer)),
        )
    }

    fn offline_message(&self, streamer: &Streamer) -> String {
        self.decorate(
            "😴",
            format!("{} is Offline!", self.streamer_label(streamer)),
        )
    }

    fn game_change_message(
        &self,
        streamer: &Streamer,
        previous: &str,
        current: &str,
    ) -> Option<String> {
        if !self.show_game {
            return None;
        }
        let previous = previous.trim();
        let current = current.trim();
        if previous.is_empty() || current.is_empty() || previous.eq_ignore_ascii_case(current) {
            return None;
        }
        Some(self.decorate(
            "🎮",
            format!("{} now playing: {}!", self.streamer_name(streamer), current),
        ))
    }

    fn points_earned_message(&self, streamer: &Streamer, earned: i64, reason: &str) -> String {
        let reason = reason.trim().to_uppercase();
        let mut message = format!(
            "{} {} - Reason: {}",
            signed_points(earned),
            self.streamer_label(streamer),
            reason
        );
        if self.show_game && reason == "WATCH" {
            if let Some(game_name) = streamer_game_name(streamer) {
                message.push_str(" | Game: ");
                message.push_str(&game_name);
            }
        }
        self.decorate("🚀", message)
    }

    fn join_raid_message(&self, from: &str, target_login: &str) -> String {
        self.decorate("🎭", format!("Joining raid from {from} to {target_login}"))
    }

    fn chat_presence_message(&self, join: bool, streamer_name: &str) -> String {
        let action = if join { "Join" } else { "Leave" };
        self.decorate("💬", format!("{action} IRC Chat: {streamer_name}"))
    }

    async fn send_event(&self, event: DiscordEvent, message: &str) {
        send_discord_event(self.discord.as_ref(), &self.discord_client, event, message).await;
    }

    fn spawn_event(&self, event: DiscordEvent, message: String) {
        let this = self.clone();
        tokio::spawn(async move {
            this.send_event(event, &message).await;
        });
    }
}

fn streamer_game_name(streamer: &Streamer) -> Option<String> {
    streamer
        .stream
        .as_ref()
        .and_then(|stream| stream.game.as_ref())
        .and_then(|game| {
            game.display_name
                .as_deref()
                .or(game.name.as_deref())
                .map(str::trim)
        })
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn signed_points(amount: i64) -> String {
    let sign = if amount >= 0 { "+" } else { "-" };
    format!("{sign}{} →", format_channel_points(amount.abs()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let has_override = has_override(&cli);
    let paths = resolve_app_paths_from_env(cli.config, cli.data_dir)?;
    prepare_work_dir(&paths)?;
    set_console_title(DEFAULT_CONSOLE_TITLE);
    clear_console();

    let (config, config_path) = load_config_with_fallback(&paths, has_override)?;
    let timezone_validation = validate_timezone_override(config.timezone.as_deref());

    init_tracing(&TracingInitOptions {
        settings: build_logger_settings(&config),
        base_dir: env::current_dir()?,
        username: config.username.clone(),
        timezone: config.timezone.clone(),
    })?;
    log_timezone_validation(timezone_validation.as_ref());

    if run_auto_update_if_enabled(&config).await? {
        return Ok(());
    }

    let observability = build_observability(&config)?;

    let session_id = new_session_id();
    let started_at = time_now();
    let http_client = build_http_client(config.disable_ssl_cert_verification)?;
    let session = load_or_login_session(&config, &paths.work_dir, http_client.clone()).await?;
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
    let state = bootstrap_runtime_state(
        &config,
        &twitch,
        user_id.as_deref(),
        started_at,
        &observability,
    )
    .await?;
    claim_startup_drops_if_enabled(&config, &state.streamers, &twitch, &observability).await?;
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

    log_startup(&paths, &config_path, &summary, &session_id, &observability).await;

    wait_for_shutdown_signal().await?;
    shutdown_background_tasks(stop_tx, tasks).await;
    tracing::info!(session_id = %session_id, "shutdown requested");
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

fn has_override(cli: &Cli) -> bool {
    cli.config.is_some()
        || cli.data_dir.is_some()
        || env_has_value("TCPM_CONFIG")
        || env_has_value("TCPM_DATA_DIR")
}

fn env_has_value(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

fn load_config_with_fallback(
    paths: &AppPaths,
    has_override: bool,
) -> Result<(ConfigFile, PathBuf), ConfigError> {
    match load_or_create_config(&paths.config_path) {
        Ok(config) => {
            validate_config(&config)?;
            Ok((config, paths.config_path.clone()))
        }
        Err(ConfigError::Io(error)) if !has_override && should_fallback_to_user_config(&error) => {
            let fallback_dir = default_user_config_dir().ok_or(error)?;
            std::fs::create_dir_all(&fallback_dir)?;
            env::set_current_dir(&fallback_dir)?;
            let fallback_path = fallback_dir.join("config.json");
            let config = load_or_create_config(&fallback_path)?;
            validate_config(&config)?;
            Ok((config, fallback_path))
        }
        Err(error) => Err(error),
    }
}

fn prepare_work_dir(paths: &AppPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.work_dir)?;
    env::set_current_dir(&paths.work_dir)?;
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
    match tm_updater::run_auto_update(env!("CARGO_PKG_VERSION"), &args).await? {
        tm_updater::AutoUpdateOutcome::UpToDate => Ok(false),
        tm_updater::AutoUpdateOutcome::UpdateAvailableForDevRun { latest_version } => {
            tracing::warn!(
                latest_version = %latest_version,
                "auto-update skipped for development run"
            );
            Ok(false)
        }
        tm_updater::AutoUpdateOutcome::UpdatedAndRestarting { latest_version } => {
            tracing::info!(
                latest_version = %latest_version,
                "auto-update installed a newer version; restarting"
            );
            Ok(true)
        }
    }
}

fn build_observability(config: &ConfigFile) -> Result<AppObservability> {
    let discord = new_discord_webhook(&DiscordSettings {
        webhook_api: config.discord.webhook_api.clone(),
        events: config.discord.events.clone(),
    });
    let discord_client = DiscordClient::new(Duration::from_secs(15))?;
    Ok(AppObservability::new(
        discord,
        discord_client,
        config.privacy.anonymize_logs,
        config.emojis,
        config.show_claimed_bonus_msg,
        config.show_game,
    ))
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
        minute,
        drop,
        chat,
    })
}

async fn log_startup(
    paths: &AppPaths,
    config_path: &Path,
    summary: &tm_runtime::RuntimeSummary,
    session_id: &str,
    observability: &AppObservability,
) {
    tracing::info!(
        session_id = %session_id,
        work_dir = %paths.work_dir.display(),
        config_path = %config_path.display(),
        configured_streamers = summary.configured_streamers,
        follower_mode = summary.follower_mode,
        "bootstrap complete"
    );
    send_discord_event(
        observability.discord.as_ref(),
        &observability.discord_client,
        DiscordEvent::Startup,
        &format!(
            "Start session: '{}' | configured_streamers={} follower_mode={}",
            session_id, summary.configured_streamers, summary.follower_mode
        ),
    )
    .await;
    if config_path != paths.config_path {
        tracing::info!(
            requested_config_path = %paths.config_path.display(),
            active_config_path = %config_path.display(),
            "using fallback user config directory"
        );
    }
}

async fn shutdown_background_tasks(
    stop_tx: tokio::sync::watch::Sender<bool>,
    tasks: BackgroundTasks,
) {
    let _ = stop_tx.send(true);
    if let Some(task) = tasks.pubsub {
        let _ = task.await;
    }
    if let Some(task) = tasks.context {
        let _ = task.await;
    }
    if let Some(task) = tasks.minute {
        let _ = task.await;
    }
    if let Some(task) = tasks.drop {
        let _ = task.await;
    }
    if let Some(task) = tasks.chat {
        let _ = task.await;
    }
}

fn build_http_client(disable_ssl_cert_verification: bool) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .danger_accept_invalid_certs(disable_ssl_cert_verification)
        .build()
        .context("build http client")
}

async fn load_or_login_session(
    config: &ConfigFile,
    base_dir: &Path,
    client: reqwest::Client,
) -> Result<AuthSession> {
    let auth_client = TwitchAuthClient::with_client(client);
    load_or_login_session_with_auth_client(config, base_dir, &auth_client).await
}

async fn load_or_login_session_with_auth_client(
    config: &ConfigFile,
    base_dir: &Path,
    auth_client: &TwitchAuthClient,
) -> Result<AuthSession> {
    let username = normalized_username(&config.username)?;
    let device_id = generate_device_id();

    match AuthSession::load_from_dir(base_dir, &username) {
        Ok(mut session) => {
            if let Some(auth_token) = session.auth_token().map(str::to_string) {
                match auth_client
                    .validate_login(&auth_token, &device_id, &username, DEFAULT_USER_AGENT)
                    .await
                {
                    Ok(user_id) => {
                        session.set_user_id(user_id);
                        session.save_to_dir(base_dir)?;
                        tracing::info!(username = %username, "loaded cookies from disk");
                        return Ok(session);
                    }
                    Err(error) => {
                        tracing::warn!(
                            username = %username,
                            %error,
                            "saved cookies are invalid; starting device login"
                        );
                    }
                }
            } else {
                tracing::warn!(
                    username = %username,
                    "saved cookies missing auth-token; starting device login"
                );
            }
        }
        Err(AuthSessionError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                username = %username,
                %error,
                "unable to read saved cookies; starting device login"
            );
        }
    }

    let prompt = auth_client.request_device_code(&device_id).await?;
    tracing::info!(
        verification_uri = %prompt.verification_uri,
        user_code = %prompt.user_code,
        expires_in_seconds = prompt.expires_in.as_secs(),
        "complete Twitch device login"
    );
    let started = tokio::time::Instant::now();
    let auth_token = loop {
        if started.elapsed() >= prompt.expires_in {
            return Err(anyhow!("device code expired before authorization"));
        }
        match auth_client
            .poll_access_token(&device_id, &prompt.device_code)
            .await?
        {
            Some(token) => break token,
            None => tokio::time::sleep(prompt.interval).await,
        }
    };

    let user_id = auth_client
        .validate_login(&auth_token, &device_id, &username, DEFAULT_USER_AGENT)
        .await?;
    let mut session = AuthSession::new(&username, tm_auth::CookieStore::new());
    session.set_auth_token(auth_token);
    session.set_user_id(user_id);
    session.save_to_dir(base_dir)?;
    tracing::info!(username = %username, "device login completed");
    Ok(session)
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
        streamers = state.streamers.len(),
        "loading streamer context"
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
    streamer.channel_points = context.balance;
    streamer.active_multipliers = context.active_multipliers;
    streamer.community_goals = context
        .community_goals
        .into_iter()
        .map(|goal| (goal.id.clone(), goal))
        .collect::<HashMap<_, _>>();
    streamer.points_init = true;

    if let Some(claim_id) = context.claim_id.as_deref() {
        twitch
            .claim_bonus(&streamer.channel_id, claim_id, user_id)
            .await
            .with_context(|| format!("claim startup bonus for {}", streamer.username))?;
        if observability.show_claimed_bonus {
            let message = format!(
                "Claimed startup bonus for {}",
                observability.streamer_label(streamer)
            );
            tracing::info!(claim_id = %claim_id, "{message}");
            observability
                .send_event(DiscordEvent::BonusClaim, &message)
                .await;
        }
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
    } else {
        streamer.online_at = None;
        streamer.offline_at = Some(started_at);
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

fn normalized_username(username: &str) -> Result<String> {
    let username = username.trim().to_lowercase();
    if username.is_empty() || username == "your-twitch-username" {
        return Err(anyhow!("config.username must be set to a Twitch username"));
    }
    Ok(username)
}

fn drop_is_claimable(drop: &InventoryDrop) -> bool {
    !drop.is_claimed
        && !drop.drop_instance_id.trim().is_empty()
        && drop.current_minutes_watched >= drop.required_minutes_watched
}

fn prediction_wait_duration(
    event: &tm_domain::PredictionEvent,
    now: tm_runtime::RuntimeTime,
) -> std::time::Duration {
    let target_seconds = event
        .streamer
        .prediction_window_seconds(event.window_seconds);
    let target_millis =
        i128::try_from(std::time::Duration::from_secs_f64(target_seconds).as_millis())
            .unwrap_or(i128::MAX);
    let elapsed_millis = (now - event.created_at).whole_milliseconds();
    let remaining_millis = (target_millis - elapsed_millis).max(0);
    std::time::Duration::from_millis(u64::try_from(remaining_millis).unwrap_or(u64::MAX))
}

fn validate_timezone_override(raw: Option<&str>) -> Option<TimezoneValidation> {
    let zone = raw
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))?;
    match zone.parse::<chrono_tz::Tz>() {
        Ok(_) => Some(TimezoneValidation::Valid(zone.to_string())),
        Err(_) => Some(TimezoneValidation::Invalid(zone.to_string())),
    }
}

fn log_timezone_validation(validation: Option<&TimezoneValidation>) {
    match validation {
        Some(TimezoneValidation::Valid(zone)) => {
            tracing::info!(timezone = %zone, "using configured timezone");
        }
        Some(TimezoneValidation::Invalid(zone)) => {
            tracing::warn!(
                timezone = %zone,
                "timezone override ignored; falling back to system time"
            );
        }
        None => {}
    }
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
        tracing::info!(
            mode,
            reward = %drop.reward_name,
            campaign = %drop.campaign_name,
            progress = %format_drop_progress(drop.current_minutes_watched, drop.required_minutes_watched),
            percent = progress_percent(drop.current_minutes_watched, drop.required_minutes_watched),
            "claimed drop"
        );
        observability
            .send_event(
                DiscordEvent::DropClaim,
                &format!(
                    "Claimed drop {} ({}) {}",
                    drop.reward_name,
                    drop.campaign_name,
                    format_drop_progress(
                        drop.current_minutes_watched,
                        drop.required_minutes_watched
                    )
                ),
            )
            .await;
    }
    Ok(())
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
                    match result {
                        Ok(Ok(())) => {
                            tracing::warn!(
                                "PubSub[{connection_index}] connection closed; reconnecting ({} topic(s))",
                                topics.len()
                            );
                        }
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
                    false
                }
            };

            if should_stop {
                break;
            }

            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
    })
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
        let message = format!(
            "Claimed bonus for {}",
            observability.streamer_label(&streamer)
        );
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
    let contributions = load_goal_contributions(twitch, &streamer.username).await?;
    let mut available_points = streamer.channel_points;
    for goal in streamer.community_goals.values() {
        if goal.id.trim().is_empty() || !goal.is_in_stock || goal.status.to_uppercase() != "STARTED"
        {
            continue;
        }
        let user_points = contributions.get(&goal.id).copied().unwrap_or_default();
        let amount =
            tm_twitch::community_goal_contribution_amount(goal, user_points, available_points);
        if amount <= 0 {
            continue;
        }
        twitch
            .contribute_community_goal(amount, &streamer.channel_id, &goal.id)
            .await?;
        available_points -= amount;
        tracing::info!(
            streamer = %streamer.username,
            goal_id = %goal.id,
            title = %goal.title,
            amount,
            "contributed to community goal"
        );
    }
    refresh_streamer_context(
        runtime,
        twitch,
        &streamer,
        Some(persistent_user_id),
        observability,
    )
    .await
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

async fn runtime_streamer_by_channel_id(
    runtime: &tm_runtime::RuntimeHandle,
    channel_id: &str,
) -> Result<Option<Streamer>> {
    let snapshot = runtime.state_snapshot().await?;
    Ok(snapshot
        .streamers
        .into_iter()
        .find(|streamer| streamer.channel_id == channel_id))
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
    runtime.stop_tracking_prediction(event_id, "SKIPPED").await
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
        Ok(_) => {
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
                        break;
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
            let result = refresh_streamer_context(
                &runtime,
                twitch.as_ref(),
                &streamer,
                Some(&persistent_user_id),
                &observability,
            )
            .await;
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
) -> Result<()> {
    let context = twitch
        .fetch_channel_points_context(&streamer.username)
        .await
        .with_context(|| format!("fetch context for {}", streamer.username))?;
    runtime
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
            let message = format!(
                "Claimed bonus for {}",
                observability.streamer_label(streamer)
            );
            tracing::info!(claim_id = %claim_id, "{message}");
            observability
                .send_event(DiscordEvent::BonusClaim, &message)
                .await;
        }
    }
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
        'outer: loop {
            if *stop.borrow() {
                break;
            }

            let now = time_now();
            let snapshot = match runtime.state_snapshot().await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(%error, "minute watcher snapshot failed");
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

                if let Err(error) = send_minute_watched_for_streamer(
                    &runtime,
                    &twitch,
                    &spade_urls,
                    &streamer,
                    &user_id,
                    &observability,
                )
                .await
                {
                    tracing::warn!(streamer = %streamer.username, %error, "minute watched failed");
                }

                if sleep_or_stop(&mut stop, interval).await {
                    break 'outer;
                }
            }
        }
    })
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

struct TracingChatLogger {
    observability: AppObservability,
}

impl ChatLogger for TracingChatLogger {
    fn printf(&mut self, message: &str) {
        tracing::info!("{message}");
    }

    fn errorf(&mut self, message: &str) {
        tracing::error!("{message}");
    }

    fn emoji_eventf(&mut self, _emoji: &str, event: ChatEventKind, message: &str) {
        tracing::info!("{message}");
        if matches!(event, ChatEventKind::Mention) {
            self.observability
                .spawn_event(DiscordEvent::ChatMention, message.to_string());
        }
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

fn should_fallback_to_user_config(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(READ_ONLY_FILE_SYSTEM_ERROR)
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

fn log_session_summary(summary: &tm_runtime::SessionSummary) {
    tracing::info!(duration = %summary.duration, "session ended");
    tracing::info!("{}", summary.total_points_line);
    for streamer in &summary.streamers {
        tracing::info!(
            "{} ({} points), {}",
            streamer.username,
            streamer.current_points,
            streamer.total_points_line
        );
        for line in &streamer.history_lines {
            tracing::info!("{:width$}{}", "", line, width = SESSION_SUMMARY_INDENT);
        }
    }
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
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use tm_auth::AuthEndpoints;
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
            "🥳 alice (1.25k points) is Online!"
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
                    let spade_url = spade_url.to_string();
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
