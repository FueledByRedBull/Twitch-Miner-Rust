use anyhow::{Context, Result};
use tm_config::ConfigFile;
use tm_domain::{Game, Streamer};
use tm_observability::{Event as DiscordEvent, LoggerSettings};
use tm_twitch::TwitchClient;

use crate::context::{apply_context_to_streamer, contribute_streamer_community_goals};
use crate::identity_cache::IdentityCache;
use crate::observability::AppObservability;
use crate::streak_cache::StreakCache;
use crate::streak_recovery::milestone_resolves_current_stream;

pub(crate) fn build_logger_settings(config: &ConfigFile) -> LoggerSettings {
    LoggerSettings {
        save: config.save_logs,
        emoji: config.emojis,
        show_seconds: config.show_seconds,
        console_username: config.show_username_in_console,
        show_claimed_bonus: config.show_claimed_bonus_msg,
        debug: config.debug,
        debug_deep: config.debug_deep,
        anonymize_logs: config.privacy.anonymize_logs,
    }
}

pub(crate) fn build_canary_logger_settings(config: &ConfigFile) -> LoggerSettings {
    let mut settings = build_logger_settings(config);
    settings.save = false;
    settings
}

#[cfg(test)]
pub(crate) async fn bootstrap_runtime_state(
    config: &ConfigFile,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
    streak_cache: &mut StreakCache,
) -> Result<tm_runtime::RuntimeState> {
    let mut identity_cache = IdentityCache::default();
    bootstrap_runtime_state_with_identity_cache(
        config,
        twitch,
        user_id,
        started_at,
        observability,
        streak_cache,
        &mut identity_cache,
    )
    .await
}

pub(crate) async fn bootstrap_runtime_state_with_identity_cache(
    config: &ConfigFile,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
    streak_cache: &mut StreakCache,
    identity_cache: &mut IdentityCache,
) -> Result<tm_runtime::RuntimeState> {
    let targets = load_targets(config, twitch).await?;
    let mut state = tm_runtime::RuntimeState::from_targets(config, &targets, started_at);
    tracing::info!(
        operation = "run",
        "{}",
        observability.loading_streamers_message(state.streamers.len())
    );
    if !state.streamers.is_empty() {
        twitch
            .update_client_version()
            .await
            .context("load Twitch client version")?;
    }

    let configured = state.streamers.len();
    let mut skipped = Vec::new();
    for (index, streamer) in state.streamers.iter_mut().enumerate() {
        if let Err(error) = bootstrap_streamer(
            streamer,
            twitch,
            user_id,
            started_at,
            observability,
            streak_cache,
            identity_cache,
        )
        .await
        {
            if !is_permanent_streamer_failure(&error) {
                return Err(error);
            }
            tracing::warn!(
                operation = "run",
                error_class = "streamer-skipped",
                %error,
                "skipping a streamer that could not be loaded; it stays skipped until restart"
            );
            skipped.push(index);
        }
    }
    if configured > 0 && skipped.len() == configured {
        anyhow::bail!("no configured streamer could be loaded");
    }
    for index in skipped.into_iter().rev() {
        state.streamers.remove(index);
    }
    state.capture_initial_points();
    Ok(state)
}

/// A renamed, banned, or malformed channel fails the same way on every retry,
/// so it is skipped. Network, rate-limit, server, auth, and integrity failures
/// are not specific to one streamer and still abort startup for a clean retry.
/// So does a connection dropped mid-response or a truncated body.
fn is_permanent_streamer_failure(error: &anyhow::Error) -> bool {
    use tm_twitch::TwitchClientError;
    match error.downcast_ref::<TwitchClientError>() {
        Some(TwitchClientError::Http(_) | TwitchClientError::Json(_)) => false,
        Some(error) => error.failure_class() == tm_twitch::TwitchFailureClass::Other,
        None => true,
    }
}

/// Startup claims and contributions are opportunistic: Twitch declining one
/// says nothing about whether the channel can be mined.
fn is_rejected_mutation(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<tm_twitch::TwitchClientError>(),
        Some(tm_twitch::TwitchClientError::MutationRejected { .. })
    )
}

pub(crate) async fn load_targets(
    config: &ConfigFile,
    twitch: &TwitchClient,
) -> Result<Vec<String>> {
    if !config.streamers.is_empty() {
        return Ok(config.streamers.clone());
    }
    twitch
        .fetch_followers(100, config.followers_order.as_str())
        .await
        .context("load followers")
}

pub(crate) async fn bootstrap_streamer(
    streamer: &mut Streamer,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
    streak_cache: &mut StreakCache,
    identity_cache: &mut IdentityCache,
) -> Result<()> {
    let configured_login = streamer.username.clone();
    let (channel_id, context, verified_login) =
        resolve_startup_identity(&configured_login, twitch, identity_cache, started_at).await?;
    streamer.username = verified_login;
    streamer.channel_id = channel_id;
    bootstrap_channel_context(streamer, twitch, user_id, observability, context).await?;
    bootstrap_presence(streamer, twitch, started_at, observability, streak_cache).await
}

async fn resolve_startup_identity(
    configured_login: &str,
    twitch: &TwitchClient,
    identity_cache: &mut IdentityCache,
    verified_at: tm_runtime::RuntimeTime,
) -> Result<(String, tm_twitch::ChannelPointsContext, String)> {
    let configured_login = tm_auth::normalize_username(configured_login)
        .with_context(|| format!("validate startup login {configured_login}"))?;
    let initial_resolution = tokio::try_join!(
        async {
            twitch
                .fetch_channel_id(&configured_login)
                .await
                .with_context(|| format!("load channel id for {configured_login}"))
        },
        async {
            twitch
                .fetch_channel_points_context(&configured_login)
                .await
                .with_context(|| format!("load channel points context for {configured_login}"))
        }
    );
    match initial_resolution {
        Ok((channel_id, context)) => {
            identity_cache.record(
                &configured_login,
                &channel_id,
                &configured_login,
                verified_at,
            );
            Ok((channel_id, context, configured_login))
        }
        Err(initial_error) => {
            let Some(cached) = identity_cache.lookup(&configured_login, verified_at) else {
                return Err(initial_error);
            };
            let verified_login = twitch
                .fetch_channel_login_by_id(&cached.channel_id)
                .await
                .with_context(|| {
                    format!(
                        "resolve cached channel identity for configured login {configured_login}"
                    )
                })?;
            let context = twitch
                .fetch_channel_points_context(&verified_login)
                .await
                .with_context(|| format!("load channel points context for {verified_login}"))?;
            identity_cache.record(
                &configured_login,
                &cached.channel_id,
                &verified_login,
                verified_at,
            );
            tracing::debug!(
                error = %initial_error,
                "startup configured-login lookup failed; recovered through cached channel identity"
            );
            Ok((cached.channel_id, context, verified_login))
        }
    }
}

async fn bootstrap_channel_context(
    streamer: &mut Streamer,
    twitch: &TwitchClient,
    user_id: Option<&str>,
    observability: &AppObservability,
    mut context: tm_twitch::ChannelPointsContext,
) -> Result<()> {
    apply_context_to_streamer(streamer, &context);

    if streamer.can_earn_channel_points() {
        if let Some(claim_id) = context.claim_id.as_deref() {
            let claimed = twitch
                .claim_bonus(&streamer.channel_id, claim_id, user_id)
                .await
                .with_context(|| format!("claim startup bonus for {}", streamer.username));
            match claimed {
                Err(error) if is_rejected_mutation(&error) => {
                    tracing::warn!(error_class = "startup-claim-rejected", %error, "startup bonus claim was rejected");
                }
                Err(error) => return Err(error),
                Ok(_) => {
                    if observability.show_claimed_bonus {
                        let message = observability.bonus_claim_message(streamer, true);
                        tracing::info!(operation = "claim_bonus", "{message}");
                        observability.spawn_event(DiscordEvent::BonusClaim, message);
                    }
                    context = twitch
                        .fetch_channel_points_context(&streamer.username)
                        .await
                        .with_context(|| {
                            format!("refresh claimed bonus context for {}", streamer.username)
                        })?;
                    apply_context_to_streamer(streamer, &context);
                }
            }
        }
    }

    let contributed = match contribute_streamer_community_goals(twitch, streamer).await {
        Err(error) if is_rejected_mutation(&error) => {
            tracing::warn!(error_class = "startup-goal-rejected", %error, "startup community goal contribution was rejected");
            false
        }
        result => result?,
    };
    if contributed {
        context = twitch
            .fetch_channel_points_context(&streamer.username)
            .await
            .with_context(|| format!("refresh channel points context for {}", streamer.username))?;
        apply_context_to_streamer(streamer, &context);
    }

    Ok(())
}

async fn bootstrap_presence(
    streamer: &mut Streamer,
    twitch: &TwitchClient,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
    streak_cache: &mut StreakCache,
) -> Result<()> {
    let is_live = twitch
        .is_stream_live(&streamer.channel_id)
        .await
        .with_context(|| format!("check live state for {}", streamer.username))?;
    streamer.presence_known = true;
    streamer.is_online = is_live;
    if is_live {
        bootstrap_online_stream(streamer, twitch, started_at, observability, streak_cache).await?;
    } else {
        streamer.online_at = None;
        streamer.offline_at = Some(started_at);
        tracing::info!(
            operation = "set_offline",
            "{}",
            observability.offline_message(streamer)
        );
    }

    Ok(())
}

async fn bootstrap_online_stream(
    streamer: &mut Streamer,
    twitch: &TwitchClient,
    started_at: tm_runtime::RuntimeTime,
    observability: &AppObservability,
    streak_cache: &mut StreakCache,
) -> Result<()> {
    streamer.online_at = Some(started_at);
    streamer.offline_at = None;
    let info = twitch
        .fetch_stream_info(&streamer.username)
        .await
        .with_context(|| format!("load stream info for {}", streamer.username))?;
    let recover_watch_streak = streamer.settings.watch_streak && streamer.can_earn_channel_points();
    let stream = streamer
        .stream
        .get_or_insert_with(tm_domain::Stream::default);
    stream.stream_up_at = Some(started_at);
    stream.update(
        &info.id,
        &info.title,
        Game::from_name(&info.game_name),
        info.game_id.clone(),
        &info.tags,
        info.viewers_count,
        tm_twitch::DROP_ID,
        started_at,
    );
    if recover_watch_streak {
        reconcile_startup_watch_streak(
            twitch,
            &streamer.channel_id,
            stream,
            info.created_at,
            started_at,
            streak_cache,
        )
        .await;
    }
    tracing::info!(
        operation = "set_online",
        "{}",
        observability.online_message(streamer)
    );
    Ok(())
}

async fn reconcile_startup_watch_streak(
    twitch: &TwitchClient,
    channel_id: &str,
    stream: &mut tm_domain::Stream,
    broadcast_created_at: Option<tm_runtime::RuntimeTime>,
    started_at: tm_runtime::RuntimeTime,
    streak_cache: &mut StreakCache,
) {
    match twitch.fetch_watch_streak_milestone(channel_id).await {
        Ok(Some(milestone)) => {
            streak_cache.record_milestone(
                channel_id,
                milestone.value,
                milestone.achievement_timestamp,
                milestone.expires_at,
                started_at,
            );
            stream.watch_streak_count = milestone.value;
            stream.watch_streak_resolved_at = Some(milestone.achievement_timestamp);
            stream.watch_streak_expires_at = milestone.expires_at;
            if broadcast_created_at.is_some_and(|created_at| {
                milestone_resolves_current_stream(&milestone, created_at, started_at)
            }) {
                stream.watch_streak_missing = false;
            }
        }
        Ok(None) => {
            streak_cache.apply_to_stream(channel_id, stream, broadcast_created_at, started_at);
        }
        Err(error) => {
            tracing::debug!(
                failure_class = ?error.failure_class(),
                "watch streak startup reconciliation unavailable"
            );
            streak_cache.apply_to_stream(channel_id, stream, broadcast_created_at, started_at);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use tm_domain::OffsetDateTime;
    use tm_twitch::{TwitchClient, TwitchEndpoints};

    use super::*;

    fn ts(unix: i64) -> OffsetDateTime {
        match OffsetDateTime::from_unix_timestamp(unix) {
            Ok(value) => value,
            Err(error) => panic!("invalid fixture timestamp: {error}"),
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        let header_end = loop {
            let read = stream
                .read(&mut buffer)
                .expect("read identity test request");
            assert!(read > 0, "identity test client closed before headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buffer).expect("read identity test body");
            assert!(read > 0, "identity test client closed before body");
            request.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn spawn_identity_server(
        recover_through_cached_id: bool,
    ) -> (
        TwitchEndpoints,
        Arc<Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind identity test server");
        let address = listener.local_addr().expect("identity test server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let expected_requests = if recover_through_cached_id { 5 } else { 3 };
        let server = thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept identity test request");
                let request = read_http_request(&mut stream);
                recorded
                    .lock()
                    .expect("lock identity test requests")
                    .push(request.clone());
                let response = if request.starts_with("GET / ") {
                    http_response(
                        "200 OK",
                        r#"<!doctype html><script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#,
                    )
                } else if request.contains(r#""operationName":"GetIDFromLogin"#) {
                    if recover_through_cached_id {
                        http_response("200 OK", r#"{"data":{"user":null}}"#)
                    } else {
                        http_response("200 OK", r#"{"data":{"user":{"id":"100"}}}"#)
                    }
                } else if request.contains(r#""operationName":"ResolveLoginById"#) {
                    http_response(
                        "200 OK",
                        r#"{"data":{"user":{"id":"100","login":"Renamed"}}}"#,
                    )
                } else if request.contains(r#""operationName":"ChannelPointsContext"#) {
                    http_response(
                        "200 OK",
                        include_str!("../../../tests/fixtures/twitch.channel_points_context.json"),
                    )
                } else {
                    panic!("unexpected identity test request: {request}");
                };
                stream
                    .write_all(&response)
                    .expect("write identity test response");
            }
        });
        (
            TwitchEndpoints {
                twitch_url: format!("http://{address}"),
                gql_url: format!("http://{address}/gql"),
                playback_url: format!("http://{address}/hls/"),
            },
            requests,
            server,
        )
    }

    fn test_twitch(endpoints: TwitchEndpoints) -> TwitchClient {
        TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("build identity test client"),
            "token",
            "ua",
            endpoints,
        )
    }

    #[tokio::test]
    async fn startup_identity_keeps_configured_reads_parallel_on_common_path() -> Result<()> {
        let (endpoints, requests, server) = spawn_identity_server(false);
        let twitch = test_twitch(endpoints);
        twitch.update_client_version().await?;
        let mut cache = IdentityCache::default();

        let (channel_id, context, verified_login) =
            resolve_startup_identity("alice", &twitch, &mut cache, ts(1)).await?;
        server
            .join()
            .map_err(|_| anyhow!("identity test server panicked"))?;

        assert_eq!(channel_id, "100");
        assert_eq!(verified_login, "alice");
        assert_eq!(context.balance, 1234);
        let requests = requests
            .lock()
            .map_err(|_| anyhow!("identity requests poisoned"))?;
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|request| !request.contains(r#""operationName":"ResolveLoginById"#)));
        assert_eq!(
            cache.lookup("alice", ts(2)).map(|entry| entry.channel_id),
            Some("100".into())
        );
        Ok(())
    }

    #[tokio::test]
    async fn startup_identity_recovers_a_renamed_channel_after_fresh_lookup_failure() -> Result<()>
    {
        let (endpoints, requests, server) = spawn_identity_server(true);
        let twitch = test_twitch(endpoints);
        twitch.update_client_version().await?;
        let mut cache = IdentityCache::default();
        cache.record("alice", "100", "alice", ts(1_000));

        let (channel_id, context, verified_login) =
            resolve_startup_identity("alice", &twitch, &mut cache, ts(1_001)).await?;
        server
            .join()
            .map_err(|_| anyhow!("identity test server panicked"))?;

        assert_eq!(channel_id, "100");
        assert_eq!(verified_login, "renamed");
        assert_eq!(context.balance, 1234);
        let requests = requests
            .lock()
            .map_err(|_| anyhow!("identity requests poisoned"))?;
        assert_eq!(requests.len(), 5);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains(r#""operationName":"ResolveLoginById"#))
                .count(),
            1
        );
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""channelLogin":"alice"#)));
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""channelLogin":"renamed"#)));
        assert_eq!(
            cache
                .lookup("alice", ts(1_002))
                .map(|entry| entry.verified_login),
            Some(String::from("renamed"))
        );
        Ok(())
    }

    #[test]
    fn only_streamer_specific_failures_are_skipped() {
        use tm_twitch::{TwitchClientError, TwitchFailureClass};
        let remote = |failure| {
            anyhow::Error::new(TwitchClientError::RemoteRequest {
                context: "test",
                failure,
            })
            .context("load channel id for tester")
        };
        assert!(super::is_permanent_streamer_failure(&anyhow::Error::new(
            TwitchClientError::MissingField("data.user")
        )));
        assert!(super::is_permanent_streamer_failure(&anyhow::anyhow!(
            "validate startup login"
        )));
        let truncated = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(!super::is_permanent_streamer_failure(&anyhow::Error::new(
            TwitchClientError::Json(truncated)
        )));
        let rejected = anyhow::Error::new(TwitchClientError::MutationRejected {
            context: String::from("test"),
            detail: String::from("test"),
        })
        .context("claim startup bonus for tester");
        assert!(super::is_rejected_mutation(&rejected));
        for failure in [
            TwitchFailureClass::Unauthorized,
            TwitchFailureClass::RateLimited,
            TwitchFailureClass::ServerError,
            TwitchFailureClass::Timeout,
            TwitchFailureClass::ConnectionReset,
            TwitchFailureClass::IntegrityRequired,
        ] {
            assert!(!super::is_permanent_streamer_failure(&remote(failure)));
        }
    }
}
