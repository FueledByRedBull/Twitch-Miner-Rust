use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tm_domain::Streamer;
use tm_observability::{event_from_gain_reason, Event as DiscordEvent};
use tm_pubsub::{build_topic_batches, PubSubClient, PubSubConnectionEvent};
use tm_twitch::TwitchClient;

use crate::observability::AppObservability;
use crate::runtime_effects::execute_runtime_effects;
use crate::status::HealthTracker;
use crate::utilities::time_now;

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_pubsub_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    auth_token: String,
    user_id: String,
    username: String,
    tracked_streamers: Vec<Streamer>,
    persistent_user_id: String,
    observability: AppObservability,
    health: HealthTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (sender, receiver) = tokio::sync::mpsc::channel(128);
        let (effect_sender, effect_receiver) = tokio::sync::mpsc::channel(128);
        let topic_batches = match build_topic_batches(&user_id, &tracked_streamers) {
            Ok(batches) => batches,
            Err(error) => {
                tracing::warn!(task = "pubsub", error_class = "topic-build", %error, "pubsub topic build failed");
                return;
            }
        };
        let effect_task = spawn_pubsub_effect_task(
            runtime.clone(),
            Arc::clone(&twitch),
            persistent_user_id.clone(),
            observability.clone(),
            effect_receiver,
        );
        let event_task = spawn_pubsub_event_task(
            stop.clone(),
            runtime.clone(),
            observability.clone(),
            receiver,
            effect_sender.clone(),
            health.clone(),
        );

        let mut connections = Vec::with_capacity(topic_batches.len());
        for (index, topics) in topic_batches.into_iter().enumerate() {
            connections.push(spawn_pubsub_connection_loop(PubSubConnectionParams {
                stop: stop.clone(),
                sender: sender.clone(),
                auth_token: auth_token.clone(),
                username: username.clone(),
                tracked_streamers: tracked_streamers.clone(),
                topics,
                connection_index: index + 1,
                health: health.clone(),
            }));
        }

        for connection in connections {
            let _ = connection.await;
        }

        drop(sender);
        drop(effect_sender);
        let _ = event_task.await;
        let _ = effect_task.await;
    })
}

fn spawn_pubsub_effect_task(
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    persistent_user_id: String,
    observability: AppObservability,
    mut receiver: tokio::sync::mpsc::Receiver<Vec<tm_runtime::RuntimeEffect>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(effects) = receiver.recv().await {
            if let Err(error) = execute_runtime_effects(
                &runtime,
                &twitch,
                &persistent_user_id,
                effects,
                &observability,
            )
            .await
            {
                tracing::warn!(task = "pubsub", error_class = "runtime-effect", %error, "runtime effect execution failed");
            }
        }
    })
}

fn spawn_pubsub_event_task(
    mut stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    observability: AppObservability,
    mut receiver: tokio::sync::mpsc::Receiver<PubSubConnectionEvent>,
    effect_sender: tokio::sync::mpsc::Sender<Vec<tm_runtime::RuntimeEffect>>,
    health: HealthTracker,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                message = receiver.recv() => {
                    let Some(message) = message else {
                        break;
                    };
                    if handle_pubsub_message(&runtime, &observability, &effect_sender, &health, message).await {
                        break;
                    }
                }
            }
        }
    })
}

async fn handle_pubsub_message(
    runtime: &tm_runtime::RuntimeHandle,
    observability: &AppObservability,
    effect_sender: &tokio::sync::mpsc::Sender<Vec<tm_runtime::RuntimeEffect>>,
    health: &HealthTracker,
    message: PubSubConnectionEvent,
) -> bool {
    match message {
        PubSubConnectionEvent::Heartbeat => health.success("pubsub"),
        PubSubConnectionEvent::Event(event) => {
            let log_event = (*event).clone();
            match runtime.apply_pubsub_event(*event, time_now()).await {
                Ok(effects) => {
                    health.success("pubsub");
                    if let Err(error) = log_pubsub_event(runtime, observability, &log_event).await {
                        tracing::warn!(task = "pubsub", error_class = "log-handling", %error, "pubsub log handling failed");
                    }
                    if effect_sender.send(effects).await.is_err() {
                        tracing::warn!(
                            task = "pubsub",
                            error_class = "effect-queue-closed",
                            "pubsub runtime effect queue closed unexpectedly"
                        );
                        return true;
                    }
                }
                Err(error) => {
                    health.failure("pubsub", "event-application");
                    tracing::warn!(task = "pubsub", error_class = "event-application", %error, "pubsub event application failed");
                }
            }
        }
        PubSubConnectionEvent::ResponseError { error, nonce } => {
            health.failure("pubsub", "response");
            tracing::warn!(
                task = "pubsub",
                error_class = "response",
                nonce_present = nonce.is_some(),
                %error,
                "PubSub response error"
            );
        }
    }
    false
}

struct PubSubConnectionParams {
    stop: tokio::sync::watch::Receiver<bool>,
    sender: tokio::sync::mpsc::Sender<PubSubConnectionEvent>,
    auth_token: String,
    username: String,
    tracked_streamers: Vec<Streamer>,
    topics: Vec<String>,
    connection_index: usize,
    health: HealthTracker,
}

fn spawn_pubsub_connection_loop(params: PubSubConnectionParams) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let PubSubConnectionParams {
            stop,
            sender,
            auth_token,
            username,
            tracked_streamers,
            topics,
            connection_index,
            health,
        } = params;
        let mut stop = stop;
        let mut failure_attempt = 0_u32;
        loop {
            if *stop.borrow() {
                break;
            }

            let client = PubSubClient::default();
            let connected_at = std::time::Instant::now();
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
                    if connected_at.elapsed() >= Duration::from_secs(5 * 60) {
                        failure_attempt = 0;
                    } else {
                        failure_attempt = failure_attempt.saturating_add(1);
                    }
                    let outcome = classify_pubsub_connection_result(
                        &result,
                        &health,
                        connection_index,
                        topics.len(),
                        failure_attempt,
                    );
                    if matches!(&outcome, PubSubConnectionOutcome::Exit) {
                        return;
                    }
                    if let PubSubConnectionOutcome::Reconnect(Some(delay)) = outcome {
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

enum PubSubConnectionOutcome {
    Exit,
    Reconnect(Option<Duration>),
}

fn classify_pubsub_connection_result(
    result: &std::result::Result<
        std::result::Result<(), tm_pubsub::PubSubError>,
        tokio::task::JoinError,
    >,
    health: &HealthTracker,
    connection_index: usize,
    topic_count: usize,
    failure_attempt: u32,
) -> PubSubConnectionOutcome {
    let error_class = match result {
        Ok(Ok(())) => Some("connection-closed"),
        Ok(Err(tm_pubsub::PubSubError::ReconnectRequested)) => None,
        Ok(Err(_)) => Some("connection-error"),
        Err(error) if error.is_cancelled() => return PubSubConnectionOutcome::Exit,
        Err(_) => Some("connection-task"),
    };
    if let Some(error_class) = error_class {
        health.failure("pubsub", error_class);
    }
    match result {
        Ok(Ok(())) => tracing::warn!(
            task = "pubsub",
            error_class = "connection-closed",
            connection_index,
            topic_count,
            failure_attempt,
            "PubSub connection closed; reconnecting"
        ),
        Ok(Err(tm_pubsub::PubSubError::ReconnectRequested)) => {}
        Ok(Err(error)) => tracing::error!(
            task = "pubsub",
            error_class = "connection-error",
            connection_index,
            topic_count,
            failure_attempt,
            %error,
            "PubSub connection error"
        ),
        Err(error) => tracing::error!(
            task = "pubsub",
            error_class = "connection-task",
            connection_index,
            topic_count,
            failure_attempt,
            %error,
            "PubSub connection task failed"
        ),
    }
    PubSubConnectionOutcome::Reconnect(pubsub_reconnect_delay(
        result,
        connection_index,
        topic_count,
        failure_attempt,
    ))
}

pub(crate) fn pubsub_reconnect_delay(
    result: &std::result::Result<
        std::result::Result<(), tm_pubsub::PubSubError>,
        tokio::task::JoinError,
    >,
    connection_index: usize,
    topic_count: usize,
    failure_attempt: u32,
) -> Option<Duration> {
    match result {
        Ok(Err(tm_pubsub::PubSubError::ReconnectRequested)) => {
            tracing::warn!(
                "PubSub[{connection_index}] reconnect requested; waiting 60 seconds ({topic_count} topic(s))"
            );
            Some(Duration::from_secs(60))
        }
        Ok(Ok(())) => Some(exponential_backoff_with_jitter(
            5,
            failure_attempt,
            connection_index,
            topic_count,
        )),
        Err(error) if error.is_cancelled() => None,
        Ok(Err(_)) | Err(_) => Some(exponential_backoff_with_jitter(
            10,
            failure_attempt,
            connection_index,
            topic_count,
        )),
    }
}

fn exponential_backoff_with_jitter(
    base_seconds: u64,
    failure_attempt: u32,
    connection_index: usize,
    topic_count: usize,
) -> Duration {
    let exponent = failure_attempt.saturating_sub(1).min(5);
    let backoff = base_seconds.saturating_mul(1_u64 << exponent).min(5 * 60);
    let jitter = (u64::from(failure_attempt)
        + u64::try_from(connection_index).unwrap_or_default() * 3
        + u64::try_from(topic_count).unwrap_or_default() * 5)
        % 7;
    Duration::from_secs((backoff + jitter).min(5 * 60))
}

pub(crate) async fn log_pubsub_event(
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
