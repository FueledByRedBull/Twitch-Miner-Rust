#![warn(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use anyhow::{anyhow, Result};
use clap::Parser;
use tm_config::resolve_app_paths_from_env;
use tm_observability::{init_tracing, Event as DiscordEvent, TracingInitOptions};
use tm_twitch::TwitchClient;

mod bootstrap;
mod canary;
mod chat;
mod context;
mod drops;
mod effects;
mod eventsub;
mod minute_watcher;
mod observability;
mod prediction;
mod pubsub;
mod runtime_effects;
mod shutdown;
mod startup;
mod tasks;
mod utilities;
mod watching;

use bootstrap::{
    build_http_client, has_override, load_config_with_fallback, load_or_login_session,
    log_timezone_validation, prepare_work_dir, validate_timezone_override, LoadedConfig,
    DEFAULT_USER_AGENT,
};
use drops::claim_startup_drops_if_enabled;
use observability::{build_observability, log_session_summary, log_startup};
use shutdown::{shutdown_background_tasks, wait_for_shutdown_or_task_failure};
use startup::{bootstrap_runtime_state, build_logger_settings};
use tasks::{spawn_background_tasks, BackgroundTaskParams, BackgroundTasks};
use utilities::{clear_console, new_session_id, set_console_title, time_now};

mod build_info;
mod status;

const DEFAULT_CONSOLE_TITLE: &str = "Klaro's Twitch Miner";
const CONTEXT_REFRESH_CONCURRENCY: usize = 8;
const WATCH_SELECTION_REFRESH_CONCURRENCY: usize = 4;
const PENDING_CLAIMS_INTERVAL: Duration = Duration::from_secs(5 * 60 * 60);
const SPADE_URL_TTL: Duration = Duration::from_secs(15 * 60);
const MINUTE_WATCHER_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const SHUTDOWN_TASK_GRACE_PERIOD: Duration = Duration::from_secs(5);
const SESSION_SUMMARY_INDENT: usize = 25;

#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)]
#[command(version = build_info::VERSION_BANNER)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long = "data-dir")]
    data_dir: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["status", "check_config", "support_bundle", "canary"])]
    health: bool,
    #[arg(long, conflicts_with_all = ["health", "status", "support_bundle", "canary"])]
    check_config: bool,
    #[arg(long, conflicts_with = "check_config")]
    status: bool,
    #[arg(long, requires = "check_config")]
    json: bool,
    #[arg(long, value_name = "PATH", conflicts_with_all = ["health", "status", "check_config", "canary"])]
    support_bundle: Option<PathBuf>,
    #[arg(long, conflicts_with_all = ["health", "status", "check_config", "support_bundle"])]
    canary: bool,
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let has_override = has_override(&cli);
    let requested_paths = resolve_app_paths_from_env(cli.config, cli.data_dir)?;
    if cli.health {
        return status::check_health(&requested_paths.work_dir);
    }
    if cli.status {
        return status::print_status(&requested_paths.work_dir);
    }
    if cli.check_config {
        let result = tm_config::preview_config(&requested_paths.config_path).and_then(|preview| {
            tm_config::validate_config(&preview.config)?;
            Ok(preview)
        });
        let preview = match result {
            Ok(preview) => preview,
            Err(error) if cli.json => {
                println!(
                    "{}",
                    serde_json::json!({"valid": false, "error": error.to_string()})
                );
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        if cli.json {
            println!(
                "{}",
                serde_json::json!({
                    "valid": true,
                    "schema_version": preview.config.config_schema_version,
                    "migration_required": preview.migration_required
                })
            );
        } else {
            println!(
                "config valid; schema_version={}; migration_required={}",
                preview.config.config_schema_version, preview.migration_required
            );
        }
        return Ok(());
    }
    if let Some(destination) = cli.support_bundle.as_deref() {
        status::write_support_bundle(&requested_paths.work_dir, destination)?;
        println!("support bundle written to {}", destination.display());
        return Ok(());
    }
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

    if cli.canary {
        let http_client = build_http_client(config.disable_ssl_cert_verification)?;
        return canary::run_read_only_canary(&config, &active_paths.work_dir, http_client).await;
    }

    let observability = build_observability(&config)?;
    tracing::info!(
        "{} | {}",
        build_info::DISPLAY_NAME,
        build_info::VERSION_BANNER
    );
    tracing::info!("{}", build_info::REPOSITORY_URL);

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
    let health = status::HealthTracker::default();
    let tasks = spawn_background_tasks(BackgroundTaskParams {
        config: &config,
        stop_rx,
        runtime: &runtime,
        twitch: &twitch,
        auth_token: &auth_token,
        user_id: user_id.as_ref(),
        initial_streamers: &initial_streamers,
        observability: &observability,
        health: &health,
    })?;
    let status =
        status::StatusReporter::ready(&active_paths.work_dir, health, runtime.metrics_handle())?;
    let summary = runtime.runtime_summary().await?;

    log_startup(
        &requested_paths,
        &active_paths,
        &summary,
        &session_id,
        &observability,
    )
    .await;

    let runtime_failure = wait_for_shutdown_or_task_failure(&tasks, &status)
        .await
        .err();
    if let Some(error) = runtime_failure.as_ref() {
        tracing::error!(%error, session_id = %session_id, "runtime supervision requested shutdown");
    } else {
        tracing::info!(session_id = %session_id, "shutdown requested");
    }
    shutdown_background_tasks(stop_tx, tasks).await;
    tracing::info!(session_id = %session_id, "background tasks stopped");
    observability.shutdown_pending_tasks().await;
    let summary = runtime
        .shutdown(config.privacy.anonymize_logs, time_now())
        .await?;
    observability.spawn_event(
        DiscordEvent::Shutdown,
        format!("Ending session: '{session_id}'"),
    );
    observability.shutdown_pending_tasks().await;
    log_session_summary(&summary);
    if let Some(error) = runtime_failure {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod app_tests;
