use std::path::Path;

use anyhow::{anyhow, Result};
use tm_config::ConfigFile;
use tm_domain::Streamer;
use tm_pubsub::{
    build_topic_batches_with_policy, EventSubClient, EventSubClientSettings,
    EventSubConnectionEvent, EventSubSetupReport, PubSubClient, PubSubConnectionEvent,
    TransportSourcePolicy,
};
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

#[allow(clippy::too_many_lines)]
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
    let prediction_eventsub_authorized = prediction_eventsub_authorized(&session);

    let own_channel_id = canary_step(
        "own-channel-id",
        twitch.fetch_channel_id(&config.username).await,
    )?;
    let target = canary_target(config);
    let target_channel_id =
        canary_step("target-channel-id", twitch.fetch_channel_id(target).await)?;
    let _ = canary_step(
        "target-login-by-id",
        twitch.fetch_channel_login_by_id(&target_channel_id).await,
    )?;
    let base_settings = tm_config::build_base_streamer_settings(config);
    let override_settings =
        tm_config::build_override_settings(&base_settings, &config.streamer_overrides);

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
        "watch-streak-reward-list",
        twitch
            .fetch_watch_streak_achievement(&target_channel_id)
            .await,
    )?;
    let _ = canary_step(
        "archived-videos",
        twitch.fetch_recent_archived_videos(target).await,
    )?;
    let _ = canary_step("recent-clips", twitch.fetch_recent_clips(target).await)?;
    let target_is_live = canary_step(
        "stream-live",
        twitch.is_stream_live(&target_channel_id).await,
    )?;
    // The overlay operation validly returns `stream: null` while a channel is
    // offline. Match the runtime path and validate its live-only fields only
    // when Twitch reports that the target is currently live.
    let stream_info_checked = canary_step(
        "stream-info",
        fetch_stream_info_if_live(&twitch, target, target_is_live).await,
    )?;
    let playback_checked = canary_step(
        "playback-preflight",
        prime_playback_if_live(&twitch, target, target_is_live).await,
    )?;
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
    let tracked = resolve_canary_streamers(
        config,
        &twitch,
        target,
        &target_channel_id,
        &base_settings,
        &override_settings,
    )
    .await?;
    canary_step(
        "eventsub",
        run_eventsub_canary(
            &twitch,
            &own_channel_id,
            prediction_eventsub_authorized,
            &tracked,
        )
        .await,
    )?;
    canary_step(
        "pubsub",
        run_pubsub_canary(&twitch, &own_channel_id, &tracked).await,
    )?;

    tracing::info!(
        read_operations = if stream_info_checked { 15 } else { 13 },
        stream_info_applicable = stream_info_checked,
        playback_preflight_applicable = playback_checked,
        own_channel_id_present = !own_channel_id.is_empty(),
        "credential-safe Twitch canary passed"
    );
    Ok(())
}

fn prediction_eventsub_authorized(session: &tm_auth::AuthSession) -> bool {
    session.has_any_scope(&["channel:read:predictions", "channel:manage:predictions"])
}

fn canary_target(config: &ConfigFile) -> &str {
    config
        .streamers
        .first()
        .map_or(config.username.as_str(), String::as_str)
}

async fn resolve_canary_streamers(
    config: &ConfigFile,
    twitch: &TwitchClient,
    primary_login: &str,
    primary_channel_id: &str,
    base_settings: &tm_domain::StreamerSettings,
    override_settings: &std::collections::HashMap<String, tm_domain::StreamerSettings>,
) -> std::result::Result<Vec<Streamer>, CanaryCheckError> {
    let targets = if config.streamers.is_empty() {
        vec![config.username.clone()]
    } else {
        config.streamers.clone()
    };
    let mut tracked = Vec::with_capacity(targets.len());
    let mut seen = std::collections::HashSet::new();
    for login in targets {
        let normalized = login.trim().to_lowercase();
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        let channel_id = if normalized == primary_login.trim().to_lowercase() {
            primary_channel_id.to_string()
        } else {
            canary_step("tracked-channel-id", twitch.fetch_channel_id(&login).await)?
        };
        tracked.push(Streamer {
            username: login,
            channel_id,
            settings: override_settings
                .get(&normalized)
                .cloned()
                .unwrap_or_else(|| base_settings.clone()),
            ..Streamer::default()
        });
    }
    if tracked.is_empty() {
        return Err(CanaryCheckError::new(
            "tracked-streamers",
            anyhow!("canary has no valid tracked streamers"),
        ));
    }
    Ok(tracked)
}

async fn fetch_stream_info_if_live(
    twitch: &TwitchClient,
    target: &str,
    target_is_live: bool,
) -> std::result::Result<bool, TwitchClientError> {
    if !target_is_live {
        return Ok(false);
    }
    let _ = twitch.fetch_stream_info(target).await?;
    Ok(true)
}

async fn prime_playback_if_live(
    twitch: &TwitchClient,
    target: &str,
    target_is_live: bool,
) -> std::result::Result<bool, TwitchClientError> {
    if !target_is_live {
        return Ok(false);
    }
    twitch.prime_live_playback(target).await?;
    Ok(true)
}

fn canary_failure_message(error: &CanaryCheckError) -> String {
    format!("{}:{}", error.stage, canary_failure_class(&error.source))
}

async fn run_eventsub_canary(
    twitch: &TwitchClient,
    own_channel_id: &str,
    prediction_eventsub_authorized: bool,
    tracked: &[Streamer],
) -> Result<()> {
    let client = EventSubClient::new(EventSubClientSettings {
        client_id: tm_twitch::CLIENT_ID.to_string(),
        auth_token: twitch.auth_token().to_string(),
        websocket_url: tm_pubsub::EVENTSUB_WEBSOCKET_URL.to_string(),
        subscriptions_url: tm_pubsub::EVENTSUB_SUBSCRIPTIONS_URL.to_string(),
        allow_prediction_scope_fallback: false,
        source_policy: TransportSourcePolicy::viewer_compatibility(),
        authorized_prediction_broadcaster_id: prediction_eventsub_authorized
            .then(|| own_channel_id.to_string()),
        verify_subscriptions: true,
        http_client: reqwest::Client::new(),
    });
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let tracked = tracked.to_vec();
    let task = tokio::spawn(async move { client.connect_and_listen(&tracked, sender).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut setup_verified = false;
        while let Some(message) = receiver.recv().await {
            match message {
                EventSubConnectionEvent::Setup(report) => {
                    validate_eventsub_canary_report(&report)?;
                    setup_verified = true;
                }
                EventSubConnectionEvent::Heartbeat if setup_verified => {
                    return Ok::<(), anyhow::Error>(());
                }
                EventSubConnectionEvent::Heartbeat | EventSubConnectionEvent::Event(_) => {}
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

fn validate_eventsub_canary_report(report: &EventSubSetupReport) -> Result<()> {
    if report.planned_subscriptions == 0
        || report.active_subscriptions != report.planned_subscriptions
        || report.failed_subscriptions != 0
        || !report.verified
        || report.capabilities.iter().any(|capability| {
            capability.active_subscription_types != capability.planned_subscription_types
        })
    {
        return Err(anyhow!("EventSub subscription verification failed"));
    }
    Ok(())
}

async fn run_pubsub_canary(
    twitch: &TwitchClient,
    user_id: &str,
    tracked: &[Streamer],
) -> Result<()> {
    let topic_batches = build_topic_batches_with_policy(
        user_id,
        tracked,
        TransportSourcePolicy::viewer_compatibility(),
    )?;
    if topic_batches.is_empty() {
        return Err(anyhow!("PubSub canary has no configured topics"));
    }

    let expected_acknowledgements = topic_batches.iter().map(Vec::len).sum::<usize>();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(32);
    let auth_token = twitch.auth_token().to_string();
    let mut tasks = tokio::task::JoinSet::new();
    for topics in topic_batches {
        let sender = sender.clone();
        let auth_token = auth_token.clone();
        let tracked = tracked.to_vec();
        tasks.spawn(async move {
            PubSubClient::default()
                .connect_topics_and_listen(&topics, &auth_token, None, &tracked, sender)
                .await
        });
    }
    drop(sender);
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut acknowledged = 0_usize;
        loop {
            tokio::select! {
                message = receiver.recv() => match message {
                    Some(PubSubConnectionEvent::ListenAcknowledged { .. }) => {
                        acknowledged += 1;
                        if acknowledged == expected_acknowledgements {
                            return Ok(());
                        }
                    }
                    Some(PubSubConnectionEvent::ResponseError { .. }) => {
                        return Err(anyhow!("PubSub LISTEN rejected"));
                    }
                    Some(PubSubConnectionEvent::Heartbeat | PubSubConnectionEvent::Event(_)) => {}
                    None => return Err(anyhow!("PubSub closed before LISTEN confirmation")),
                },
                result = tasks.join_next() => {
                    return match result {
                        Some(Ok(Ok(()))) => Err(anyhow!("PubSub closed before LISTEN confirmation")),
                        Some(Ok(Err(_))) => Err(anyhow!("PubSub connection failed")),
                        Some(Err(_)) => Err(anyhow!("PubSub connection task failed")),
                        None => Err(anyhow!("PubSub connections ended before LISTEN confirmation")),
                    };
                }
            }
        }
    })
    .await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow!("PubSub LISTEN timeout")),
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

    #[tokio::test]
    async fn offline_to_live_race_does_not_require_live_only_stream_info() {
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::new(),
            "token",
            "ua",
            tm_twitch::TwitchEndpoints {
                twitch_url: String::from("http://127.0.0.1:9"),
                gql_url: String::from("http://127.0.0.1:9"),
                ..tm_twitch::TwitchEndpoints::default()
            },
        );

        assert!(matches!(
            fetch_stream_info_if_live(&twitch, "fixture", false).await,
            Ok(false)
        ));
        assert!(matches!(
            prime_playback_if_live(&twitch, "fixture", false).await,
            Ok(false)
        ));
    }

    #[test]
    fn eventsub_canary_requires_complete_verified_setup() {
        let capability = tm_pubsub::EventSubStreamerCapability {
            streamer_index: 0,
            presence_source: String::from("eventsub+gql-polling"),
            prediction_source: String::from("pubsub-compatibility"),
            raid_source: String::from("disabled"),
            planned_subscription_types: vec![String::from("stream.online")],
            active_subscription_types: vec![String::from("stream.online")],
            skipped_subscription_types: Vec::new(),
            failure_class: None,
        };
        let mut report = EventSubSetupReport {
            planned_subscriptions: 1,
            active_subscriptions: 1,
            failed_subscriptions: 0,
            overflow_streamers: 0,
            total_cost: 1,
            max_total_cost: 10,
            verified: true,
            capabilities: vec![capability],
        };

        assert!(validate_eventsub_canary_report(&report).is_ok());
        report.failed_subscriptions = 1;
        assert!(validate_eventsub_canary_report(&report).is_err());
        report.failed_subscriptions = 0;
        report.verified = false;
        assert!(validate_eventsub_canary_report(&report).is_err());
    }
}
