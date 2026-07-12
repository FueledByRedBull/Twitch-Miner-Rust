use std::collections::HashMap;

use anyhow::Result;
use tm_irc::ChatClient;

use crate::observability::{AppObservability, TracingChatLogger};
use crate::status::HealthTracker;
use crate::utilities::sleep_or_stop;

const CHAT_WATCHER_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) fn spawn_chat_manager_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    auth_token: String,
    username: String,
    disable_at_in_nickname: bool,
    observability: AppObservability,
    health: HealthTracker,
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
        let mut idle_heartbeat = tokio::time::interval(std::time::Duration::from_secs(60));
        idle_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        if let Err(error) = reconcile_chat_watchers(
            &runtime,
            &mut watchers,
            &auth_token,
            &username,
            disable_at_in_nickname,
            &observability,
            &health,
        )
        .await
        {
            health.failure("chat", "manager-snapshot");
            tracing::warn!(task = "chat", error_class = "manager-snapshot", %error, "chat manager snapshot failed");
            return;
        }
        health.success("chat");

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
                        &health,
                    )
                    .await
                    {
                        health.failure("chat", "manager-snapshot");
                        tracing::warn!(task = "chat", error_class = "manager-snapshot", %error, "chat manager snapshot failed");
                        break;
                    }
                    health.success("chat");
                }
                _ = idle_heartbeat.tick(), if watchers.is_empty() => {
                    health.success("chat");
                }
            }
        }

        for (_, (watcher_stop, task)) in watchers {
            let _ = watcher_stop.send(true);
            await_chat_watcher_task("chat-watcher", task).await;
        }
    })
}

pub(crate) async fn reconcile_chat_watchers(
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
    health: &HealthTracker,
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
            await_chat_watcher_task("chat-watcher", task).await;
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
            health.clone(),
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

async fn await_chat_watcher_task(name: &str, mut task: tokio::task::JoinHandle<()>) {
    match tokio::time::timeout(CHAT_WATCHER_STOP_GRACE, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(task = name, %error, "chat watcher failed while stopping"),
        Err(_) => {
            tracing::warn!(
                task = name,
                timeout_seconds = CHAT_WATCHER_STOP_GRACE.as_secs(),
                "chat watcher exceeded stop grace period; aborting"
            );
            task.abort();
            let _ = task.await;
        }
    }
}

pub(crate) fn spawn_chat_watcher_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    username: String,
    auth_token: String,
    channel: String,
    disable_at_in_nickname: bool,
    observability: AppObservability,
    health: HealthTracker,
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
                    health: health.clone(),
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
                        health.failure("chat", "irc-disconnect");
                        tracing::warn!(task = "chat", error_class = "irc-disconnect", channel = %channel, %error, "irc watcher disconnected");
                    } else {
                        health.failure("chat", "irc-closed");
                    }
                }
            }

            if sleep_or_stop(&mut stop, std::time::Duration::from_secs(5)).await {
                break;
            }
        }
    })
}
