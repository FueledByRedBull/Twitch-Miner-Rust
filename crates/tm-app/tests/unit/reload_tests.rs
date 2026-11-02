use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;
use tm_config::{load_or_create_config, StreamerSettingsOverride};
use tm_domain::{PredictionEvent, StreamerSettings};
use tm_observability::Event;
use tm_twitch::TwitchEndpoints;
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::tasks::{TransportSpawner, TransportTasks};

fn setting(name: &str, class: ReloadClass, values: Option<(&str, &str)>) -> SettingChange {
    SettingChange {
        name: name.to_string(),
        class,
        values: values.map(|(before, after)| (before.to_string(), after.to_string())),
    }
}

fn tracked(channel_id: &str, names: &[&str]) -> TrackedChannel {
    TrackedChannel {
        channel_id: channel_id.to_string(),
        names: names.iter().map(ToString::to_string).collect(),
        discovered: false,
    }
}

fn discovered(channel_id: &str, names: &[&str]) -> TrackedChannel {
    TrackedChannel {
        discovered: true,
        ..tracked(channel_id, names)
    }
}

fn named(streamers: &[&str], excluded: &[&str]) -> ConfigFile {
    ConfigFile {
        streamers: streamers.iter().map(ToString::to_string).collect(),
        streamers_exclude: excluded.iter().map(ToString::to_string).collect(),
        ..ConfigFile::default()
    }
}

fn logins(values: &[&str]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

fn desired(config: &ConfigFile) -> Vec<String> {
    named_channels(config)
}

// ---------------------------------------------------------------- planning

#[test]
fn channel_plan_adds_new_names_and_drops_removed_ones() {
    let running = named(&["alpha", "bravo"], &[]);
    let next = named(&["alpha", "charlie"], &[]);
    let channels = [tracked("1", &["alpha"]), tracked("2", &["bravo"])];

    let plan = plan_channels(&running, &next, &desired(&next), &channels);

    assert_eq!(plan.resolve, logins(&["charlie"]));
    assert_eq!(plan.remove, logins(&["2"]));
}

#[test]
fn channel_plan_does_not_retry_a_name_the_running_config_already_listed() {
    // "ghost" failed at startup and is not tracked; it must not block this reload.
    let running = named(&["alpha", "ghost"], &[]);
    let next = named(&["alpha", "ghost", "charlie"], &[]);
    let channels = [tracked("1", &["alpha"])];

    let plan = plan_channels(&running, &next, &desired(&next), &channels);

    assert_eq!(plan.resolve, logins(&["charlie"]));
    assert!(plan.remove.is_empty());
}

#[test]
fn channel_plan_removes_newly_excluded_channels_and_matches_renamed_ones() {
    let running = named(&["alpha", "bravo"], &[]);
    let next = named(&["alpha", "bravo"], &["bravo"]);
    // The configured name differs from the channel's current login after a rename.
    let channels = [
        tracked("1", &["renamed", "alpha"]),
        tracked("2", &["bravo"]),
    ];

    let plan = plan_channels(&running, &next, &desired(&next), &channels);

    assert!(plan.resolve.is_empty());
    assert_eq!(plan.remove, logins(&["2"]));
}

#[test]
fn channel_plan_in_follower_mode_only_reacts_to_exclusions_and_new_followers() {
    let running = named(&[], &[]);
    let next = named(&[], &["bravo"]);
    let followers = logins(&["alpha", "bravo", "charlie"]);
    let channels = [
        tracked("1", &["alpha"]),
        tracked("2", &["bravo"]),
        tracked("9", &["discovered"]),
    ];
    let wanted = followers
        .iter()
        .filter(|login| login.as_str() != "bravo")
        .cloned()
        .collect::<Vec<_>>();

    let plan = plan_channels(&running, &next, &wanted, &channels);

    assert_eq!(plan.resolve, logins(&["charlie"]));
    assert_eq!(plan.remove, logins(&["2"]));
}

#[test]
fn channel_plan_replacing_follower_mode_with_a_named_list_drops_unlisted_channels() {
    let running = named(&[], &[]);
    let next = named(&["alpha"], &[]);
    let channels = [tracked("1", &["alpha"]), tracked("2", &["bravo"])];

    let plan = plan_channels(&running, &next, &desired(&next), &channels);
    assert_eq!(plan.remove, logins(&["2"]));

    let mut discovering = running.clone();
    discovering.drop_discovery_games = logins(&["Chess"]);
    let plan = plan_channels(&discovering, &next, &desired(&next), &channels);
    assert!(plan.remove.is_empty(), "discovered channels stay put");
}

#[test]
fn channel_inputs_change_only_with_the_list_or_its_exclusions() {
    let base = named(&["alpha"], &[]);
    let mut other = base.clone();
    other.emojis = !base.emojis;
    other.streamers = logins(&[" Alpha "]);
    assert!(!channel_inputs_changed(&base, &other));
    other.streamers_exclude = logins(&["bravo"]);
    assert!(channel_inputs_changed(&base, &other));
    assert!(channel_inputs_changed(&base, &named(&[], &[])));
}

// ---------------------------------------------------------------- gates

fn settings(claim_drops: bool, watch_streak: bool, vod_recovery: bool) -> StreamerSettings {
    StreamerSettings {
        claim_drops,
        watch_streak,
        watch_streak_vod_recovery: vod_recovery,
        ..StreamerSettings::default()
    }
}

fn gate_streamer(login: &str, settings: StreamerSettings) -> Streamer {
    Streamer {
        username: login.to_string(),
        settings,
        ..Streamer::default()
    }
}

#[test]
fn startup_gates_hold_back_a_change_that_would_start_or_stop_a_task() {
    let started = [gate_streamer("alpha", settings(false, true, false))];
    let gates = StartupGates::from_streamers(&started);
    assert!(!gates.claim_drops && !gates.streak_recovery);
    let running = HashMap::from([(String::from("1"), settings(false, true, false))]);

    let mut retained = HashMap::from([(String::from("1"), settings(true, true, true))]);
    let mut added = [gate_streamer("bravo", settings(true, true, true))];
    let held = gates.hold(&running, &mut retained, &mut added);

    assert_eq!(held, ["claim_drops", "watch_streak_vod_recovery"]);
    assert!(!retained["1"].claim_drops && !retained["1"].watch_streak_vod_recovery);
    assert!(!added[0].settings.claim_drops && !added[0].settings.watch_streak_vod_recovery);
}

#[test]
fn startup_gates_apply_a_change_that_keeps_the_task_layout() {
    let started = [
        gate_streamer("alpha", settings(true, true, true)),
        gate_streamer("bravo", settings(false, true, false)),
    ];
    let gates = StartupGates::from_streamers(&started);
    let running = HashMap::from([
        (String::from("1"), settings(true, true, true)),
        (String::from("2"), settings(false, true, false)),
    ]);

    // Another channel takes over the Drop claims and the recovery.
    let mut retained = HashMap::from([
        (String::from("1"), settings(false, true, false)),
        (String::from("2"), settings(true, true, true)),
    ]);
    let held = gates.hold(&running, &mut retained, &mut []);

    assert!(held.is_empty());
    assert!(retained["2"].claim_drops && retained["2"].watch_streak_vod_recovery);
}

#[test]
fn startup_gates_hold_a_change_that_would_stop_the_only_claiming_channel() {
    let gates = StartupGates::from_streamers(&[gate_streamer("alpha", settings(true, true, true))]);
    let running = HashMap::from([(String::from("1"), settings(true, true, true))]);
    let mut retained = HashMap::from([(String::from("1"), settings(false, true, false))]);

    let held = gates.hold(&running, &mut retained, &mut []);

    assert_eq!(held, ["claim_drops", "watch_streak_vod_recovery"]);
    assert!(retained["1"].claim_drops);
}

#[test]
fn discovered_channels_never_change_a_startup_gate() {
    let discovered_settings = settings(true, true, true).for_discovered();
    for started in [settings(false, true, false), settings(true, true, true)] {
        let gates = StartupGates::from_streamers(&[gate_streamer("alpha", started.clone())]);
        let running = HashMap::from([
            (String::from("1"), started.clone()),
            (String::from("9"), discovered_settings.clone()),
        ]);
        let mut retained = running.clone();
        let held = gates.hold(&running, &mut retained, &mut []);
        assert!(held.is_empty(), "{started:?}");
        assert_eq!(retained, running);
    }
}

// ---------------------------------------------------------------- observer

#[test]
fn an_unchanged_file_is_not_evaluated_again() {
    let mut observer = FileObserver::default();
    let now = Instant::now();

    assert_eq!(
        observer.observe(Ok(b"one".to_vec()), now),
        Observation::Changed
    );
    for tick in 1..5 {
        let later = now + CHECK_INTERVAL * tick;
        assert_eq!(
            observer.observe(Ok(b"one".to_vec()), later),
            Observation::Unchanged
        );
    }
    assert_eq!(
        observer.observe(Ok(b"two".to_vec()), now + CHECK_INTERVAL * 5),
        Observation::Changed
    );
}

#[test]
fn an_unreadable_file_is_reported_once_until_it_reads_again() {
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    let mut observer = FileObserver::default();
    let now = Instant::now();
    assert_eq!(
        observer.observe(Ok(b"one".to_vec()), now),
        Observation::Changed
    );

    assert_eq!(
        observer.observe(Err(NotFound), now),
        Observation::Unreadable { first: true }
    );
    assert_eq!(
        observer.observe(Err(NotFound), now),
        Observation::Unreadable { first: false }
    );
    assert_eq!(
        observer.observe(Err(PermissionDenied), now),
        Observation::Unreadable { first: true }
    );
    // Back with the contents it had before: nothing to evaluate.
    assert_eq!(
        observer.observe(Ok(b"one".to_vec()), now),
        Observation::Unchanged
    );
}

#[test]
fn a_failed_attempt_is_retried_at_most_every_five_minutes_and_only_while_unchanged() {
    let mut observer = FileObserver::default();
    let start = Instant::now();
    assert_eq!(
        observer.observe(Ok(b"one".to_vec()), start),
        Observation::Changed
    );
    observer.retry_later(start);

    for seconds in [30, 120, 299] {
        assert_eq!(
            observer.observe(Ok(b"one".to_vec()), start + Duration::from_secs(seconds)),
            Observation::Unchanged
        );
    }
    assert_eq!(
        observer.observe(Ok(b"one".to_vec()), start + RETRY_INTERVAL),
        Observation::RetryDue
    );
    // One retry per schedule; the attempt must ask again to be retried.
    assert_eq!(
        observer.observe(Ok(b"one".to_vec()), start + RETRY_INTERVAL * 2),
        Observation::Unchanged
    );

    observer.retry_later(start);
    assert_eq!(
        observer.observe(Ok(b"two".to_vec()), start + Duration::from_secs(60)),
        Observation::Changed
    );
    assert_eq!(
        observer.observe(Ok(b"two".to_vec()), start + RETRY_INTERVAL * 3),
        Observation::Unchanged
    );
}

// ---------------------------------------------------------------- reporting

fn applied_with(
    live: Vec<SettingChange>,
    needs_restart: Vec<SettingChange>,
    channels: [&[&str]; 3],
) -> Applied {
    Applied {
        live,
        needs_restart,
        held: vec!["claim_drops"],
        added: logins(channels[0]),
        removed: logins(channels[1]),
        deferred: logins(channels[2]),
    }
}

#[test]
fn the_diff_line_names_settings_and_shows_only_safe_values() {
    let changes = diff_for_secrets();
    let applied = applied_with(
        changes
            .iter()
            .filter(|change| change.class == ReloadClass::Live)
            .cloned()
            .collect(),
        changes
            .iter()
            .filter(|change| change.class == ReloadClass::NeedsRestart)
            .cloned()
            .collect(),
        [&["Streamer1"], &["Streamer2"], &["Streamer3"]],
    );

    let line = format_applied(&applied);

    for secret in [
        "secret-token",
        "secret-topic",
        "secret-account",
        "secret-channel",
        "secret-excluded",
        "secret-override",
    ] {
        assert!(!line.contains(secret), "{secret} leaked: {line}");
    }
    for name in [
        "discord.webhook_api",
        "webhook.url",
        "username",
        "streamers",
    ] {
        assert!(line.contains(name), "{name} missing from {line}");
    }
    assert!(
        line.contains("chat_presence (\"ONLINE\" -> \"NEVER\")"),
        "{line}"
    );
    assert!(
        line.contains("needs restart: username, claim_drops"),
        "{line}"
    );
    assert!(line.contains("channels added: Streamer1"), "{line}");
    assert!(line.contains("channels removed: Streamer2"), "{line}");
    assert!(
        line.contains("channels kept until their predictions settle: Streamer3"),
        "{line}"
    );
}

fn diff_for_secrets() -> Vec<SettingChange> {
    let old = ConfigFile::default();
    let mut new = old.clone();
    new.discord.webhook_api = String::from("https://discord.com/api/webhooks/1/secret-token");
    new.webhook.url = String::from("https://ntfy.example/secret-topic");
    new.username = String::from("secret-account");
    new.streamers = logins(&["secret-channel"]);
    new.streamers_exclude = logins(&["secret-excluded"]);
    new.streamer_overrides.insert(
        String::from("secret-override"),
        StreamerSettingsOverride::default(),
    );
    new.chat_presence = String::from("NEVER");
    diff_configs(&old, &new).unwrap()
}

#[test]
fn long_values_are_shortened_in_the_diff_line() {
    let long = "x".repeat(500);
    let line = render_change(&setting(
        "game_priority",
        ReloadClass::Live,
        Some(("[]", &long)),
    ));
    assert!(line.chars().count() < 120, "{line}");
    assert!(line.ends_with("...)"));
}

#[test]
fn rejection_text_never_quotes_the_file() {
    let secret = r#"{"username": "tester", "discord": {"webhook_api": "https://secret.example/x" "#;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    std::fs::write(&path, secret).unwrap();

    let error = preview_config(&path).unwrap_err();
    let detail = describe_config_error(&error);

    assert!(!detail.contains("secret.example"), "{detail}");
    assert!(detail.contains("line 1"), "{detail}");
}

// ---------------------------------------------------------------- status

#[test]
fn the_status_block_round_trips_and_older_files_without_it_still_parse() {
    let status = ConfigReloadStatus {
        last_applied_unix: Some(1_700_000_000),
        last_result: Some(ReloadResult::Applied),
        last_error_class: None,
        rejected_attempts: 2,
        needs_restart: logins(&["save_logs"]),
        deferred_removals: logins(&["Streamer1"]),
        deferred_removal_count: 1,
    };
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["last_result"], "applied");
    assert_eq!(
        serde_json::from_value::<ConfigReloadStatus>(json).unwrap(),
        status
    );

    let empty: ConfigReloadStatus = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, ConfigReloadStatus::default());
}

#[test]
fn the_support_bundle_keeps_setting_names_and_drops_channel_names() {
    let directory = tempfile::tempdir().unwrap();
    let health = HealthTracker::default();
    health.register("pubsub", Duration::from_secs(60));
    health.reload().update(|status| {
        status.needs_restart = logins(&["save_logs"]);
        status.deferred_removals = logins(&["secret-channel"]);
        status.deferred_removal_count = 1;
    });
    let reporter = crate::status::StatusReporter::ready(
        directory.path(),
        health,
        Arc::new(tm_runtime::RuntimeMetrics::default()),
    )
    .unwrap();
    reporter.supervision_heartbeat().unwrap();

    let status_json: Value = serde_json::from_slice(
        &std::fs::read(directory.path().join(crate::status::STATUS_FILE_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        status_json["config_reload"]["deferred_removals"][0],
        "secret-channel"
    );

    let bundle = directory.path().join("bundle.json");
    crate::status::write_support_bundle(directory.path(), &bundle).unwrap();
    let bundle: Value = serde_json::from_slice(&std::fs::read(bundle).unwrap()).unwrap();
    let block = &bundle["runtime_status"]["config_reload"];
    assert_eq!(block["needs_restart"][0], "save_logs");
    assert_eq!(block["deferred_removal_count"], 1);
    assert!(block["deferred_removals"].as_array().unwrap().is_empty());
    assert!(!bundle.to_string().contains("secret-channel"));
}

#[test]
fn a_status_file_written_before_the_block_existed_still_loads() {
    let directory = tempfile::tempdir().unwrap();
    let health = HealthTracker::default();
    let reporter = crate::status::StatusReporter::ready(
        directory.path(),
        health,
        Arc::new(tm_runtime::RuntimeMetrics::default()),
    )
    .unwrap();
    reporter.supervision_heartbeat().unwrap();
    let path = directory.path().join(crate::status::STATUS_FILE_NAME);
    let mut status: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(status
        .as_object_mut()
        .unwrap()
        .remove("config_reload")
        .is_some());
    std::fs::write(&path, serde_json::to_vec(&status).unwrap()).unwrap();

    crate::status::print_status(directory.path()).unwrap();
    crate::status::check_health(directory.path()).unwrap();
    let bundle = directory.path().join("bundle.json");
    crate::status::write_support_bundle(directory.path(), &bundle).unwrap();
    let bundle: Value = serde_json::from_slice(&std::fs::read(bundle).unwrap()).unwrap();
    assert_eq!(
        bundle["runtime_status"]["config_reload"]["rejected_attempts"],
        0
    );
}

// ---------------------------------------------------------------- watcher

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl CapturedLog {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn capture_logs() -> (CapturedLog, tracing::subscriber::DefaultGuard) {
    let log = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .finish();
    (log, tracing::subscriber::set_default(subscriber))
}

/// What the fake game directory answers, and how often it was asked.
#[derive(Default)]
struct Directory {
    games: HashMap<String, Vec<String>>,
    fail: bool,
    requests: usize,
}

/// A Twitch stand-in that answers the reads a new channel needs. Names
/// containing "ghost" do not exist. A name starting with "found" is a live
/// channel with an id of its own, except "foundoff", which exists but is
/// offline; one containing "slots" plays Slots. The game directory lists what
/// the test puts in [`Directory`].
struct FakeTwitch {
    endpoints: TwitchEndpoints,
    id_lookups: Arc<AtomicUsize>,
    directory: Arc<Mutex<Directory>>,
}

/// The text between `prefix` and the next quote in a request body.
fn quoted_after<'a>(request: &'a str, prefix: &str) -> &'a str {
    request
        .split(prefix)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default()
}

fn http_json(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    let header_end = loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            return String::new();
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < header_end + length {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn spawn_fake_twitch() -> FakeTwitch {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let id_lookups = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&id_lookups);
    let directory = Arc::new(Mutex::new(Directory::default()));
    let shared_directory = Arc::clone(&directory);
    thread::spawn(move || loop {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let request = read_request(&mut stream);
        let body = if request.starts_with("GET / ") {
            String::from(
                r#"<!doctype html><script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#,
            )
        } else if request.contains(r#""operationName":"GetIDFromLogin""#) {
            counted.fetch_add(1, Ordering::SeqCst);
            let login = quoted_after(&request, r#""login":""#);
            if request.contains("ghost") {
                String::from(r#"{"data":{"user":null}}"#)
            } else if login.starts_with("foundoff") {
                String::from(r#"{"data":{"user":{"id":"900"}}}"#)
            } else if let Some(suffix) = login.strip_prefix("found") {
                format!(r#"{{"data":{{"user":{{"id":"3{suffix}"}}}}}}"#)
            } else if request.contains("delta") {
                String::from(r#"{"data":{"user":{"id":"200"}}}"#)
            } else {
                String::from(r#"{"data":{"user":{"id":"201"}}}"#)
            }
        } else if request.contains(r#""operationName":"ChannelPointsContext""#) {
            let points = if quoted_after(&request, r#""channelLogin":""#).starts_with("found") {
                r#"{"balance":1234}"#
            } else {
                "null"
            };
            format!(
                r#"{{"data":{{"community":{{"channel":{{"self":{{"communityPoints":{points}}},"communityPointsSettings":{{"isEnabled":{},"goals":[]}}}}}}}}}}"#,
                points != "null"
            )
        } else if request.contains(r#""operationName":"WithIsStreamLiveQuery""#) {
            if quoted_after(&request, r#""id":""#).starts_with('3') {
                String::from(r#"{"data":{"user":{"stream":{"id":"stream-1"}}}}"#)
            } else {
                String::from(r#"{"data":{"user":{"stream":null}}}"#)
            }
        } else if request.contains(r#""operationName":"VideoPlayerStreamInfoOverlayChannel""#) {
            let game = if quoted_after(&request, r#""channel":""#).contains("slots") {
                "Slots"
            } else {
                "Game Name"
            };
            format!(
                r#"{{"data":{{"user":{{"broadcastSettings":{{"title":"Title","game":{{"id":"g","displayName":"{game}"}}}},"stream":{{"id":"stream-1","viewersCount":5,"tags":[]}}}}}}}}"#
            )
        } else if request.contains(r#""operationName":"UserPointsContribution""#) {
            String::from(
                r#"{"data":{"user":{"channel":{"self":{"communityPoints":{"goalContributions":[]}}}}}}"#,
            )
        } else if request.contains(r#""operationName":"DirectoryPage_Game""#) {
            let mut directory = shared_directory.lock().unwrap();
            directory.requests += 1;
            if directory.fail {
                String::from(r#"{"errors":[{"message":"boom"}]}"#)
            } else {
                let edges = directory
                    .games
                    .get(quoted_after(&request, r#""slug":""#))
                    .map(|logins| {
                        logins
                            .iter()
                            .map(|login| {
                                format!(r#"{{"node":{{"broadcaster":{{"login":"{login}"}}}}}}"#)
                            })
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                format!(r#"{{"data":{{"game":{{"streams":{{"edges":[{edges}]}}}}}}}}"#)
            }
        } else {
            String::from(r#"{"data":{}}"#)
        };
        let _ = stream.write_all(&http_json(&body));
    });
    FakeTwitch {
        endpoints: TwitchEndpoints {
            twitch_url: format!("http://{address}"),
            gql_url: format!("http://{address}/gql"),
            playback_url: format!("http://{address}/hls/"),
        },
        id_lookups,
        directory,
    }
}

struct Harness {
    directory: tempfile::TempDir,
    path: PathBuf,
    watcher: ConfigWatcher,
    runtime: tm_runtime::RuntimeHandle,
    observability: AppObservability,
    restarts: Arc<Mutex<Vec<usize>>>,
    stop_tx: tokio::sync::watch::Sender<bool>,
    twitch: FakeTwitch,
}

/// Edits the file the way a user would, then fills in the keys the loader would
/// add, so the result is not a pending migration.
fn edit_config(path: &std::path::Path, edit: impl FnOnce(&mut Value)) {
    edit_config_raw(path, edit);
    load_or_create_config(path).unwrap();
}

fn edit_config_raw(path: &std::path::Path, edit: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    edit(&mut value);
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn fake_spawner(restarts: Arc<Mutex<Vec<usize>>>) -> TransportSpawner {
    Box::new(move |streamers, stop, _| {
        restarts.lock().unwrap().push(streamers.len());
        let mut stop = stop;
        TransportTasks {
            eventsub: Some(tokio::spawn(async move {
                while !*stop.borrow() {
                    if stop.changed().await.is_err() {
                        return;
                    }
                }
            })),
            pubsub: None,
            presence_poll: None,
        }
    })
}

/// A watcher over a file written like a startup config, a runtime tracking
/// `alpha` and `bravo`, and a transport controller that records its restarts.
async fn harness(edit: impl FnOnce(&mut Value)) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    load_or_create_config(&path).unwrap();
    edit_config(&path, |value| {
        value["username"] = Value::from("tester");
        value["streamers"] = serde_json::json!(["alpha", "bravo"]);
        edit(value);
    });
    let running = preview_config(&path).unwrap().config;
    let mut state = tm_runtime::RuntimeState::from_config(&running, time_now());
    for (index, streamer) in state.streamers.iter_mut().enumerate() {
        streamer.channel_id = (100 + index).to_string();
        streamer.is_online = true;
        streamer.presence_known = true;
    }
    let gates = StartupGates::from_streamers(&state.streamers);
    let runtime = tm_runtime::spawn_runtime_state(state);
    let observability =
        crate::observability::build_observability(&running, &tm_net::NetworkPolicy::direct())
            .unwrap();
    let twitch = spawn_fake_twitch();
    let client = Arc::new(TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        "token",
        "ua",
        twitch.endpoints.clone(),
    ));
    client.update_client_version().await.unwrap();
    let restarts = Arc::new(Mutex::new(Vec::new()));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let health = HealthTracker::default();
    let transports = TransportController::start(stop_rx, &[], fake_spawner(Arc::clone(&restarts)));
    restarts.lock().unwrap().clear();
    let watcher = ConfigWatcher::new(ConfigWatchContext {
        config_path: path.clone(),
        work_dir: directory.path().to_path_buf(),
        running,
        gates,
        user_id: Some(String::from("user-1")),
        runtime: runtime.clone(),
        twitch: client,
        observability: observability.clone(),
        health,
        transports,
    });
    Harness {
        directory,
        path,
        watcher,
        runtime,
        observability,
        restarts,
        stop_tx,
        twitch,
    }
}

impl Harness {
    async fn tick(&mut self, after: Duration) {
        self.watcher.tick(Instant::now() + after).await;
    }

    fn status(&self) -> ConfigReloadStatus {
        self.watcher.status.status()
    }

    async fn logins(&self) -> Vec<String> {
        self.runtime
            .state_snapshot()
            .await
            .unwrap()
            .streamers
            .into_iter()
            .map(|streamer| streamer.username)
            .collect()
    }

    fn restarts(&self) -> Vec<usize> {
        self.restarts.lock().unwrap().clone()
    }

    fn list(&self, game: &str, logins: &[&str]) {
        self.twitch
            .directory
            .lock()
            .unwrap()
            .games
            .insert(game.to_string(), self::logins(logins));
    }

    fn directory_requests(&self) -> usize {
        self.twitch.directory.lock().unwrap().requests
    }

    async fn discovered(&self) -> Vec<String> {
        self.runtime
            .state_snapshot()
            .await
            .unwrap()
            .streamers
            .into_iter()
            .filter(|streamer| streamer.discovered)
            .map(|streamer| streamer.username)
            .collect()
    }
}

/// A watcher whose settings would make a discovered channel dangerous if they
/// were not forced: predictions, raids, goals and chat are all on.
async fn discovering(games: &[&str], edit: impl FnOnce(&mut Value)) -> Harness {
    harness(|value| {
        value["streak_discovery_games"] = serde_json::json!(games);
        value["betting(make_predictions)"] = Value::from(true);
        value["follow_raid"] = Value::from(true);
        value["community_goals"] = Value::from(true);
        value["chat_presence"] = Value::from("ALWAYS");
        edit(value);
    })
    .await
}

const SIX_HOURS: Duration = Duration::from_secs(6 * 60 * 60);

#[tokio::test]
async fn an_unchanged_file_does_nothing() {
    let mut harness = harness(|_| {}).await;
    let before = std::fs::read(&harness.path).unwrap();

    for tick in 0..4 {
        harness.tick(CHECK_INTERVAL * tick).await;
    }

    assert_eq!(harness.status(), ConfigReloadStatus::default());
    assert!(harness.restarts().is_empty());
    assert_eq!(
        std::fs::read(&harness.path).unwrap(),
        before,
        "never written"
    );
}

#[tokio::test]
async fn settings_that_change_pubsub_topics_restart_the_transports_once() {
    let mut harness = harness(|_| {}).await;
    edit_config(&harness.path, |value| {
        value["betting(make_predictions)"] = Value::from(true);
        value["streamer_overrides"] = serde_json::json!({"bravo": {"follow_raid": true}});
    });

    harness.tick(Duration::ZERO).await;
    harness.tick(CHECK_INTERVAL).await;

    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot
        .streamers
        .iter()
        .all(|streamer| streamer.settings.make_predictions));
    assert!(!snapshot.streamers[0].settings.follow_raid);
    assert!(snapshot.streamers[1].settings.follow_raid);
    let topics = tm_pubsub::build_topics("user-1", &snapshot.streamers).unwrap();
    assert!(topics.contains(&String::from("raid.101")));
    assert_eq!(harness.restarts(), [2]);
}

#[tokio::test]
async fn live_settings_are_applied_without_touching_transports() {
    let mut harness = harness(|_| {}).await;
    let clone = harness.observability.clone();
    assert!(clone.config_reload_message("x").starts_with('\u{1F504}'));
    edit_config(&harness.path, |value| {
        value["emojis"] = Value::from(false);
        value["chat_presence"] = Value::from("NEVER");
        value["game_priority"] = serde_json::json!(["Chess"]);
        value["bet"]["percentage"] = Value::from(7);
    });

    harness.tick(Duration::ZERO).await;

    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot
        .streamers
        .iter()
        .all(|streamer| streamer.settings.irc_mode == tm_domain::IrcMode::Never));
    assert_eq!(snapshot.game_priority, ["chess"]);
    // Every clone of the observability handle sees the same new flags.
    assert_eq!(clone.config_reload_message("x"), "x");
    assert_eq!(harness.observability.config_reload_message("x"), "x");
    assert!(harness.restarts().is_empty());
    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::Applied));
    assert!(status.last_applied_unix.is_some());
    assert!(status.needs_restart.is_empty());
    assert_eq!(status.rejected_attempts, 0);
    assert_eq!(
        harness.watcher.context.running.chat_presence, "NEVER",
        "the running view follows the applied file"
    );
}

#[tokio::test]
async fn restart_only_changes_apply_nothing_and_are_reported() {
    let mut harness = harness(|_| {}).await;
    edit_config(&harness.path, |value| {
        value["save_logs"] = Value::from(true);
        value["privacy"]["anonymize_logs"] = Value::from(false);
    });

    harness.tick(Duration::ZERO).await;

    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::NeedsRestart));
    assert_eq!(status.last_applied_unix, None);
    assert_eq!(
        status.needs_restart,
        ["save_logs", "privacy.anonymize_logs"]
    );
    assert!(harness.restarts().is_empty());
    assert!(!harness.watcher.context.running.save_logs);
}

#[tokio::test]
async fn a_mixed_change_applies_the_live_part_and_reports_the_rest() {
    let mut harness = harness(|_| {}).await;
    edit_config(&harness.path, |value| {
        value["betting(make_predictions)"] = Value::from(true);
        value["timezone"] = Value::from("Europe/Athens");
    });

    harness.tick(Duration::ZERO).await;

    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::Applied));
    assert_eq!(status.needs_restart, ["timezone"]);
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot.streamers[0].settings.make_predictions);
    assert_eq!(harness.watcher.context.running.timezone, None);
}

#[tokio::test]
async fn a_change_that_would_toggle_a_startup_task_is_held_back() {
    let mut harness = harness(|value| {
        value["claim_drops"] = Value::from(false);
        value["farm_drops"] = Value::from(false);
    })
    .await;
    edit_config(&harness.path, |value| {
        value["claim_drops"] = Value::from(true);
        value["farm_drops"] = Value::from(true);
    });

    harness.tick(Duration::ZERO).await;

    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::Applied));
    assert_eq!(status.needs_restart, ["claim_drops"]);
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot
        .streamers
        .iter()
        .all(|s| s.settings.farm_drops && !s.settings.claim_drops));
    assert!(!harness.watcher.context.running.claim_drops);
    assert!(harness.watcher.context.running.farm_drops);
}

#[tokio::test]
async fn an_invalid_file_keeps_the_running_config_and_warns_once_per_content() {
    let (log, _guard) = capture_logs();
    let mut harness = harness(|_| {}).await;
    std::fs::write(&harness.path, b"{ not json").unwrap();

    for tick in 0..4 {
        harness.tick(CHECK_INTERVAL * tick).await;
    }
    assert_eq!(log.text().matches("Config reload rejected").count(), 1);
    assert_eq!(harness.status().rejected_attempts, 1);
    assert_eq!(harness.status().last_result, Some(ReloadResult::Rejected));

    // A different broken file warns again; the running config never changes.
    std::fs::write(&harness.path, b"{ still not json").unwrap();
    harness.tick(CHECK_INTERVAL * 5).await;
    harness.tick(CHECK_INTERVAL * 6).await;
    assert_eq!(log.text().matches("Config reload rejected").count(), 2);
    assert_eq!(harness.status().rejected_attempts, 2);

    assert_eq!(harness.logins().await, ["alpha", "bravo"]);
}

#[tokio::test]
async fn a_semantically_invalid_file_is_rejected_without_applying_anything() {
    let mut harness = harness(|_| {}).await;
    edit_config(&harness.path, |value| {
        value["betting(make_predictions)"] = Value::from(true);
        value["discord"]["webhook_api"] = Value::from("https://example.com/not-a-webhook");
    });

    harness.tick(Duration::ZERO).await;

    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::Rejected));
    assert_eq!(status.last_error_class.as_deref(), Some("invalid-config"));
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot
        .streamers
        .iter()
        .all(|s| !s.settings.make_predictions));
}

#[tokio::test]
async fn a_missing_file_keeps_the_running_config_and_warns_once() {
    let (log, _guard) = capture_logs();
    let mut harness = harness(|_| {}).await;
    std::fs::remove_file(&harness.path).unwrap();

    for tick in 0..3 {
        harness.tick(CHECK_INTERVAL * tick).await;
    }

    assert_eq!(
        log.text()
            .matches("config file is missing or unreadable")
            .count(),
        1
    );
    assert_eq!(harness.status().rejected_attempts, 1);
    assert!(!harness.path.exists(), "never recreated by the watcher");
    assert_eq!(harness.logins().await, ["alpha", "bravo"]);
}

#[tokio::test]
async fn a_file_with_missing_keys_applies_as_a_restart_would_without_being_written() {
    let mut harness = harness(|_| {}).await;
    edit_config_raw(&harness.path, |value| {
        value.as_object_mut().unwrap().remove("debug_deep");
        value["streamer_overrides"] = serde_json::json!({"bravo": {"make_predictions": true}});
    });

    harness.tick(Duration::ZERO).await;

    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::Applied));
    assert!(status.needs_restart.is_empty());
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(!snapshot.streamers[0].settings.make_predictions);
    assert!(snapshot.streamers[1].settings.make_predictions);
    let text = std::fs::read_to_string(&harness.path).unwrap();
    assert!(
        !text.contains("debug_deep"),
        "the watcher must not write the file"
    );
}

#[tokio::test]
async fn a_new_channel_that_cannot_be_resolved_applies_nothing_and_retries_every_five_minutes() {
    let (log, _guard) = capture_logs();
    let mut harness = harness(|_| {}).await;
    edit_config(&harness.path, |value| {
        value["streamers"] = serde_json::json!(["alpha", "bravo", "ghost"]);
        value["betting(make_predictions)"] = Value::from(true);
        value["emojis"] = Value::from(false);
    });

    harness.tick(Duration::ZERO).await;
    assert_eq!(harness.twitch.id_lookups.load(Ordering::SeqCst), 1);
    harness.tick(Duration::from_secs(60)).await;
    harness.tick(Duration::from_secs(240)).await;
    assert_eq!(harness.twitch.id_lookups.load(Ordering::SeqCst), 1, "paced");
    harness.tick(RETRY_INTERVAL + Duration::from_secs(1)).await;
    assert_eq!(harness.twitch.id_lookups.load(Ordering::SeqCst), 2);

    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert_eq!(harness.logins().await, ["alpha", "bravo"]);
    assert!(snapshot
        .streamers
        .iter()
        .all(|s| !s.settings.make_predictions));
    assert!(harness
        .observability
        .config_reload_message("x")
        .starts_with('\u{1F504}'));
    assert!(harness.restarts().is_empty());
    assert!(!harness.watcher.context.running.betting_make_predictions);
    let status = harness.status();
    assert_eq!(status.rejected_attempts, 2);
    assert_eq!(
        status.last_error_class.as_deref(),
        Some("channel-resolution")
    );
    assert_eq!(status.last_applied_unix, None);
    assert_eq!(
        log.text().matches("Config reload failed").count(),
        1,
        "one warning per content"
    );

    // Fixing the file applies the whole change at once.
    edit_config(&harness.path, |value| {
        value["streamers"] = serde_json::json!(["alpha", "bravo"]);
    });
    harness.tick(RETRY_INTERVAL * 3).await;
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot
        .streamers
        .iter()
        .all(|s| s.settings.make_predictions));
    assert_eq!(harness.status().last_result, Some(ReloadResult::Applied));
}

#[tokio::test]
async fn channels_are_added_and_removed_and_the_transports_restart_once() {
    let mut harness = harness(|_| {}).await;
    edit_config(&harness.path, |value| {
        value["streamers"] = serde_json::json!(["alpha", "delta"]);
    });

    harness.tick(Duration::ZERO).await;

    assert_eq!(harness.logins().await, ["alpha", "delta"]);
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert_eq!(snapshot.streamers[1].channel_id, "200");
    assert_eq!(harness.restarts(), [2]);
    assert_eq!(snapshot.retired_streamers.len(), 1);
    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::Applied));
    assert!(status.deferred_removals.is_empty());
    assert!(harness
        .directory
        .path()
        .join(crate::identity_cache::IDENTITY_CACHE_FILE_NAME)
        .exists());
}

#[tokio::test]
async fn a_removal_waits_for_an_unsettled_prediction_then_completes() {
    let mut harness = harness(|value| {
        value["betting(make_predictions)"] = Value::from(true);
    })
    .await;
    // A placement on bravo whose outcome is not known.
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    let event = PredictionEvent {
        streamer: snapshot.streamers[1].clone(),
        event_id: String::from("event-1"),
        title: String::from("Fixture prediction"),
        status: String::from("ACTIVE"),
        created_at: time_now(),
        window_seconds: 30.0,
        outcomes: Vec::new(),
        decision: tm_domain::PredictionDecision::default(),
        bet_placed: false,
        bet_confirmed: false,
        result_type: String::new(),
        result_string: String::new(),
    };
    let created = tm_domain::MinerEvent::PredictionChannel {
        kind: tm_domain::PredictionChannelKind::EventCreated,
        event: Box::new(event),
        winning_outcome_id: None,
    };
    harness
        .runtime
        .apply_event(created, time_now())
        .await
        .unwrap();
    harness
        .runtime
        .mark_prediction_placement_unknown("event-1", tm_domain::PredictionDecision::default())
        .await
        .unwrap();
    edit_config(&harness.path, |value| {
        value["streamers"] = serde_json::json!(["alpha"]);
    });

    harness.tick(Duration::ZERO).await;

    assert_eq!(
        harness.logins().await,
        ["alpha", "bravo"],
        "bravo stays tracked"
    );
    let status = harness.status();
    assert_eq!(status.last_result, Some(ReloadResult::Applied));
    assert_eq!(status.deferred_removal_count, 1);
    assert_eq!(status.deferred_removals.len(), 1);
    assert!(
        harness.restarts().is_empty(),
        "the channel set did not change"
    );

    // Still unsettled on later checks: nothing changes and nothing is restarted.
    harness.tick(CHECK_INTERVAL).await;
    assert_eq!(harness.logins().await, ["alpha", "bravo"]);

    harness.runtime.release_prediction("event-1").await.unwrap();
    harness.tick(CHECK_INTERVAL * 2).await;
    assert_eq!(harness.logins().await, ["alpha"]);
    assert_eq!(harness.restarts(), [1]);
    assert!(harness.watcher.deferred.is_empty());
    assert!(harness.status().deferred_removals.is_empty());
}

#[tokio::test]
async fn a_change_that_would_leave_no_channel_is_rejected() {
    let mut harness = harness(|_| {}).await;
    edit_config(&harness.path, |value| {
        value["streamers_exclude"] = serde_json::json!(["alpha", "bravo"]);
    });

    harness.tick(Duration::ZERO).await;

    assert_eq!(harness.logins().await, ["alpha", "bravo"]);
    assert_eq!(
        harness.status().last_error_class.as_deref(),
        Some("no-channels")
    );
    assert_eq!(harness.status().rejected_attempts, 1);
}

// ---------------------------------------------------------------- discovery

#[tokio::test]
async fn discovery_adds_live_channels_with_safe_settings_and_restarts_once() {
    let mut harness = discovering(&["chess"], |value| {
        value["streamers_exclude"] = serde_json::json!(["found3"]);
    })
    .await;
    // alpha is configured, found3 is excluded and foundoff4 is offline.
    harness.list(
        "chess",
        &["alpha", "found1", "found2", "found3", "foundoff4"],
    );

    harness.tick(Duration::ZERO).await;

    assert_eq!(
        harness.logins().await,
        ["alpha", "bravo", "found1", "found2"]
    );
    assert_eq!(harness.discovered().await, ["found1", "found2"]);
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot.streamers[0].settings.make_predictions);
    assert!(snapshot.streamers[0].settings.follow_raid);
    for found in &snapshot.streamers[2..] {
        let settings = &found.settings;
        assert!(!settings.make_predictions && !settings.follow_raid);
        assert!(!settings.community_goals && !settings.claim_moments);
        assert!(!settings.farm_drops && !settings.claim_drops);
        assert!(!settings.watch_streak_vod_recovery && settings.watch_streak);
        assert_eq!(settings.irc_mode, tm_domain::IrcMode::Never);
    }
    let topics = tm_pubsub::build_topics("user-1", &snapshot.streamers).unwrap();
    assert!(topics.contains(&String::from("raid.100")));
    assert!(topics.contains(&String::from("video-playback-by-id.31")));
    assert!(!topics
        .iter()
        .any(|topic| topic.ends_with(".31") && !topic.starts_with("video")));
    assert_eq!(snapshot.desired_chat_logins(), ["alpha", "bravo"]);
    assert_eq!(harness.restarts(), [4]);
    assert_eq!(harness.directory_requests(), 1);

    // Configured channels first, then the discovered ones while their streak is open.
    assert_eq!(
        snapshot.watch_target_logins(time_now()),
        ["alpha", "bravo", "found1", "found2"]
    );
}

#[tokio::test]
async fn an_unchanged_listing_neither_reloads_nor_restarts() {
    let mut harness = discovering(&["chess"], |_| {}).await;
    harness.list("chess", &["found1", "found2", "foundoff4"]);
    harness.tick(Duration::ZERO).await;
    assert_eq!(harness.restarts(), [4]);

    harness.tick(CHECK_INTERVAL).await;
    harness.tick(Duration::from_secs(6 * 60 * 60 - 60)).await;
    assert_eq!(harness.directory_requests(), 1, "at most every six hours");

    harness.tick(SIX_HOURS + Duration::from_secs(60)).await;
    assert_eq!(harness.directory_requests(), 2);
    assert_eq!(harness.restarts(), [4], "same set, no restart");
    assert_eq!(harness.discovered().await, ["found1", "found2"]);
}

#[tokio::test]
async fn a_failed_listing_changes_nothing_and_warns_once() {
    let (log, _guard) = capture_logs();
    let mut harness = discovering(&["chess"], |_| {}).await;
    harness.list("chess", &["found1"]);
    harness.twitch.directory.lock().unwrap().fail = true;

    harness.tick(Duration::ZERO).await;
    harness.tick(RETRY_INTERVAL + Duration::from_secs(1)).await;

    assert_eq!(
        harness.directory_requests(),
        2,
        "retried after five minutes"
    );
    assert_eq!(harness.logins().await, ["alpha", "bravo"]);
    assert!(harness.restarts().is_empty());
    assert_eq!(
        log.text().matches("could not list live channels").count(),
        1
    );

    harness.twitch.directory.lock().unwrap().fail = false;
    harness
        .tick(RETRY_INTERVAL * 2 + Duration::from_secs(2))
        .await;
    assert_eq!(harness.discovered().await, ["found1"]);
    assert_eq!(harness.restarts(), [3]);
}

#[tokio::test]
async fn channels_that_are_unlisted_or_offline_at_a_refresh_are_removed_in_one_change() {
    let mut harness = discovering(&["chess"], |_| {}).await;
    harness.list("chess", &["found1", "found2", "found3"]);
    harness.tick(Duration::ZERO).await;
    assert_eq!(harness.discovered().await, ["found1", "found2", "found3"]);

    // found2 goes offline but is still listed, found3 drops out of the listing.
    harness
        .runtime
        .apply_event(
            tm_domain::MinerEvent::Playback {
                channel_id: String::from("32"),
                kind: tm_domain::PlaybackType::StreamDown,
            },
            time_now(),
        )
        .await
        .unwrap();
    harness.list("chess", &["found1", "found2", "found4"]);
    harness.tick(SIX_HOURS + Duration::from_secs(60)).await;

    assert_eq!(harness.discovered().await, ["found1", "found4"]);
    assert_eq!(harness.restarts(), [5, 4], "one restart for both changes");
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert_eq!(snapshot.retired_streamers.len(), 2);
}

#[tokio::test]
async fn discovery_stays_within_five_per_game_and_twenty_in_total() {
    let games = ["g1", "g2", "g3", "g4", "g5"];
    let mut harness = discovering(&games, |_| {}).await;
    for (index, game) in games.iter().enumerate() {
        let names = (0..8)
            .map(|channel| format!("found{index}x{channel}"))
            .collect::<Vec<_>>();
        harness.list(game, &names.iter().map(String::as_str).collect::<Vec<_>>());
    }

    harness.tick(Duration::ZERO).await;

    assert_eq!(harness.discovered().await.len(), 20);
    assert_eq!(
        harness.directory_requests(),
        4,
        "the fifth game is not read"
    );
    let discovered = harness.discovered().await;
    for game in 0..4 {
        let count = discovered
            .iter()
            .filter(|login| login.starts_with(&format!("found{game}x")))
            .count();
        assert_eq!(count, 5, "game {game}");
    }
    assert_eq!(harness.restarts(), [22]);
}

#[tokio::test]
async fn exclusions_are_honored_for_logins_and_games() {
    let mut harness = discovering(&["chess"], |value| {
        value["game_exclude"] = serde_json::json!(["Slots"]);
        value["streamers_exclude"] = serde_json::json!(["found2"]);
    })
    .await;
    harness.list("chess", &["found1", "found2", "foundslots3"]);

    harness.tick(Duration::ZERO).await;

    assert_eq!(harness.discovered().await, ["found1"]);

    // A game excluded later removes the channels playing it at the next refresh.
    edit_config(&harness.path, |value| {
        value["game_exclude"] = serde_json::json!(["slots", "game name"]);
        value["streak_discovery_games"] = serde_json::json!(["chess", "go"]);
    });
    harness.tick(CHECK_INTERVAL).await;
    assert!(harness.discovered().await.is_empty());
    assert_eq!(harness.logins().await, ["alpha", "bravo"]);
}

#[tokio::test]
async fn a_reload_that_changes_streamers_leaves_discovered_channels_alone() {
    let mut harness = discovering(&["chess"], |_| {}).await;
    harness.list("chess", &["found1", "found2"]);
    harness.tick(Duration::ZERO).await;

    edit_config(&harness.path, |value| {
        value["streamers"] = serde_json::json!(["alpha", "delta"]);
        value["betting(make_predictions)"] = Value::from(true);
        value["bet"]["percentage"] = Value::from(9);
    });
    harness.tick(CHECK_INTERVAL).await;

    assert_eq!(
        harness.logins().await,
        ["alpha", "found1", "found2", "delta"]
    );
    assert_eq!(harness.discovered().await, ["found1", "found2"]);
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    assert!(snapshot.streamers[0].settings.make_predictions);
    assert!(
        !snapshot.streamers[1].settings.make_predictions,
        "the reload keeps the safe settings"
    );
    assert_eq!(harness.status().last_result, Some(ReloadResult::Applied));
    assert_eq!(harness.restarts(), [4, 4]);
}

#[tokio::test]
async fn a_configured_name_takes_over_a_discovered_channel() {
    let mut harness = discovering(&["chess"], |_| {}).await;
    harness.list("chess", &["found1", "found2"]);
    harness.tick(Duration::ZERO).await;
    assert_eq!(harness.discovered().await, ["found1", "found2"]);

    edit_config(&harness.path, |value| {
        value["streamers"] = serde_json::json!(["alpha", "bravo", "found1"]);
    });
    harness.tick(CHECK_INTERVAL).await;

    assert_eq!(harness.discovered().await, ["found2"]);
    let snapshot = harness.runtime.state_snapshot().await.unwrap();
    let taken = snapshot
        .streamers
        .iter()
        .find(|streamer| streamer.username == "found1")
        .unwrap();
    assert!(taken.settings.make_predictions && taken.settings.follow_raid);
    assert_eq!(snapshot.streamers.len(), 4, "tracked once, not twice");
    // The new raid and prediction topics need the transports restarted.
    assert_eq!(harness.restarts(), [4, 4]);

    // Later refreshes never turn it back into a discovered channel or drop it.
    harness.tick(SIX_HOURS + Duration::from_secs(60)).await;
    assert_eq!(harness.discovered().await, ["found2"]);
    assert_eq!(harness.logins().await.len(), 4);
}

#[tokio::test]
async fn turning_the_key_off_removes_the_discovered_channels_at_once() {
    let mut harness = discovering(&["chess"], |_| {}).await;
    harness.list("chess", &["found1", "found2"]);
    harness.tick(Duration::ZERO).await;

    edit_config(&harness.path, |value| {
        value["streak_discovery_games"] = serde_json::json!([]);
    });
    harness.tick(CHECK_INTERVAL).await;

    assert_eq!(harness.logins().await, ["alpha", "bravo"]);
    assert_eq!(harness.restarts(), [4, 2]);
    assert_eq!(harness.status().last_result, Some(ReloadResult::Applied));
}

#[tokio::test]
async fn discovery_never_runs_without_games() {
    let mut harness = harness(|_| {}).await;

    harness.tick(Duration::ZERO).await;
    harness.tick(SIX_HOURS * 2).await;

    assert_eq!(harness.directory_requests(), 0);
    assert!(harness.restarts().is_empty());
}

#[test]
fn channels_found_by_discovery_are_the_configs_only_to_exclude() {
    let running = named(&["alpha", "bravo"], &[]);
    let channels = [
        tracked("1", &["alpha"]),
        discovered("2", &["found1"]),
        discovered("3", &["bravo"]),
    ];

    // Dropping the configured list never drops a discovered channel.
    let next = named(&["alpha"], &[]);
    let plan = plan_channels(&running, &next, &desired(&next), &channels);
    assert!(plan.remove.is_empty());
    assert!(plan.resolve.is_empty());

    // A newly excluded one goes, and a name that discovery tracks is still resolved.
    let next = named(&["alpha", "found1"], &["bravo"]);
    let plan = plan_channels(&running, &next, &desired(&next), &channels);
    assert_eq!(plan.remove, logins(&["3"]));
    assert_eq!(plan.resolve, logins(&["found1"]));
}

#[tokio::test]
async fn the_watch_loop_stops_with_the_process_even_mid_wait() {
    let harness = harness(|_| {}).await;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let context = harness.watcher.context;
    let task = spawn_config_watch_loop(stop_rx, context);
    tokio::task::yield_now().await;

    stop_tx.send(true).unwrap();

    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    drop(harness.stop_tx);
}

#[test]
fn the_reload_event_name_is_filterable() {
    assert_eq!(
        tm_observability::normalize_event_name("config_reload"),
        Some(Event::ConfigReload)
    );
}
