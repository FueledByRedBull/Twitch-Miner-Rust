use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use tm_domain::Streamer;
use tm_observability::{event_from_gain_reason, Event as DiscordEvent};
use tm_pubsub::{build_topic_batches, PubSubClient, PubSubConnectionEvent};

use crate::observability::AppObservability;
use crate::runtime_effects::{execute_runtime_effects, RuntimeEffectContext};
use crate::status::HealthTracker;
use crate::utilities::time_now;

enum SupervisedPubSubEvent {
    Transport(PubSubConnectionEvent),
    ConnectionLost {
        acknowledged_by_class: HashMap<String, usize>,
        configured_classes: Vec<String>,
        failure_class: &'static str,
    },
}

pub(crate) struct PubSubTaskContext {
    pub(crate) effects: RuntimeEffectContext,
    pub(crate) auth_token: String,
    pub(crate) user_id: String,
    pub(crate) username: String,
    pub(crate) tracked_streamers: Vec<Streamer>,
}

pub(crate) fn spawn_pubsub_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    context: PubSubTaskContext,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (sender, receiver) = tokio::sync::mpsc::channel(128);
        let (effect_sender, effect_receiver) = tokio::sync::mpsc::channel(128);
        let topic_batches = match build_topic_batches(&context.user_id, &context.tracked_streamers)
        {
            Ok(batches) => batches,
            Err(error) => {
                context
                    .effects
                    .health
                    .failure("pubsub", pubsub_error_class(&error));
                context
                    .effects
                    .health
                    .record_pubsub_setup(failed_pubsub_setup_report(&error));
                tracing::warn!(
                    task = "pubsub",
                    error_class = pubsub_error_class(&error),
                    "pubsub topic build failed"
                );
                return;
            }
        };
        context
            .effects
            .health
            .record_pubsub_setup(tm_pubsub::pubsub_setup_report(&topic_batches));
        let effect_task = spawn_pubsub_effect_task(context.effects.clone(), effect_receiver);
        let event_task = spawn_pubsub_event_task(
            stop.clone(),
            context.effects.runtime.clone(),
            context.effects.observability.clone(),
            receiver,
            effect_sender.clone(),
            context.effects.health.clone(),
        );

        let mut connections = Vec::with_capacity(topic_batches.len());
        for (index, topics) in topic_batches.into_iter().enumerate() {
            connections.push(spawn_pubsub_connection_loop(PubSubConnectionParams {
                stop: stop.clone(),
                sender: sender.clone(),
                auth_token: context.auth_token.clone(),
                username: context.username.clone(),
                tracked_streamers: context.tracked_streamers.clone(),
                topics,
                connection_index: index + 1,
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
    context: RuntimeEffectContext,
    mut receiver: tokio::sync::mpsc::Receiver<Vec<tm_runtime::RuntimeEffect>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(effects) = receiver.recv().await {
            if let Err(error) = execute_runtime_effects(&context, effects).await {
                tracing::warn!(task = "pubsub", error_class = "runtime-effect", %error, "runtime effect execution failed");
            }
        }
    })
}

fn spawn_pubsub_event_task(
    mut stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    observability: AppObservability,
    mut receiver: tokio::sync::mpsc::Receiver<SupervisedPubSubEvent>,
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
    message: SupervisedPubSubEvent,
) -> bool {
    let message = match message {
        SupervisedPubSubEvent::Transport(message) => message,
        SupervisedPubSubEvent::ConnectionLost {
            acknowledged_by_class,
            configured_classes,
            failure_class,
        } => {
            health.failure("pubsub", failure_class);
            for topic_class in configured_classes {
                health.record_pubsub_disconnect(
                    &topic_class,
                    acknowledged_by_class
                        .get(&topic_class)
                        .copied()
                        .unwrap_or_default(),
                    failure_class,
                );
            }
            return false;
        }
    };
    match message {
        PubSubConnectionEvent::Heartbeat => {
            if health.pubsub_ready() {
                health.success("pubsub");
            }
        }
        PubSubConnectionEvent::ListenAcknowledged { topic_class } => {
            if health.record_pubsub_acknowledgement(&topic_class) {
                health.success("pubsub");
            }
        }
        PubSubConnectionEvent::Event(event) => {
            health.record_pubsub_message(pubsub_event_topic_class(&event));
            let log_event = (*event).clone();
            let received_at = std::time::Instant::now();
            match runtime.apply_event_with_outcome(*event, time_now()).await {
                Ok(application) => {
                    runtime
                        .metrics_handle()
                        .record_transport_latency(received_at.elapsed());
                    if health.pubsub_ready() {
                        health.success("pubsub");
                    }
                    if application.changed {
                        if let Err(error) =
                            log_pubsub_event(runtime, observability, &log_event).await
                        {
                            tracing::warn!(task = "pubsub", error_class = "log-handling", %error, "pubsub log handling failed");
                        }
                    }
                    if effect_sender.send(application.effects).await.is_err() {
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
        PubSubConnectionEvent::ResponseError { nonce, topic_class } => {
            health.failure("pubsub", "response");
            if let Some(topic_class) = topic_class.as_deref() {
                health.record_pubsub_failure(topic_class, "listen-rejected");
            }
            tracing::warn!(
                task = "pubsub",
                error_class = "response",
                nonce_present = nonce.is_some(),
                "PubSub response error"
            );
        }
    }
    false
}

fn pubsub_event_topic_class(event: &tm_domain::MinerEvent) -> &'static str {
    match event {
        tm_domain::MinerEvent::PointsEarned { .. }
        | tm_domain::MinerEvent::ClaimAvailable { .. } => "points-user",
        tm_domain::MinerEvent::Playback { .. } => "presence",
        tm_domain::MinerEvent::Raid { .. } => "raid",
        tm_domain::MinerEvent::Moment { .. } => "moments",
        tm_domain::MinerEvent::PredictionChannel { .. } => "prediction-channel",
        tm_domain::MinerEvent::PredictionUser { .. } => "prediction-user",
        tm_domain::MinerEvent::CommunityGoal { .. } => "community-goals",
    }
}

fn failed_pubsub_setup_report(error: &tm_pubsub::PubSubError) -> tm_pubsub::PubSubSetupReport {
    let (total_topics, configured_topics) = match error {
        tm_pubsub::PubSubError::CapacityExceeded { configured, .. } => (*configured, *configured),
        _ => (0, 1),
    };
    tm_pubsub::PubSubSetupReport {
        connection_count: 0,
        total_topics,
        capabilities: vec![tm_pubsub::PubSubCapabilityStatus {
            topic_class: String::from("transport-setup"),
            configured_topics,
            acknowledged_topics: 0,
            last_message_unix: None,
            reconnects: 0,
            failure_class: Some(pubsub_error_class(error).to_string()),
        }],
    }
}

struct PubSubConnectionParams {
    stop: tokio::sync::watch::Receiver<bool>,
    sender: tokio::sync::mpsc::Sender<SupervisedPubSubEvent>,
    auth_token: String,
    username: String,
    tracked_streamers: Vec<Streamer>,
    topics: Vec<String>,
    connection_index: usize,
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
        } = params;
        let mut stop = stop;
        let mut failure_attempt = 0_u32;
        loop {
            if *stop.borrow() {
                break;
            }

            let client = PubSubClient::default();
            let connected_at = std::time::Instant::now();
            let (connection_sender, connection_receiver) = tokio::sync::mpsc::channel(128);
            let forwarder = spawn_pubsub_forwarder(connection_receiver, sender.clone());
            let connect = tokio::spawn({
                let auth_token = auth_token.clone();
                let username = username.clone();
                let tracked_streamers = tracked_streamers.clone();
                let topics = topics.clone();
                async move {
                    client
                        .connect_topics_and_listen(
                            &topics,
                            &auth_token,
                            Some(&username),
                            &tracked_streamers,
                            connection_sender,
                        )
                        .await
                }
            });
            tokio::pin!(connect);

            let should_stop = tokio::select! {
                changed = stop.changed() => {
                    connect.as_mut().abort();
                    let _ = connect.await;
                    let _ = forwarder.await;
                    changed.is_err() || *stop.borrow()
                }
                result = &mut connect => {
                    let acknowledged_by_class = forwarder.await.unwrap_or_default();
                    if connected_at.elapsed() >= Duration::from_secs(5 * 60) {
                        failure_attempt = 0;
                    } else {
                        failure_attempt = failure_attempt.saturating_add(1);
                    }
                    let outcome = classify_pubsub_connection_result(
                        &result,
                        connection_index,
                        &topics,
                        failure_attempt,
                    );
                    if let Some(failure_class) = pubsub_connection_failure_class(&result) {
                        if sender
                            .send(SupervisedPubSubEvent::ConnectionLost {
                                acknowledged_by_class,
                                configured_classes: configured_pubsub_classes(&topics),
                                failure_class,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
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

fn spawn_pubsub_forwarder(
    mut receiver: tokio::sync::mpsc::Receiver<PubSubConnectionEvent>,
    sender: tokio::sync::mpsc::Sender<SupervisedPubSubEvent>,
) -> tokio::task::JoinHandle<HashMap<String, usize>> {
    tokio::spawn(async move {
        let mut acknowledged_by_class = HashMap::<String, usize>::new();
        while let Some(message) = receiver.recv().await {
            if let PubSubConnectionEvent::ListenAcknowledged { topic_class } = &message {
                *acknowledged_by_class
                    .entry(topic_class.clone())
                    .or_default() += 1;
            }
            if sender
                .send(SupervisedPubSubEvent::Transport(message))
                .await
                .is_err()
            {
                break;
            }
        }
        acknowledged_by_class
    })
}

fn configured_pubsub_classes(topics: &[String]) -> Vec<String> {
    let mut classes = topics
        .iter()
        .map(|topic| tm_pubsub::pubsub_topic_class(topic).to_string())
        .collect::<Vec<_>>();
    classes.sort_unstable();
    classes.dedup();
    classes
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
    connection_index: usize,
    topics: &[String],
    failure_attempt: u32,
) -> PubSubConnectionOutcome {
    let error_class = pubsub_connection_failure_class(result);
    if matches!(result, Err(error) if error.is_cancelled()) {
        return PubSubConnectionOutcome::Exit;
    }
    let topic_count = topics.len();
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
            error_class = pubsub_error_class(error),
            connection_index,
            topic_count,
            failure_attempt,
            "PubSub connection error"
        ),
        Err(_) => tracing::error!(
            task = "pubsub",
            error_class = "connection-task",
            connection_index,
            topic_count,
            failure_attempt,
            "PubSub connection task failed"
        ),
    }
    debug_assert!(error_class.is_some());
    PubSubConnectionOutcome::Reconnect(pubsub_reconnect_delay(
        result,
        connection_index,
        topic_count,
        failure_attempt,
    ))
}

fn pubsub_connection_failure_class(
    result: &std::result::Result<
        std::result::Result<(), tm_pubsub::PubSubError>,
        tokio::task::JoinError,
    >,
) -> Option<&'static str> {
    match result {
        Ok(Ok(())) => Some("connection-closed"),
        Ok(Err(tm_pubsub::PubSubError::ReconnectRequested)) => Some("reconnect-requested"),
        Ok(Err(error)) => Some(pubsub_error_class(error)),
        Err(error) if error.is_cancelled() => None,
        Err(_) => Some("connection-task"),
    }
}

fn pubsub_error_class(error: &tm_pubsub::PubSubError) -> &'static str {
    match error {
        tm_pubsub::PubSubError::MissingUserId => "configuration",
        tm_pubsub::PubSubError::CapacityExceeded { .. } => "capacity",
        tm_pubsub::PubSubError::InvalidPayload(_)
        | tm_pubsub::PubSubError::InvalidText(_)
        | tm_pubsub::PubSubError::Protocol(_) => "protocol",
        tm_pubsub::PubSubError::WebSocket(_) => "connection-error",
        tm_pubsub::PubSubError::EventChannelClosed => "event-channel-closed",
        tm_pubsub::PubSubError::ReconnectRequested => "reconnect",
        tm_pubsub::PubSubError::BadAuth { .. } => "bad-auth",
        tm_pubsub::PubSubError::PongTimeout => "pong-timeout",
    }
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
    event: &tm_domain::MinerEvent,
) -> Result<()> {
    match event {
        tm_domain::MinerEvent::PointsEarned {
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
            tracing::info!(operation = "on_message", "{message}");
            if let Some(event) = event_from_gain_reason(reason) {
                observability.send_event(event, &message).await;
            }
        }
        tm_domain::MinerEvent::Playback { channel_id, kind } => {
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
                    tracing::info!(operation = "set_online", "{message}");
                    observability
                        .send_event(DiscordEvent::StreamerOnline, &message)
                        .await;
                }
                tm_pubsub::PlaybackType::StreamDown => {
                    let message = observability.offline_message(streamer);
                    tracing::info!(operation = "set_offline", "{message}");
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        classify_pubsub_connection_result, configured_pubsub_classes, failed_pubsub_setup_report,
        handle_pubsub_message, pubsub_error_class, pubsub_event_topic_class, spawn_pubsub_loop,
        PubSubConnectionOutcome, PubSubTaskContext, SupervisedPubSubEvent,
    };
    use tm_pubsub::{
        CommunityGoalKind, MinerEvent, PlaybackType, PredictionChannelKind, PredictionUserKind,
    };

    use crate::observability::{AppObservability, AppObservabilitySettings};
    use crate::runtime_effects::RuntimeEffectContext;
    use crate::status::HealthTracker;

    fn test_observability() -> AppObservability {
        AppObservability::new(
            None,
            tm_observability::DiscordClient::new(Duration::from_secs(1)).unwrap(),
            AppObservabilitySettings::default(),
        )
    }

    fn test_runtime() -> tm_runtime::RuntimeHandle {
        tm_runtime::spawn_runtime_state(tm_runtime::RuntimeState::from_targets(
            &tm_config::ConfigFile::default(),
            &[],
            tm_domain::OffsetDateTime::UNIX_EPOCH,
        ))
    }

    fn test_effects(
        runtime: tm_runtime::RuntimeHandle,
        health: &HealthTracker,
    ) -> RuntimeEffectContext {
        RuntimeEffectContext::new(
            runtime,
            Arc::new(tm_twitch::TwitchClient::with_client_and_endpoints(
                reqwest::Client::new(),
                "token",
                "user-agent",
                tm_twitch::TwitchEndpoints::default(),
            )),
            String::from("viewer-1"),
            test_observability(),
            health.clone(),
        )
    }

    fn error_cases() -> Vec<(tm_pubsub::PubSubError, &'static str)> {
        vec![
            (tm_pubsub::PubSubError::MissingUserId, "configuration"),
            (
                tm_pubsub::PubSubError::CapacityExceeded {
                    configured: 501,
                    maximum: 500,
                },
                "capacity",
            ),
            (
                tm_pubsub::PubSubError::InvalidPayload(
                    serde_json::from_str::<serde_json::Value>("not-json").unwrap_err(),
                ),
                "protocol",
            ),
            (
                tm_pubsub::PubSubError::InvalidText(String::from_utf8(vec![0xff]).unwrap_err()),
                "protocol",
            ),
            (tm_pubsub::PubSubError::Protocol("test"), "protocol"),
            (
                tm_pubsub::PubSubError::EventChannelClosed,
                "event-channel-closed",
            ),
            (tm_pubsub::PubSubError::ReconnectRequested, "reconnect"),
            (
                tm_pubsub::PubSubError::BadAuth {
                    cookie_file: String::from("private-cookie"),
                },
                "bad-auth",
            ),
            (tm_pubsub::PubSubError::PongTimeout, "pong-timeout"),
        ]
    }

    #[test]
    fn requested_reconnect_is_counted_until_fresh_listen_acknowledgement() {
        let topics = vec![String::from("community-points-user-v1.viewer")];
        let result = Ok(Err(tm_pubsub::PubSubError::ReconnectRequested));

        let outcome = classify_pubsub_connection_result(&result, 1, &topics, 1);

        assert!(matches!(outcome, PubSubConnectionOutcome::Reconnect(_)));
    }

    #[test]
    fn capacity_failure_is_visible_without_topic_identifiers() {
        let report = failed_pubsub_setup_report(&tm_pubsub::PubSubError::CapacityExceeded {
            configured: 501,
            maximum: 500,
        });

        assert_eq!(report.connection_count, 0);
        assert_eq!(report.total_topics, 501);
        assert_eq!(report.capabilities[0].topic_class, "transport-setup");
        assert_eq!(
            report.capabilities[0].failure_class.as_deref(),
            Some("capacity")
        );
    }

    #[test]
    fn pubsub_error_classes_are_stable_and_payload_free() {
        for (error, expected) in error_cases() {
            assert_eq!(pubsub_error_class(&error), expected);
        }
    }

    #[test]
    fn configured_topics_collapse_to_sorted_capability_classes() {
        let topics = vec![
            String::from("raid.100"),
            String::from("video-playback-by-id.100"),
            String::from("raid.101"),
            String::from("community-points-user-v1.user"),
        ];
        assert_eq!(
            configured_pubsub_classes(&topics),
            vec![
                String::from("points-user"),
                String::from("presence"),
                String::from("raid"),
            ]
        );
    }

    #[test]
    fn every_transport_event_maps_to_its_health_topic_class() {
        let prediction = tm_domain::PredictionEvent {
            streamer: tm_domain::Streamer::default(),
            event_id: String::from("event"),
            title: String::from("test"),
            status: String::from("ACTIVE"),
            created_at: tm_domain::OffsetDateTime::UNIX_EPOCH,
            window_seconds: 30.0,
            outcomes: Vec::new(),
            decision: tm_domain::PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };
        let events = [
            (
                MinerEvent::PointsEarned {
                    channel_id: String::from("100"),
                    earned: 1,
                    reason: String::from("watch"),
                    balance: 2,
                },
                "points-user",
            ),
            (
                MinerEvent::ClaimAvailable {
                    channel_id: String::from("100"),
                    claim_id: String::from("claim"),
                },
                "points-user",
            ),
            (
                MinerEvent::Playback {
                    channel_id: String::from("100"),
                    kind: PlaybackType::StreamUp,
                },
                "presence",
            ),
            (
                MinerEvent::Raid {
                    channel_id: String::from("100"),
                    raid_id: String::from("raid"),
                    target_login: String::from("target"),
                },
                "raid",
            ),
            (
                MinerEvent::Moment {
                    channel_id: String::from("100"),
                    moment_id: String::from("moment"),
                },
                "moments",
            ),
            (
                MinerEvent::PredictionChannel {
                    kind: PredictionChannelKind::EventCreated,
                    event: Box::new(prediction.clone()),
                    winning_outcome_id: None,
                },
                "prediction-channel",
            ),
            (
                MinerEvent::PredictionUser {
                    event_id: String::from("event"),
                    kind: PredictionUserKind::PredictionMade,
                    result: None,
                },
                "prediction-user",
            ),
            (
                MinerEvent::CommunityGoal {
                    channel_id: String::from("100"),
                    kind: CommunityGoalKind::Created,
                    goal: None,
                    goal_id: None,
                },
                "community-goals",
            ),
        ];

        for (event, expected) in events {
            assert_eq!(pubsub_event_topic_class(&event), expected);
        }
    }

    #[test]
    fn failed_setup_report_preserves_non_capacity_failure_shape() {
        let report = failed_pubsub_setup_report(&tm_pubsub::PubSubError::MissingUserId);
        assert_eq!(report.total_topics, 0);
        assert_eq!(report.capabilities.len(), 1);
        assert_eq!(report.capabilities[0].configured_topics, 1);
        assert_eq!(
            report.capabilities[0].failure_class.as_deref(),
            Some("configuration")
        );
    }

    #[tokio::test]
    async fn connection_outcomes_distinguish_exit_and_reconnect_paths() {
        let topics = vec![String::from("raid.100")];
        let reconnect = classify_pubsub_connection_result(
            &Ok(Err(tm_pubsub::PubSubError::ReconnectRequested)),
            1,
            &topics,
            1,
        );
        assert!(matches!(reconnect, PubSubConnectionOutcome::Reconnect(_)));

        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let cancelled_task = task.await.expect_err("abort should produce a join error");
        assert!(matches!(
            classify_pubsub_connection_result(&Err(cancelled_task), 1, &topics, 1),
            PubSubConnectionOutcome::Exit
        ));
    }

    #[tokio::test]
    async fn connection_loss_and_listen_ack_restore_pubsub_health_by_class() {
        let runtime = test_runtime();
        let observability = test_observability();
        let health = HealthTracker::default();
        health.register("pubsub", Duration::from_secs(60));
        health.record_pubsub_setup(tm_pubsub::PubSubSetupReport {
            connection_count: 1,
            total_topics: 2,
            capabilities: vec![
                tm_pubsub::PubSubCapabilityStatus {
                    topic_class: String::from("points-user"),
                    configured_topics: 1,
                    acknowledged_topics: 1,
                    last_message_unix: None,
                    reconnects: 0,
                    failure_class: None,
                },
                tm_pubsub::PubSubCapabilityStatus {
                    topic_class: String::from("raid"),
                    configured_topics: 1,
                    acknowledged_topics: 1,
                    last_message_unix: None,
                    reconnects: 0,
                    failure_class: None,
                },
            ],
        });
        let (effect_sender, _effects) = tokio::sync::mpsc::channel(1);

        assert!(
            !handle_pubsub_message(
                &runtime,
                &observability,
                &effect_sender,
                &health,
                SupervisedPubSubEvent::ConnectionLost {
                    acknowledged_by_class: HashMap::from([(String::from("points-user"), 1)]),
                    configured_classes: vec![String::from("points-user"), String::from("raid")],
                    failure_class: "connection-reset",
                },
            )
            .await
        );
        assert_eq!(health.task_consecutive_failures("pubsub"), Some(1));
        assert!(!health.pubsub_ready());

        assert!(
            !handle_pubsub_message(
                &runtime,
                &observability,
                &effect_sender,
                &health,
                SupervisedPubSubEvent::Transport(
                    tm_pubsub::PubSubConnectionEvent::ListenAcknowledged {
                        topic_class: String::from("points-user"),
                    },
                ),
            )
            .await
        );
        assert!(!health.pubsub_ready());
        assert!(
            !handle_pubsub_message(
                &runtime,
                &observability,
                &effect_sender,
                &health,
                SupervisedPubSubEvent::Transport(
                    tm_pubsub::PubSubConnectionEvent::ListenAcknowledged {
                        topic_class: String::from("raid"),
                    },
                ),
            )
            .await
        );
        assert!(health.pubsub_ready());
        assert_eq!(health.task_consecutive_failures("pubsub"), Some(0));

        assert!(
            !handle_pubsub_message(
                &runtime,
                &observability,
                &effect_sender,
                &health,
                SupervisedPubSubEvent::Transport(tm_pubsub::PubSubConnectionEvent::ResponseError {
                    nonce: Some(String::from("nonce")),
                    topic_class: Some(String::from("raid")),
                }),
            )
            .await
        );
        assert_eq!(health.task_consecutive_failures("pubsub"), Some(1));
        assert_eq!(
            health.pubsub_snapshot().unwrap().capabilities[1]
                .failure_class
                .as_deref(),
            Some("listen-rejected")
        );
    }

    #[tokio::test]
    async fn setup_failure_records_configuration_classification_and_report() {
        let runtime = test_runtime();
        let health = HealthTracker::default();
        health.register("pubsub", Duration::from_secs(60));
        let context = PubSubTaskContext {
            effects: test_effects(runtime, &health),
            auth_token: String::from("token"),
            user_id: String::new(),
            username: String::from("tester"),
            tracked_streamers: Vec::new(),
        };
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(true);

        spawn_pubsub_loop(stop_rx, context)
            .await
            .expect("setup failure should be handled by the supervised task");

        assert_eq!(health.task_consecutive_failures("pubsub"), Some(1));
        let report = health
            .pubsub_snapshot()
            .expect("setup report should be recorded");
        assert_eq!(report.connection_count, 0);
        assert_eq!(report.total_topics, 0);
        assert_eq!(report.capabilities.len(), 1);
        assert_eq!(report.capabilities[0].configured_topics, 1);
        assert_eq!(
            report.capabilities[0].failure_class.as_deref(),
            Some("configuration")
        );
    }

    #[tokio::test]
    async fn successful_setup_records_capabilities_before_stop_shutdown() {
        let runtime = test_runtime();
        let health = HealthTracker::default();
        health.register("pubsub", Duration::from_secs(60));
        let context = PubSubTaskContext {
            effects: test_effects(runtime, &health),
            auth_token: String::from("token"),
            user_id: String::from("viewer-1"),
            username: String::from("tester"),
            tracked_streamers: Vec::new(),
        };
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(true);

        spawn_pubsub_loop(stop_rx, context)
            .await
            .expect("a pre-stopped transport should shut down cleanly");

        assert_eq!(health.task_consecutive_failures("pubsub"), Some(0));
        let report = health
            .pubsub_snapshot()
            .expect("setup report should be recorded");
        assert_eq!(report.connection_count, 1);
        assert_eq!(report.total_topics, 1);
        assert_eq!(report.capabilities.len(), 1);
        assert_eq!(report.capabilities[0].topic_class, "points-user");
        assert_eq!(report.capabilities[0].configured_topics, 1);
        assert_eq!(report.capabilities[0].acknowledged_topics, 0);
    }

    #[tokio::test]
    async fn heartbeat_only_marks_ready_pubsub_healthy_and_closed_effect_queue_stops_task() {
        let runtime = test_runtime();
        let observability = test_observability();
        let health = HealthTracker::default();
        health.register("pubsub", Duration::from_secs(60));
        health.record_pubsub_setup(tm_pubsub::PubSubSetupReport {
            connection_count: 1,
            total_topics: 1,
            capabilities: vec![tm_pubsub::PubSubCapabilityStatus {
                topic_class: String::from("points-user"),
                configured_topics: 1,
                acknowledged_topics: 1,
                last_message_unix: None,
                reconnects: 0,
                failure_class: None,
            }],
        });
        let (effect_sender, effect_receiver) = tokio::sync::mpsc::channel(1);
        drop(effect_receiver);

        assert!(
            !handle_pubsub_message(
                &runtime,
                &observability,
                &effect_sender,
                &health,
                SupervisedPubSubEvent::Transport(tm_pubsub::PubSubConnectionEvent::Heartbeat),
            )
            .await
        );
        assert_eq!(health.task_consecutive_failures("pubsub"), Some(0));

        assert!(
            handle_pubsub_message(
                &runtime,
                &observability,
                &effect_sender,
                &health,
                SupervisedPubSubEvent::Transport(tm_pubsub::PubSubConnectionEvent::Event(
                    Box::new(MinerEvent::ClaimAvailable {
                        channel_id: String::from("unknown"),
                        claim_id: String::from("claim"),
                    },)
                )),
            )
            .await
        );
    }

    #[tokio::test]
    async fn response_error_without_topic_class_degrades_transport_without_capability_mutation() {
        let runtime = test_runtime();
        let observability = test_observability();
        let health = HealthTracker::default();
        health.register("pubsub", Duration::from_secs(60));
        health.record_pubsub_setup(tm_pubsub::PubSubSetupReport {
            connection_count: 1,
            total_topics: 1,
            capabilities: vec![tm_pubsub::PubSubCapabilityStatus {
                topic_class: String::from("points-user"),
                configured_topics: 1,
                acknowledged_topics: 0,
                last_message_unix: None,
                reconnects: 0,
                failure_class: None,
            }],
        });
        let (effect_sender, _effects) = tokio::sync::mpsc::channel(1);

        assert!(
            !handle_pubsub_message(
                &runtime,
                &observability,
                &effect_sender,
                &health,
                SupervisedPubSubEvent::Transport(tm_pubsub::PubSubConnectionEvent::ResponseError {
                    nonce: None,
                    topic_class: None,
                }),
            )
            .await
        );
        assert_eq!(health.task_consecutive_failures("pubsub"), Some(1));
        assert_eq!(
            health.pubsub_snapshot().unwrap().capabilities[0]
                .failure_class
                .as_deref(),
            None
        );
    }
}
