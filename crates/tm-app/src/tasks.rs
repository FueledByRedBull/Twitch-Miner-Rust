use std::sync::Arc;

use anyhow::Result;
use tm_domain::Streamer;
use tm_twitch::TwitchClient;

use crate::bootstrap::normalized_username;
use crate::chat::spawn_chat_manager_loop;
use crate::context::{spawn_context_refresh_loop, spawn_pending_claim_loop};
use crate::drops::spawn_drop_claim_loop;
use crate::eventsub::{spawn_eventsub_loop, spawn_eventsub_presence_poll_loop};
use crate::minute_watcher::spawn_minute_watcher_loop;
use crate::observability::AppObservability;
use crate::pubsub::spawn_pubsub_loop;
use crate::status::HealthTracker;
use crate::streak_cache::{spawn_streak_cache_loop, StreakCache};
use crate::streak_recovery::spawn_streak_recovery_loop;

pub(crate) struct BackgroundTasks {
    pub(crate) eventsub: Option<tokio::task::JoinHandle<()>>,
    pub(crate) pubsub: Option<tokio::task::JoinHandle<()>>,
    pub(crate) presence_poll: Option<tokio::task::JoinHandle<()>>,
    pub(crate) context: Option<tokio::task::JoinHandle<()>>,
    pub(crate) pending_claims: Option<tokio::task::JoinHandle<()>>,
    pub(crate) minute: Option<tokio::task::JoinHandle<()>>,
    pub(crate) drop: Option<tokio::task::JoinHandle<()>>,
    pub(crate) chat: Option<tokio::task::JoinHandle<()>>,
    pub(crate) streak_cache: Option<tokio::task::JoinHandle<()>>,
    pub(crate) streak_recovery: Option<tokio::task::JoinHandle<()>>,
}

struct TransportTasks {
    eventsub: Option<tokio::task::JoinHandle<()>>,
    pubsub: Option<tokio::task::JoinHandle<()>>,
    presence_poll: Option<tokio::task::JoinHandle<()>>,
}

impl BackgroundTasks {
    pub(crate) fn unexpectedly_finished(&self) -> Vec<&'static str> {
        [
            ("eventsub", self.eventsub.as_ref()),
            ("presence-poll", self.presence_poll.as_ref()),
            ("context", self.context.as_ref()),
            ("pending-claims", self.pending_claims.as_ref()),
            ("minute", self.minute.as_ref()),
            ("drop", self.drop.as_ref()),
            ("chat", self.chat.as_ref()),
            ("streak-cache", self.streak_cache.as_ref()),
            ("streak-recovery", self.streak_recovery.as_ref()),
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
    pub(crate) prediction_eventsub_authorized: bool,
    pub(crate) initial_streamers: &'a [Streamer],
    pub(crate) observability: &'a AppObservability,
    pub(crate) health: &'a HealthTracker,
    pub(crate) streak_cache: &'a StreakCache,
    pub(crate) work_dir: &'a std::path::Path,
}

pub(crate) fn spawn_background_tasks(params: &BackgroundTaskParams<'_>) -> Result<BackgroundTasks> {
    let username = normalized_username(&params.config.username)?;
    register_background_health(params);
    let transports = spawn_transport_tasks(params, &username);
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
        params.stop_rx.clone(),
        params.runtime.clone(),
        params.auth_token.to_string(),
        username,
        params.config.disable_at_in_nickname,
        params.observability.clone(),
        params.health.clone(),
    ));
    let streak_cache = Some(spawn_streak_cache_loop(
        params.stop_rx.clone(),
        params.runtime.clone(),
        params.streak_cache.clone(),
        params.work_dir.to_path_buf(),
        params.health.clone(),
    ));
    let streak_recovery = params.user_id.and_then(|user_id| {
        params
            .initial_streamers
            .iter()
            .any(|streamer| {
                streamer.settings.watch_streak && streamer.settings.watch_streak_vod_recovery
            })
            .then(|| {
                params
                    .health
                    .register("streak-recovery", std::time::Duration::from_secs(20 * 60));
                spawn_streak_recovery_loop(
                    params.stop_rx.clone(),
                    params.runtime.clone(),
                    Arc::clone(params.twitch),
                    user_id.clone(),
                    params.observability.clone(),
                    params.health.clone(),
                )
            })
    });
    Ok(BackgroundTasks {
        eventsub: transports.eventsub,
        pubsub: transports.pubsub,
        presence_poll: transports.presence_poll,
        context,
        pending_claims,
        minute,
        drop,
        chat,
        streak_cache,
        streak_recovery,
    })
}

fn register_background_health(params: &BackgroundTaskParams<'_>) {
    if params.user_id.is_some() {
        params
            .health
            .register("eventsub", std::time::Duration::from_secs(8 * 60));
        params
            .health
            .register("pubsub", std::time::Duration::from_secs(8 * 60));
        params
            .health
            .register("presence-poll", std::time::Duration::from_secs(5 * 60));
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
    params
        .health
        .register("streak-cache", std::time::Duration::from_secs(10 * 60));
}

fn spawn_transport_tasks(params: &BackgroundTaskParams<'_>, username: &str) -> TransportTasks {
    let initial_fallback = (0..params.initial_streamers.len()).collect::<Vec<_>>();
    let (fallback_tx, fallback_rx) = tokio::sync::watch::channel(initial_fallback);
    let eventsub = params.user_id.map(|user_id| {
        spawn_eventsub_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            params.auth_token.to_string(),
            params.initial_streamers.to_vec(),
            user_id.clone(),
            params.prediction_eventsub_authorized,
            params.observability.clone(),
            params.health.clone(),
            fallback_tx,
        )
    });
    let presence_poll = params.user_id.map(|_| {
        spawn_eventsub_presence_poll_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            params.initial_streamers.to_vec(),
            fallback_rx,
            params.observability.clone(),
            params.health.clone(),
        )
    });
    let pubsub = params.user_id.map(|user_id| {
        spawn_pubsub_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            params.auth_token.to_string(),
            user_id.clone(),
            username.to_string(),
            params.initial_streamers.to_vec(),
            user_id.clone(),
            params.observability.clone(),
            params.health.clone(),
        )
    });
    TransportTasks {
        eventsub,
        pubsub,
        presence_poll,
    }
}

#[cfg(test)]
mod tests {
    use super::BackgroundTasks;

    fn empty_tasks() -> BackgroundTasks {
        BackgroundTasks {
            eventsub: None,
            pubsub: None,
            presence_poll: None,
            context: None,
            pending_claims: None,
            minute: None,
            drop: None,
            chat: None,
            streak_cache: None,
            streak_recovery: None,
        }
    }

    #[tokio::test]
    async fn reports_unexpectedly_finished_tasks() {
        let mut tasks = empty_tasks();
        tasks.eventsub = Some(tokio::spawn(async {}));

        tokio::task::yield_now().await;

        assert_eq!(tasks.unexpectedly_finished(), vec!["eventsub"]);
    }

    #[tokio::test]
    async fn pubsub_exit_does_not_terminate_other_transports() {
        let mut tasks = empty_tasks();
        let (_eventsub_tx, eventsub_rx) = tokio::sync::oneshot::channel::<()>();
        tasks.eventsub = Some(tokio::spawn(async move {
            let _ = eventsub_rx.await;
        }));
        tasks.pubsub = Some(tokio::spawn(async {}));

        tokio::task::yield_now().await;

        assert!(tasks.unexpectedly_finished().is_empty());
        assert!(!tasks
            .eventsub
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished));
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
