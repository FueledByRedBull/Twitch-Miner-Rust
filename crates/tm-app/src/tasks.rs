use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tm_auth::TwitchAuthClient;
use tm_domain::Streamer;
use tm_twitch::{generate_device_id, TwitchClient};

use crate::alarms;
use crate::bootstrap::{build_http_client, normalized_username};
use crate::chat::spawn_chat_manager_loop;
use crate::context::spawn_context_refresh_loop;
use crate::drops::{spawn_drop_claim_loop, DropClaimCoordinator};
use crate::eventsub::{
    spawn_eventsub_loop, spawn_eventsub_presence_poll_loop, EventSubTaskContext,
};
use crate::minute_watcher::spawn_minute_watcher_loop;
use crate::observability::AppObservability;
use crate::prediction_journal::PredictionPlacementJournal;
use crate::pubsub::{spawn_pubsub_loop, DropEventContext, PubSubTaskContext};
use crate::reload;
use crate::runtime_effects::{PredictionEvaluationScheduler, RuntimeEffectContext};
use crate::session_watch::{spawn_session_watch_loop, SESSION_CHECK_INTERVAL};
use crate::shutdown::AbortTasksOnDrop;
use crate::status::HealthTracker;
use crate::streak_cache::{spawn_streak_cache_loop, StreakCache};
use crate::streak_recovery::spawn_streak_recovery_loop;
use crate::utilities::configured_timezone;

/// How long a reload waits for the old transports to stop before aborting them.
const TRANSPORT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// A transport task that a reload may replace while the process runs. The
/// supervisor and the shutdown path read the current task through the slot.
#[derive(Clone, Default)]
pub(crate) struct TransportSlot(Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>);

impl TransportSlot {
    pub(crate) fn new(task: Option<tokio::task::JoinHandle<()>>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(task)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<tokio::task::JoinHandle<()>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn is_some(&self) -> bool {
        self.lock().is_some()
    }

    #[cfg(test)]
    pub(crate) fn is_none(&self) -> bool {
        self.lock().is_none()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.lock()
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }

    pub(crate) fn take(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.lock().take()
    }

    fn put(&self, task: Option<tokio::task::JoinHandle<()>>) {
        *self.lock() = task;
    }
}

pub(crate) struct BackgroundTasks {
    pub(crate) eventsub: TransportSlot,
    pub(crate) pubsub: TransportSlot,
    pub(crate) presence_poll: TransportSlot,
    pub(crate) context: Option<tokio::task::JoinHandle<()>>,
    pub(crate) minute: Option<tokio::task::JoinHandle<()>>,
    pub(crate) drop: Option<tokio::task::JoinHandle<()>>,
    pub(crate) chat: Option<tokio::task::JoinHandle<()>>,
    pub(crate) streak_cache: Option<tokio::task::JoinHandle<()>>,
    pub(crate) streak_recovery: Option<tokio::task::JoinHandle<()>>,
    pub(crate) session_watch: Option<tokio::task::JoinHandle<()>>,
    pub(crate) earnings_watch: Option<tokio::task::JoinHandle<()>>,
    pub(crate) config_watch: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) struct TransportTasks {
    pub(crate) eventsub: Option<tokio::task::JoinHandle<()>>,
    pub(crate) pubsub: Option<tokio::task::JoinHandle<()>>,
    pub(crate) presence_poll: Option<tokio::task::JoinHandle<()>>,
}

impl BackgroundTasks {
    pub(crate) fn unexpectedly_finished(&self) -> Vec<&'static str> {
        let mut finished = [
            ("eventsub", self.eventsub.is_finished()),
            ("presence-poll", self.presence_poll.is_finished()),
        ]
        .into_iter()
        .filter_map(|(name, done)| done.then_some(name))
        .collect::<Vec<_>>();
        finished.extend(
            [
                ("context", self.context.as_ref()),
                ("minute", self.minute.as_ref()),
                ("drop", self.drop.as_ref()),
                ("chat", self.chat.as_ref()),
                ("streak-cache", self.streak_cache.as_ref()),
                ("streak-recovery", self.streak_recovery.as_ref()),
                ("session-watch", self.session_watch.as_ref()),
                ("earnings-watch", self.earnings_watch.as_ref()),
                ("config-watch", self.config_watch.as_ref()),
            ]
            .into_iter()
            .filter_map(|(name, task)| {
                task.is_some_and(tokio::task::JoinHandle::is_finished)
                    .then_some(name)
            }),
        );
        finished
    }
}

pub(crate) struct BackgroundTaskParams<'a> {
    pub(crate) config: &'a tm_config::ConfigFile,
    pub(crate) config_path: &'a std::path::Path,
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

#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_background_tasks(params: &BackgroundTaskParams<'_>) -> Result<BackgroundTasks> {
    let username = normalized_username(&params.config.username)?;
    let prediction_journal = PredictionPlacementJournal::open(params.work_dir)?;
    params.health.set_journal(prediction_journal.clone());
    let prediction_scheduler = params.user_id.map(|_| {
        PredictionEvaluationScheduler::start(params.stop_rx.clone(), params.observability)
    });
    let drop_coordinator = DropClaimCoordinator::with_account_link(params.config.drop_account_link);
    register_background_health(params);
    let transports = TransportController::start(
        params.stop_rx.clone(),
        params.initial_streamers,
        production_spawner(
            params,
            username.clone(),
            prediction_journal,
            prediction_scheduler,
            drop_coordinator.clone(),
        ),
    );
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
    let minute = params.user_id.map(|user_id| {
        spawn_minute_watcher_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
            params.health.clone(),
            drop_coordinator.clone(),
            params.work_dir.to_path_buf(),
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
                drop_coordinator,
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
        eventsub: transports.eventsub.clone(),
        pubsub: transports.pubsub.clone(),
        presence_poll: transports.presence_poll.clone(),
        context,
        minute,
        drop,
        chat,
        streak_cache,
        streak_recovery,
        session_watch: Some(spawn_session_watch(params)?),
        earnings_watch: Some(spawn_earnings_watch(params)),
        config_watch: Some(spawn_config_watch(params, transports)),
    })
}

fn spawn_session_watch(params: &BackgroundTaskParams<'_>) -> Result<tokio::task::JoinHandle<()>> {
    Ok(spawn_session_watch_loop(
        params.stop_rx.clone(),
        TwitchAuthClient::with_client(build_http_client()?),
        params.auth_token.to_string(),
        generate_device_id(),
        normalized_username(&params.config.username)?,
        params.observability.clone(),
        SESSION_CHECK_INTERVAL,
    ))
}

fn spawn_earnings_watch(params: &BackgroundTaskParams<'_>) -> tokio::task::JoinHandle<()> {
    params
        .health
        .register(alarms::TASK_NAME, alarms::STALE_AFTER);
    alarms::spawn_earnings_watch_loop(
        params.stop_rx.clone(),
        alarms::EarningsWatchContext {
            observability: params.observability.clone(),
            health: params.health.clone(),
            work_dir: params.work_dir.to_path_buf(),
            timezone: configured_timezone(params.config),
        },
    )
}

fn spawn_config_watch(
    params: &BackgroundTaskParams<'_>,
    transports: Arc<TransportController>,
) -> tokio::task::JoinHandle<()> {
    params
        .health
        .register(reload::TASK_NAME, reload::STALE_AFTER);
    reload::spawn_config_watch_loop(
        params.stop_rx.clone(),
        reload::ConfigWatchContext {
            config_path: params.config_path.to_path_buf(),
            work_dir: params.work_dir.to_path_buf(),
            running: params.config.clone(),
            gates: reload::StartupGates::from_streamers(params.initial_streamers),
            user_id: params.user_id.cloned(),
            runtime: params.runtime.clone(),
            twitch: Arc::clone(params.twitch),
            observability: params.observability.clone(),
            health: params.health.clone(),
            transports,
        },
    )
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
        params
            .health
            .register("minute", std::time::Duration::from_secs(10 * 60));
        params
            .health
            .register("minute-metadata", std::time::Duration::from_secs(10 * 60));
        params
            .health
            .register("minute-watch", std::time::Duration::from_secs(10 * 60));
    }
    params
        .health
        .register("chat", std::time::Duration::from_secs(8 * 60));
    params
        .health
        .register("streak-cache", std::time::Duration::from_secs(10 * 60));
}

/// Starts the transports whose subscriptions depend on the channel list. The
/// `bool` asks the presence poll to re-check every channel once it starts.
pub(crate) type TransportSpawner = Box<
    dyn Fn(&[Streamer], tokio::sync::watch::Receiver<bool>, bool) -> TransportTasks + Send + Sync,
>;

/// Owns the generation of `EventSub`, `PubSub` and presence-poll tasks.
///
/// Each generation has its own stop signal, which also fires when the process
/// stops. A reload stops the current generation, waits for it, then starts the
/// next one, so two generations never process events at the same time and the
/// supervisor, which reads the slots, never sees a restart as a task exit.
pub(crate) struct TransportController {
    pub(crate) eventsub: TransportSlot,
    pub(crate) pubsub: TransportSlot,
    pub(crate) presence_poll: TransportSlot,
    process_stop: tokio::sync::watch::Receiver<bool>,
    spawner: TransportSpawner,
    stop_timeout: Duration,
    generation: tokio::sync::Mutex<Arc<tokio::sync::watch::Sender<bool>>>,
}

impl TransportController {
    pub(crate) fn start(
        process_stop: tokio::sync::watch::Receiver<bool>,
        streamers: &[Streamer],
        spawner: TransportSpawner,
    ) -> Arc<Self> {
        Self::start_with_stop_timeout(process_stop, streamers, spawner, TRANSPORT_STOP_TIMEOUT)
    }

    fn start_with_stop_timeout(
        process_stop: tokio::sync::watch::Receiver<bool>,
        streamers: &[Streamer],
        spawner: TransportSpawner,
        stop_timeout: Duration,
    ) -> Arc<Self> {
        let (stop_tx, stop_rx) = generation_stop(&process_stop);
        let tasks = spawner(streamers, stop_rx, false);
        Arc::new(Self {
            eventsub: TransportSlot::new(tasks.eventsub),
            pubsub: TransportSlot::new(tasks.pubsub),
            presence_poll: TransportSlot::new(tasks.presence_poll),
            process_stop,
            spawner,
            stop_timeout,
            generation: tokio::sync::Mutex::new(stop_tx),
        })
    }

    /// Replaces the running transports with ones built for `streamers`.
    /// Returns `false` without starting anything when the process is stopping.
    pub(crate) async fn restart(&self, streamers: &[Streamer]) -> bool {
        let mut generation = self.generation.lock().await;
        let _ = generation.send(true);
        let old = [
            self.eventsub.take(),
            self.pubsub.take(),
            self.presence_poll.take(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        stop_generation(old, self.stop_timeout).await;
        if *self.process_stop.borrow() {
            return false;
        }
        let (stop_tx, stop_rx) = generation_stop(&self.process_stop);
        let tasks = (self.spawner)(streamers, stop_rx, true);
        self.eventsub.put(tasks.eventsub);
        self.pubsub.put(tasks.pubsub);
        self.presence_poll.put(tasks.presence_poll);
        *generation = stop_tx;
        true
    }
}

/// A stop signal for one generation that also fires when the process stops.
fn generation_stop(
    process_stop: &tokio::sync::watch::Receiver<bool>,
) -> (
    Arc<tokio::sync::watch::Sender<bool>>,
    tokio::sync::watch::Receiver<bool>,
) {
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stop_tx = Arc::new(stop_tx);
    let relay_tx = Arc::clone(&stop_tx);
    let mut process_stop = process_stop.clone();
    tokio::spawn(async move {
        loop {
            if *process_stop.borrow() {
                let _ = relay_tx.send(true);
                return;
            }
            tokio::select! {
                changed = process_stop.changed() => {
                    if changed.is_err() {
                        let _ = relay_tx.send(true);
                        return;
                    }
                }
                () = relay_tx.closed() => return,
            }
        }
    });
    (stop_tx, stop_rx)
}

async fn stop_generation(tasks: Vec<tokio::task::JoinHandle<()>>, timeout: Duration) {
    let _abort_on_drop = AbortTasksOnDrop(
        tasks
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect(),
    );
    let deadline = tokio::time::Instant::now() + timeout;
    for mut task in tasks {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            tracing::warn!(
                task = "config-watch",
                error_class = "transport-stop-timeout",
                "a transport did not stop in time during a channel list reload; aborting it"
            );
            task.abort();
            let _ = task.await;
        }
    }
}

fn production_spawner(
    params: &BackgroundTaskParams<'_>,
    username: String,
    prediction_journal: PredictionPlacementJournal,
    prediction_scheduler: Option<PredictionEvaluationScheduler>,
    drop_coordinator: DropClaimCoordinator,
) -> TransportSpawner {
    let runtime = params.runtime.clone();
    let twitch = Arc::clone(params.twitch);
    let user_id = params.user_id.cloned();
    let auth_token = params.auth_token.to_string();
    let prediction_eventsub_authorized = params.prediction_eventsub_authorized;
    let observability = params.observability.clone();
    let health = params.health.clone();
    Box::new(move |streamers, stop, resync_presence| {
        let initial_fallback = (0..streamers.len()).collect::<Vec<_>>();
        let (fallback_tx, fallback_rx) = tokio::sync::watch::channel(initial_fallback);
        let effects = |user_id: &String, scheduler: &PredictionEvaluationScheduler| {
            RuntimeEffectContext::new_with_journal(
                runtime.clone(),
                Arc::clone(&twitch),
                user_id.clone(),
                observability.clone(),
                health.clone(),
                prediction_journal.clone(),
                scheduler.clone(),
            )
        };
        let eventsub = user_id.as_ref().and_then(|user_id| {
            let scheduler = prediction_scheduler.as_ref()?;
            Some(spawn_eventsub_loop(
                stop.clone(),
                EventSubTaskContext {
                    effects: effects(user_id, scheduler),
                    auth_token: auth_token.clone(),
                    tracked_streamers: streamers.to_vec(),
                    prediction_eventsub_authorized,
                    fallback_tx,
                },
            ))
        });
        let presence_poll = user_id.as_ref().map(|_| {
            spawn_eventsub_presence_poll_loop(
                stop.clone(),
                runtime.clone(),
                Arc::clone(&twitch),
                streamers.to_vec(),
                fallback_rx,
                resync_presence,
                observability.clone(),
                health.clone(),
            )
        });
        let pubsub = user_id.as_ref().and_then(|user_id| {
            let scheduler = prediction_scheduler.as_ref()?;
            Some(spawn_pubsub_loop(
                stop.clone(),
                PubSubTaskContext {
                    effects: effects(user_id, scheduler),
                    auth_token: auth_token.clone(),
                    user_id: user_id.clone(),
                    username: username.clone(),
                    tracked_streamers: streamers.to_vec(),
                    drop_events: Some(DropEventContext {
                        twitch: Arc::clone(&twitch),
                        coordinator: drop_coordinator.clone(),
                        claim_drops: streamers
                            .iter()
                            .any(|streamer| streamer.settings.claim_drops),
                    }),
                },
            ))
        });
        for task in ["eventsub", "pubsub", "presence-poll"] {
            health.activity(task);
        }
        TransportTasks {
            eventsub,
            pubsub,
            presence_poll,
        }
    })
}
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tm_observability::DiscordClient;
    use tm_twitch::{TwitchClient, TwitchEndpoints};

    use super::{spawn_background_tasks, BackgroundTaskParams, BackgroundTasks, TransportSlot};
    use crate::observability::{AppObservability, AppObservabilitySettings};
    use crate::shutdown::shutdown_background_tasks;
    use crate::status::HealthTracker;
    use crate::streak_cache::StreakCache;

    fn empty_tasks() -> BackgroundTasks {
        BackgroundTasks {
            eventsub: TransportSlot::default(),
            pubsub: TransportSlot::default(),
            presence_poll: TransportSlot::default(),
            context: None,
            minute: None,
            drop: None,
            chat: None,
            streak_cache: None,
            streak_recovery: None,
            session_watch: None,
            earnings_watch: None,
            config_watch: None,
        }
    }

    #[tokio::test]
    async fn reports_unexpectedly_finished_tasks() {
        let mut tasks = empty_tasks();
        tasks.eventsub = TransportSlot::new(Some(tokio::spawn(async {})));

        tokio::task::yield_now().await;

        assert_eq!(tasks.unexpectedly_finished(), vec!["eventsub"]);
    }

    #[tokio::test]
    async fn pubsub_exit_does_not_terminate_other_transports() {
        let mut tasks = empty_tasks();
        let (_eventsub_tx, eventsub_rx) = tokio::sync::oneshot::channel::<()>();
        tasks.eventsub = TransportSlot::new(Some(tokio::spawn(async move {
            let _ = eventsub_rx.await;
        })));
        tasks.pubsub = TransportSlot::new(Some(tokio::spawn(async {})));

        tokio::task::yield_now().await;

        assert!(tasks.unexpectedly_finished().is_empty());
        assert!(!tasks.eventsub.is_finished());
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
    #[allow(clippy::too_many_lines)]
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
        let config_path = directory.path().join("config.json");

        let (stop_tx, stop_rx) = tokio::sync::watch::channel(true);
        let health = HealthTracker::default();
        let tasks = spawn_background_tasks(&BackgroundTaskParams {
            config: &config,
            config_path: &config_path,
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
        assert!(tasks.minute.is_none());
        assert!(tasks.drop.is_none());
        assert!(tasks.streak_recovery.is_none());
        assert!(tasks.chat.is_some());
        assert!(tasks.streak_cache.is_some());
        assert!(tasks.earnings_watch.is_some());
        assert!(tasks.config_watch.is_some());
        assert_eq!(health.task_consecutive_failures("chat"), Some(0));
        assert_eq!(health.task_consecutive_failures("streak-cache"), Some(0));
        assert_eq!(health.task_consecutive_failures("earnings-watch"), Some(0));
        assert_eq!(health.task_consecutive_failures("config-watch"), Some(0));
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
            config_path: &config_path,
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
        assert!(tasks.minute.is_some());
        assert!(tasks.drop.is_some());
        assert!(tasks.streak_recovery.is_some());
        assert!(tasks.chat.is_some());
        assert!(tasks.streak_cache.is_some());
        assert!(tasks.earnings_watch.is_some());
        assert!(tasks.config_watch.is_some());
        for task in [
            "eventsub",
            "pubsub",
            "presence-poll",
            "context",
            "minute",
            "minute-metadata",
            "minute-watch",
            "drop",
            "chat",
            "streak-cache",
            "earnings-watch",
            "config-watch",
            "streak-recovery",
        ] {
            assert_eq!(health.task_consecutive_failures(task), Some(0), "{task}");
        }
        shutdown_background_tasks(stop_tx, tasks).await;
    }

    #[tokio::test]
    async fn authenticated_wiring_keeps_core_loops_without_optional_feature_tasks() {
        let config = tm_config::ConfigFile {
            username: String::from("tester"),
            ..tm_config::ConfigFile::default()
        };
        let streamer = tm_domain::Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..tm_domain::Streamer::default()
        };
        let twitch = test_twitch();
        let runtime = tm_runtime::spawn_runtime_state(tm_runtime::RuntimeState::from_targets(
            &config,
            std::slice::from_ref(&streamer.username),
            tm_domain::OffsetDateTime::UNIX_EPOCH,
        ));
        let observability = test_observability();
        let cache = StreakCache::default();
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.json");
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(true);
        let health = HealthTracker::default();

        let tasks = spawn_background_tasks(&BackgroundTaskParams {
            config: &config,
            config_path: &config_path,
            stop_rx,
            runtime: &runtime,
            twitch: &twitch,
            auth_token: "token",
            user_id: Some(&String::from("user-1")),
            prediction_eventsub_authorized: false,
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
        assert!(tasks.minute.is_some());
        assert!(tasks.drop.is_none());
        assert!(tasks.streak_recovery.is_none());
        assert!(tasks.chat.is_some());
        assert!(tasks.streak_cache.is_some());
        assert!(tasks.earnings_watch.is_some());
        assert!(tasks.config_watch.is_some());
        for task in [
            "eventsub",
            "pubsub",
            "presence-poll",
            "context",
            "minute",
            "minute-metadata",
            "minute-watch",
            "chat",
            "streak-cache",
            "earnings-watch",
            "config-watch",
        ] {
            assert_eq!(health.task_consecutive_failures(task), Some(0), "{task}");
        }
        assert_eq!(health.task_consecutive_failures("drop"), None);
        assert_eq!(health.task_consecutive_failures("streak-recovery"), None);

        shutdown_background_tasks(stop_tx, tasks).await;
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use tm_domain::Streamer;

    use super::{TransportController, TransportSpawner, TransportTasks};

    /// Counts the fake transport tasks that are alive, and how many were alive
    /// at the same time, across generations.
    #[derive(Default)]
    struct Generations {
        starts: AtomicUsize,
        alive: AtomicUsize,
        most_alive: AtomicUsize,
        resyncs: Mutex<Vec<bool>>,
        channel_counts: Mutex<Vec<usize>>,
    }

    fn fake_spawner(generations: &Arc<Generations>, obey_stop: bool) -> TransportSpawner {
        let generations = Arc::clone(generations);
        Box::new(move |streamers, stop, resync| {
            generations.starts.fetch_add(1, Ordering::SeqCst);
            generations.resyncs.lock().unwrap().push(resync);
            generations
                .channel_counts
                .lock()
                .unwrap()
                .push(streamers.len());
            let spawn = || {
                let alive = generations.alive.fetch_add(1, Ordering::SeqCst) + 1;
                generations.most_alive.fetch_max(alive, Ordering::SeqCst);
                let generations = Arc::clone(&generations);
                let mut stop = stop.clone();
                tokio::spawn(async move {
                    if obey_stop {
                        while !*stop.borrow() {
                            if stop.changed().await.is_err() {
                                break;
                            }
                        }
                        generations.alive.fetch_sub(1, Ordering::SeqCst);
                    } else {
                        std::future::pending::<()>().await;
                    }
                })
            };
            TransportTasks {
                eventsub: Some(spawn()),
                pubsub: Some(spawn()),
                presence_poll: None,
            }
        })
    }

    fn tasks_over(controller: &TransportController) -> BackgroundTasks {
        let mut tasks = empty_tasks();
        tasks.eventsub = controller.eventsub.clone();
        tasks.pubsub = controller.pubsub.clone();
        tasks.presence_poll = controller.presence_poll.clone();
        tasks
    }

    #[tokio::test]
    async fn restarting_transports_never_looks_like_an_exited_task_and_never_overlaps() {
        let generations = Arc::new(Generations::default());
        let (_process_tx, process_rx) = tokio::sync::watch::channel(false);
        let controller =
            TransportController::start(process_rx, &[], fake_spawner(&generations, true));
        let tasks = tasks_over(&controller);
        let watching = Arc::new(tokio::sync::Notify::new());
        let supervisor = tokio::spawn({
            let watching = Arc::clone(&watching);
            let tasks = tasks_over(&controller);
            async move {
                let mut sightings = Vec::new();
                loop {
                    let finished = tasks.unexpectedly_finished();
                    if !finished.is_empty() {
                        sightings.push(finished);
                    }
                    tokio::select! {
                        () = watching.notified() => return sightings,
                        () = tokio::task::yield_now() => {}
                    }
                }
            }
        });

        for channels in 1..=3 {
            let streamers = vec![Streamer::default(); channels];
            assert!(controller.restart(&streamers).await);
        }

        watching.notify_one();
        assert!(supervisor.await.unwrap().is_empty());
        assert_eq!(generations.starts.load(Ordering::SeqCst), 4);
        assert_eq!(
            *generations.resyncs.lock().unwrap(),
            [false, true, true, true]
        );
        assert_eq!(*generations.channel_counts.lock().unwrap(), [0, 1, 2, 3]);
        assert_eq!(
            generations.most_alive.load(Ordering::SeqCst),
            2,
            "only one generation of two tasks is ever alive"
        );
        assert!(tasks.eventsub.is_some() && tasks.pubsub.is_some());
        assert!(tasks.unexpectedly_finished().is_empty());
    }

    #[tokio::test]
    async fn shutdown_after_a_restart_joins_the_current_generation() {
        let generations = Arc::new(Generations::default());
        let (process_tx, process_rx) = tokio::sync::watch::channel(false);
        let controller =
            TransportController::start(process_rx, &[], fake_spawner(&generations, true));
        assert!(controller.restart(&[Streamer::default()]).await);
        assert_eq!(generations.alive.load(Ordering::SeqCst), 2);

        shutdown_background_tasks(process_tx, tasks_over(&controller)).await;

        assert_eq!(generations.alive.load(Ordering::SeqCst), 0);
        assert!(controller.eventsub.is_none() && controller.pubsub.is_none());
        // Nothing starts once the process is stopping.
        assert!(!controller.restart(&[Streamer::default()]).await);
        assert_eq!(generations.starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn shutdown_during_a_restart_aborts_the_wait_and_starts_nothing() {
        let generations = Arc::new(Generations::default());
        let (process_tx, process_rx) = tokio::sync::watch::channel(false);
        // The old generation ignores its stop signal, so the restart is stuck
        // waiting for it until the stop timeout.
        let controller = TransportController::start_with_stop_timeout(
            process_rx,
            &[],
            fake_spawner(&generations, false),
            Duration::from_millis(100),
        );
        let reload = {
            let controller = Arc::clone(&controller);
            tokio::spawn(async move {
                controller.restart(&[Streamer::default()]).await;
            })
        };
        while controller.eventsub.is_some() {
            tokio::task::yield_now().await;
        }
        let mut tasks = tasks_over(&controller);
        tasks.config_watch = Some(reload);

        shutdown_background_tasks(process_tx, tasks).await;

        assert_eq!(generations.starts.load(Ordering::SeqCst), 1);
        assert!(controller.eventsub.is_none());
    }

    #[tokio::test]
    async fn a_stuck_old_generation_is_aborted_after_the_stop_timeout_and_a_cancelled_restart_aborts_it(
    ) {
        let generations = Arc::new(Generations::default());
        let (_process_tx, process_rx) = tokio::sync::watch::channel(false);
        let controller = TransportController::start_with_stop_timeout(
            process_rx.clone(),
            &[],
            fake_spawner(&generations, false),
            Duration::from_millis(50),
        );
        assert!(controller.restart(&[Streamer::default()]).await);
        assert_eq!(generations.starts.load(Ordering::SeqCst), 2);

        let slow = TransportController::start_with_stop_timeout(
            process_rx,
            &[],
            fake_spawner(&generations, false),
            Duration::from_secs(60),
        );
        let old = slow.eventsub.take().unwrap();
        let abort = old.abort_handle();
        slow.eventsub.put(Some(old));
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            slow.restart(&[Streamer::default()]),
        )
        .await;
        assert!(cancelled.is_err());
        for _ in 0..50 {
            if abort.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            abort.is_finished(),
            "dropping a restart aborts the old tasks"
        );
    }
}
