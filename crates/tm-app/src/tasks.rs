use std::sync::Arc;

use anyhow::Result;
use tm_domain::Streamer;
use tm_twitch::TwitchClient;

use crate::bootstrap::normalized_username;
use crate::chat::spawn_chat_manager_loop;
use crate::context::{spawn_context_refresh_loop, spawn_pending_claim_loop};
use crate::drops::spawn_drop_claim_loop;
use crate::minute_watcher::spawn_minute_watcher_loop;
use crate::observability::AppObservability;
use crate::pubsub::spawn_pubsub_loop;
use crate::status::HealthTracker;

pub(crate) struct BackgroundTasks {
    pub(crate) pubsub: Option<tokio::task::JoinHandle<()>>,
    pub(crate) context: Option<tokio::task::JoinHandle<()>>,
    pub(crate) pending_claims: Option<tokio::task::JoinHandle<()>>,
    pub(crate) minute: Option<tokio::task::JoinHandle<()>>,
    pub(crate) drop: Option<tokio::task::JoinHandle<()>>,
    pub(crate) chat: Option<tokio::task::JoinHandle<()>>,
}

impl BackgroundTasks {
    pub(crate) fn unexpectedly_finished(&self) -> Vec<&'static str> {
        [
            ("pubsub", self.pubsub.as_ref()),
            ("context", self.context.as_ref()),
            ("pending-claims", self.pending_claims.as_ref()),
            ("minute", self.minute.as_ref()),
            ("drop", self.drop.as_ref()),
            ("chat", self.chat.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, task)| {
            task.is_some_and(tokio::task::JoinHandle::is_finished)
                .then_some(name)
        })
        .collect()
    }
}

pub(crate) struct BackgroundTaskParams<'a> {
    pub(crate) config: &'a tm_config::ConfigFile,
    pub(crate) stop_rx: tokio::sync::watch::Receiver<bool>,
    pub(crate) runtime: &'a tm_runtime::RuntimeHandle,
    pub(crate) twitch: &'a Arc<TwitchClient>,
    pub(crate) auth_token: &'a str,
    pub(crate) user_id: Option<&'a String>,
    pub(crate) initial_streamers: &'a [Streamer],
    pub(crate) observability: &'a AppObservability,
    pub(crate) health: &'a HealthTracker,
}

pub(crate) fn spawn_background_tasks(params: BackgroundTaskParams<'_>) -> Result<BackgroundTasks> {
    let username = normalized_username(&params.config.username)?;
    if params.user_id.is_some() {
        params
            .health
            .register("pubsub", std::time::Duration::from_secs(8 * 60));
        params
            .health
            .register("context", std::time::Duration::from_secs(30 * 60));
        params.health.register(
            "pending-claims",
            std::time::Duration::from_secs(6 * 60 * 60),
        );
        params
            .health
            .register("minute", std::time::Duration::from_secs(10 * 60));
    }
    params
        .health
        .register("chat", std::time::Duration::from_secs(8 * 60));
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
            params.health.clone(),
        )
    });
    let context = params.user_id.map(|user_id| {
        spawn_context_refresh_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
            params.health.clone(),
        )
    });
    let pending_claims = params.user_id.map(|user_id| {
        spawn_pending_claim_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
            params.health.clone(),
        )
    });
    let minute = params.user_id.map(|user_id| {
        spawn_minute_watcher_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
            params.health.clone(),
        )
    });
    let drop = params
        .initial_streamers
        .iter()
        .any(|streamer| streamer.settings.claim_drops)
        .then(|| {
            params
                .health
                .register("drop", std::time::Duration::from_secs(45 * 60));
            spawn_drop_claim_loop(
                params.stop_rx.clone(),
                Arc::clone(params.twitch),
                params.observability.clone(),
                params.health.clone(),
            )
        });
    let chat = Some(spawn_chat_manager_loop(
        params.stop_rx,
        params.runtime.clone(),
        params.auth_token.to_string(),
        username,
        params.config.disable_at_in_nickname,
        params.observability.clone(),
        params.health.clone(),
    ));
    Ok(BackgroundTasks {
        pubsub,
        context,
        pending_claims,
        minute,
        drop,
        chat,
    })
}

#[cfg(test)]
mod tests {
    use super::BackgroundTasks;

    fn empty_tasks() -> BackgroundTasks {
        BackgroundTasks {
            pubsub: None,
            context: None,
            pending_claims: None,
            minute: None,
            drop: None,
            chat: None,
        }
    }

    #[tokio::test]
    async fn reports_unexpectedly_finished_tasks() {
        let mut tasks = empty_tasks();
        tasks.pubsub = Some(tokio::spawn(async {}));

        tokio::task::yield_now().await;

        assert_eq!(tasks.unexpectedly_finished(), vec!["pubsub"]);
    }

    #[tokio::test]
    async fn ignores_running_and_absent_tasks() {
        let mut tasks = empty_tasks();
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        tasks.chat = Some(tokio::spawn(async move {
            let _ = rx.await;
        }));

        assert!(tasks.unexpectedly_finished().is_empty());
    }

    #[tokio::test]
    async fn reports_panicked_tasks() {
        let mut tasks = empty_tasks();
        tasks.minute = Some(tokio::spawn(async { panic!("synthetic task failure") }));

        while !tasks
            .minute
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            tokio::task::yield_now().await;
        }

        assert_eq!(tasks.unexpectedly_finished(), vec!["minute"]);
    }
}
