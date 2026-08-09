use std::sync::Arc;

use anyhow::Result;
use tm_domain::Streamer;
use tm_twitch::TwitchClient;

use crate::bootstrap::normalized_username;
use crate::chat::spawn_chat_manager_loop;
use crate::context::{spawn_context_refresh_loop, spawn_pending_claim_loop};
use crate::drops::spawn_drop_claim_loop;
use crate::eventsub::{
    spawn_eventsub_loop, spawn_eventsub_presence_poll_loop, EventSubTaskContext,
};
use crate::minute_watcher::spawn_minute_watcher_loop;
use crate::observability::AppObservability;
use crate::pubsub::{spawn_pubsub_loop, PubSubTaskContext};
use crate::runtime_effects::RuntimeEffectContext;
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
            EventSubTaskContext {
                effects: RuntimeEffectContext::new(
                    params.runtime.clone(),
                    Arc::clone(params.twitch),
                    user_id.clone(),
                    params.observability.clone(),
                    params.health.clone(),
                ),
                auth_token: params.auth_token.to_string(),
                tracked_streamers: params.initial_streamers.to_vec(),
                prediction_eventsub_authorized: params.prediction_eventsub_authorized,
                fallback_tx,
            },
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
            PubSubTaskContext {
                effects: RuntimeEffectContext::new(
                    params.runtime.clone(),
                    Arc::clone(params.twitch),
                    user_id.clone(),
                    params.observability.clone(),
                    params.health.clone(),
                ),
                auth_token: params.auth_token.to_string(),
                user_id: user_id.clone(),
                username: username.to_string(),
                tracked_streamers: params.initial_streamers.to_vec(),
            },
        )
    });
    TransportTasks {
        eventsub,
        pubsub,
        presence_poll,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tm_observability::DiscordClient;
    use tm_twitch::{TwitchClient, TwitchEndpoints};

    use super::{spawn_background_tasks, BackgroundTaskParams, BackgroundTasks};
    use crate::observability::{AppObservability, AppObservabilitySettings};
    use crate::shutdown::shutdown_background_tasks;
    use crate::status::HealthTracker;
    use crate::streak_cache::StreakCache;

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

    fn test_observability() -> AppObservability {
        AppObservability::new(
            None,
            DiscordClient::new(Duration::from_secs(1)).unwrap(),
            AppObservabilitySettings::default(),
        )
    }

    fn test_twitch() -> Arc<TwitchClient> {
        Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::new(),
            "token",
            "user-agent",
            TwitchEndpoints::default(),
        ))
    }

    #[tokio::test]
    async fn task_wiring_respects_authenticated_and_optional_feature_boundaries() {
        let config = tm_config::ConfigFile {
            username: String::from("tester"),
            ..tm_config::ConfigFile::default()
        };
        let twitch = test_twitch();
        let runtime = tm_runtime::spawn_runtime_state(tm_runtime::RuntimeState::from_targets(
            &config,
            &[],
            tm_domain::OffsetDateTime::UNIX_EPOCH,
        ));
        let observability = test_observability();
        let cache = StreakCache::default();
        let directory = tempfile::tempdir().unwrap();

        let (stop_tx, stop_rx) = tokio::sync::watch::channel(true);
        let health = HealthTracker::default();
        let tasks = spawn_background_tasks(&BackgroundTaskParams {
            config: &config,
            stop_rx,
            runtime: &runtime,
            twitch: &twitch,
            auth_token: "token",
            user_id: None,
            prediction_eventsub_authorized: false,
            initial_streamers: &[],
            observability: &observability,
            health: &health,
            streak_cache: &cache,
            work_dir: directory.path(),
        })
        .unwrap();
        assert!(tasks.eventsub.is_none());
        assert!(tasks.pubsub.is_none());
        assert!(tasks.presence_poll.is_none());
        assert!(tasks.context.is_none());
        assert!(tasks.pending_claims.is_none());
        assert!(tasks.minute.is_none());
        assert!(tasks.drop.is_none());
        assert!(tasks.streak_recovery.is_none());
        assert!(tasks.chat.is_some());
        assert!(tasks.streak_cache.is_some());
        assert_eq!(health.task_consecutive_failures("chat"), Some(0));
        assert_eq!(health.task_consecutive_failures("streak-cache"), Some(0));
        shutdown_background_tasks(stop_tx, tasks).await;

        let streamer = tm_domain::Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            settings: tm_domain::StreamerSettings {
                claim_drops: true,
                watch_streak: true,
                watch_streak_vod_recovery: true,
                ..tm_domain::StreamerSettings::default()
            },
            ..tm_domain::Streamer::default()
        };
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(true);
        let health = HealthTracker::default();
        let tasks = spawn_background_tasks(&BackgroundTaskParams {
            config: &config,
            stop_rx,
            runtime: &runtime,
            twitch: &twitch,
            auth_token: "token",
            user_id: Some(&String::from("user-1")),
            prediction_eventsub_authorized: true,
            initial_streamers: std::slice::from_ref(&streamer),
            observability: &observability,
            health: &health,
            streak_cache: &cache,
            work_dir: directory.path(),
        })
        .unwrap();
        assert!(tasks.eventsub.is_some());
        assert!(tasks.pubsub.is_some());
        assert!(tasks.presence_poll.is_some());
        assert!(tasks.context.is_some());
        assert!(tasks.pending_claims.is_some());
        assert!(tasks.minute.is_some());
        assert!(tasks.drop.is_some());
        assert!(tasks.streak_recovery.is_some());
        assert!(tasks.chat.is_some());
        assert!(tasks.streak_cache.is_some());
        for task in [
            "eventsub",
            "pubsub",
            "presence-poll",
            "context",
            "pending-claims",
            "minute",
            "drop",
            "chat",
            "streak-cache",
            "streak-recovery",
        ] {
            assert_eq!(health.task_consecutive_failures(task), Some(0), "{task}");
        }
        shutdown_background_tasks(stop_tx, tasks).await;
    }
}
