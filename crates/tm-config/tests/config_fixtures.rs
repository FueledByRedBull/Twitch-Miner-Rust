use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::tempdir;
use tm_config::load_or_create_config;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

#[test]
fn empty_config_fixture_is_extended_on_load() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::copy(fixture_path("config.empty.json"), &target).unwrap();

    let config = load_or_create_config(&target).unwrap();
    assert_eq!(config.chat_presence, "ONLINE");

    let written: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert!(written["privacy"]["anonymize_logs"].is_boolean());
    assert!(written["bet"]["filter_condition"].is_object());
}

#[test]
fn partial_config_fixture_preserves_existing_values() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::copy(fixture_path("config.partial.json"), &target).unwrap();

    let config = load_or_create_config(&target).unwrap();
    assert_eq!(config.username, "tester");
    assert_eq!(config.streamers, vec!["alice", "bob"]);
    assert_eq!(config.bet.percentage, Some(10));
}

#[test]
fn full_config_fixture_deserializes_parity_fields() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::copy(fixture_path("config.full.json"), &target).unwrap();

    let config = load_or_create_config(&target).unwrap();
    assert_eq!(config.username, "tester");
    assert_eq!(config.streamers_exclude, vec!["eve"]);
    assert_eq!(config.game_priority, vec!["valorant"]);
    assert_eq!(config.discord.webhook_api, "");
    assert_eq!(config.privacy.anonymize_logs, false);
    assert!(config.streamer_overrides.contains_key("alice"));
}

#[test]
fn invalid_nested_fixture_is_normalized() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::copy(fixture_path("config.invalid_nested.json"), &target).unwrap();

    let _ = load_or_create_config(&target).unwrap();
    let written: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert!(written["privacy"].is_object());
    assert!(written["discord"].is_object());
    assert!(written["bet"].is_object());
    assert!(written["bet"]["filter_condition"].is_object());
}

#[test]
fn streamer_override_nested_sections_are_completed_on_write_back() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::write(
        &target,
        serde_json::to_vec_pretty(&serde_json::json!({
            "username": "tester",
            "streamer_overrides": {
                "alice": {
                    "bet": {}
                },
                "bob": "invalid"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let _ = load_or_create_config(&target).unwrap();
    let written: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert!(written["streamer_overrides"]["alice"]["bet"]["filter_condition"].is_object());
    assert!(written["streamer_overrides"]["alice"]["claim_moments"].is_null());
    assert!(written["streamer_overrides"]["bob"].is_object());
    assert!(written["streamer_overrides"]["bob"]["bet"].is_object());
}

#[test]
fn non_object_top_level_fixture_is_rejected_without_write_back() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::write(&target, "null").unwrap();

    let error = load_or_create_config(&target).unwrap_err();
    assert!(matches!(error, tm_config::ConfigError::InvalidConfig(_)));
    assert_eq!(fs::read_to_string(&target).unwrap(), "null");
}
