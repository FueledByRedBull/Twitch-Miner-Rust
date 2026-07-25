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
    assert_eq!(config.password, "");
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
fn validation_rejects_disabling_tls_certificate_verification() {
    let config = ConfigFile {
        username: String::from("Alice"),
        disable_ssl_cert_verification: true,
        ..ConfigFile::default()
    };

    let error = validate_config(&config).unwrap_err();
    assert!(
        matches!(error, ConfigError::Validation(message) if message.contains("disable_ssl_cert_verification"))
    );
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
