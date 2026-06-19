use std::cmp::Ordering;

use tm_updater::{
    compare_versions, configured_release_contract, escape_for_batch, format_args,
    generate_windows_updater_script, is_dev_run_executable, latest_release_request,
    normalize_release_tag, normalize_version, parse_latest_release, pick_asset, ReleaseAsset,
    UpdateError, PROJECT_DISPLAY_NAME, PROJECT_REPOSITORY_URL, RELEASES_API_URL,
    UPDATER_USER_AGENT,
};

#[test]
fn normalizes_versions_like_go() {
    assert_eq!(normalize_version(" v1.2 "), "1.2");
    assert_eq!(normalize_version("v1.2.3-alpha+build"), "1.2.3");
    assert_eq!(normalize_version("1.2 beta"), "1.2");
    assert_eq!(normalize_version("1.2-beta"), "1.2");
    assert_eq!(normalize_release_tag("v2.0.1+meta"), "2.0.1");
}

#[test]
fn rust_project_has_no_release_contract_yet() {
    assert_eq!(PROJECT_DISPLAY_NAME, "Twitch Channel Points Miner");
    assert_eq!(
        PROJECT_REPOSITORY_URL,
        "https://github.com/FueledByRedBull/Twitch-Miner-Rust"
    );
    assert_eq!(configured_release_contract().release_prefix, "tm-app");
    assert_eq!(
        configured_release_contract().releases_url,
        "https://api.github.com/repos/FueledByRedBull/Twitch-Miner-Rust/releases/latest"
    );
    assert!(tm_updater::release_contract().is_none());
}

#[test]
fn configured_release_contract_builds_latest_release_request() {
    let contract = configured_release_contract();
    let request = latest_release_request(contract);

    assert_eq!(request.url, RELEASES_API_URL);
    assert!(request
        .headers
        .contains(&(String::from("User-Agent"), String::from(UPDATER_USER_AGENT))));
}

#[test]
fn parses_latest_release_payload() {
    let parsed = parse_latest_release(
        br#"{"tag_name":"v1.2.3","assets":[{"name":"TwitchChannelPointsMiner-linux-amd64","browser_download_url":"https://example.invalid/linux"}]}"#,
    )
    .unwrap();
    assert_eq!(parsed.tag_name, "v1.2.3");
    assert_eq!(parsed.assets.len(), 1);
    assert_eq!(
        parsed.assets[0].name,
        "TwitchChannelPointsMiner-linux-amd64"
    );
}

#[test]
fn compares_versions_numerically() {
    assert_eq!(compare_versions("1.2", "1.2"), Ordering::Equal);
    assert_eq!(compare_versions("1.2.1", "1.2.0"), Ordering::Greater);
    assert_eq!(compare_versions("1.2", "1.2.0"), Ordering::Equal);
    assert_eq!(compare_versions("1.2.0", "1.10.0"), Ordering::Less);
    assert_eq!(compare_versions("1.x.3", "1.0.4"), Ordering::Less);
}

#[test]
fn detects_dev_run_executables() {
    assert!(is_dev_run_executable(
        "C:/Users/me/AppData/Local/Temp/go-build1234/b001/exe/main.exe",
        "C:/Users/me/AppData/Local/Temp"
    ));
    assert!(is_dev_run_executable(
        "C:/Users/me/AppData/Local/Temp/main.exe",
        "C:/Users/me/AppData/Local/Temp"
    ));
    assert!(is_dev_run_executable(
        "C:/work/Twitch-Miner-Rust/target/debug/tm-app.exe",
        "C:/Users/me/AppData/Local/Temp"
    ));
    assert!(is_dev_run_executable(
        "C:/work/Twitch-Miner-Rust/target/release/tm-app.exe",
        "C:/Users/me/AppData/Local/Temp"
    ));
    assert!(!is_dev_run_executable(
        "C:/Program Files/TwitchMiner/twitch-miner.exe",
        "C:/Users/me/AppData/Local/Temp"
    ));
}

#[test]
fn picks_matching_assets() {
    let contract = configured_release_contract();
    let assets = vec![
        ReleaseAsset {
            name: String::from("tm-app-linux-amd64"),
            browser_download_url: String::from("https://example.invalid/linux"),
        },
        ReleaseAsset {
            name: String::from("tm-app-windows-amd64.exe"),
            browser_download_url: String::from("https://example.invalid/windows"),
        },
        ReleaseAsset {
            name: String::from("tm-app-macos-arm64"),
            browser_download_url: String::from("https://example.invalid/macos"),
        },
    ];

    let linux = pick_asset(&assets, "linux", "amd64", contract).unwrap();
    assert_eq!(linux.name, "tm-app-linux-amd64");

    let windows = pick_asset(&assets, "windows", "amd64", contract).unwrap();
    assert_eq!(windows.name, "tm-app-windows-amd64.exe");

    let macos = pick_asset(&assets, "darwin", "arm64", contract).unwrap();
    assert_eq!(macos.name, "tm-app-macos-arm64");

    let err = pick_asset(&assets, "freebsd", "amd64", contract).unwrap_err();
    assert_eq!(
        err,
        UpdateError::NoMatchingAsset {
            goos: String::from("freebsd"),
            arch: String::from("amd64"),
        }
    );
}

#[test]
fn legacy_go_asset_names_are_not_accepted() {
    let contract = configured_release_contract();
    let assets = vec![ReleaseAsset {
        name: String::from("TwitchChannelPointsMiner-windows-amd64.exe"),
        browser_download_url: String::from("https://example.invalid/windows"),
    }];
    let err = pick_asset(&assets, "windows", "amd64", contract).unwrap_err();
    assert_eq!(
        err,
        UpdateError::NoMatchingAsset {
            goos: String::from("windows"),
            arch: String::from("amd64"),
        }
    );
}

#[test]
fn quotes_and_escapes_arguments_like_go() {
    let args = vec![
        String::from("plain"),
        String::from("with space"),
        String::from("a\"b"),
        String::from("line\nbreak"),
    ];
    assert_eq!(
        format_args(&args),
        " \"plain\" \"with space\" \"a\\\"b\" \"line\\nbreak\""
    );
}

#[test]
fn escapes_batch_paths_and_builds_script() {
    let (target, new_file, expected_script_path, expected_workdir) = if cfg!(windows) {
        (
            "C:\\app\\twitch-miner.exe",
            "C:\\app\\miner-update.exe",
            "C:\\app\\update-123.bat",
            "C:\\app",
        )
    } else {
        (
            "/app/twitch-miner.exe",
            "/app/miner-update.exe",
            "/app/update-123.bat",
            "/app",
        )
    };
    let script = generate_windows_updater_script(
        target,
        new_file,
        &[String::from("--flag"), String::from("two words")],
        123,
    )
    .unwrap();

    assert_eq!(script.script_path, expected_script_path);
    assert!(script.script.contains(&format!("set \"TARGET={target}\"")));
    assert!(script
        .script
        .contains(&format!("set \"NEWFILE={new_file}\"")));
    assert!(script
        .script
        .contains(&format!("set \"WORKDIR={expected_workdir}\"")));
    assert!(script
        .script
        .contains("start \"\" /b \"%TARGET%\" \"--flag\" \"two words\""));
}

#[test]
fn batch_escape_matches_go_characters() {
    assert_eq!(
        escape_for_batch(r#"C:\path\100% ^ & | < > ""#),
        r#"C:\path\100%% ^^ ^& ^| ^< ^> """#
    );
}
