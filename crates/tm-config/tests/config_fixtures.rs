use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::tempdir;
use tm_config::{load_or_create_config, preview_config};

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
    assert!(!config.privacy.anonymize_logs);
    assert!(config.farm_drops);
    assert!(config.watch_one_stream_when_drops_active);
    assert!(config.claim_moments);
    let alice = &config.streamer_overrides["alice"];
    assert_eq!(alice.farm_drops, Some(true));
    assert_eq!(alice.watch_one_stream_when_drops_active, Some(false));
}

#[test]
fn invalid_nested_fixture_is_rejected_without_normalizing() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::copy(fixture_path("config.invalid_nested.json"), &target).unwrap();

    let error = load_or_create_config(&target).unwrap_err();
    assert!(matches!(
        error,
        tm_config::ConfigError::Validation(message)
        if message == "config.privacy must be a JSON object"
    ));
    let written: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert!(written["privacy"].is_array());
    assert!(written["discord"].is_boolean());
    assert!(written["bet"].is_string());
    assert!(written["bet"].get("filter_condition").is_none());
}

#[test]
fn streamer_override_invalid_shapes_are_rejected_without_write_back() {
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

    let error = load_or_create_config(&target).unwrap_err();
    assert!(matches!(
        error,
        tm_config::ConfigError::Validation(message)
        if message == "config.streamer_overrides.bob must be a JSON object"
    ));
    let written: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert!(written["streamer_overrides"]["alice"]["bet"]["filter_condition"].is_null());
    assert!(written["streamer_overrides"]["alice"]
        .get("claim_moments")
        .is_none());
    assert!(written["streamer_overrides"]["bob"].is_string());
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

#[test]
fn invalid_enum_values_are_rejected_without_write_or_backup() {
    let cases = [
        (
            "config.bet.strategy",
            serde_json::json!({"username": "tester", "bet": {"strategy": "INVALID"}}),
        ),
        (
            "config.bet.delay_mode",
            serde_json::json!({"username": "tester", "bet": {"delay_mode": "INVALID"}}),
        ),
        (
            "config.bet.filter_condition.by",
            serde_json::json!({
                "username": "tester",
                "bet": {"filter_condition": {"by": "INVALID"}}
            }),
        ),
        (
            "config.bet.filter_condition.where",
            serde_json::json!({
                "username": "tester",
                "bet": {"filter_condition": {"where": "INVALID"}}
            }),
        ),
        (
            "config.chat_presence",
            serde_json::json!({"username": "tester", "chat_presence": "INVALID"}),
        ),
        (
            "config.watch_priority[1]",
            serde_json::json!({
                "username": "tester",
                "watch_priority": ["ORDER", "INVALID"]
            }),
        ),
        (
            "config.streamer_overrides.alice.bet.strategy",
            serde_json::json!({
                "username": "tester",
                "streamer_overrides": {"alice": {"bet": {"strategy": "INVALID"}}}
            }),
        ),
        (
            "config.streamer_overrides.alice.bet.delay_mode",
            serde_json::json!({
                "username": "tester",
                "streamer_overrides": {"alice": {"bet": {"delay_mode": "INVALID"}}}
            }),
        ),
        (
            "config.streamer_overrides.alice.bet.filter_condition.by",
            serde_json::json!({
                "username": "tester",
                "streamer_overrides": {
                    "alice": {"bet": {"filter_condition": {"by": "INVALID"}}}
                }
            }),
        ),
        (
            "config.streamer_overrides.alice.bet.filter_condition.where",
            serde_json::json!({
                "username": "tester",
                "streamer_overrides": {
                    "alice": {"bet": {"filter_condition": {"where": "INVALID"}}}
                }
            }),
        ),
        (
            "config.streamer_overrides.alice.chat_presence",
            serde_json::json!({
                "username": "tester",
                "streamer_overrides": {"alice": {"chat_presence": "INVALID"}}
            }),
        ),
    ];

    for (index, (path, value)) in cases.into_iter().enumerate() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config.json");
        let original = serde_json::to_vec(&value).unwrap();
        fs::write(&target, &original).unwrap();

        let error = if index % 2 == 0 {
            preview_config(&target).unwrap_err()
        } else {
            load_or_create_config(&target).unwrap_err()
        };
        assert!(matches!(
            error,
            tm_config::ConfigError::Validation(message)
            if message.contains(path) && message.contains("\"INVALID\"")
        ));
        assert_eq!(fs::read(&target).unwrap(), original);
        assert!(!target.with_extension("json.bak").exists());
    }
}

#[test]
fn empty_watch_priority_keeps_default_behavior() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    fs::write(
        &target,
        serde_json::json!({"username": "tester", "watch_priority": []}).to_string(),
    )
    .unwrap();

    let config = load_or_create_config(&target).unwrap();
    assert!(config.watch_priority.is_empty());
}

#[test]
fn every_supported_watch_priority_alias_passes_loader_validation() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("config.json");
    let priorities = [
        "ORDER",
        "STREAK",
        "DROPS",
        "SUBSCRIBED",
        "SUBS",
        "MULTIPLIER",
        "POINTS_ASC",
        "POINTS_ASCENDING",
        "POINTS_DESC",
        "POINTS_DESCENDING",
        "LONGEST_STREAK",
        "STREAK_LONGEST",
        "EXPIRING_STREAK",
        "STREAK_EXPIRING",
    ];
    fs::write(
        &target,
        serde_json::json!({"username": "tester", "watch_priority": priorities}).to_string(),
    )
    .unwrap();

    let config = load_or_create_config(&target).unwrap();
    assert_eq!(config.watch_priority, priorities);
}
