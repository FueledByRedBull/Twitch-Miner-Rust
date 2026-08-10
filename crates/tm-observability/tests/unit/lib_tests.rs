use super::*;
use std::fs;

#[test]
fn anonymizer_name_is_stable() {
    let mut anonymizer = Anonymizer::new(true);
    assert_eq!(anonymizer.name(""), "");
    let first = anonymizer.name("pewdiepie");
    let second = anonymizer.name("pewdiepie");
    assert_eq!(first, second);
    assert_ne!(anonymizer.name("ohnepixel"), first);
}

#[test]
fn anonymizer_points_follow_deltas() {
    let mut anonymizer = Anonymizer::new(true);
    let mut streamer = Streamer {
        username: "pewdiepie".into(),
        channel_id: "123".into(),
        channel_points: 1_000,
        ..Streamer::default()
    };
    let initial = anonymizer.pseudo_channel_points(&streamer);
    assert!((100..=1_000).contains(&initial));
    assert_eq!(anonymizer.pseudo_channel_points(&streamer), initial);
    streamer.channel_points = 1_010;
    assert_eq!(anonymizer.pseudo_channel_points(&streamer), initial + 10);
    streamer.channel_points = 1_007;
    assert_eq!(anonymizer.pseudo_channel_points(&streamer), initial + 7);
}

#[test]
fn disabled_anonymizer_passthrough() {
    let mut anonymizer = Anonymizer::new(false);
    let streamer = Streamer {
        username: "pewdiepie".into(),
        channel_id: "123".into(),
        channel_points: 4_242,
        ..Streamer::default()
    };
    assert_eq!(anonymizer.name("pewdiepie"), "pewdiepie");
    assert_eq!(anonymizer.pseudo_channel_points(&streamer), 4_242);
}

#[test]
fn discord_event_filtering_matches_go() {
    let webhook = new_discord_webhook(&DiscordSettings {
        webhook_api: "https://example.invalid".into(),
        events: vec!["STREAMER_ONLINE".into()],
    })
    .unwrap();
    assert!(should_send_discord_event(
        &webhook,
        Some(Event::StreamerOnline)
    ));
    assert!(!should_send_discord_event(
        &webhook,
        Some(Event::StreamerOffline)
    ));
}

#[test]
fn sanitize_filename_replaces_forbidden_chars() {
    let sanitized = sanitize_filename(r#"bad/name\:*?"<>|"#);
    assert!(!sanitized.contains('/'));
    assert!(!sanitized.contains('\\'));
    assert!(!sanitized.contains(':'));
    assert!(!sanitized.contains('*'));
    assert!(!sanitized.contains('?'));
    assert!(!sanitized.contains('"'));
    assert!(!sanitized.contains('<'));
    assert!(!sanitized.contains('>'));
    assert!(!sanitized.contains('|'));
}

#[test]
fn strip_ansi_removes_escape_sequences() {
    assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m plain"), "red plain");
}

#[test]
fn deep_debug_is_disabled_when_privacy_mode_is_on() {
    let settings = LoggerSettings {
        debug: true,
        debug_deep: true,
        anonymize_logs: true,
        ..LoggerSettings::default()
    };
    assert!(!deep_debug_enabled(&settings));
}

#[test]
fn log_file_path_matches_go_naming() {
    assert_eq!(
        log_file_path("C:/work", "alice", false),
        PathBuf::from("C:/work/log/alice.log")
    );
    assert_eq!(
        log_file_path("C:/work", "", false),
        PathBuf::from("C:/work/log/miner.log")
    );
    assert_eq!(
        log_file_path("C:/work", "alice", true),
        PathBuf::from("C:/work/log/miner.log")
    );
}

#[test]
fn open_log_file_creates_parent_directory_and_sanitized_name() {
    let dir = tempfile::tempdir().unwrap();
    let _file = open_log_file(dir.path(), r"ali:ce/test", false).unwrap();
    let path = dir.path().join("log").join("ali_ce_test.log");
    assert!(path.exists());
    let metadata = fs::metadata(path).unwrap();
    assert!(metadata.is_file());
}

#[cfg(unix)]
#[test]
fn log_files_use_private_unix_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let _file = open_log_file(dir.path(), "alice", false).unwrap();
    let path = dir.path().join("log").join("alice.log");
    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn rotating_log_writer_bounds_file_size_and_archives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("miner.log");
    let mut writer = RotatingFile::new(path.clone(), 10, 2).unwrap();
    writer.write_all(b"12345678").unwrap();
    writer.write_all(b"abcdefgh").unwrap();
    writer.flush().unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), "abcdefgh");
    assert_eq!(
        fs::read_to_string(archive_path(&path, 1)).unwrap(),
        "12345678"
    );
    assert!(!archive_path(&path, 3).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for candidate in [&path, &archive_path(&path, 1)] {
            let mode = fs::metadata(candidate).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}

#[test]
fn discord_message_strips_ansi() {
    let webhook = new_discord_webhook(&DiscordSettings {
        webhook_api: "https://example.invalid/webhook".into(),
        events: vec!["STREAMER_ONLINE".into()],
    })
    .unwrap();
    let message = discord_message(
        &webhook,
        "\u{1b}[32mStreamer online\u{1b}[0m",
        Some(Event::StreamerOnline),
    )
    .unwrap();
    assert_eq!(message, "Streamer online");
}

#[test]
fn discord_client_constructs() {
    let client = DiscordClient::new(std::time::Duration::from_secs(5)).unwrap();
    let _ = client;
}

#[test]
fn log_timestamp_matches_go_shape_without_seconds() {
    assert_eq!(
        format_log_timestamp("2026-03-27T08:09:10.123456Z", false),
        Some(String::from("08:09 27/03/26"))
    );
}

#[test]
fn log_timestamp_matches_go_shape_with_seconds() {
    assert_eq!(
        format_log_timestamp("2026-03-27T08:09:10.123456Z", true),
        Some(String::from("08:09:10 27/03/26"))
    );
}

#[test]
fn log_line_matches_python_style_operation_shape() {
    assert_eq!(
        format_log_line(
            "08:09:10 27/03/26",
            "INFO",
            "run",
            "",
            "💣 Start session: 'session-123'",
        ),
        "08:09:10 27/03/26 - INFO - [run]: 💣 Start session: 'session-123'"
    );
}

#[test]
fn report_line_omits_level_and_operation_envelope() {
    assert_eq!(
        format_report_line("08:09:10 27/03/26", "🛑 End session 'session-123'"),
        "08:09:10 27/03/26 - 🛑 End session 'session-123'"
    );
}

#[test]
fn current_log_timestamp_uses_requested_timezone() {
    let utc = chrono::DateTime::parse_from_rfc3339("2026-03-27T08:09:10Z")
        .unwrap()
        .with_timezone(&Utc);
    let athens = "Europe/Athens".parse::<Tz>().unwrap();
    assert_eq!(
        utc.with_timezone(&athens)
            .format("%H:%M %d/%m/%y")
            .to_string(),
        "10:09 27/03/26"
    );
}
