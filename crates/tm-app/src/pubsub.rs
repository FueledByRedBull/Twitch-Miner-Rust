#![allow(unused_imports)]
#![allow(clippy::wildcard_imports)]
use crate::*;

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

pub(crate) fn spawn_pubsub_connection_loop(
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

pub(crate) fn pubsub_reconnect_delay(
    result: &std::result::Result<
        std::result::Result<(), tm_pubsub::PubSubError>,
        tokio::task::JoinError,
    >,
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
