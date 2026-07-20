use std::sync::Arc;
use std::time::{Duration, Instant};

use tm_pubsub::{EventSubClient, EventSubClientSettings, EventSubConnectionEvent, EventSubError};
use tm_twitch::{TwitchClient, TwitchFailureClass};

use crate::observability::AppObservability;
use crate::runtime_effects::execute_runtime_effects;
use crate::status::HealthTracker;
use crate::utilities::time_now;

const EVENTSUB_RECONNECT_BASE_SECONDS: u64 = 5;
const EVENTSUB_RECONNECT_MAX_SECONDS: u64 = 5 * 60;
const PRESENCE_POLL_INTERVAL: Duration = Duration::from_secs(60);
const PRESENCE_POLL_CONCURRENCY: usize = 4;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn spawn_eventsub_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    auth_token: String,
    tracked_streamers: Vec<tm_domain::Streamer>,
    persistent_user_id: String,
    prediction_eventsub_authorized: bool,
    observability: AppObservability,
    health: HealthTracker,
    fallback_tx: tokio::sync::watch::Sender<Vec<usize>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        let mut failure_attempt = 0_u32;
        loop {
            if *stop.borrow() {
                break;
            }
            let _ = fallback_tx.send((0..tracked_streamers.len()).collect());

            let client = EventSubClient::new(EventSubClientSettings {
                client_id: tm_twitch::CLIENT_ID.to_string(),
                auth_token: auth_token.clone(),
                websocket_url: tm_pubsub::EVENTSUB_WEBSOCKET_URL.to_string(),
                subscriptions_url: tm_pubsub::EVENTSUB_SUBSCRIPTIONS_URL.to_string(),
                allow_prediction_scope_fallback: true,
                source_policy: tm_pubsub::TransportSourcePolicy::viewer_compatibility(),
                authorized_prediction_broadcaster_id: prediction_eventsub_authorized
                    .then(|| persistent_user_id.clone()),
                verify_subscriptions: false,
                http_client: reqwest::Client::new(),
            });
            let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
            let connect = tokio::spawn({
                let tracked_streamers = tracked_streamers.clone();
                async move { client.connect_and_listen(&tracked_streamers, sender).await }
            });
            tokio::pin!(connect);
            let connection_result = loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            connect.as_mut().abort();
                            let _ = connect.as_mut().await;
                            return;
                        }
                    }
                    message = receiver.recv() => {
                        let Some(message) = message else {
                            continue;
                        };
                        if matches!(&message, EventSubConnectionEvent::Heartbeat) {
                            failure_attempt = 0;
                        }
                        update_presence_fallback(&fallback_tx, &message);
                        if handle_eventsub_message(
                            &runtime,
                            &twitch,
                            &persistent_user_id,
                            &observability,
                            &health,
                            message,
                        ).await {
                            connect.as_mut().abort();
                            let _ = connect.await;
                            return;
                        }
                    }
                    result = &mut connect => {
                        break result;
                    }
                }
            };
            while let Ok(message) = receiver.try_recv() {
                update_presence_fallback(&fallback_tx, &message);
                if handle_eventsub_message(
                    &runtime,
                    &twitch,
                    &persistent_user_id,
                    &observability,
                    &health,
                    message,
                )
                .await
                {
                    return;
                }
            }
            match connection_result {
                Ok(Ok(())) => {
                    failure_attempt = failure_attempt.saturating_add(1);
                    health.failure("eventsub", "connection-closed");
                    tracing::warn!(
                        task = "eventsub",
                        error_class = "connection-closed",
                        failure_attempt,
                        "EventSub connection closed; reconnecting"
                    );
                }
                Ok(Err(EventSubError::Revoked { .. })) => {
                    failure_attempt = failure_attempt.saturating_add(1);
                    health.failure("eventsub", "revoked");
                    tracing::error!(
                        task = "eventsub",
                        error_class = "revoked",
                        "EventSub subscription revoked; retrying after backoff"
                    );
                }
                Ok(Err(error)) => {
                    failure_attempt = failure_attempt.saturating_add(1);
                    health.failure("eventsub", classify_eventsub_error(&error));
                    tracing::warn!(
                        task = "eventsub",
                        error_class = classify_eventsub_error(&error),
                        failure_attempt,
                        "EventSub connection failed; reconnecting"
                    );
                }
                Err(error) if error.is_cancelled() => return,
                Err(error) => {
                    failure_attempt = failure_attempt.saturating_add(1);
                    health.failure("eventsub", "connection-task");
                    tracing::error!(
                        task = "eventsub",
                        error_class = "connection-task",
                        failure_attempt,
                        %error,
                        "EventSub task failed; reconnecting"
                    );
                }
            }

            let delay = eventsub_reconnect_delay(failure_attempt);
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                () = tokio::time::sleep(delay) => {}
            }
        }
    })
}

pub(crate) fn spawn_eventsub_presence_poll_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    tracked_streamers: Vec<tm_domain::Streamer>,
    fallback_rx: tokio::sync::watch::Receiver<Vec<usize>>,
    observability: AppObservability,
    health: HealthTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        let mut fallback_rx = fallback_rx;
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + PRESENCE_POLL_INTERVAL,
            PRESENCE_POLL_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                changed = fallback_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    let indices = fallback_rx.borrow().clone();
                    poll_presence_fallback(
                        &runtime,
                        &twitch,
                        &tracked_streamers,
                        &indices,
                        &observability,
                        &health,
                    ).await;
                }
            }
        }
    })
}

fn update_presence_fallback(
    fallback_tx: &tokio::sync::watch::Sender<Vec<usize>>,
    message: &EventSubConnectionEvent,
) {
    let EventSubConnectionEvent::Setup(report) = message else {
        return;
    };
    let indices = report
        .capabilities
        .iter()
        .filter(|capability| capability.presence_source == "gql-polling")
        .map(|capability| capability.streamer_index)
        .collect();
    let _ = fallback_tx.send(indices);
}

async fn poll_presence_fallback(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    tracked_streamers: &[tm_domain::Streamer],
    indices: &[usize],
    observability: &AppObservability,
    health: &HealthTracker,
) {
    let mut queries = tokio::task::JoinSet::new();
    let mut next = 0_usize;
    let mut failure_class = None;
    while next < indices.len() || !queries.is_empty() {
        while next < indices.len() && queries.len() < PRESENCE_POLL_CONCURRENCY {
            let streamer_index = indices[next];
            next += 1;
            let Some(streamer) = tracked_streamers.get(streamer_index) else {
                failure_class.get_or_insert("missing-streamer");
                continue;
            };
            let channel_id = streamer.channel_id.clone();
            let twitch = Arc::clone(twitch);
            queries.spawn(async move {
                let result = twitch.is_stream_live(&channel_id).await;
                (streamer_index, channel_id, result)
            });
        }

        let Some(result) = queries.join_next().await else {
            continue;
        };
        match result {
            Ok((streamer_index, channel_id, Ok(online))) => {
                match runtime
                    .set_presence_if_changed(&channel_id, online, time_now())
                    .await
                {
                    Ok(true) => {
                        let event = tm_pubsub::PubSubEvent::Playback {
                            channel_id,
                            kind: if online {
                                tm_pubsub::PlaybackType::StreamUp
                            } else {
                                tm_pubsub::PlaybackType::StreamDown
                            },
                        };
                        if let Err(error) =
                            crate::pubsub::log_pubsub_event(runtime, observability, &event).await
                        {
                            failure_class.get_or_insert("log-handling");
                            tracing::warn!(
                                task = "presence-poll",
                                error_class = "log-handling",
                                streamer_index,
                                %error,
                                "presence polling transition log failed"
                            );
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        failure_class.get_or_insert("state-update");
                        tracing::warn!(
                            task = "presence-poll",
                            error_class = "state-update",
                            streamer_index,
                            %error,
                            "presence polling state update failed"
                        );
                    }
                }
            }
            Ok((streamer_index, _, Err(error))) => {
                let error_class = classify_presence_poll_error(error.failure_class());
                failure_class.get_or_insert(error_class);
                tracing::warn!(
                    task = "presence-poll",
                    error_class,
                    streamer_index,
                    "presence polling request failed"
                );
            }
            Err(error) => {
                failure_class.get_or_insert("poll-task");
                tracing::warn!(
                    task = "presence-poll",
                    error_class = "poll-task",
                    %error,
                    "presence polling task failed"
                );
            }
        }
    }
    match failure_class {
        Some(error_class) => health.failure("presence-poll", error_class),
        None => health.success("presence-poll"),
    }
}

fn classify_presence_poll_error(error: TwitchFailureClass) -> &'static str {
    match error {
        TwitchFailureClass::Unauthorized => "unauthorized",
        TwitchFailureClass::RateLimited => "rate-limited",
        TwitchFailureClass::ServerError => "server-error",
        TwitchFailureClass::Timeout => "timeout",
        TwitchFailureClass::ConnectionReset => "connection-reset",
        TwitchFailureClass::Other => "contract-or-shape",
    }
}

async fn handle_eventsub_message(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    observability: &AppObservability,
    health: &HealthTracker,
    message: EventSubConnectionEvent,
) -> bool {
    match message {
        EventSubConnectionEvent::Setup(report) => {
            health.record_eventsub_setup(*report);
            health.success("eventsub");
        }
        EventSubConnectionEvent::Heartbeat => health.success("eventsub"),
        EventSubConnectionEvent::Event(event) => {
            let log_event = (*event).clone();
            let received_at = Instant::now();
            match runtime.apply_event_with_outcome(*event, time_now()).await {
                Ok(application) => {
                    runtime
                        .metrics_handle()
                        .record_transport_latency(received_at.elapsed());
                    health.success("eventsub");
                    if application.changed {
                        if let Err(error) =
                            crate::pubsub::log_pubsub_event(runtime, observability, &log_event)
                                .await
                        {
                            tracing::warn!(
                                task = "eventsub",
                                error_class = "log-handling",
                                %error,
                                "EventSub log handling failed"
                            );
                        }
                    }
                    if let Err(error) = execute_runtime_effects(
                        runtime,
                        twitch,
                        persistent_user_id,
                        application.effects,
                        observability,
                        health.clone(),
                    )
                    .await
                    {
                        tracing::warn!(
                            task = "eventsub",
                            error_class = "runtime-effect",
                            %error,
                            "EventSub runtime effect execution failed"
                        );
                    }
                }
                Err(error) => {
                    health.failure("eventsub", "event-application");
                    tracing::warn!(
                        task = "eventsub",
                        error_class = "event-application",
                        %error,
                        "EventSub event application failed"
                    );
                }
            }
        }
    }
    false
}

fn classify_eventsub_error(error: &EventSubError) -> &'static str {
    match error {
        EventSubError::HttpStatus { status, .. } if matches!(status.as_u16(), 401 | 403) => {
            "unauthorized"
        }
        EventSubError::HttpStatus { status, .. } if status.as_u16() == 429 => "rate-limited",
        EventSubError::HttpStatus { status, .. } if status.is_server_error() => "server-error",
        EventSubError::HttpStatus { .. } => "http-status",
        EventSubError::Http(_) => "http-error",
        EventSubError::WebSocket(_) => "connection-reset",
        EventSubError::Timeout(_) => "timeout",
        EventSubError::Revoked { .. } => "revoked",
        EventSubError::ReconnectRequested { .. } => "reconnect",
        EventSubError::Json(_) | EventSubError::Protocol(_) | EventSubError::Timestamp => {
            "protocol"
        }
        EventSubError::NoSubscriptions => "no-subscriptions",
    }
}

fn eventsub_reconnect_delay(failure_attempt: u32) -> Duration {
    let exponent = failure_attempt.saturating_sub(1).min(6);
    let seconds = EVENTSUB_RECONNECT_BASE_SECONDS
        .saturating_mul(1_u64 << exponent)
        .min(EVENTSUB_RECONNECT_MAX_SECONDS);
    Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        classify_eventsub_error, eventsub_reconnect_delay, poll_presence_fallback,
        update_presence_fallback,
    };
    use reqwest::StatusCode;
    use tm_domain::Streamer;
    use tm_observability::DiscordClient;
    use tm_pubsub::{
        EventSubConnectionEvent, EventSubError, EventSubSetupReport, EventSubStreamerCapability,
    };
    use tm_twitch::{TwitchClient, TwitchEndpoints};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::observability::AppObservability;
    use crate::status::HealthTracker;

    fn test_observability() -> anyhow::Result<AppObservability> {
        Ok(AppObservability::new(
            None,
            DiscordClient::new(Duration::from_secs(1))?,
            false,
            false,
            false,
            true,
        ))
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> anyhow::Result<()> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).await?;
            assert!(read > 0, "HTTP request ended before its body was complete");
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length: ")
                        .or_else(|| line.strip_prefix("Content-Length: "))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or_default();
            if request.len() >= header_end + 4 + content_length {
                return Ok(());
            }
        }
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        assert_eq!(eventsub_reconnect_delay(1).as_secs(), 5);
        assert_eq!(eventsub_reconnect_delay(20).as_secs(), 300);
    }

    #[test]
    fn errors_are_classified_without_payloads() {
        let error = EventSubError::HttpStatus {
            status: StatusCode::TOO_MANY_REQUESTS,
            context: "test",
        };
        assert_eq!(classify_eventsub_error(&error), "rate-limited");
        assert_eq!(
            classify_eventsub_error(&EventSubError::Timeout("test")),
            "timeout"
        );
    }

    #[test]
    fn setup_report_updates_only_polling_fallback_indices() {
        let (sender, receiver) = tokio::sync::watch::channel(Vec::new());
        let message = EventSubConnectionEvent::Setup(Box::new(EventSubSetupReport {
            planned_subscriptions: 2,
            active_subscriptions: 2,
            failed_subscriptions: 0,
            overflow_streamers: 1,
            total_cost: 2,
            max_total_cost: 10,
            verified: false,
            capabilities: vec![
                EventSubStreamerCapability {
                    streamer_index: 0,
                    presence_source: String::from("eventsub+gql-polling"),
                    prediction_source: String::from("disabled"),
                    raid_source: String::from("disabled"),
                    planned_subscription_types: Vec::new(),
                    active_subscription_types: Vec::new(),
                    skipped_subscription_types: Vec::new(),
                    failure_class: None,
                },
                EventSubStreamerCapability {
                    streamer_index: 1,
                    presence_source: String::from("gql-polling"),
                    prediction_source: String::from("pubsub-compatibility"),
                    raid_source: String::from("disabled"),
                    planned_subscription_types: Vec::new(),
                    active_subscription_types: Vec::new(),
                    skipped_subscription_types: Vec::new(),
                    failure_class: Some(String::from("capacity-overflow")),
                },
            ],
        }));

        update_presence_fallback(&sender, &message);

        assert_eq!(*receiver.borrow(), vec![1]);
    }

    #[tokio::test]
    async fn presence_fallback_poll_applies_live_transition_once() -> anyhow::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await?;
                read_http_request(&mut stream).await?;
                let body = if index == 0 {
                    r#"<!doctype html><script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#
                } else {
                    r#"{"data":{"user":{"stream":{"id":"stream-1"}}}}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
            }
            Ok::<(), anyhow::Error>(())
        });
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?,
            "token",
            "ua",
            TwitchEndpoints {
                twitch_url: format!("http://{address}"),
                gql_url: format!("http://{address}/gql"),
                ..TwitchEndpoints::default()
            },
        ));
        let config = tm_config::ConfigFile {
            streamers: vec![String::from("tester")],
            ..tm_config::ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(
            &config,
            &config.streamers,
            tm_domain::OffsetDateTime::UNIX_EPOCH,
        );
        state.streamers[0] = Streamer {
            username: String::from("tester"),
            channel_id: String::from("100"),
            ..Streamer::default()
        };
        let runtime = tm_runtime::spawn_runtime_state(state);
        let health = HealthTracker::default();
        health.register("presence-poll", Duration::from_secs(60));

        assert!(twitch.is_stream_live("100").await?);

        poll_presence_fallback(
            &runtime,
            &twitch,
            &runtime.state_snapshot().await?.streamers,
            &[0],
            &test_observability()?,
            &health,
        )
        .await;

        assert!(runtime.state_snapshot().await?.streamers[0].is_online);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn presence_fallback_counts_one_failed_cycle_not_each_streamer() -> anyhow::Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let server: tokio::task::JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(error) => return Err(error.into()),
                };
                read_http_request(&mut stream).await?;
                stream
                    .write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
            }
        });
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?,
            "token",
            "ua",
            TwitchEndpoints {
                twitch_url: format!("http://{address}"),
                gql_url: format!("http://{address}/gql"),
                ..TwitchEndpoints::default()
            },
        ));
        let tracked_streamers = (0..9)
            .map(|index| Streamer {
                username: format!("streamer-{index}"),
                channel_id: format!("channel-{index}"),
                ..Streamer::default()
            })
            .collect::<Vec<_>>();
        let runtime = tm_runtime::spawn_runtime_state(tm_runtime::RuntimeState::from_targets(
            &tm_config::ConfigFile::default(),
            &[],
            tm_domain::OffsetDateTime::UNIX_EPOCH,
        ));
        let health = HealthTracker::default();
        health.register("presence-poll", Duration::from_secs(60));

        poll_presence_fallback(
            &runtime,
            &twitch,
            &tracked_streamers,
            &(0..tracked_streamers.len()).collect::<Vec<_>>(),
            &test_observability()?,
            &health,
        )
        .await;

        assert_eq!(health.task_consecutive_failures("presence-poll"), Some(1));
        server.abort();
        Ok(())
    }
}
