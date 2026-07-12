use std::path::Path;

use anyhow::{anyhow, Result};
use tm_config::ConfigFile;
use tm_domain::{Streamer, StreamerSettings};
use tm_pubsub::{EventSubClient, EventSubClientSettings, EventSubConnectionEvent};
use tm_twitch::{TwitchClient, TwitchClientError, TwitchFailureClass};

use crate::bootstrap::{load_and_validate_existing_session, DEFAULT_USER_AGENT};

#[derive(Debug)]
struct CanaryCheckError {
    stage: &'static str,
    source: anyhow::Error,
}

impl CanaryCheckError {
    fn new(stage: &'static str, source: impl Into<anyhow::Error>) -> Self {
        Self {
            stage,
            source: source.into(),
        }
    }
}

fn canary_step<T, E>(
    stage: &'static str,
    result: std::result::Result<T, E>,
) -> std::result::Result<T, CanaryCheckError>
where
    E: Into<anyhow::Error>,
{
    result.map_err(|error| CanaryCheckError::new(stage, error))
}

pub(crate) async fn run_read_only_canary(
    config: &ConfigFile,
    work_dir: &Path,
    http_client: reqwest::Client,
) -> Result<()> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(90),
        run_read_only_canary_inner(config, work_dir, http_client),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!("canary failed: {}", canary_failure_message(&error))),
        Err(_) => Err(anyhow!("canary failed: overall:timeout")),
    }
}

async fn run_read_only_canary_inner(
    config: &ConfigFile,
    work_dir: &Path,
    http_client: reqwest::Client,
) -> std::result::Result<(), CanaryCheckError> {
    let session = canary_step(
        "session",
        load_and_validate_existing_session(config, work_dir, http_client.clone()).await,
    )?;
    let auth_token = session.auth_token().ok_or_else(|| {
        CanaryCheckError::new(
            "session-token",
            anyhow!("validated canary session has no auth token"),
        )
    })?;
    let twitch = TwitchClient::with_client_and_cookie_header(
        http_client,
        auth_token,
        DEFAULT_USER_AGENT,
        session.cookie_header_for_host("twitch.tv"),
    );

    let own_channel_id = canary_step(
        "own-channel-id",
        twitch.fetch_channel_id(&config.username).await,
    )?;
    let target = config
        .streamers
        .first()
        .map_or(config.username.as_str(), String::as_str);
    let target_channel_id =
        canary_step("target-channel-id", twitch.fetch_channel_id(target).await)?;
    let base_settings = tm_config::build_base_streamer_settings(config);
    let override_settings =
        tm_config::build_override_settings(&base_settings, &config.streamer_overrides);
    let target_settings = override_settings
        .get(&target.trim().to_lowercase())
        .unwrap_or(&base_settings);

    let _ = canary_step(
        "followers",
        twitch
            .fetch_followers(1, config.followers_order.as_str())
            .await,
    )?;
    let _ = canary_step(
        "channel-points-context",
        twitch.fetch_channel_points_context(target).await,
    )?;
    let _ = canary_step(
        "stream-live",
        twitch.is_stream_live(&target_channel_id).await,
    )?;
    let _ = canary_step("stream-info", twitch.fetch_stream_info(target).await)?;
    let _ = canary_step("inventory", twitch.fetch_inventory_typed().await)?;
    let _ = canary_step(
        "drops-dashboard",
        twitch.fetch_viewer_drops_dashboard_typed().await,
    )?;
    let _ = canary_step(
        "available-drops",
        twitch
            .fetch_available_drop_campaigns_typed(&target_channel_id)
            .await,
    )?;
    let _ = canary_step(
        "points-contribution",
        twitch.fetch_user_points_contribution_typed(target).await,
    )?;
    canary_step(
        "eventsub",
        run_eventsub_canary(&twitch, target_channel_id, target_settings.make_predictions).await,
    )?;

    tracing::info!(
        read_operations = 10,
        own_channel_id_present = !own_channel_id.is_empty(),
        "credential-safe Twitch canary passed"
    );
    Ok(())
}

fn canary_failure_message(error: &CanaryCheckError) -> String {
    format!("{}:{}", error.stage, canary_failure_class(&error.source))
}

async fn run_eventsub_canary(
    twitch: &TwitchClient,
    target_channel_id: String,
    make_predictions: bool,
) -> Result<()> {
    let tracked = [Streamer {
        channel_id: target_channel_id,
        settings: StreamerSettings {
            make_predictions,
            ..StreamerSettings::default()
        },
        ..Streamer::default()
    }];
    let client = EventSubClient::new(EventSubClientSettings {
        client_id: tm_twitch::CLIENT_ID.to_string(),
        auth_token: twitch.auth_token().to_string(),
        websocket_url: tm_pubsub::EVENTSUB_WEBSOCKET_URL.to_string(),
        subscriptions_url: tm_pubsub::EVENTSUB_SUBSCRIPTIONS_URL.to_string(),
        allow_prediction_scope_fallback: false,
        http_client: reqwest::Client::new(),
    });
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(async move { client.connect_and_listen(&tracked, sender).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Some(message) = receiver.recv().await {
            if matches!(message, EventSubConnectionEvent::Heartbeat) {
                return Ok::<(), anyhow::Error>(());
            }
        }
        Err(anyhow!(
            "EventSub closed before welcome/subscription confirmation"
        ))
    })
    .await;
    task.abort();
    let _ = task.await;
    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow!("EventSub handshake timeout")),
    }
}

#[allow(clippy::redundant_closure_for_method_calls)]
fn canary_failure_class(error: &anyhow::Error) -> &'static str {
    if let Some(error) = error.downcast_ref::<TwitchClientError>() {
        return match error.failure_class() {
            TwitchFailureClass::Unauthorized => "unauthorized",
            TwitchFailureClass::RateLimited => "rate-limited",
            TwitchFailureClass::ServerError => "server-error",
            TwitchFailureClass::Timeout => "timeout",
            TwitchFailureClass::ConnectionReset => "connection-reset",
            TwitchFailureClass::Other => "contract-or-shape",
        };
    }
    if error.chain().any(|cause| cause.is::<reqwest::Error>()) {
        return "network";
    }
    if error
        .chain()
        .any(|cause| cause.is::<tm_auth::AuthClientError>())
    {
        return "auth";
    }
    "canary-check"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canary_failure_message_keeps_only_fixed_stage_and_failure_class() {
        let error = CanaryCheckError::new(
            "inventory",
            TwitchClientError::MissingField("sensitive-response-field"),
        );

        let message = canary_failure_message(&error);

        assert_eq!(message, "inventory:contract-or-shape");
        assert!(!message.contains("sensitive-response-field"));
    }
}
