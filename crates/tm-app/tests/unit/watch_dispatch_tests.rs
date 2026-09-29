#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use crate::observability::AppObservabilitySettings;
use std::net::TcpListener;
use std::thread;
use std::time::Duration;
use tm_config::ConfigFile;
use tm_observability::DiscordClient;
use tm_twitch::TwitchEndpoints;

#[test]
fn watch_dispatch_prioritizes_arrivals_and_preserves_order_and_slots() {
    let mut previous = Vec::new();
    for (selected, expected) in [
        (vec!["alice", "bob"], vec![(0, "alice"), (1, "bob")]),
        (vec!["alice", "charlie"], vec![(1, "charlie"), (0, "alice")]),
        (vec!["alice", "charlie"], vec![(1, "charlie"), (0, "alice")]),
        (vec!["charlie", "alice"], vec![(0, "charlie"), (1, "alice")]),
        (vec!["bob", "alice"], vec![(0, "bob"), (1, "alice")]),
        (vec!["alice", "bob"], vec![(1, "bob"), (0, "alice")]),
        (vec!["dana", "eve"], vec![(0, "dana"), (1, "eve")]),
        (vec!["eve"], vec![(0, "eve")]),
        (vec!["eve", "dana"], vec![(1, "dana"), (0, "eve")]),
        (vec![], vec![]),
        (vec!["eve", "dana"], vec![(0, "eve"), (1, "dana")]),
    ] {
        let requests = order_watch_requests(
            selected.into_iter().map(String::from).collect(),
            &mut previous,
        );
        assert_eq!(
            requests
                .iter()
                .map(|(slot, login)| (*slot, login.as_str()))
                .collect::<Vec<_>>(),
            expected,
        );
        assert_eq!(
            previous,
            requests
                .into_iter()
                .map(|(_, login)| login)
                .collect::<Vec<_>>()
        );
    }
}

fn playback_failure_server(
    sender: tokio::sync::watch::Sender<bool>,
) -> (String, thread::JoinHandle<serde_json::Value>) {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || loop {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
        }
        assert!(length <= 8192);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        if first.starts_with("GET / ") {
            let response = r#"<script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .unwrap();
            continue;
        }
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let response = r#"{"data":{"streamPlaybackAccessToken":null}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .unwrap();
        sender.send(true).unwrap();
        break request;
    });
    (base, server)
}

#[tokio::test]
async fn watcher_pass_dispatches_arrival_first_and_attributes_its_failure() {
    let (sender, mut stop) = tokio::sync::watch::channel(false);
    let (base, server) = playback_failure_server(sender);
    let now = time_now();
    let mut runtime_state = tm_runtime::RuntimeState::from_targets(
        &ConfigFile::default(),
        &[String::from("alice"), String::from("charlie")],
        now,
    );
    for (streamer, login) in runtime_state.streamers.iter_mut().zip(["alice", "charlie"]) {
        *streamer = Streamer {
            username: login.to_string(),
            channel_id: format!("channel-{login}"),
            is_online: true,
            channel_points_enabled: Some(true),
            stream: Some(Stream {
                broadcast_id: String::from("synthetic-broadcast"),
                last_update: Some(now),
                ..Stream::default()
            }),
            ..Streamer::default()
        };
    }
    let context = MinuteWatcherContext {
        runtime: tm_runtime::spawn_runtime_state(runtime_state),
        twitch: Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            "synthetic-token",
            "test-agent",
            TwitchEndpoints {
                twitch_url: base.clone(),
                gql_url: format!("{base}/gql"),
                playback_url: format!("{base}/hls/"),
            },
        )),
        user_id: String::from("synthetic-viewer"),
        observability: AppObservability::new(
            None,
            DiscordClient::new(Duration::from_secs(1)).unwrap(),
            AppObservabilitySettings::default(),
        ),
        health: HealthTracker::default(),
        claim_coordinator: crate::drops::DropClaimCoordinator::default(),
        spade_urls: tokio::sync::Mutex::new(HashMap::new()),
    };
    let mut state = MinuteWatcherState {
        watch_rotation: WatchRotation::default(),
        selected_channel_ids: HashSet::from([
            String::from("channel-alice"),
            String::from("channel-bob"),
        ]),
        dispatch_order: vec![String::from("alice"), String::from("bob")],
        last_loop_at: now,
        metadata_refresh: Some(MetadataRefreshHandle::new(tokio::spawn(
            std::future::pending(),
        ))),
        watch_failures: HashMap::new(),
        watchdog: WatchdogState::default(),
    };
    let action = tokio::time::timeout(
        Duration::from_secs(3),
        run_minute_watcher_pass(&mut stop, &context, &mut state),
    )
    .await
    .unwrap();
    assert!(action == WatchAction::Stop);
    let request = server.join().unwrap();
    assert_eq!(request["variables"]["login"], "charlie");
    assert_eq!(state.dispatch_order, ["charlie", "alice"]);
    assert_eq!(state.watch_failures.len(), 1);
    assert_eq!(state.watch_failures["charlie"].consecutive_requests, 1);

    let output = tempfile::tempdir().unwrap();
    crate::status::StatusReporter::ready(
        output.path(),
        context.health.clone(),
        Arc::new(tm_runtime::RuntimeMetrics::default()),
    )
    .unwrap();
    let status: serde_json::Value = serde_json::from_slice(
        &std::fs::read(output.path().join(crate::status::STATUS_FILE_NAME)).unwrap(),
    )
    .unwrap();
    let slots = status["watch_slots"].as_array().unwrap();
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0]["channel_index"], 0);
    assert_eq!(slots[0]["consecutive_failures"], 0);
    assert_eq!(slots[1]["channel_index"], 1);
    assert_eq!(slots[1]["consecutive_failures"], 1);
    assert_eq!(slots[1]["last_error_class"], "watch-request");
    stop_metadata_refresh(&mut state).await;
}
