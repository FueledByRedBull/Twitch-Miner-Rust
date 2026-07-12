use std::sync::Arc;
use std::time::{Duration, Instant};

use tm_pubsub::{EventSubClient, EventSubClientSettings, EventSubConnectionEvent, EventSubError};
use tm_twitch::TwitchClient;

use crate::observability::AppObservability;
use crate::runtime_effects::execute_runtime_effects;
use crate::status::HealthTracker;
use crate::utilities::time_now;

const EVENTSUB_RECONNECT_BASE_SECONDS: u64 = 5;
const EVENTSUB_RECONNECT_MAX_SECONDS: u64 = 5 * 60;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn spawn_eventsub_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    auth_token: String,
    tracked_streamers: Vec<tm_domain::Streamer>,
    persistent_user_id: String,
    observability: AppObservability,
    health: HealthTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut stop = stop;
        let mut failure_attempt = 0_u32;
        loop {
            if *stop.borrow() {
                break;
            }

            let client = EventSubClient::new(EventSubClientSettings {
                client_id: tm_twitch::CLIENT_ID.to_string(),
                auth_token: auth_token.clone(),
                websocket_url: tm_pubsub::EVENTSUB_WEBSOCKET_URL.to_string(),
                subscriptions_url: tm_pubsub::EVENTSUB_SUBSCRIPTIONS_URL.to_string(),
                allow_prediction_scope_fallback: true,
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
                Ok(Err(EventSubError::Revoked { reason })) => {
                    failure_attempt = failure_attempt.saturating_add(1);
                    health.failure("eventsub", "revoked");
                    tracing::error!(
                        task = "eventsub",
                        error_class = "revoked",
                        reason = %reason,
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
                        %error,
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

async fn handle_eventsub_message(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    observability: &AppObservability,
    health: &HealthTracker,
    message: EventSubConnectionEvent,
) -> bool {
    match message {
        EventSubConnectionEvent::Heartbeat => health.success("eventsub"),
        EventSubConnectionEvent::Event(event) => {
            let log_event = (*event).clone();
            let received_at = Instant::now();
            match runtime.apply_event(*event, time_now()).await {
                Ok(effects) => {
                    runtime
                        .metrics_handle()
                        .record_transport_latency(received_at.elapsed());
                    health.success("eventsub");
                    if let Err(error) =
                        crate::pubsub::log_pubsub_event(runtime, observability, &log_event).await
                    {
                        tracing::warn!(
                            task = "eventsub",
                            error_class = "log-handling",
                            %error,
                            "EventSub log handling failed"
                        );
                    }
                    if let Err(error) = execute_runtime_effects(
                        runtime,
                        twitch,
                        persistent_user_id,
                        effects,
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
    use super::{classify_eventsub_error, eventsub_reconnect_delay};
    use reqwest::StatusCode;
    use tm_pubsub::EventSubError;

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
}
