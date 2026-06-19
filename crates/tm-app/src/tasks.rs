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

pub(crate) struct BackgroundTasks {
    pub(crate) pubsub: Option<tokio::task::JoinHandle<()>>,
    pub(crate) context: Option<tokio::task::JoinHandle<()>>,
    pub(crate) pending_claims: Option<tokio::task::JoinHandle<()>>,
    pub(crate) minute: Option<tokio::task::JoinHandle<()>>,
    pub(crate) drop: Option<tokio::task::JoinHandle<()>>,
    pub(crate) chat: Option<tokio::task::JoinHandle<()>>,
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
}

pub(crate) fn spawn_background_tasks(params: BackgroundTaskParams<'_>) -> Result<BackgroundTasks> {
    let username = normalized_username(&params.config.username)?;
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
        )
    });
    let context = params.user_id.map(|user_id| {
        spawn_context_refresh_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
        )
    });
    let pending_claims = params.user_id.map(|user_id| {
        spawn_pending_claim_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
        )
    });
    let minute = params.user_id.map(|user_id| {
        spawn_minute_watcher_loop(
            params.stop_rx.clone(),
            params.runtime.clone(),
            Arc::clone(params.twitch),
            user_id.clone(),
            params.observability.clone(),
        )
    });
    let drop = params
        .initial_streamers
        .iter()
        .any(|streamer| streamer.settings.claim_drops)
        .then(|| {
            spawn_drop_claim_loop(
                params.stop_rx.clone(),
                Arc::clone(params.twitch),
                params.observability.clone(),
            )
        });
    let chat = Some(spawn_chat_manager_loop(
        params.stop_rx,
        params.runtime.clone(),
        params.auth_token.to_string(),
        username,
        params.config.disable_at_in_nickname,
        params.observability.clone(),
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
