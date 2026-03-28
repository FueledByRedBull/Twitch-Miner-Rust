use std::sync::Arc;

use tm_domain::Streamer;
use tm_twitch::TwitchClient;

use crate::AppObservability;

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
