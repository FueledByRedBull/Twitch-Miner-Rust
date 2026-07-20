#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant as StdInstant};

    use crate::bootstrap::{
        env_has_value, has_override, load_config_with_fallback_using,
        load_or_login_session_with_auth_client, load_or_login_session_with_auth_client_and_retry,
        normalized_username, preview_config_with_fallback, should_fallback_to_user_config,
        validate_timezone_override, TimezoneValidation, READ_ONLY_FILE_SYSTEM_ERROR,
    };
    use crate::context::{refresh_snapshot_streamers, spawn_pending_claim_loop};
    use crate::drops::{claim_available_drops, drop_is_claimable};
    use crate::minute_watcher::{
        build_minute_watched_event, refresh_watch_selection_metadata, resolve_spade_url,
        send_minute_watched_for_streamer, send_minute_watched_with_spade_cache,
    };
    use crate::observability::{format_resume_gap, streamer_game_name, AppObservability};
    use crate::prediction::prediction_wait_duration;
    use crate::pubsub::pubsub_reconnect_delay;
    use crate::startup::{bootstrap_runtime_state, build_canary_logger_settings, load_targets};
    use crate::status::HealthTracker;
    use crate::utilities::new_session_id;
    use crate::watching::{minute_watcher_resume_gap, CachedSpadeUrl, SpadeCacheEntry};
    use crate::Cli;
    use clap::Parser;
    use reqwest::StatusCode;
    use tm_auth::{AuthEndpoints, AuthSession, CookieStore, TwitchAuthClient};
    use tm_config::{AppPaths, ConfigError, ConfigFile};
    use tm_domain::{
        BetSettings, DelayMode, Game, OffsetDateTime, PredictionDecision, PredictionEvent,
        PredictionOutcome, Streamer,
    };
    use tm_observability::DiscordClient;
    use tm_twitch::{InventoryDrop, TwitchClient, TwitchEndpoints};

    fn ts(seconds: i64) -> tm_runtime::RuntimeTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    fn unique_temp_dir() -> PathBuf {
        env::temp_dir().join(format!(
            "tm-app-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn fixture_json(name: &str) -> String {
        fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn operator_cli_requires_explicit_config_json_and_status_modes() {
        let status = Cli::try_parse_from(["tm-app", "--status"]).unwrap();
        assert!(status.status);
        assert!(!status.json);

        let config_json = Cli::try_parse_from(["tm-app", "--check-config", "--json"]).unwrap();
        assert!(config_json.check_config);
        assert!(config_json.json);

        assert!(Cli::try_parse_from(["tm-app", "--json"]).is_err());
        assert!(Cli::try_parse_from(["tm-app", "--status", "--check-config"]).is_err());
    }

    #[test]
    fn canary_logger_never_opens_a_log_file() {
        let config = ConfigFile {
            save_logs: true,
            ..ConfigFile::default()
        };

        assert!(!build_canary_logger_settings(&config).save);
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut header_end = None;
        let mut content_length = 0_usize;

        let read_started = std::time::Instant::now();
        loop {
            let read = match stream.read(&mut chunk) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if read_started.elapsed() >= Duration::from_secs(2) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("read test request failed: {error}"),
            };
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if header_end.is_none() {
                header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                if let Some(position) = header_end {
                    let headers = String::from_utf8_lossy(&buffer[..position + 4]);
                    content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("Content-Length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or_default();
                    if buffer.len() >= position + 4 + content_length {
                        break;
                    }
                }
            } else if let Some(position) = header_end {
                if buffer.len() >= position + 4 + content_length {
                    break;
                }
            }
        }

        String::from_utf8(buffer).unwrap()
    }

    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn empty_http_response(status: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
    }

    fn spawn_auth_server() -> (AuthEndpoints, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                requests.push(request);
                let response = match index {
                    0 => http_response(
                        "200 OK",
                        r#"{"device_code":"device-code","user_code":"ABCD","interval":0,"expires_in":60}"#,
                    ),
                    1 => http_response(
                        "400 Bad Request",
                        r#"{"status":400,"message":"authorization_pending"}"#,
                    ),
                    2 => http_response("200 OK", r#"{"access_token":"token-123"}"#),
                    3 => http_response(
                        "200 OK",
                        r#"{"login":"tester","user_id":"user-123","scopes":["channel:read:predictions"]}"#,
                    ),
                    _ => unreachable!(),
                };
                stream.write_all(&response).unwrap();
            }
            requests
        });

        (
            AuthEndpoints {
                device_code_url: format!("http://{address}/oauth2/device"),
                token_url: format!("http://{address}/oauth2/token"),
                validate_url: format!("http://{address}/oauth2/validate"),
            },
            handle,
        )
    }

    fn spawn_auth_validation_server(
        responses: Vec<Vec<u8>>,
    ) -> (
        AuthEndpoints,
        Arc<Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                recorded
                    .lock()
                    .unwrap()
                    .push(read_http_request(&mut stream));
                stream.write_all(&response).unwrap();
            }
        });
        let validate_url = format!("http://{address}/oauth2/validate");
        (
            AuthEndpoints {
                device_code_url: validate_url.clone(),
                token_url: validate_url.clone(),
                validate_url,
            },
            requests,
            handle,
        )
    }

    fn spawn_twitch_server(
        expected_requests: usize,
    ) -> (
        TwitchEndpoints,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                recorded.lock().unwrap().push(request.clone());
                let response = if request.starts_with("GET / ") {
                    http_response(
                        "200 OK",
                        r#"<!doctype html><script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#,
                    )
                } else if request.contains(r#""operationName":"ChannelFollows""#) {
                    http_response(
                        "200 OK",
                        r#"{"data":{"user":{"follows":{"edges":[{"node":{"login":"alice"},"cursor":"cursor-1"},{"node":{"login":"bob"},"cursor":"cursor-2"}],"pageInfo":{"hasNextPage":false}}}}}"#,
                    )
                } else if request.contains(r#""operationName":"GetIDFromLogin""#) {
                    http_response("200 OK", r#"{"data":{"user":{"id":"100"}}}"#)
                } else if request.contains(r#""operationName":"ChannelPointsContext""#) {
                    http_response(
                        "200 OK",
                        &fixture_json("twitch.channel_points_context.json"),
                    )
                } else if request.contains(r#""operationName":"ClaimCommunityPoints""#) {
                    http_response(
                        "200 OK",
                        r#"{"data":{"claimCommunityPoints":{"balance":1550}}}"#,
                    )
                } else if request.contains(r#""operationName":"WithIsStreamLiveQuery""#) {
                    http_response(
                        "200 OK",
                        r#"{"data":{"user":{"stream":{"id":"stream-1"}}}}"#,
                    )
                } else if request
                    .contains(r#""operationName":"VideoPlayerStreamInfoOverlayChannel""#)
                {
                    http_response("200 OK", &fixture_json("twitch.stream_info.json"))
                } else if request.contains(r#""operationName":"RewardList""#) {
                    http_response("200 OK", &fixture_json("twitch.reward_list.json"))
                } else {
                    panic!("unexpected request: {request}");
                };
                stream.write_all(&response).unwrap();
            }
        });

        (
            TwitchEndpoints {
                twitch_url: format!("http://{address}"),
                gql_url: format!("http://{address}/gql"),
            },
            requests,
            handle,
        )
    }

    fn spawn_status_server(
        statuses: Vec<&'static str>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let handle = thread::spawn(move || {
            for status in statuses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                recorded.lock().unwrap().push(request);
                stream.write_all(&empty_http_response(status)).unwrap();
            }
        });
        (format!("http://{address}/spade"), requests, handle)
    }

    fn spawn_json_response_server(
        responses: Vec<String>,
    ) -> (
        TwitchEndpoints,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let mut responses = std::collections::VecDeque::from(responses);
            while !responses.is_empty() {
                let wait_started = std::time::Instant::now();
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if wait_started.elapsed() >= Duration::from_secs(5) {
                                return;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept json test request failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let request = read_http_request(&mut stream);
                recorded.lock().unwrap().push(request);
                let latest_request = recorded.lock().unwrap().last().cloned().unwrap_or_default();
                let response = if latest_request.starts_with("GET / ") {
                    http_response(
                        "200 OK",
                        r#"<!doctype html><script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#,
                    )
                } else {
                    let body = responses.pop_front().unwrap();
                    http_response("200 OK", &body)
                };
                stream.write_all(&response).unwrap();
            }
        });
        (
            TwitchEndpoints {
                twitch_url: format!("http://{address}"),
                gql_url: format!("http://{address}/gql"),
            },
            requests,
            handle,
        )
    }

    fn test_observability() -> AppObservability {
        AppObservability::new(
            None,
            DiscordClient::new(Duration::from_secs(1)).unwrap(),
            false,
            false,
            false,
            true,
        )
    }

    #[test]
    fn env_has_value_ignores_missing_and_blank_values() {
        let key = format!(
            "TM_APP_TEST_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        env::remove_var(&key);
        assert!(!env_has_value(&key));
        env::set_var(&key, "   ");
        assert!(!env_has_value(&key));
        env::set_var(&key, "value");
        assert!(env_has_value(&key));
        env::remove_var(&key);
    }

    #[test]
    fn cli_override_detection_matches_path_and_env_inputs() {
        let cli = Cli {
            config: None,
            data_dir: None,
            health: false,
            check_config: false,
            status: false,
            json: false,
            support_bundle: None,
            canary: false,
        };
        assert!(!has_override(&cli));

        let cli = Cli {
            config: Some(PathBuf::from("config.json")),
            data_dir: None,
            health: false,
            check_config: false,
            status: false,
            json: false,
            support_bundle: None,
            canary: false,
        };
        assert!(has_override(&cli));
    }

    #[test]
    fn should_fallback_to_user_config_matches_go_permission_cases() {
        let permission = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        assert!(should_fallback_to_user_config(&permission));

        let read_only = io::Error::from_raw_os_error(READ_ONLY_FILE_SYSTEM_ERROR);
        assert!(should_fallback_to_user_config(&read_only));

        let missing = io::Error::new(io::ErrorKind::NotFound, "missing");
        assert!(!should_fallback_to_user_config(&missing));
    }

    #[test]
    fn config_fallback_switches_active_work_dir_and_config_path() {
        let requested_dir = unique_temp_dir();
        let fallback_dir = unique_temp_dir();
        fs::create_dir_all(&requested_dir).unwrap();

        let requested_paths = AppPaths {
            work_dir: requested_dir.clone(),
            config_path: requested_dir.join("config.json"),
        };
        let loaded = load_config_with_fallback_using(
            &requested_paths,
            false,
            || Some(fallback_dir.clone()),
            |path| {
                if path == requested_paths.config_path {
                    return Err(ConfigError::Io(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "read only",
                    )));
                }
                if path == fallback_dir.join("config.json") {
                    return Ok(ConfigFile {
                        username: String::from("tester"),
                        ..ConfigFile::default()
                    });
                }
                panic!("unexpected config path: {}", path.display());
            },
        )
        .unwrap();

        assert_eq!(loaded.active_paths.work_dir, fallback_dir);
        assert_eq!(
            loaded.active_paths.config_path,
            fallback_dir.join("config.json")
        );
        assert_eq!(loaded.config.username, "tester");

        fs::remove_dir_all(&requested_dir).unwrap();
        fs::remove_dir_all(&fallback_dir).unwrap();
    }

    #[test]
    fn canary_config_preview_does_not_persist_migration() {
        let requested_dir = unique_temp_dir();
        fs::create_dir_all(&requested_dir).unwrap();
        let config_path = requested_dir.join("config.json");
        let original = br#"{"username":"tester"}"#;
        fs::write(&config_path, original).unwrap();
        let requested_paths = AppPaths {
            work_dir: requested_dir.clone(),
            config_path: config_path.clone(),
        };

        let loaded = preview_config_with_fallback(&requested_paths, true).unwrap();

        assert_eq!(loaded.config.username, "tester");
        assert_eq!(fs::read(&config_path).unwrap(), original);
        assert!(!config_path.with_extension("json.bak").exists());

        fs::remove_dir_all(&requested_dir).unwrap();
    }

    #[test]
    fn new_session_id_is_stable_shape() {
        let session_id = new_session_id();
        assert!(session_id.contains('-'));
        assert!(!session_id.ends_with('-'));
    }

    #[test]
    fn normalized_username_rejects_default_placeholder() {
        assert!(normalized_username("your-twitch-username").is_err());
        assert_eq!(normalized_username(" Alice ").unwrap(), "alice");
    }

    #[test]
    fn drop_is_claimable_requires_unclaimed_completed_drop() {
        let claimable = InventoryDrop {
            drop_instance_id: "drop-1".into(),
            reward_name: "Reward".into(),
            campaign_name: "Campaign".into(),
            current_minutes_watched: 60,
            required_minutes_watched: 60,
            is_claimed: false,
        };
        assert!(drop_is_claimable(&claimable));

        let claimed = InventoryDrop {
            is_claimed: true,
            ..claimable.clone()
        };
        assert!(!drop_is_claimable(&claimed));

        let incomplete = InventoryDrop {
            current_minutes_watched: 59,
            ..claimable.clone()
        };
        assert!(!drop_is_claimable(&incomplete));

        let missing_requirement = InventoryDrop {
            required_minutes_watched: 0,
            current_minutes_watched: 0,
            ..claimable
        };
        assert!(!drop_is_claimable(&missing_requirement));
    }

    #[test]
    fn streamer_game_name_prefers_display_name() {
        let streamer = Streamer {
            stream: Some(tm_domain::Stream {
                game: Some(Game {
                    display_name: Some(String::from("VALORANT")),
                    name: Some(String::from("valorant")),
                }),
                ..tm_domain::Stream::default()
            }),
            ..Streamer::default()
        };
        assert_eq!(
            streamer_game_name(&streamer),
            Some(String::from("VALORANT"))
        );
    }

    #[test]
    fn observability_online_message_includes_game_when_enabled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            channel_points: 1_250,
            stream: Some(tm_domain::Stream {
                game: Some(Game {
                    display_name: Some(String::from("VALORANT")),
                    name: Some(String::from("valorant")),
                }),
                ..tm_domain::Stream::default()
            }),
            ..Streamer::default()
        };

        assert_eq!(
            observability.online_message(&streamer),
            "🥳 Streamer(username=alice, channel_id=100, channel_points=1250) is Online! | Playing: VALORANT"
        );
    }

    #[test]
    fn observability_game_change_requires_enabled_distinct_games() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            ..Streamer::default()
        };

        assert_eq!(
            observability.game_change_message(&streamer, "Just Chatting", "VALORANT"),
            Some(String::from("🎮 alice now playing: VALORANT!"))
        );
        assert_eq!(
            observability.game_change_message(&streamer, "VALORANT", "valorant"),
            None
        );
    }

    #[test]
    fn observability_points_message_matches_sample_shape() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            false,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            channel_points: 1_250,
            stream: Some(tm_domain::Stream {
                game: Some(Game {
                    display_name: Some(String::from("VALORANT")),
                    name: Some(String::from("valorant")),
                }),
                ..tm_domain::Stream::default()
            }),
            ..Streamer::default()
        };

        assert_eq!(
            observability.points_earned_message(&streamer, 10, "watch"),
            "🚀 +10 → Streamer(username=alice, channel_id=100, channel_points=1250) - Reason: WATCH | Game: VALORANT"
        );
    }

    #[test]
    fn observability_claim_messages_are_styled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let streamer = Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            channel_points: 1_250,
            ..Streamer::default()
        };

        assert_eq!(
            observability.bonus_claim_message(&streamer, false),
            "🎁 Claimed bonus → Streamer(username=alice, channel_id=100, channel_points=1250)"
        );
        assert_eq!(
            observability.bonus_claim_message(&streamer, true),
            "🎁 Claimed startup bonus → Streamer(username=alice, channel_id=100, channel_points=1250)"
        );
    }

    #[test]
    fn observability_startup_messages_are_styled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );

        assert_eq!(
            observability.start_session_message("session-123"),
            "💣 Start session: 'session-123'"
        );
        assert_eq!(
            observability.loading_streamers_message(16),
            "🤓 Loading data for 16 streamers. Please wait ..."
        );
        assert_eq!(
            observability.loaded_streamers_message(16, Duration::from_millis(20_500)),
            "✅ 16 Streamer loaded! (20.5 seconds)"
        );
    }

    #[test]
    fn python_style_messages_remain_redacted_in_privacy_mode() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            true,
            true,
            true,
            true,
        );
        let streamer = Streamer {
            username: String::from("private-user"),
            channel_id: String::from("private-channel-id"),
            channel_points: 42_424,
            ..Streamer::default()
        };
        let event = PredictionEvent {
            streamer: streamer.clone(),
            event_id: String::from("private-event-id"),
            title: String::from("private title"),
            status: String::from("ACTIVE"),
            created_at: ts(1),
            window_seconds: 30.0,
            outcomes: Vec::new(),
            decision: tm_domain::PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };

        let rendered = format!(
            "{} {} {} {}",
            observability.streamer_label(&streamer),
            observability.prediction_label(&event),
            observability.prediction_result_message(
                &event.event_id,
                &event.title,
                "WIN, Gained: +100"
            ),
            observability.join_raid_message("Streamer1", "private-raid-target\nforged")
        );
        for private in [
            "private-user",
            "private-channel-id",
            "private-event-id",
            "private title",
            "42_424",
            "42424",
            "Gained: +100",
            "private-raid-target",
            "forged",
        ] {
            assert!(!rendered.contains(private));
        }
        assert!(rendered.contains("Streamer(username=Streamer1, channel_id=[hidden]"));
        assert!(rendered.contains("event_id=[hidden]"));
    }

    #[test]
    fn observability_drop_claim_message_is_styled() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            false,
            true,
            true,
            true,
        );
        let drop = InventoryDrop {
            drop_instance_id: String::from("drop-1"),
            reward_name: String::from("60 min."),
            campaign_name: String::from("Crimson Desert Drops #2"),
            current_minutes_watched: 61,
            required_minutes_watched: 60,
            is_claimed: false,
        };

        assert_eq!(
            observability.drop_claim_message("periodic", &drop),
            "🎁 Claimed drop → 60 min. | Campaign: Crimson Desert Drops #2 | Progress: 61/60 (101%) | Mode: PERIODIC"
        );
    }

    #[test]
    fn drop_progress_message_is_typed_and_privacy_aware() {
        let observability = AppObservability::new(
            None,
            DiscordClient::new(std::time::Duration::from_secs(1)).unwrap(),
            true,
            false,
            true,
            true,
        );
        let drop = InventoryDrop {
            drop_instance_id: String::from("private-id"),
            reward_name: String::from("private-reward"),
            campaign_name: String::from("private-campaign"),
            current_minutes_watched: 15,
            required_minutes_watched: 60,
            is_claimed: false,
        };

        let message = observability.drop_progress_message(&drop);
        assert!(message.contains("Progress: 15/60 (25%)"));
        assert!(!message.contains("private"));
    }

    #[test]
    fn minute_watcher_resume_gap_uses_threshold() {
        assert_eq!(minute_watcher_resume_gap(ts(0), ts(599)), None);
        assert_eq!(
            minute_watcher_resume_gap(ts(0), ts(600)),
            Some(Duration::from_secs(600))
        );
        assert_eq!(format_resume_gap(Duration::from_secs(6_123)), "1h 42m 3s");
    }

    #[test]
    fn pubsub_reconnect_delay_distinguishes_requested_and_generic_retries() {
        let reconnect_requested = Ok(Err(tm_pubsub::PubSubError::ReconnectRequested));
        let generic_failure = Ok(Err(tm_pubsub::PubSubError::PongTimeout));
        let clean_close = Ok(Ok(()));

        assert_eq!(
            pubsub_reconnect_delay(&reconnect_requested, 0, 1, 1),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            pubsub_reconnect_delay(&generic_failure, 0, 1, 1),
            Some(Duration::from_secs(16))
        );
        assert_eq!(
            pubsub_reconnect_delay(&clean_close, 0, 1, 1),
            Some(Duration::from_secs(11))
        );
        assert_eq!(
            pubsub_reconnect_delay(&generic_failure, 0, 1, 6),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn timezone_validation_accepts_iana_names() {
        assert_eq!(
            validate_timezone_override(Some("Europe/Athens")),
            Some(TimezoneValidation::Valid(String::from("Europe/Athens")))
        );
        assert_eq!(
            validate_timezone_override(Some("not/a-timezone")),
            Some(TimezoneValidation::Invalid(String::from("not/a-timezone")))
        );
        assert_eq!(validate_timezone_override(Some("auto")), None);
    }

    #[test]
    fn prediction_wait_duration_uses_streamer_delay_settings() {
        let streamer = Streamer {
            settings: tm_domain::StreamerSettings {
                bet: BetSettings {
                    delay_mode: DelayMode::FromEnd,
                    delay: Some(15.0),
                    ..BetSettings::default()
                },
                ..tm_domain::StreamerSettings::default()
            },
            ..Streamer::default()
        };
        let event = PredictionEvent {
            streamer,
            event_id: String::from("event-1"),
            title: String::from("Prediction"),
            status: String::from("ACTIVE"),
            created_at: ts(0),
            window_seconds: 100.0,
            outcomes: vec![PredictionOutcome::default()],
            decision: PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };

        assert_eq!(
            prediction_wait_duration(&event, ts(10)),
            Duration::from_secs(75)
        );
    }

    #[test]
    fn prediction_wait_duration_fails_safe_for_invalid_delay_values() {
        let mut event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::from("event-1"),
            title: String::from("Prediction"),
            status: String::from("ACTIVE"),
            created_at: ts(0),
            window_seconds: 100.0,
            outcomes: vec![PredictionOutcome::default()],
            decision: PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };

        for (delay_mode, delay) in [
            (DelayMode::FromStart, -1.0),
            (DelayMode::Percentage, f64::INFINITY),
            (DelayMode::Percentage, f64::NEG_INFINITY),
        ] {
            event.streamer.settings.bet.delay_mode = delay_mode;
            event.streamer.settings.bet.delay = Some(delay);
            assert_eq!(prediction_wait_duration(&event, ts(10)), Duration::ZERO);
        }
    }

    #[tokio::test]
    async fn load_targets_uses_mocked_followers_in_follower_mode() {
        let (endpoints, requests, server) = spawn_twitch_server(2);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );
        let config = ConfigFile::default();

        let targets = load_targets(&config, &twitch).await.unwrap();
        assert_eq!(targets, vec![String::from("alice"), String::from("bob")]);

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests.iter().any(|request| request.starts_with("GET / ")));
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""operationName":"ChannelFollows""#)));
    }

    #[tokio::test]
    async fn bootstrap_runtime_state_claims_startup_bonus_in_manual_mode() {
        let (endpoints, requests, server) = spawn_twitch_server(7);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );
        let observability = test_observability();
        let config = ConfigFile {
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut streak_cache = crate::streak_cache::StreakCache::default();

        let state = bootstrap_runtime_state(
            &config,
            &twitch,
            Some("user-1"),
            ts(0),
            &observability,
            &mut streak_cache,
        )
        .await
        .unwrap();

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(state.streamers.len(), 1);
        assert_eq!(state.streamers[0].channel_id, "100");
        assert_eq!(state.streamers[0].channel_points, 1234);
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""operationName":"ClaimCommunityPoints""#)));
    }

    #[tokio::test]
    async fn mocked_login_and_bootstrap_flow_rehydrates_session_into_twitch_client() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();

        let (auth_endpoints, auth_server) = spawn_auth_server();
        let auth_client = TwitchAuthClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            auth_endpoints,
        );
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };

        let session = load_or_login_session_with_auth_client(&config, &temp_dir, &auth_client)
            .await
            .unwrap();
        assert_eq!(session.auth_token(), Some("token-123"));
        assert_eq!(session.user_id(), Some("user-123"));
        assert!(session.has_scope("channel:read:predictions"));

        let (twitch_endpoints, requests, twitch_server) = spawn_twitch_server(8);
        let twitch = TwitchClient::with_client_and_cookie_header_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            session.auth_token().unwrap(),
            "ua",
            session.cookie_header_for_host("twitch.tv"),
            twitch_endpoints,
        );

        let mut streak_cache = crate::streak_cache::StreakCache::default();
        let state = bootstrap_runtime_state(
            &config,
            &twitch,
            session.user_id(),
            ts(0),
            &test_observability(),
            &mut streak_cache,
        )
        .await
        .unwrap();

        auth_server.join().unwrap();
        twitch_server.join().unwrap();
        assert_eq!(state.streamers[0].username, "alice");
        assert!(
            !state.streamers[0]
                .stream
                .as_ref()
                .unwrap()
                .watch_streak_missing
        );
        assert_eq!(
            session.cookie_header_for_host("gql.twitch.tv").as_deref(),
            Some("auth-token=token-123; persistent=user-123")
        );
        assert!(requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(r#""operationName":"ChannelPointsContext""#)));

        fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[tokio::test]
    async fn saved_session_validation_recovers_without_device_login_after_transient_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut stored = AuthSession::new("tester", CookieStore::new());
        stored.set_auth_token("saved-token");
        stored.save_to_dir(directory.path()).unwrap();

        let responses = vec![
            Vec::new(),
            http_response(
                "200 OK",
                r#"{"login":"tester","user_id":"user-123","scopes":["channel:read:predictions"]}"#,
            ),
        ];
        let (endpoints, requests, server) = spawn_auth_validation_server(responses);
        let auth_client = TwitchAuthClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            endpoints,
        );
        let config = ConfigFile {
            username: String::from("tester"),
            ..ConfigFile::default()
        };

        let session = load_or_login_session_with_auth_client_and_retry(
            &config,
            directory.path(),
            &auth_client,
            Duration::ZERO,
            Duration::ZERO,
        )
        .await
        .unwrap();

        server.join().unwrap();
        assert_eq!(session.auth_token(), Some("saved-token"));
        assert_eq!(session.user_id(), Some("user-123"));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn refresh_snapshot_streamers_updates_runtime_context() {
        let (endpoints, requests, server) = spawn_twitch_server(3);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);

        refresh_snapshot_streamers(
            &runtime,
            &twitch,
            "user-1",
            &test_observability(),
            &HealthTracker::default(),
        )
        .await
        .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        server.join().unwrap();
        assert_eq!(snapshot.streamers[0].channel_points, 1234);
        assert!(requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.contains(r#""operationName":"ClaimCommunityPoints""#)));
    }

    #[tokio::test]
    async fn refresh_snapshot_streamers_deduplicates_pubsub_and_context_claim_ids() {
        let (endpoints, requests, server) = spawn_twitch_server(4);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);
        let health = HealthTracker::default();
        let observability = test_observability();

        let pubsub_effects = runtime
            .apply_event(
                tm_runtime::MinerEvent::ClaimAvailable {
                    channel_id: String::from("100"),
                    claim_id: String::from("claim-1"),
                },
                ts(1),
            )
            .await
            .unwrap();
        crate::runtime_effects::execute_runtime_effects(
            &runtime,
            &twitch,
            "user-1",
            pubsub_effects,
            &observability,
            health.clone(),
        )
        .await
        .unwrap();

        for _ in 0..2 {
            refresh_snapshot_streamers(&runtime, &twitch, "user-1", &observability, &health)
                .await
                .unwrap();
        }

        server.join().unwrap();
        let channel_points = runtime.state_snapshot().await.unwrap().streamers[0].channel_points;
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains(r#""operationName":"ClaimCommunityPoints""#))
                .count(),
            1
        );
        assert_eq!(channel_points, 1234);
    }

    #[tokio::test]
    async fn pending_claim_loop_waits_for_interval_before_refreshing() {
        let (endpoints, requests, server) = spawn_json_response_server(vec![
            fixture_json("twitch.channel_points_context.json"),
            serde_json::json!({
                "data": {
                    "claimCommunityPoints": {
                        "balance": 1550
                    }
                }
            })
            .to_string(),
        ]);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = spawn_pending_claim_loop(
            stop_rx,
            runtime.clone(),
            twitch,
            String::from("user-1"),
            test_observability(),
            HealthTracker::default(),
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runtime.state_snapshot().await.unwrap().streamers[0].channel_points,
            0
        );

        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn pending_claim_loop_stays_idle_without_immediate_bonus_sweep() {
        let (endpoints, requests, server) = spawn_json_response_server(vec![serde_json::json!({
            "data": {
                "community": {
                    "channel": {
                        "self": {
                            "communityPoints": {
                                "balance": 1234,
                                "availableClaim": null,
                                "activeMultipliers": []
                            }
                        },
                        "communityPointsSettings": {
                            "goals": []
                        }
                    }
                }
            }
        })
        .to_string()]);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = spawn_pending_claim_loop(
            stop_rx,
            runtime.clone(),
            twitch,
            String::from("user-1"),
            test_observability(),
            HealthTracker::default(),
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runtime.state_snapshot().await.unwrap().streamers[0].channel_points,
            0
        );

        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests.is_empty());
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn refresh_watch_selection_metadata_updates_candidate_choice() {
        let (endpoints, requests, server) = spawn_json_response_server(vec![
            fixture_json("twitch.stream_info.json"),
            fixture_json("twitch.available_drop_campaigns.json"),
        ]);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![
                String::from("alice"),
                String::from("bob"),
                String::from("carol"),
            ],
            watch_priority: vec![String::from("ORDER")],
            game_exclude: vec![String::from("game name")],
            ..ConfigFile::default()
        };
        let now = ts(300);
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![
            Streamer {
                username: String::from("alice"),
                channel_id: String::from("100"),
                is_online: true,
                presence_known: true,
                online_at: Some(ts(0)),
                settings: tm_domain::StreamerSettings {
                    farm_drops: true,
                    ..tm_domain::StreamerSettings::default()
                },
                stream: Some(tm_domain::Stream {
                    game: Some(Game::from_name("Chess")),
                    last_update: Some(ts(0)),
                    ..tm_domain::Stream::default()
                }),
                ..Streamer::default()
            },
            Streamer {
                username: String::from("bob"),
                channel_id: String::from("200"),
                is_online: true,
                presence_known: true,
                online_at: Some(ts(0)),
                stream: Some(tm_domain::Stream {
                    game: Some(Game::from_name("Chess")),
                    last_update: Some(now),
                    ..tm_domain::Stream::default()
                }),
                ..Streamer::default()
            },
            Streamer {
                username: String::from("carol"),
                channel_id: String::from("300"),
                is_online: true,
                presence_known: true,
                online_at: Some(ts(0)),
                stream: Some(tm_domain::Stream {
                    game: Some(Game::from_name("Chess")),
                    last_update: Some(now),
                    ..tm_domain::Stream::default()
                }),
                ..Streamer::default()
            },
        ];
        let runtime = tm_runtime::spawn_runtime_state(state);

        assert_eq!(
            runtime
                .state_snapshot()
                .await
                .unwrap()
                .watch_target_logins(now),
            vec![
                String::from("alice"),
                String::from("bob"),
                String::from("carol")
            ]
        );

        let stale_streamer = runtime.state_snapshot().await.unwrap().streamers[0].clone();
        refresh_watch_selection_metadata(
            &runtime,
            &twitch,
            &[stale_streamer],
            &test_observability(),
            now,
        )
        .await
        .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        let request_count_after_refresh = requests.lock().unwrap().len();
        refresh_watch_selection_metadata(
            &runtime,
            &twitch,
            &snapshot.streamers,
            &test_observability(),
            now,
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(
            snapshot.watch_target_logins(now),
            vec![String::from("bob"), String::from("carol")]
        );
        assert_eq!(
            snapshot.streamers[0]
                .stream
                .as_ref()
                .and_then(|stream| stream.game.as_ref())
                .and_then(|game| game.display_name.clone()),
            Some(String::from("Game Name"))
        );
        assert_eq!(
            snapshot.streamers[0]
                .stream
                .as_ref()
                .and_then(|stream| stream.drop_campaign_eligible),
            Some(true)
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), request_count_after_refresh);
        assert!(requests
            .iter()
            .any(|request| request
                .contains(r#""operationName":"VideoPlayerStreamInfoOverlayChannel""#)));
        assert!(requests
            .iter()
            .any(|request| request
                .contains(r#""operationName":"DropsHighlightService_AvailableDrops""#)));
    }

    #[tokio::test]
    async fn campaign_refresh_failure_preserves_still_applicable_known_result() {
        let (endpoints, _requests, server) = spawn_json_response_server(vec![
            fixture_json("twitch.stream_info.json"),
            String::from(r#"{"errors":[{"message":"temporary"}]}"#),
        ]);
        let twitch = Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        ));
        let now = ts(300);
        let state = tm_runtime::RuntimeState {
            started_at: ts(0),
            follower_mode: false,
            watch_priorities: vec![tm_domain::WatchPriority::Drops],
            game_priority: Vec::new(),
            game_exclusions: Vec::new(),
            streamers: vec![Streamer {
                username: String::from("alice"),
                channel_id: String::from("100"),
                is_online: true,
                presence_known: true,
                online_at: Some(ts(0)),
                settings: tm_domain::StreamerSettings {
                    farm_drops: true,
                    ..tm_domain::StreamerSettings::default()
                },
                stream: Some(tm_domain::Stream {
                    broadcast_id: String::from("stream-1"),
                    game: Some(Game::from_name("Game Name")),
                    drop_campaign_eligible: Some(true),
                    last_update: Some(ts(0)),
                    ..tm_domain::Stream::default()
                }),
                ..Streamer::default()
            }],
            initial_points: std::collections::HashMap::new(),
            predictions: std::collections::HashMap::new(),
            processed_prediction_ids: std::collections::VecDeque::new(),
            completed_predictions: std::collections::VecDeque::new(),
        };
        let runtime = tm_runtime::spawn_runtime_state(state);
        let streamer = runtime.state_snapshot().await.unwrap().streamers[0].clone();

        refresh_watch_selection_metadata(
            &runtime,
            &twitch,
            &[streamer],
            &test_observability(),
            now,
        )
        .await
        .unwrap();

        server.join().unwrap();
        let snapshot = runtime.state_snapshot().await.unwrap();
        assert_eq!(
            snapshot.streamers[0]
                .stream
                .as_ref()
                .and_then(|stream| stream.drop_campaign_eligible),
            Some(true)
        );
        assert_eq!(
            snapshot.watch_target_logins(now),
            vec![String::from("alice")]
        );
    }

    #[tokio::test]
    async fn claim_available_drops_rejects_invalid_claim_status() {
        let (endpoints, requests, server) = spawn_json_response_server(vec![
            serde_json::json!({
                "data": {
                    "currentUser": {
                        "inventory": {
                            "dropCampaignsInProgress": [{
                                "name": "Campaign",
                                "timeBasedDrops": [{
                                    "name": "Reward",
                                    "requiredMinutesWatched": 60,
                                    "self": {
                                        "dropInstanceID": "drop-1",
                                        "currentMinutesWatched": 60,
                                        "isClaimed": false
                                    }
                                }]
                            }]
                        }
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "data": {
                    "claimDropRewards": {
                        "status": "INELIGIBLE"
                    }
                }
            })
            .to_string(),
        ]);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );

        let error = claim_available_drops(&twitch, "periodic", &test_observability())
            .await
            .unwrap_err();

        server.join().unwrap();
        assert!(
            error.chain().any(|cause| cause
                .to_string()
                .contains("unexpected drop claim status INELIGIBLE")),
            "{error:?}"
        );
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn send_minute_watched_for_streamer_updates_presence_and_watch_progress() {
        let (endpoints, _requests, twitch_server) = spawn_twitch_server(2);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );
        let (spade_url, _spade_requests, spade_server) =
            spawn_status_server(vec!["204 No Content"]);
        let spade_urls = tokio::sync::Mutex::new(HashMap::from([(
            String::from("alice"),
            SpadeCacheEntry::Ready(CachedSpadeUrl {
                url: spade_url,
                fetched_at: StdInstant::now(),
            }),
        )]));
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            ..Streamer::default()
        }];
        let runtime = tm_runtime::spawn_runtime_state(state);

        let mut snapshot = runtime.state_snapshot().await.unwrap();
        let streamer = snapshot.streamers.remove(0);
        send_minute_watched_for_streamer(
            &runtime,
            &twitch,
            &spade_urls,
            &streamer,
            "user-1",
            &test_observability(),
        )
        .await
        .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        twitch_server.join().unwrap();
        spade_server.join().unwrap();
        assert!(snapshot.streamers[0].is_online);
        assert!(snapshot.streamers[0]
            .stream
            .as_ref()
            .and_then(|stream| stream.last_minute_update)
            .is_some());
    }

    #[test]
    fn minute_watched_drop_metadata_depends_on_farming_not_claiming() {
        let info = tm_twitch::StreamInfo {
            id: String::from("broadcast-1"),
            title: String::from("Title"),
            game_id: Some(String::from("42")),
            game_name: String::from("Game Name"),
            viewers_count: 1,
            tags: Vec::new(),
            created_at: None,
        };
        let mut streamer = Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            settings: tm_domain::StreamerSettings {
                farm_drops: true,
                claim_drops: false,
                ..tm_domain::StreamerSettings::default()
            },
            ..Streamer::default()
        };

        let farming = build_minute_watched_event(&streamer, &info, "user-1");
        assert_eq!(farming["properties"]["game"], "Game Name");
        assert_eq!(farming["properties"]["game_id"], "42");

        streamer.settings.farm_drops = false;
        streamer.settings.claim_drops = true;
        let claiming_only = build_minute_watched_event(&streamer, &info, "user-1");
        assert!(claiming_only["properties"].get("game").is_none());
        assert!(claiming_only["properties"].get("game_id").is_none());
    }

    #[tokio::test]
    async fn minute_watcher_recovers_channel_rename_by_stable_id() {
        let (endpoints, requests, twitch_server) = spawn_json_response_server(vec![
            String::from(r#"{"data":{"user":null}}"#),
            fixture_json("twitch.stream_live.online.json"),
            String::from(r#"{"data":{"user":{"id":"100","login":"new-login"}}}"#),
            fixture_json("twitch.stream_info.json"),
        ]);
        let twitch = TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            "token",
            "ua",
            endpoints,
        );
        let (spade_url, spade_requests, spade_server) = spawn_status_server(vec!["204 No Content"]);
        let spade_urls = tokio::sync::Mutex::new(HashMap::from([(
            String::from("new-login"),
            SpadeCacheEntry::Ready(CachedSpadeUrl {
                url: spade_url,
                fetched_at: StdInstant::now(),
            }),
        )]));
        let mut state = tm_runtime::RuntimeState::from_targets(
            &ConfigFile::default(),
            &[String::from("old-login")],
            ts(0),
        );
        state.streamers = vec![Streamer {
            username: String::from("old-login"),
            channel_id: String::from("100"),
            channel_points: 500,
            is_online: true,
            presence_known: true,
            online_at: Some(ts(0)),
            stream: Some(tm_domain::Stream::default()),
            ..Streamer::default()
        }];
        state.initial_points = HashMap::from([(String::from("old-login"), 500)]);
        let runtime = tm_runtime::spawn_runtime_state(state);
        let streamer = runtime.state_snapshot().await.unwrap().streamers[0].clone();

        send_minute_watched_for_streamer(
            &runtime,
            &twitch,
            &spade_urls,
            &streamer,
            "user-1",
            &test_observability(),
        )
        .await
        .unwrap();

        twitch_server.join().unwrap();
        spade_server.join().unwrap();
        let snapshot = runtime.state_snapshot().await.unwrap();
        assert_eq!(snapshot.streamers[0].username, "new-login");
        assert_eq!(snapshot.initial_points.get("new-login"), Some(&500));
        assert!(snapshot.streamers[0]
            .stream
            .as_ref()
            .and_then(|stream| stream.last_minute_update)
            .is_some());
        let requests = requests.lock().unwrap();
        assert!(requests
            .iter()
            .any(|request| request.contains(r#""operationName":"ResolveLoginById""#)));
        assert_eq!(spade_requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn spade_cache_retries_with_fresh_url_after_failure() {
        let spade_urls = tokio::sync::Mutex::new(HashMap::new());
        let fetches = Arc::new(AtomicUsize::new(0));
        let sent_urls = Arc::new(Mutex::new(Vec::<String>::new()));

        let status = send_minute_watched_with_spade_cache(
            &spade_urls,
            "alice",
            {
                let fetches = Arc::clone(&fetches);
                move |_login| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        let next = fetches.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok::<_, std::io::Error>(format!("https://spade-{next}.example"))
                    }
                }
            },
            {
                let sent_urls = Arc::clone(&sent_urls);
                move |spade_url| {
                    let sent_urls = Arc::clone(&sent_urls);
                    let spade_url = spade_url.clone();
                    async move {
                        sent_urls.lock().unwrap().push(spade_url);
                        if sent_urls.lock().unwrap().len() == 1 {
                            Ok(StatusCode::BAD_REQUEST)
                        } else {
                            Ok(StatusCode::NO_CONTENT)
                        }
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert_eq!(
            sent_urls.lock().unwrap().as_slice(),
            ["https://spade-1.example", "https://spade-2.example"]
        );
    }

    #[tokio::test]
    async fn spade_cache_recovers_from_unauthorized_rate_limit_and_server_errors() {
        for failure in [
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let cache = tokio::sync::Mutex::new(HashMap::new());
            let fetches = Arc::new(AtomicUsize::new(0));
            let sends = Arc::new(AtomicUsize::new(0));
            let status = send_minute_watched_with_spade_cache(
                &cache,
                "alice",
                {
                    let fetches = Arc::clone(&fetches);
                    move |_login| {
                        let attempt = fetches.fetch_add(1, Ordering::SeqCst) + 1;
                        async move {
                            Ok::<_, std::io::Error>(format!("https://spade-{attempt}.example"))
                        }
                    }
                },
                {
                    let sends = Arc::clone(&sends);
                    move |_url| {
                        let attempt = sends.fetch_add(1, Ordering::SeqCst);
                        async move {
                            Ok::<_, std::io::Error>(if attempt == 0 {
                                failure
                            } else {
                                StatusCode::NO_CONTENT
                            })
                        }
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(fetches.load(Ordering::SeqCst), 2);
            assert_eq!(sends.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn spade_cache_uses_single_inflight_fetch_per_streamer() {
        let spade_urls = tokio::sync::Mutex::new(HashMap::new());
        let fetches = Arc::new(AtomicUsize::new(0));

        let (first, second) = tokio::join!(
            resolve_spade_url(&spade_urls, "alice", false, {
                let fetches = Arc::clone(&fetches);
                move |_login| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        fetches.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(String::from("https://spade.example"))
                    }
                }
            }),
            resolve_spade_url(&spade_urls, "alice", false, {
                let fetches = Arc::clone(&fetches);
                move |_login| {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        fetches.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(String::from("https://spade.example"))
                    }
                }
            })
        );

        assert_eq!(first.unwrap(), "https://spade.example");
        assert_eq!(second.unwrap(), "https://spade.example");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn record_prediction_placed_can_skip_balance_deduction() {
        let config = ConfigFile {
            username: String::from("tester"),
            streamers: vec![String::from("alice")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_targets(&config, &config.streamers, ts(0));
        state.streamers = vec![Streamer {
            username: String::from("alice"),
            channel_id: String::from("100"),
            channel_points: 1_000,
            ..Streamer::default()
        }];
        state.predictions.insert(
            String::from("event-1"),
            PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: String::from("event-1"),
                title: String::from("Prediction"),
                status: String::from("ACTIVE"),
                created_at: ts(0),
                window_seconds: 30.0,
                outcomes: vec![PredictionOutcome {
                    id: String::from("a"),
                    title: String::from("Alpha"),
                    color: String::from("blue"),
                    ..PredictionOutcome::default()
                }],
                decision: PredictionDecision::default(),
                bet_placed: false,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        let runtime = tm_runtime::spawn_runtime_state(state);

        runtime
            .record_prediction_placed(
                "event-1",
                PredictionDecision {
                    choice: Some(0),
                    outcome_id: String::from("a"),
                    amount: 250,
                },
                false,
            )
            .await
            .unwrap();

        let snapshot = runtime.state_snapshot().await.unwrap();
        assert_eq!(snapshot.streamers[0].channel_points, 1_000);
        assert_eq!(snapshot.predictions["event-1"].decision.amount, 250);
    }
}
