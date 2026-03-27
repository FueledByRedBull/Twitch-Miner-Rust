use std::cmp::Ordering;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const RELEASE_PREFIX: &str = "TwitchChannelPointsMiner";
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/0x8fv/Twitch-Channel-Points-Miner/releases/latest";
pub const UPDATER_USER_AGENT: &str = "TwitchChannelPointsMiner-Updater";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsUpdaterScript {
    pub script_path: String,
    pub script: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoUpdateOutcome {
    UpToDate,
    UpdateAvailableForDevRun { latest_version: String },
    UpdatedAndRestarting { latest_version: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpdateError {
    #[error("no matching asset for {goos}/{arch}")]
    NoMatchingAsset { goos: String, arch: String },
    #[error("missing target directory for {target_path}")]
    MissingTargetDirectory { target_path: String },
}

#[derive(Debug, Error)]
pub enum AutoUpdateError {
    #[error("http error: {0}")]
    Http(#[from] UpdateHttpError),
    #[error("update asset error: {0:?}")]
    Update(#[from] UpdateError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Error)]
pub enum UpdateHttpError {
    #[error("http client build failed: {0}")]
    BuildClient(#[from] reqwest::Error),
    #[error("release fetch failed with status {0}")]
    UnexpectedStatus(reqwest::StatusCode),
    #[error("download failed with status {0}")]
    UnexpectedDownloadStatus(reqwest::StatusCode),
    #[error("release decode failed: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub fn new_http_client(timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder().timeout(timeout).build()
}

#[must_use]
pub fn latest_release_request() -> HttpRequest {
    HttpRequest {
        url: RELEASES_URL.to_string(),
        headers: vec![
            ("Accept".into(), "application/vnd.github+json".into()),
            ("User-Agent".into(), UPDATER_USER_AGENT.into()),
        ],
    }
}

#[must_use]
pub fn download_asset_request(url: &str) -> HttpRequest {
    HttpRequest {
        url: url.to_string(),
        headers: vec![("User-Agent".into(), UPDATER_USER_AGENT.into())],
    }
}

pub fn parse_latest_release(bytes: &[u8]) -> Result<GitHubRelease, UpdateHttpError> {
    Ok(serde_json::from_slice(bytes)?)
}

pub async fn fetch_latest_release(
    client: &reqwest::Client,
) -> Result<GitHubRelease, UpdateHttpError> {
    let request = latest_release_request();
    let response = client
        .get(&request.url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", UPDATER_USER_AGENT)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(UpdateHttpError::UnexpectedStatus(response.status()));
    }
    let body = response.bytes().await?;
    parse_latest_release(body.as_ref())
}

pub async fn download_asset_bytes(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<u8>, UpdateHttpError> {
    let request = download_asset_request(url);
    let mut response = client
        .get(&request.url)
        .header("User-Agent", UPDATER_USER_AGENT)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(UpdateHttpError::UnexpectedDownloadStatus(response.status()));
    }

    let total_bytes = response.content_length();
    tracing::info!(url = %url, total_bytes, "downloading update asset");

    let mut bytes = Vec::with_capacity(
        total_bytes
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default(),
    );
    let mut downloaded = 0_u64;
    let progress_step = total_bytes.map_or(1024 * 1024, |length| (length / 4).max(64 * 1024));
    let mut next_progress_log = progress_step;

    while let Some(chunk) = response.chunk().await? {
        downloaded = downloaded.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        bytes.extend_from_slice(&chunk);
        if downloaded >= next_progress_log {
            tracing::info!(
                downloaded_bytes = downloaded,
                total_bytes,
                "update download progress"
            );
            next_progress_log = next_progress_log.saturating_add(progress_step);
        }
    }

    tracing::info!(
        downloaded_bytes = downloaded,
        total_bytes,
        "update asset download complete"
    );
    Ok(bytes)
}

pub async fn run_auto_update(
    current_version: &str,
    args: &[String],
) -> Result<AutoUpdateOutcome, AutoUpdateError> {
    let exe_path = env::current_exe()?;
    let temp_dir = env::temp_dir();
    let client = new_http_client(Duration::from_secs(15)).map_err(UpdateHttpError::BuildClient)?;
    let release = fetch_latest_release(&client).await?;
    let latest_version = normalize_release_tag(&release.tag_name);

    if compare_versions(&latest_version, &normalize_version(current_version)) != Ordering::Greater {
        return Ok(AutoUpdateOutcome::UpToDate);
    }

    if is_dev_run_executable(&exe_path.to_string_lossy(), &temp_dir.to_string_lossy()) {
        return Ok(AutoUpdateOutcome::UpdateAvailableForDevRun { latest_version });
    }

    let (goos, arch) = normalized_target();
    let asset = pick_asset(&release.assets, goos, arch)?;
    let temp_path =
        download_asset_to_dir(&client, &asset.browser_download_url, exe_path.parent()).await?;

    if goos == "windows" {
        launch_windows_updater(&exe_path, &temp_path, args)?;
    } else {
        replace_executable(&exe_path, &temp_path)?;
        relaunch(&exe_path, args)?;
    }

    Ok(AutoUpdateOutcome::UpdatedAndRestarting { latest_version })
}

#[must_use]
pub fn normalize_version(raw: &str) -> String {
    let mut version = raw.trim();
    if let Some(stripped) = version.strip_prefix('v') {
        version = stripped;
    }

    if let Some(index) = version.find([' ', '-']) {
        version = &version[..index];
    }
    if let Some(index) = version.find('+') {
        version = &version[..index];
    }
    version.to_string()
}

#[must_use]
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let left = parse_version(a);
    let right = parse_version(b);
    let max_len = left.len().max(right.len());

    for index in 0..max_len {
        let lhs = left.get(index).copied().unwrap_or(0);
        let rhs = right.get(index).copied().unwrap_or(0);
        match lhs.cmp(&rhs) {
            Ordering::Equal => {}
            other => return other,
        }
    }

    Ordering::Equal
}

#[must_use]
pub fn is_dev_run_executable(path: &str, temp_dir: &str) -> bool {
    let lower = path.to_ascii_lowercase().replace('\\', "/");
    let temp_dir = temp_dir.to_ascii_lowercase().replace('\\', "/");
    lower.contains("go-build")
        || lower.starts_with(&temp_dir)
        || lower.contains("/target/debug/")
        || lower.ends_with("/target/debug")
        || lower.contains("/target/release/")
        || lower.ends_with("/target/release")
}

pub fn pick_asset(
    assets: &[ReleaseAsset],
    goos: &str,
    arch: &str,
) -> Result<ReleaseAsset, UpdateError> {
    let mut expected = vec![format!("{RELEASE_PREFIX}-{goos}-{arch}")];
    if goos == "windows" {
        expected[0].push_str(".exe");
    }
    if goos == "darwin" {
        expected.push(format!("{RELEASE_PREFIX}-macos-{arch}"));
        expected.push(format!("{RELEASE_PREFIX}-osx-{arch}"));
    }

    for asset in assets {
        if expected
            .iter()
            .any(|candidate| asset.name.eq_ignore_ascii_case(candidate))
        {
            return Ok(asset.clone());
        }
    }

    Err(UpdateError::NoMatchingAsset {
        goos: goos.to_string(),
        arch: arch.to_string(),
    })
}

#[must_use]
pub fn format_args(args: &[String]) -> String {
    let mut out = String::new();
    for arg in args {
        out.push(' ');
        out.push_str(&quote_arg(arg));
    }
    out
}

#[must_use]
pub fn escape_for_batch(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '^' => escaped.push_str("^^"),
            '&' => escaped.push_str("^&"),
            '|' => escaped.push_str("^|"),
            '<' => escaped.push_str("^<"),
            '>' => escaped.push_str("^>"),
            '%' => escaped.push_str("%%"),
            '"' => escaped.push_str("\"\""),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub fn generate_windows_updater_script(
    target_path: &str,
    new_path: &str,
    args: &[String],
    timestamp_nanos: i64,
) -> Result<WindowsUpdaterScript, UpdateError> {
    let target_dir =
        Path::new(target_path)
            .parent()
            .ok_or_else(|| UpdateError::MissingTargetDirectory {
                target_path: target_path.to_string(),
            })?;
    let script_path = target_dir.join(format!("update-{timestamp_nanos}.bat"));

    let script = format!(
        r#"@echo off
setlocal
set "TARGET={}"
set "NEWFILE={}"
set "WORKDIR={}"
cd /D "%WORKDIR%"
:wait
ping 127.0.0.1 -n 2 >nul 2>nul
:loop
move /Y "%NEWFILE%" "%TARGET%" >nul 2>nul
if errorlevel 1 (
  ping 127.0.0.1 -n 3 >nul 2>nul
  goto loop
)
start "" /b "%TARGET%"{}
start "" /b cmd /c "del /q ""%~f0"""
exit /b
"#,
        escape_for_batch(target_path),
        escape_for_batch(new_path),
        escape_for_batch(&target_dir.to_string_lossy()),
        format_args(args)
    );

    Ok(WindowsUpdaterScript {
        script_path: script_path.to_string_lossy().to_string(),
        script,
    })
}

#[must_use]
pub fn normalize_release_tag(tag_name: &str) -> String {
    normalize_version(tag_name)
}

fn parse_version(raw: &str) -> Vec<i64> {
    raw.split('.')
        .map(|part| part.trim().parse::<i64>().unwrap_or(0))
        .collect()
}

fn quote_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for ch in arg.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\0' => quoted.push_str("\\0"),
            _ => {
                if ch.is_control() {
                    write!(quoted, "\\x{:02x}", ch as u32).expect("write to string");
                } else {
                    quoted.push(ch);
                }
            }
        }
    }
    quoted.push('"');
    quoted
}

async fn download_asset_to_dir(
    client: &reqwest::Client,
    url: &str,
    dir: Option<&Path>,
) -> Result<PathBuf, AutoUpdateError> {
    let bytes = download_asset_bytes(client, url).await?;
    let target_dir = dir.unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(target_dir)?;
    let temp_path = target_dir.join(format!(
        "miner-update-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::write(&temp_path, bytes)?;
    Ok(temp_path)
}

fn replace_executable(target_path: &Path, new_path: &Path) -> Result<(), AutoUpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(new_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(new_path, permissions)?;
    }
    fs::rename(new_path, target_path)?;
    Ok(())
}

fn relaunch(target_path: &Path, args: &[String]) -> Result<(), AutoUpdateError> {
    let mut command = Command::new(target_path);
    command.args(args);
    command.stdin(std::process::Stdio::inherit());
    command.stdout(std::process::Stdio::inherit());
    command.stderr(std::process::Stdio::inherit());
    command.spawn()?;
    Ok(())
}

fn launch_windows_updater(
    target_path: &Path,
    new_path: &Path,
    args: &[String],
) -> Result<(), AutoUpdateError> {
    let script = generate_windows_updater_script(
        &target_path.to_string_lossy(),
        &new_path.to_string_lossy(),
        args,
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        )
        .unwrap_or(i64::MAX),
    )?;
    fs::write(&script.script_path, script.script)?;
    Command::new("cmd")
        .args(["/C", &script.script_path])
        .spawn()?;
    Ok(())
}

fn normalized_target() -> (&'static str, &'static str) {
    let goos = match env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        other => other,
    };
    (goos, arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_versions_like_go() {
        assert_eq!(normalize_version(" v1.2 "), "1.2");
        assert_eq!(normalize_version("v1.2.3-alpha+build"), "1.2.3");
        assert_eq!(normalize_version("1.2 beta"), "1.2");
        assert_eq!(normalize_version("1.2-beta"), "1.2");
        assert_eq!(normalize_release_tag("v2.0.1+meta"), "2.0.1");
    }

    #[test]
    fn builds_release_and_download_requests() {
        let release = latest_release_request();
        assert_eq!(release.url, RELEASES_URL);
        assert!(release
            .headers
            .contains(&("Accept".into(), "application/vnd.github+json".into())));
        assert!(release
            .headers
            .contains(&("User-Agent".into(), UPDATER_USER_AGENT.into())));

        let download = download_asset_request("https://example.invalid/file");
        assert_eq!(download.url, "https://example.invalid/file");
        assert_eq!(
            download.headers,
            vec![("User-Agent".into(), UPDATER_USER_AGENT.into())]
        );
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
        let assets = vec![
            ReleaseAsset {
                name: String::from("TwitchChannelPointsMiner-linux-amd64"),
                browser_download_url: String::from("https://example.invalid/linux"),
            },
            ReleaseAsset {
                name: String::from("TwitchChannelPointsMiner-windows-amd64.exe"),
                browser_download_url: String::from("https://example.invalid/windows"),
            },
            ReleaseAsset {
                name: String::from("TwitchChannelPointsMiner-macos-arm64"),
                browser_download_url: String::from("https://example.invalid/macos"),
            },
        ];

        let linux = pick_asset(&assets, "linux", "amd64").unwrap();
        assert_eq!(linux.name, "TwitchChannelPointsMiner-linux-amd64");

        let windows = pick_asset(&assets, "windows", "amd64").unwrap();
        assert_eq!(windows.name, "TwitchChannelPointsMiner-windows-amd64.exe");

        let macos = pick_asset(&assets, "darwin", "arm64").unwrap();
        assert_eq!(macos.name, "TwitchChannelPointsMiner-macos-arm64");

        let err = pick_asset(&assets, "freebsd", "amd64").unwrap_err();
        assert_eq!(
            err,
            UpdateError::NoMatchingAsset {
                goos: String::from("freebsd"),
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
        let script = generate_windows_updater_script(
            "C:\\app\\twitch-miner.exe",
            "C:\\app\\miner-update.exe",
            &[String::from("--flag"), String::from("two words")],
            123,
        )
        .unwrap();

        assert_eq!(script.script_path, "C:\\app\\update-123.bat");
        assert!(script
            .script
            .contains("set \"TARGET=C:\\app\\twitch-miner.exe\""));
        assert!(script
            .script
            .contains("set \"NEWFILE=C:\\app\\miner-update.exe\""));
        assert!(script.script.contains("set \"WORKDIR=C:\\app\""));
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

    #[test]
    fn normalized_target_maps_rust_names() {
        let (goos, arch) = normalized_target();
        assert!(!goos.is_empty());
        assert!(!arch.is_empty());
        assert_ne!(goos, "macos");
        assert_ne!(arch, "x86_64");
        assert_ne!(arch, "aarch64");
    }
}
