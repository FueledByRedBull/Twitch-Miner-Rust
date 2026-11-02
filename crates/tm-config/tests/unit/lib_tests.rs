use super::*;

fn unique_temp_dir(name: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "tm-config-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn creates_default_config() {
    let dir = unique_temp_dir("create");
    let path = dir.join("config.json");
    let config = load_or_create_config(&path).unwrap();
    assert_eq!(config.chat_presence, "ONLINE");
    assert_eq!(config.config_schema_version, CONFIG_SCHEMA_VERSION);
    assert!(path.exists());
    let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(value.get("password").is_none());
    assert!(value.get("auto_update").is_none());
    assert_eq!(config.followers_order, FollowersOrder::Desc);
}

#[test]
fn follower_order_accepts_asc_and_preserves_desc_default() {
    let dir = unique_temp_dir("followers-order");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(&path, br#"{"username":"Alice","followers_order":"ASC"}"#).unwrap();
    let config = load_or_create_config(&path).unwrap();
    assert_eq!(config.followers_order, FollowersOrder::Asc);

    let default_path = dir.join("default.json");
    fs::write(&default_path, br#"{"username":"Alice"}"#).unwrap();
    let default = load_or_create_config(&default_path).unwrap();
    assert_eq!(default.followers_order, FollowersOrder::Desc);
}

#[test]
fn follower_order_rejects_unknown_values() {
    let dir = unique_temp_dir("followers-order-invalid");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(&path, br#"{"username":"Alice","followers_order":"MIDDLE"}"#).unwrap();
    assert!(load_or_create_config(&path).is_err());
}

#[test]
fn validation_rejects_default_username_placeholder() {
    let mut config = ConfigFile::default();
    assert!(validate_config(&config).is_err());

    config.username = String::from("Alice");
    assert!(validate_config(&config).is_ok());
}

#[test]
fn migrates_removed_security_and_logging_fields_with_backup() {
    let dir = unique_temp_dir("removed-fields");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","password":"","disable_ssl_cert_verification":false,"watch_queue_logging":true,"smart_logging":true}"#;
    fs::write(&path, original).unwrap();

    let config = load_or_create_config(&path).unwrap();
    assert_eq!(config.username, "Alice");
    assert_eq!(fs::read(config_backup_path(&path)).unwrap(), original);
    let migrated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for key in [
        "password",
        "disable_ssl_cert_verification",
        "watch_queue_logging",
        "smart_logging",
    ] {
        assert!(migrated.get(key).is_none());
    }
}

#[test]
fn previews_removed_security_and_logging_fields_without_writing() {
    let dir = unique_temp_dir("removed-fields-preview");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","password":"","disable_ssl_cert_verification":false,"watch_queue_logging":false,"smart_logging":false}"#;
    fs::write(&path, original).unwrap();

    let preview = preview_config(&path).unwrap();
    assert!(preview.migration_required);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!config_backup_path(&path).exists());
}

#[test]
fn rejects_unsafe_or_malformed_removed_fields_without_writing() {
    for (name, original, path_fragment) in [
        (
            "password-value",
            br#"{"username":"Alice","password":"secret"}"#.as_slice(),
            "password",
        ),
        (
            "password-type",
            br#"{"username":"Alice","password":1}"#.as_slice(),
            "password",
        ),
        (
            "tls-enabled",
            br#"{"username":"Alice","disable_ssl_cert_verification":true}"#.as_slice(),
            "disable_ssl_cert_verification",
        ),
        (
            "tls-type",
            br#"{"username":"Alice","disable_ssl_cert_verification":"false"}"#.as_slice(),
            "disable_ssl_cert_verification",
        ),
        (
            "queue-type",
            br#"{"username":"Alice","watch_queue_logging":"yes"}"#.as_slice(),
            "watch_queue_logging",
        ),
        (
            "smart-logging-type",
            br#"{"username":"Alice","smart_logging":"yes"}"#.as_slice(),
            "smart_logging",
        ),
    ] {
        let dir = unique_temp_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, original).unwrap();

        let error = load_or_create_config(&path).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Validation(message) if message.contains(path_fragment)
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!config_backup_path(&path).exists());
    }
}

#[test]
fn validation_rejects_unsafe_prediction_bet_values() {
    let mut config = ConfigFile {
        username: String::from("Alice"),
        ..ConfigFile::default()
    };

    config.bet.percentage = Some(101);
    assert!(matches!(
        validate_config(&config),
        Err(ConfigError::Validation(message)) if message.contains("percentage must be between")
    ));

    config.bet.percentage = None;
    config.bet.delay = Some(-1.0);
    assert!(matches!(
        validate_config(&config),
        Err(ConfigError::Validation(message)) if message.contains("delay must be a finite")
    ));

    config.bet.delay = Some(2.0);
    config.bet.delay_mode = Some(String::from("PERCENTAGE"));
    assert!(matches!(
        validate_config(&config),
        Err(ConfigError::Validation(message)) if message.contains("for PERCENTAGE")
    ));

    config.bet.delay = None;
    config.bet.delay_mode = None;
    config.bet.filter_condition = Some(FilterConditionConfig {
        value: Some(f64::NAN),
        ..FilterConditionConfig::default()
    });
    assert!(matches!(
        validate_config(&config),
        Err(ConfigError::Validation(message)) if message.contains("filter_condition.value")
    ));

    config.bet.filter_condition = None;
    config.bet.max_points = Some(tm_domain::MAX_PREDICTION_POINTS + 1);
    assert!(matches!(
        validate_config(&config),
        Err(ConfigError::Validation(message))
            if message.contains("config.bet.max_points") && message.contains("250000")
    ));

    config.bet.max_points = None;
    config.streamer_overrides.insert(
        String::from("alice"),
        StreamerSettingsOverride {
            bet: BetConfig {
                percentage_gap: Some(101),
                ..BetConfig::default()
            },
            ..StreamerSettingsOverride::default()
        },
    );
    assert!(matches!(
        validate_config(&config),
        Err(ConfigError::Validation(message))
            if message.contains("config.streamer_overrides.alice.bet.percentage_gap")
    ));
}

#[test]
fn load_rejects_prediction_maximum_without_rewriting() {
    for (path_fragment, value) in [
        (
            "config.bet.max_points",
            serde_json::json!({
                "username": "Alice",
                "bet": {"max_points": tm_domain::MAX_PREDICTION_POINTS + 1}
            }),
        ),
        (
            "config.streamer_overrides.alice.bet.max_points",
            serde_json::json!({
                "username": "Alice",
                "streamer_overrides": {
                    "alice": {"bet": {"max_points": tm_domain::MAX_PREDICTION_POINTS + 1}}
                }
            }),
        ),
    ] {
        let dir = unique_temp_dir("prediction-max");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let original = serde_json::to_vec(&value).unwrap();
        fs::write(&path, &original).unwrap();

        let error = preview_config(&path).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::Validation(message)
                if message.contains(path_fragment) && message.contains("250000")
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!config_backup_path(&path).exists());
    }
}

#[test]
fn load_rejects_enabled_legacy_auto_update_without_rewriting() {
    let dir = unique_temp_dir("auto-update");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(&path, br#"{"username":"Alice","auto_update":true}"#).unwrap();

    let error = load_or_create_config(&path).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::Validation(message) if message.contains("auto_update")
    ));
    assert!(fs::read_to_string(path).unwrap().contains("auto_update"));
}

#[test]
fn migrates_legacy_warm_start_cache_flag_on_load_with_backup() {
    let dir = unique_temp_dir("warm-start-cache");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","watch_streak_warm_start_cache":false}"#;
    fs::write(&path, original).unwrap();

    let config = load_or_create_config(&path).unwrap();
    assert_eq!(config.username, "Alice");
    assert_eq!(fs::read(config_backup_path(&path)).unwrap(), original);
    let migrated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(migrated.get("watch_streak_warm_start_cache").is_none());
}

#[test]
fn previews_legacy_warm_start_cache_migration_without_writing() {
    let dir = unique_temp_dir("warm-start-cache-preview");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","watch_streak_warm_start_cache":true}"#;
    fs::write(&path, original).unwrap();

    let preview = preview_config(&path).unwrap();
    assert!(preview.migration_required);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!config_backup_path(&path).exists());
}

#[test]
fn rejects_invalid_legacy_warm_start_cache_without_rewriting() {
    let dir = unique_temp_dir("warm-start-cache-invalid");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","watch_streak_warm_start_cache":"yes"}"#;
    fs::write(&path, original).unwrap();

    let error = load_or_create_config(&path).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::Validation(message)
            if message == "config.watch_streak_warm_start_cache must be a boolean when present"
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!config_backup_path(&path).exists());
}

#[test]
fn migrates_legacy_betting_wrapper_and_matching_duplicate() {
    for (name, original, expected) in [
        (
            "legacy-betting-only",
            br#"{"username":"Alice","watch_streams":true,"betting":{"make_predictions":true}}"#.as_slice(),
            true,
        ),
        (
            "legacy-betting-duplicate",
            br#"{"username":"Alice","watch_streams":true,"betting":{"make_predictions":false},"betting(make_predictions)":false}"#.as_slice(),
            false,
        ),
    ] {
        let dir = unique_temp_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, original).unwrap();

        let config = load_or_create_config(&path).unwrap();
        assert_eq!(config.betting_make_predictions, expected);
        assert_eq!(fs::read(config_backup_path(&path)).unwrap(), original);
        let migrated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(migrated.get("betting").is_none());
        assert!(migrated.get("watch_streams").is_none());
        assert_eq!(
            migrated
                .get("betting(make_predictions)")
                .and_then(Value::as_bool),
            Some(config.betting_make_predictions)
        );
    }
}

#[test]
fn previews_legacy_betting_wrapper_without_writing() {
    let dir = unique_temp_dir("legacy-betting-preview");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","betting":{"make_predictions":true}}"#;
    fs::write(&path, original).unwrap();

    let preview = preview_config(&path).unwrap();
    assert!(preview.migration_required);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!config_backup_path(&path).exists());
}

#[test]
fn rejects_ambiguous_or_malformed_legacy_betting_without_rewriting() {
    for (name, original, expected) in [
        (
            "legacy-betting-conflict",
            br#"{"username":"Alice","betting":{"make_predictions":true},"betting(make_predictions)":false}"#.as_slice(),
            "config.betting.make_predictions conflicts with config.betting(make_predictions)",
        ),
        (
            "legacy-betting-invalid-value",
            br#"{"username":"Alice","betting":{"make_predictions":"yes"}}"#.as_slice(),
            "config.betting.make_predictions must be a boolean",
        ),
        (
            "legacy-betting-unknown-key",
            br#"{"username":"Alice","betting":{"make_predictions":true,"enabled":true}}"#.as_slice(),
            "config.betting.enabled is not a recognized legacy configuration key",
        ),
    ] {
        let dir = unique_temp_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, original).unwrap();

        let error = load_or_create_config(&path).unwrap_err();
        assert!(matches!(error, ConfigError::Validation(message) if message == expected));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!config_backup_path(&path).exists());
    }
}

#[test]
fn rejects_disabled_or_malformed_legacy_watch_streams_without_rewriting() {
    for (name, original, expected) in [
        (
            "legacy-watch-streams-disabled",
            br#"{"username":"Alice","watch_streams":false}"#.as_slice(),
            "config.watch_streams=false is no longer supported; stop the miner instead of starting it with watching disabled",
        ),
        (
            "legacy-watch-streams-invalid",
            br#"{"username":"Alice","watch_streams":"yes"}"#.as_slice(),
            "config.watch_streams must be a boolean when present",
        ),
    ] {
        let dir = unique_temp_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, original).unwrap();

        let error = load_or_create_config(&path).unwrap_err();
        assert!(matches!(error, ConfigError::Validation(message) if message == expected));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!config_backup_path(&path).exists());
    }
}

#[test]
fn preview_reports_migration_without_writing_and_write_back_creates_backup() {
    let dir = unique_temp_dir("preview");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","auto_update":false}"#;
    fs::write(&path, original).unwrap();

    let preview = preview_config(&path).unwrap();
    assert!(preview.migration_required);
    assert_eq!(fs::read(&path).unwrap(), original);

    let config = load_or_create_config(&path).unwrap();
    assert_eq!(config.config_schema_version, CONFIG_SCHEMA_VERSION);
    assert_eq!(fs::read(config_backup_path(&path)).unwrap(), original);
    let migrated: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(migrated.get("auto_update").is_none());
}

#[test]
fn existing_config_stays_readable_by_a_rollback_release() {
    let dir = unique_temp_dir("rollback-readable");
    fs::create_dir_all(&dir).unwrap();

    let mut previous_release = default_config_value();
    for key in OPT_IN_TOP_LEVEL_KEYS {
        previous_release.as_object_mut().unwrap().remove(*key);
    }
    let path = dir.join("config.json");
    let original = serde_json::to_vec_pretty(&previous_release).unwrap();
    fs::write(&path, &original).unwrap();
    let config = load_or_create_config(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(config.drop_account_link, DropAccountLink::Linked);
    assert!(config.drop_discovery_games.is_empty());
    assert!(config.webhook.url.is_empty());

    let migrated_path = dir.join("migrated.json");
    fs::write(&migrated_path, br#"{"username":"Alice"}"#).unwrap();
    load_or_create_config(&migrated_path).unwrap();
    let migrated: Value = serde_json::from_slice(&fs::read(&migrated_path).unwrap()).unwrap();
    for key in OPT_IN_TOP_LEVEL_KEYS {
        assert!(migrated.get(*key).is_none(), "{key} was written");
    }

    let chosen_path = dir.join("chosen.json");
    fs::write(
        &chosen_path,
        br#"{"username":"Alice","drop_account_link":"all"}"#,
    )
    .unwrap();
    assert_eq!(
        load_or_create_config(&chosen_path)
            .unwrap()
            .drop_account_link,
        DropAccountLink::All
    );

    let fresh_path = dir.join("fresh.json");
    load_or_create_config(&fresh_path).unwrap();
    let fresh: Value = serde_json::from_slice(&fs::read(&fresh_path).unwrap()).unwrap();
    for key in OPT_IN_TOP_LEVEL_KEYS {
        assert!(fresh.get(*key).is_some(), "{key} missing from a new file");
    }
}

#[test]
fn the_ranking_settings_are_never_written_into_an_existing_file() {
    let dir = unique_temp_dir("ranking-opt-in");
    fs::create_dir_all(&dir).unwrap();

    // A file that needs a migration is rewritten; the rewrite must not add
    // either key, so a rollback release still accepts it.
    let path = dir.join("config.json");
    fs::write(
        &path,
        br#"{"username":"Alice","streamer_overrides":{"bob":{"farm_drops":true}}}"#,
    )
    .unwrap();
    let config = load_or_create_config(&path).unwrap();
    assert!(config.streak_discovery_games.is_empty());
    assert_eq!(config.streamer_overrides["bob"].points_limit, None);
    let rewritten: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(rewritten.get("streak_discovery_games").is_none());
    let bob = &rewritten["streamer_overrides"]["bob"];
    assert!(bob.get("points_limit").is_none(), "{bob}");
    assert!(
        bob.get("make_predictions").is_some(),
        "other keys are filled"
    );

    // A file that sets them keeps them.
    let chosen = dir.join("chosen.json");
    fs::write(
        &chosen,
        br#"{"username":"Alice","streak_discovery_games":["chess"],"streamer_overrides":{"bob":{"points_limit":5000}}}"#,
    )
    .unwrap();
    let config = load_or_create_config(&chosen).unwrap();
    assert_eq!(config.streak_discovery_games, ["chess"]);
    assert_eq!(config.streamer_overrides["bob"].points_limit, Some(5_000));
    let rewritten: Value = serde_json::from_slice(&fs::read(&chosen).unwrap()).unwrap();
    assert_eq!(
        rewritten["streamer_overrides"]["bob"]["points_limit"],
        5_000
    );

    // A new file lists the discovery key like the other opt-in keys, and no
    // override carries a limit.
    let fresh = dir.join("fresh.json");
    load_or_create_config(&fresh).unwrap();
    let fresh: Value = serde_json::from_slice(&fs::read(&fresh).unwrap()).unwrap();
    assert_eq!(fresh["streak_discovery_games"], json!([]));
}

#[test]
fn notify_device_login_is_off_by_default_and_never_written_into_an_existing_file() {
    let dir = unique_temp_dir("notify-device-login");
    fs::create_dir_all(&dir).unwrap();

    // A file that needs a migration is rewritten; the rewrite must not add the
    // key, so a rollback release still accepts it.
    let path = dir.join("config.json");
    fs::write(
        &path,
        br#"{"username":"Alice","streamer_overrides":{"bob":{"farm_drops":true}}}"#,
    )
    .unwrap();
    assert!(!load_or_create_config(&path).unwrap().notify_device_login);
    let rewritten: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(rewritten.get("notify_device_login").is_none());
    assert!(rewritten["streamer_overrides"]["bob"]
        .get("make_predictions")
        .is_some());

    // A file that sets it keeps it through a rewrite.
    let chosen = dir.join("chosen.json");
    fs::write(
        &chosen,
        br#"{"username":"Alice","notify_device_login":true,"streamer_overrides":{"bob":{"farm_drops":true}}}"#,
    )
    .unwrap();
    assert!(load_or_create_config(&chosen).unwrap().notify_device_login);
    let rewritten: Value = serde_json::from_slice(&fs::read(&chosen).unwrap()).unwrap();
    assert_eq!(rewritten["notify_device_login"], true);

    // A new file lists it, switched off.
    let fresh = dir.join("fresh.json");
    load_or_create_config(&fresh).unwrap();
    let fresh: Value = serde_json::from_slice(&fs::read(&fresh).unwrap()).unwrap();
    assert_eq!(fresh["notify_device_login"], false);
    assert!(OPT_IN_TOP_LEVEL_KEYS.contains(&"notify_device_login"));
}

#[test]
fn notify_device_login_must_be_a_boolean() {
    let dir = unique_temp_dir("notify-device-login-invalid");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(&path, r#"{"username":"Alice","notify_device_login":"yes"}"#).unwrap();

    assert!(load_or_create_config(&path).is_err());
}

#[test]
fn notify_device_login_is_read_once_at_startup() {
    let old = ConfigFile::default();
    let mut new = old.clone();
    new.notify_device_login = true;

    assert_eq!(
        changed_names(&old, &new),
        [(
            String::from("notify_device_login"),
            ReloadClass::NeedsRestart
        )]
    );
}

#[test]
fn points_limit_must_be_a_positive_whole_number() {
    let dir = unique_temp_dir("points-limit-invalid");
    fs::create_dir_all(&dir).unwrap();
    for (name, raw) in [
        ("zero", "0"),
        ("negative", "-5"),
        ("fraction", "1.5"),
        ("text", r#""10""#),
        ("flag", "true"),
    ] {
        let path = dir.join(format!("{name}.json"));
        let original = format!(
            r#"{{"username":"Alice","streamer_overrides":{{"bob":{{"points_limit":{raw}}}}}}}"#
        );
        fs::write(&path, &original).unwrap();
        let error = load_or_create_config(&path).unwrap_err().to_string();
        assert!(
            error.contains("config.streamer_overrides.bob.points_limit"),
            "{error}"
        );
        assert!(error.contains("positive whole number"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(preview_config(&path).is_err());
    }

    let null = dir.join("null.json");
    fs::write(
        &null,
        br#"{"username":"Alice","streamer_overrides":{"bob":{"points_limit":null}}}"#,
    )
    .unwrap();
    assert_eq!(
        load_or_create_config(&null).unwrap().streamer_overrides["bob"].points_limit,
        None
    );

    let mut config = ConfigFile {
        username: String::from("alice"),
        ..ConfigFile::default()
    };
    config.streamer_overrides.insert(
        String::from("bob"),
        StreamerSettingsOverride {
            points_limit: Some(0),
            ..StreamerSettingsOverride::default()
        },
    );
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("points_limit"), "{error}");
    config
        .streamer_overrides
        .get_mut("bob")
        .unwrap()
        .points_limit = Some(1);
    validate_config(&config).unwrap();
}

#[test]
fn points_limit_is_an_override_only_setting() {
    let mut config = ConfigFile::default();
    config.streamer_overrides.insert(
        String::from("Bob"),
        StreamerSettingsOverride {
            points_limit: Some(2_500),
            ..StreamerSettingsOverride::default()
        },
    );
    config
        .streamer_overrides
        .insert(String::from("carol"), StreamerSettingsOverride::default());

    let base = build_base_streamer_settings(&config);
    let merged = build_override_settings(&base, &config.streamer_overrides);

    assert_eq!(base.points_limit, None);
    assert_eq!(merged["bob"].points_limit, Some(2_500));
    assert_eq!(merged["carol"].points_limit, None);
}

#[test]
fn log_format_defaults_to_text_and_accepts_json() {
    let dir = unique_temp_dir("log-format");
    fs::create_dir_all(&dir).unwrap();

    let default_path = dir.join("default.json");
    fs::write(&default_path, br#"{"username":"Alice"}"#).unwrap();
    assert_eq!(
        load_or_create_config(&default_path).unwrap().log_format,
        LogFormat::Text
    );

    let json_path = dir.join("json.json");
    fs::write(&json_path, br#"{"username":"Alice","log_format":"json"}"#).unwrap();
    assert_eq!(
        load_or_create_config(&json_path).unwrap().log_format,
        LogFormat::Json
    );
}

#[test]
fn unsupported_log_format_is_rejected_with_its_path_and_choices() {
    let dir = unique_temp_dir("log-format-invalid");
    fs::create_dir_all(&dir).unwrap();
    for (name, raw) in [("xml", r#""xml""#), ("upper", r#""JSON""#), ("number", "3")] {
        let path = dir.join(format!("{name}.json"));
        let original = format!(r#"{{"username":"Alice","log_format":{raw}}}"#);
        fs::write(&path, &original).unwrap();
        let error = load_or_create_config(&path).unwrap_err().to_string();
        assert!(error.contains("config.log_format"), "{error}");
        assert!(error.contains("\"text\" or \"json\""), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn atomic_write_replaces_existing_file_and_cleans_failed_temporary_files() {
    let dir = unique_temp_dir("atomic-write");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(&path, br#"{"old":true}"#).unwrap();

    atomic_write(&path, br#"{"new":true}"#).unwrap();
    assert_eq!(fs::read(&path).unwrap(), br#"{"new":true}"#);

    let target_directory = dir.join("directory-target");
    fs::create_dir_all(&target_directory).unwrap();
    assert!(atomic_write(&target_directory, b"{} ").is_err());
    let temporary = dir.join(format!(".directory-target.{}.tmp", std::process::id()));
    assert!(!temporary.exists());
}

#[cfg(unix)]
#[test]
fn migrated_config_and_backup_use_private_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = unique_temp_dir("config-permissions");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(&path, br#"{"username":"Alice","auto_update":false}"#).unwrap();
    load_or_create_config(&path).unwrap();

    for candidate in [&path, &config_backup_path(&path)] {
        let mode = fs::metadata(candidate).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[cfg(unix)]
#[test]
fn unchanged_config_uses_private_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = unique_temp_dir("unchanged-config-permissions");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&default_config_value()).unwrap(),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    load_or_create_config(&path).unwrap();

    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn rejects_future_config_schema_without_rewriting() {
    let dir = unique_temp_dir("future-schema");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = format!(
        r#"{{"config_schema_version":{},"username":"Alice"}}"#,
        CONFIG_SCHEMA_VERSION + 1
    );
    fs::write(&path, &original).unwrap();

    assert!(preview_config(&path).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

fn assert_rejects_unknown_key(name: &str, value: &Value, expected_path: &str) {
    let dir = unique_temp_dir(name);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = serde_json::to_vec_pretty(value).unwrap();
    fs::write(&path, &original).unwrap();

    let error = load_or_create_config(&path).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::Validation(message)
            if message == format!(
                "{expected_path} is not a recognized configuration key"
            )
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!config_backup_path(&path).exists());
}

#[test]
fn rejects_unknown_top_level_key_without_rewriting() {
    assert_rejects_unknown_key(
        "unknown-top-level",
        &json!({"username": "Alice", "usernmae": "typo"}),
        "config.usernmae",
    );
}

#[test]
fn rejects_unknown_nested_keys_with_actionable_paths() {
    let cases = [
        (
            "unknown-bet",
            json!({"username": "Alice", "bet": {"percentag": 10}}),
            "config.bet.percentag",
        ),
        (
            "unknown-filter",
            json!({
                "username": "Alice",
                "bet": {"filter_condition": {"wher": "GTE"}}
            }),
            "config.bet.filter_condition.wher",
        ),
        (
            "unknown-privacy",
            json!({"username": "Alice", "privacy": {"anonymize_log": true}}),
            "config.privacy.anonymize_log",
        ),
        (
            "unknown-discord",
            json!({"username": "Alice", "discord": {"event": []}}),
            "config.discord.event",
        ),
        (
            "unknown-override",
            json!({
                "username": "Alice",
                "streamer_overrides": {"alice": {"claim_drop": true}}
            }),
            "config.streamer_overrides.alice.claim_drop",
        ),
        (
            "unknown-override-bet",
            json!({
                "username": "Alice",
                "streamer_overrides": {"alice": {"bet": {"percentag": 10}}}
            }),
            "config.streamer_overrides.alice.bet.percentag",
        ),
        (
            "unknown-override-filter",
            json!({
                "username": "Alice",
                "streamer_overrides": {
                    "alice": {"bet": {"filter_condition": {"wher": "GTE"}}}
                }
            }),
            "config.streamer_overrides.alice.bet.filter_condition.wher",
        ),
    ];

    for (name, value, expected_path) in cases {
        assert_rejects_unknown_key(name, &value, expected_path);
    }
}

#[test]
fn preview_rejects_unknown_key_without_writing_or_reporting_migration() {
    let dir = unique_temp_dir("preview-unknown-key");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","auto_update":false,"usernmae":"typo"}"#;
    fs::write(&path, original).unwrap();

    let error = preview_config(&path).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::Validation(message)
            if message == "config.usernmae is not a recognized configuration key"
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!config_backup_path(&path).exists());
}

#[test]
fn migration_removal_precedes_unknown_key_rejection_without_rewriting() {
    let dir = unique_temp_dir("migration-before-unknown");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let original = br#"{"username":"Alice","auto_update":false,"usernmae":"typo"}"#;
    fs::write(&path, original).unwrap();

    let error = load_or_create_config(&path).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::Validation(message)
            if message == "config.usernmae is not a recognized configuration key"
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!config_backup_path(&path).exists());
}

#[test]
fn accepts_dynamic_streamer_login_keys() {
    let dir = unique_temp_dir("dynamic-streamer-login");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "username": "Alice",
            "streamer_overrides": {
                "some.login": {"chat_presence": "ONLINE"},
                "other_login": {"watch_streak": false}
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let config = load_or_create_config(&path).unwrap();
    assert!(config.streamer_overrides.contains_key("some.login"));
    assert!(config.streamer_overrides.contains_key("other_login"));
}

#[test]
fn strict_key_allowlists_match_the_serialized_schema() {
    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    fn sorted_names(values: &[&str]) -> Vec<String> {
        let mut names = values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    let root = default_config_value();
    assert_eq!(sorted_keys(&root), sorted_names(TOP_LEVEL_CONFIG_KEYS));
    assert_eq!(sorted_keys(&bet_defaults()), sorted_names(BET_CONFIG_KEYS));
    assert_eq!(
        sorted_keys(&json!({"by": null, "where": null, "value": null})),
        sorted_names(FILTER_CONDITION_KEYS)
    );
    assert_eq!(
        sorted_keys(&privacy_defaults()),
        sorted_names(PRIVACY_CONFIG_KEYS)
    );
    assert_eq!(
        sorted_keys(&discord_defaults()),
        sorted_names(DISCORD_CONFIG_KEYS)
    );
    assert_eq!(
        sorted_keys(&serde_json::to_value(StreamerSettingsOverride::default()).unwrap()),
        sorted_names(STREAMER_OVERRIDE_KEYS)
    );
}

#[test]
fn extends_nested_sections_like_go() {
    let dir = unique_temp_dir("extend");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "username": "user",
            "bet": {},
            "privacy": {}
        }))
        .unwrap(),
    )
    .unwrap();

    let config = load_or_create_config(&path).unwrap();
    assert_eq!(config.username, "user");
    let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(value["bet"]["filter_condition"].is_object());
    assert!(value["privacy"]["anonymize_logs"].is_boolean());
}

#[test]
fn rejects_non_object_top_level_without_rewriting() {
    let dir = unique_temp_dir("non-object");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(&path, b"[]").unwrap();

    let error = load_or_create_config(&path).unwrap_err();
    assert!(matches!(error, ConfigError::InvalidConfig(_)));
    assert_eq!(fs::read(&path).unwrap(), b"[]");
}

#[test]
fn resolves_paths_in_go_order() {
    let cwd = unique_temp_dir("cwd");
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("config.json"), "{}").unwrap();

    let data_dir_input = ResolveAppPathsInput {
        config_flag: None,
        data_dir_flag: Some(PathBuf::from("data-dir")),
        env_config: Some(String::from("ignored-config.json")),
        env_data_dir: Some(String::from("ignored-data")),
        cwd: cwd.clone(),
        executable_path: Some(PathBuf::from("C:/app/tm-app.exe")),
        executable_is_temp: false,
    };
    let paths = resolve_app_paths(&data_dir_input).unwrap();
    assert!(paths.work_dir.ends_with("data-dir"));

    let combined_input = ResolveAppPathsInput {
        config_flag: Some(PathBuf::from("custom/config.json")),
        ..data_dir_input.clone()
    };
    let paths = resolve_app_paths(&combined_input).unwrap();
    assert!(paths.work_dir.ends_with("data-dir"));
    assert!(paths.config_path.ends_with(Path::new("custom/config.json")));

    let config_input = ResolveAppPathsInput {
        data_dir_flag: None,
        config_flag: Some(PathBuf::from("custom/config.json")),
        ..data_dir_input.clone()
    };
    let paths = resolve_app_paths(&config_input).unwrap();
    assert!(paths.config_path.ends_with(Path::new("custom/config.json")));

    let cwd_input = ResolveAppPathsInput {
        data_dir_flag: None,
        config_flag: None,
        env_config: None,
        env_data_dir: None,
        cwd: cwd.clone(),
        executable_path: Some(PathBuf::from("C:/app/tm-app.exe")),
        executable_is_temp: false,
    };
    let paths = resolve_app_paths(&cwd_input).unwrap();
    assert_eq!(paths.work_dir, cwd);
}

#[test]
fn invalid_chat_presence_falls_back() {
    assert_eq!(
        parse_chat_presence("ALWAYS", IrcMode::Online),
        IrcMode::Always
    );
    assert_eq!(parse_chat_presence("", IrcMode::Offline), IrcMode::Offline);
    assert_eq!(
        parse_chat_presence("invalid", IrcMode::Online),
        IrcMode::Online
    );
}

#[test]
fn overrides_inherit_from_base() {
    let config = ConfigFile::default();
    let base = build_base_streamer_settings(&config);
    let overrides = HashMap::from([(
        String::from("SomeStreamer"),
        StreamerSettingsOverride {
            chat_presence: Some(String::from("invalid")),
            farm_drops: Some(true),
            claim_drops: Some(false),
            watch_one_stream_when_drops_active: Some(false),
            ..StreamerSettingsOverride::default()
        },
    )]);
    let merged = build_override_settings(&base, &overrides);
    let override_settings = merged.get("somestreamer").unwrap();
    assert!(override_settings.farm_drops);
    assert!(!override_settings.claim_drops);
    assert!(!override_settings.single_watcher_during_drops);
    assert_eq!(override_settings.irc_mode, base.irc_mode);
}

#[test]
fn migrates_legacy_drop_claiming_into_independent_farming_controls() {
    let dir = unique_temp_dir("drop-farming-migration");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "username": "Alice",
            "claim_drops": false,
            "streamer_overrides": {
                "enabled": { "claim_drops": true },
                "inherited": { "claim_drops": Value::Null }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let config = load_or_create_config(&path).unwrap();
    assert!(!config.farm_drops);
    assert!(config.watch_one_stream_when_drops_active);
    let base = build_base_streamer_settings(&config);
    let merged = build_override_settings(&base, &config.streamer_overrides);
    assert!(merged["enabled"].farm_drops);
    assert!(merged["enabled"].claim_drops);
    assert!(!merged["inherited"].farm_drops);

    let migrated: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(migrated["farm_drops"], Value::Bool(false));
    assert_eq!(
        migrated["streamer_overrides"]["enabled"]["farm_drops"],
        Value::Bool(true)
    );
    assert!(migrated["streamer_overrides"]["inherited"]["farm_drops"].is_null());
}

#[test]
fn notification_urls_are_validated() {
    let check = |discord: &str, webhook: &str| {
        let mut config = ConfigFile {
            username: String::from("alice"),
            ..ConfigFile::default()
        };
        config.discord.webhook_api = discord.to_string();
        config.webhook.url = webhook.to_string();
        validate_config(&config).is_ok()
    };
    assert!(check("", ""));
    assert!(check("https://discord.com/api/webhooks/1/token", ""));
    assert!(check("https://discordapp.com/api/webhooks/1/token", ""));
    assert!(!check("http://discord.com/api/webhooks/1/token", ""));
    assert!(!check(
        "https://discord.com.example/api/webhooks/1/token",
        ""
    ));
    assert!(!check("not a url", ""));
    assert!(check("", "https://ntfy.example/topic"));
    assert!(check("", "http://homeassistant.local:8123/api/webhook/x"));
    assert!(!check("", "ftp://example.com"));
    assert!(!check("", "https://"));
}

fn changed_names(old: &ConfigFile, new: &ConfigFile) -> Vec<(String, ReloadClass)> {
    diff_configs(old, new)
        .unwrap()
        .into_iter()
        .map(|change| (change.name, change.class))
        .collect()
}

#[test]
fn every_top_level_key_has_a_reload_label() {
    let labelled = reload::SETTING_RULES
        .iter()
        .map(|rule| rule.key)
        .collect::<Vec<_>>();
    for key in TOP_LEVEL_CONFIG_KEYS {
        assert!(
            labelled.contains(key),
            "{key} is not classified in reload::SETTING_RULES"
        );
    }
    for key in &labelled {
        assert!(
            TOP_LEVEL_CONFIG_KEYS.contains(key),
            "{key} is classified but is not a config key"
        );
    }
    assert_eq!(labelled.len(), TOP_LEVEL_CONFIG_KEYS.len());
    let serialized = serde_json::to_value(ConfigFile::default()).unwrap();
    for key in serialized.as_object().unwrap().keys() {
        assert!(
            labelled.contains(&key.as_str()),
            "{key} is serialized but not classified"
        );
    }
}

#[test]
fn reload_diff_is_empty_for_identical_configs() {
    let config = ConfigFile::default();
    assert!(diff_configs(&config, &config.clone()).unwrap().is_empty());
}

#[test]
fn reload_diff_labels_live_and_restart_settings() {
    let old = ConfigFile::default();
    let mut new = old.clone();
    new.betting_make_predictions = !old.betting_make_predictions;
    new.chat_presence = String::from("NEVER");
    new.emojis = !old.emojis;
    new.game_priority = vec![String::from("Chess")];
    new.watch_priority = vec![String::from("STREAK")];
    new.streamers = vec![String::from("alice")];
    new.streamers_exclude = vec![String::from("bob")];
    new.show_game = !old.show_game;
    new.discord.events = vec![String::from("STARTUP")];
    new.bet.max_points = Some(7);
    new.streamer_overrides
        .insert(String::from("alice"), StreamerSettingsOverride::default());
    new.save_logs = !old.save_logs;
    new.debug = !old.debug;
    new.log_format = LogFormat::Json;
    new.privacy.anonymize_logs = !old.privacy.anonymize_logs;
    new.timezone = Some(String::from("Europe/Athens"));
    new.followers_order = FollowersOrder::Asc;
    new.drop_discovery_games = vec![String::from("Chess")];
    new.disable_at_in_nickname = !old.disable_at_in_nickname;

    let changes = changed_names(&old, &new);
    let class_of = |name: &str| {
        changes
            .iter()
            .find(|(changed, _)| changed == name)
            .unwrap_or_else(|| panic!("{name} was not reported"))
            .1
    };
    for live in [
        "betting(make_predictions)",
        "chat_presence",
        "emojis",
        "game_priority",
        "watch_priority",
        "streamers",
        "streamers_exclude",
        "show_game",
        "discord.events",
        "bet.max_points",
        "streamer_overrides",
    ] {
        assert_eq!(class_of(live), ReloadClass::Live, "{live}");
    }
    for restart in [
        "save_logs",
        "debug",
        "log_format",
        "privacy.anonymize_logs",
        "timezone",
        "followers_order",
        "drop_discovery_games",
        "disable_at_in_nickname",
    ] {
        assert_eq!(class_of(restart), ReloadClass::NeedsRestart, "{restart}");
    }
    assert_eq!(changes.len(), 11 + 8);
}

#[test]
fn streak_discovery_games_are_live_and_points_limits_follow_the_overrides() {
    let old = ConfigFile::default();
    let mut new = old.clone();
    new.streak_discovery_games = vec![String::from("chess")];
    new.streamer_overrides.insert(
        String::from("alice"),
        StreamerSettingsOverride {
            points_limit: Some(10),
            ..StreamerSettingsOverride::default()
        },
    );

    let changes = diff_configs(&old, &new).unwrap();

    assert_eq!(
        changed_names(&old, &new),
        [
            (String::from("streak_discovery_games"), ReloadClass::Live),
            (String::from("streamer_overrides"), ReloadClass::Live),
        ]
    );
    assert_eq!(
        changes[0].values,
        Some((String::from("[]"), String::from(r#"["chess"]"#)))
    );
    assert_eq!(changes[1].values, None, "channel names are never shown");
    let merged = merge_live_settings(&old, &new).unwrap();
    assert_eq!(merged.streak_discovery_games, new.streak_discovery_games);
}

#[test]
fn reload_diff_keeps_secrets_and_names_out_of_values() {
    let old = ConfigFile::default();
    let mut new = old.clone();
    new.discord.webhook_api = String::from("https://discord.com/api/webhooks/1/secret-token");
    new.webhook.url = String::from("https://ntfy.example/secret-topic");
    new.username = String::from("secret-account");
    new.streamers = vec![String::from("secret-channel")];
    new.streamers_exclude = vec![String::from("secret-excluded")];
    new.streamer_overrides.insert(
        String::from("secret-override"),
        StreamerSettingsOverride::default(),
    );
    new.chat_presence = String::from("NEVER");

    let changes = diff_configs(&old, &new).unwrap();
    let rendered = format!("{changes:?}");
    for secret in [
        "secret-token",
        "secret-topic",
        "secret-account",
        "secret-channel",
        "secret-excluded",
        "secret-override",
    ] {
        assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
    }
    let names = changes
        .iter()
        .map(|change| change.name.as_str())
        .collect::<Vec<_>>();
    for name in ["discord.webhook_api", "webhook.url", "username"] {
        assert!(names.contains(&name), "{name} missing");
    }
    let chat = changes
        .iter()
        .find(|change| change.name == "chat_presence")
        .unwrap();
    assert_eq!(
        chat.values,
        Some((String::from("\"ONLINE\""), String::from("\"NEVER\"")))
    );
}

#[test]
fn merging_live_settings_keeps_restart_settings_at_their_running_value() {
    let running = ConfigFile::default();
    let mut next = running.clone();
    next.emojis = !running.emojis;
    next.streamers = vec![String::from("alice")];
    next.save_logs = !running.save_logs;
    next.privacy.anonymize_logs = !running.privacy.anonymize_logs;
    next.discord.events = vec![String::from("STARTUP")];

    let merged = merge_live_settings(&running, &next).unwrap();

    assert_eq!(merged.emojis, next.emojis);
    assert_eq!(merged.streamers, next.streamers);
    assert_eq!(merged.discord, next.discord);
    assert_eq!(merged.save_logs, running.save_logs);
    assert_eq!(merged.privacy, running.privacy);
    let remaining = changed_names(&merged, &next);
    assert_eq!(
        remaining,
        vec![
            (String::from("save_logs"), ReloadClass::NeedsRestart),
            (
                String::from("privacy.anonymize_logs"),
                ReloadClass::NeedsRestart
            ),
        ]
    );
}
