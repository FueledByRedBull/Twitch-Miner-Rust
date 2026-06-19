use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use tm_domain::{IrcMode, OffsetDateTime, Stream, Streamer, StreamerSettings, WatchPriority};
use tm_runtime::RuntimeState;

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name)
}

pub fn fixture_json(name: &str) -> String {
    fs::read_to_string(fixture_path(name)).unwrap()
}

pub fn ts(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

pub fn base_runtime_state() -> RuntimeState {
    RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: String::from("alpha"),
            channel_id: String::from("123"),
            channel_points: 1_000,
            settings: StreamerSettings {
                irc_mode: IrcMode::Online,
                follow_raid: true,
                claim_moments: true,
                community_goals: true,
                make_predictions: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream::default()),
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
    }
}
