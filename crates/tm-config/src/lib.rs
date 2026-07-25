#![recursion_limit = "256"]
//! Configuration loading, migration, validation, and path resolution.
//!
//! Invalid or future schemas fail before write-back. Migrations preserve user
//! intent, create private backups, and replace configuration atomically; typed
//! setting composition is kept separate from filesystem mutation.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tm_domain::FollowersOrder;
#[cfg(test)]
use tm_domain::IrcMode;

mod settings;

pub use settings::{build_base_streamer_settings, build_override_settings, parse_chat_presence};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid config: {0}")]
    InvalidConfig(#[from] serde_json::Error),
    #[error("config io error: {0}")]
    Io(#[from] io::Error),
    #[error("config validation failed: {0}")]
    Validation(String),
}

pub const CONFIG_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPreview {
    pub config: ConfigFile,
    pub migration_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub work_dir: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveAppPathsInput {
    pub config_flag: Option<PathBuf>,
    pub data_dir_flag: Option<PathBuf>,
    pub env_config: Option<String>,
    pub env_data_dir: Option<String>,
    pub cwd: PathBuf,
    pub executable_path: Option<PathBuf>,
    pub executable_is_temp: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FilterConditionConfig {
    pub by: Option<String>,
    #[serde(rename = "where")]
    pub condition: Option<String>,
    pub value: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BetConfig {
    pub strategy: Option<String>,
    pub percentage: Option<u32>,
    pub percentage_gap: Option<u32>,
    pub max_points: Option<u32>,
    pub stealth_mode: Option<bool>,
    pub deduct_stake_on_place: Option<bool>,
    pub delay_mode: Option<String>,
    pub delay: Option<f64>,
    pub minimum_points: Option<u32>,
    pub filter_condition: Option<FilterConditionConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamerSettingsOverride {
    pub make_predictions: Option<bool>,
    pub follow_raid: Option<bool>,
    pub farm_drops: Option<bool>,
    pub claim_drops: Option<bool>,
    pub watch_one_stream_when_drops_active: Option<bool>,
    pub claim_moments: Option<bool>,
    pub watch_streak: Option<bool>,
    pub watch_streak_vod_recovery: Option<bool>,
    pub community_goals: Option<bool>,
    pub chat_presence: Option<String>,
    pub bet: BetConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PrivacyConfig {
    pub anonymize_logs: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DiscordConfig {
    pub webhook_api: String,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// This is the stable, flat on-disk schema. Grouping flags would change the public config shape.
#[allow(clippy::struct_excessive_bools)]
/// Validated public configuration schema.
///
/// Obtain this through [`load_or_create_config`] or [`preview_config`] so schema
/// checks and migrations run before use.
pub struct ConfigFile {
    #[serde(default = "current_schema_version")]
    pub config_schema_version: u64,
    pub username: String,
    #[serde(default)]
    pub password: String,
    pub debug: bool,
    pub debug_deep: bool,
    pub watch_queue_logging: bool,
    pub smart_logging: bool,
    pub disable_ssl_cert_verification: bool,
    pub show_seconds: bool,
    pub claim_drops_startup: bool,
    pub farm_drops: bool,
    pub claim_drops: bool,
    pub watch_one_stream_when_drops_active: bool,
    pub claim_moments: bool,
    #[serde(default)]
    pub watch_streak_vod_recovery: bool,
    #[serde(rename = "betting(make_predictions)")]
    pub betting_make_predictions: bool,
    pub follow_raid: bool,
    pub community_goals: bool,
    pub emojis: bool,
    pub save_logs: bool,
    pub show_username_in_console: bool,
    pub show_claimed_bonus_msg: bool,
    pub show_game: bool,
    pub chat_presence: String,
    pub disable_at_in_nickname: bool,
    pub streamers: Vec<String>,
    pub streamers_exclude: Vec<String>,
    pub game_priority: Vec<String>,
    pub game_exclude: Vec<String>,
    pub watch_priority: Vec<String>,
    #[serde(default)]
    pub followers_order: FollowersOrder,
    pub bet: BetConfig,
    pub timezone: Option<String>,
    pub privacy: PrivacyConfig,
    pub discord: DiscordConfig,
    pub streamer_overrides: HashMap<String, StreamerSettingsOverride>,
}

const fn current_schema_version() -> u64 {
    CONFIG_SCHEMA_VERSION
}

impl Default for ConfigFile {
    // The built-in JSON value is compiled with this schema and covered by an exact round-trip
    // test; failure is an internal release defect, never a response to user-provided input.
    #[allow(clippy::expect_used)]
    fn default() -> Self {
        serde_json::from_value(default_config_value()).expect("default config must deserialize")
    }
}

#[must_use]
pub fn default_config_value() -> Value {
    json!({
        "config_schema_version": CONFIG_SCHEMA_VERSION,
        "username": "your-twitch-username",
        "debug": false,
        "debug_deep": false,
        "watch_queue_logging": false,
        "smart_logging": true,
        "disable_ssl_cert_verification": false,
        "show_seconds": false,
        "claim_drops_startup": true,
        "farm_drops": true,
        "claim_drops": true,
        "watch_one_stream_when_drops_active": true,
        "claim_moments": true,
        "watch_streak_vod_recovery": false,
        "betting(make_predictions)": true,
        "follow_raid": true,
        "community_goals": false,
        "emojis": true,
        "save_logs": false,
        "show_username_in_console": false,
        "show_claimed_bonus_msg": true,
        "show_game": true,
        "chat_presence": "ONLINE",
        "disable_at_in_nickname": false,
        "streamers": [],
        "streamers_exclude": [],
        "game_priority": [],
        "game_exclude": [],
        "watch_priority": ["STREAK", "DROPS", "ORDER"],
        "followers_order": "DESC",
        "timezone": Value::Null,
        "privacy": {
            "anonymize_logs": false
        },
        "discord": {
            "webhook_api": "",
            "events": []
        },
        "streamer_overrides": {},
        "bet": {
            "strategy": Value::Null,
            "percentage": Value::Null,
            "percentage_gap": Value::Null,
            "max_points": Value::Null,
            "stealth_mode": Value::Null,
            "deduct_stake_on_place": true,
            "delay_mode": Value::Null,
            "delay": Value::Null,
            "minimum_points": Value::Null,
            "filter_condition": {
                "by": Value::Null,
                "where": Value::Null,
                "value": Value::Null
            }
        }
    })
}

pub fn load_or_create_config(path: &Path) -> Result<ConfigFile, ConfigError> {
    Ok(load_config(path, true)?.config)
}

pub fn preview_config(path: &Path) -> Result<ConfigPreview, ConfigError> {
    load_config(path, false)
}

fn load_config(path: &Path, write_back: bool) -> Result<ConfigPreview, ConfigError> {
    let mut changed = false;
    let existed = path.is_file();
    let mut value = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            changed = true;
            Value::Object(Map::new())
        }
        Err(error) => return Err(error.into()),
    };

    if !value.is_object() {
        return Err(ConfigError::InvalidConfig(serde_json::Error::io(
            io::Error::new(
                io::ErrorKind::InvalidData,
                "config root must be a JSON object",
            ),
        )));
    }

    migrate_removed_options(&mut value, &mut changed)?;
    migrate_drop_farming_options(&mut value, &mut changed);
    validate_schema_version(&value)?;

    changed |= fill_missing_top_level(&mut value, &default_config_value());
    validate_object_section(&value, "privacy")?;
    validate_object_section(&value, "discord")?;
    validate_object_section(&value, "bet")?;
    validate_object_section(&value, "streamer_overrides")?;
    validate_nested_object(&value, "bet", "filter_condition")?;
    validate_streamer_override_shapes(&value)?;
    let privacy_defaults = privacy_defaults();
    let discord_defaults = discord_defaults();
    let bet_defaults = bet_defaults();
    let filter_condition_defaults = filter_condition_defaults();

    changed |= ensure_object_section(&mut value, "privacy");
    changed |= ensure_nested_defaults(&mut value, "privacy", &privacy_defaults);
    changed |= ensure_object_section(&mut value, "discord");
    changed |= ensure_nested_defaults(&mut value, "discord", &discord_defaults);
    changed |= ensure_object_section(&mut value, "bet");
    changed |= ensure_nested_defaults(&mut value, "bet", &bet_defaults);
    changed |= ensure_object_section(&mut value, "streamer_overrides");

    let bet_value = value
        .as_object_mut()
        .and_then(|root| root.get_mut("bet"))
        .ok_or_else(|| ConfigError::Validation(String::from("config.bet must be a JSON object")))?;
    changed |= ensure_object_key(
        bet_value,
        "filter_condition",
        filter_condition_defaults.clone(),
    );
    changed |= ensure_nested_defaults(bet_value, "filter_condition", &filter_condition_defaults);
    changed |=
        ensure_streamer_override_defaults(&mut value, &bet_defaults, &filter_condition_defaults);

    if write_back && existed {
        set_private_config_permissions(path)?;
    }
    if changed && write_back {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if existed {
            let backup = config_backup_path(path);
            fs::copy(path, &backup)?;
            set_private_config_permissions(&backup)?;
        }
        atomic_write(path, &serde_json::to_vec_pretty(&value)?)?;
    }

    Ok(ConfigPreview {
        config: serde_json::from_value(value)?,
        migration_required: changed,
    })
}

pub fn validate_config(config: &ConfigFile) -> Result<(), ConfigError> {
    let username = config.username.trim().to_lowercase();
    if username.is_empty() || username == "your-twitch-username" {
        return Err(ConfigError::Validation(String::from(
            "config.username must be set to a Twitch username",
        )));
    }
    if !config.password.trim().is_empty() {
        return Err(ConfigError::Validation(String::from(
            "config.password is no longer used; remove it from config.json",
        )));
    }
    if config.disable_ssl_cert_verification {
        return Err(ConfigError::Validation(String::from(
            "config.disable_ssl_cert_verification is no longer supported; remove it or set it to false",
        )));
    }
    validate_bet_config("config.bet", &config.bet)?;
    for (login, override_settings) in &config.streamer_overrides {
        validate_bet_config(
            &format!("config.streamer_overrides.{login}.bet"),
            &override_settings.bet,
        )?;
    }
    Ok(())
}

fn validate_bet_config(path: &str, bet: &BetConfig) -> Result<(), ConfigError> {
    if bet.percentage.is_some_and(|value| value > 100) {
        return Err(ConfigError::Validation(format!(
            "{path}.percentage must be between 0 and 100"
        )));
    }
    if bet.percentage_gap.is_some_and(|value| value > 100) {
        return Err(ConfigError::Validation(format!(
            "{path}.percentage_gap must be between 0 and 100"
        )));
    }
    if let Some(delay) = bet.delay {
        if !delay.is_finite() || delay < 0.0 {
            return Err(ConfigError::Validation(format!(
                "{path}.delay must be a finite, non-negative number"
            )));
        }
        if bet
            .delay_mode
            .as_deref()
            .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("PERCENTAGE"))
            && delay > 1.0
        {
            return Err(ConfigError::Validation(format!(
                "{path}.delay must be between 0 and 1 for PERCENTAGE delay_mode"
            )));
        }
    }
    if bet
        .filter_condition
        .as_ref()
        .and_then(|condition| condition.value)
        .is_some_and(|value| !value.is_finite())
    {
        return Err(ConfigError::Validation(format!(
            "{path}.filter_condition.value must be a finite number"
        )));
    }
    Ok(())
}

fn migrate_removed_options(value: &mut Value, changed: &mut bool) -> Result<(), ConfigError> {
    let root = value
        .as_object_mut()
        .ok_or_else(|| ConfigError::Validation(String::from("config root must be an object")))?;
    if let Some(auto_update) = root.get("auto_update") {
        match auto_update.as_bool() {
            Some(true) => {
                return Err(ConfigError::Validation(String::from(
                    "config.auto_update is no longer supported; remove it before starting",
                )));
            }
            Some(false) => {}
            None => {
                return Err(ConfigError::Validation(String::from(
                    "config.auto_update must be a boolean when present",
                )));
            }
        }
        root.remove("auto_update");
        *changed = true;
    }
    Ok(())
}

fn migrate_drop_farming_options(value: &mut Value, changed: &mut bool) {
    let Some(root) = value.as_object_mut() else {
        return;
    };
    if !root.contains_key("farm_drops") {
        let legacy_value = root
            .get("claim_drops")
            .and_then(Value::as_bool)
            .map_or(Value::Bool(true), Value::Bool);
        root.insert(String::from("farm_drops"), legacy_value);
        *changed = true;
    }
    let Some(overrides) = root
        .get_mut("streamer_overrides")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for override_value in overrides.values_mut().filter_map(Value::as_object_mut) {
        if override_value.contains_key("farm_drops") {
            continue;
        }
        let legacy_value = override_value
            .get("claim_drops")
            .cloned()
            .unwrap_or(Value::Null);
        override_value.insert(String::from("farm_drops"), legacy_value);
        *changed = true;
    }
}

fn validate_schema_version(value: &Value) -> Result<(), ConfigError> {
    let Some(version) = value.get("config_schema_version") else {
        return Ok(());
    };
    let Some(version) = version.as_u64() else {
        return Err(ConfigError::Validation(String::from(
            "config.config_schema_version must be a positive integer",
        )));
    };
    if version > CONFIG_SCHEMA_VERSION {
        return Err(ConfigError::Validation(format!(
            "config schema version {version} is newer than supported version {CONFIG_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

fn config_backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

fn atomic_write(path: &Path, payload: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = open_private_config_file(&temporary)?;
        file.write_all(payload)?;
        file.sync_all()?;
        match fs::rename(&temporary, path) {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(_) if path.is_file() => replace_windows_config_file(&temporary, path),
            Err(error) => Err(error),
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_windows_config_file(temporary: &Path, path: &Path) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let replacement_backup =
        path.with_file_name(format!(".{file_name}.{}.replace.tmp", std::process::id()));

    fs::rename(path, &replacement_backup)?;
    if let Err(error) = fs::rename(temporary, path) {
        let _ = fs::rename(&replacement_backup, path);
        return Err(error);
    }
    let _ = fs::remove_file(replacement_backup);
    Ok(())
}

#[cfg(unix)]
fn open_private_config_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_config_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
}

#[cfg(unix)]
fn set_private_config_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_config_permissions(path: &Path) -> io::Result<()> {
    fs::metadata(path).map(|_| ())
}

pub fn resolve_app_paths(input: &ResolveAppPathsInput) -> io::Result<AppPaths> {
    if let Some(data_dir_flag) = input.data_dir_flag.as_ref() {
        let work_dir = absolutize(data_dir_flag, &input.cwd);
        let config_path = input.config_flag.as_ref().map_or_else(
            || work_dir.join("config.json"),
            |path| absolutize(path, &input.cwd),
        );
        return Ok(AppPaths {
            work_dir,
            config_path,
        });
    }

    if let Some(config_flag) = input.config_flag.as_ref() {
        let config_path = absolutize(config_flag, &input.cwd);
        return Ok(AppPaths {
            work_dir: config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            config_path,
        });
    }

    if let Some(data_dir) = input
        .env_data_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let work_dir = absolutize(Path::new(data_dir), &input.cwd);
        return Ok(AppPaths {
            config_path: work_dir.join("config.json"),
            work_dir,
        });
    }

    if let Some(config_path) = input
        .env_config
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let config_path = absolutize(Path::new(config_path), &input.cwd);
        return Ok(AppPaths {
            work_dir: config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            config_path,
        });
    }

    let cwd_config = input.cwd.join("config.json");
    if cwd_config.is_file() {
        return Ok(AppPaths {
            work_dir: input.cwd.clone(),
            config_path: cwd_config,
        });
    }

    if let Some(executable_path) = input.executable_path.as_ref() {
        if !input.executable_is_temp {
            let work_dir = executable_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            return Ok(AppPaths {
                config_path: work_dir.join("config.json"),
                work_dir,
            });
        }
    }

    Ok(AppPaths {
        work_dir: input.cwd.clone(),
        config_path: input.cwd.join("config.json"),
    })
}

pub fn resolve_app_paths_from_env(
    config_flag: Option<PathBuf>,
    data_dir_flag: Option<PathBuf>,
) -> io::Result<AppPaths> {
    let cwd = env::current_dir()?;
    let executable_path = env::current_exe().ok();
    let executable_is_temp = executable_path
        .as_ref()
        .is_some_and(|path| is_go_run_executable(path));
    resolve_app_paths(&ResolveAppPathsInput {
        config_flag,
        data_dir_flag,
        env_config: env::var("TCPM_CONFIG").ok(),
        env_data_dir: env::var("TCPM_DATA_DIR").ok(),
        cwd,
        executable_path,
        executable_is_temp,
    })
}

#[must_use]
pub fn default_user_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join("TwitchChannelPointsMiner"))
}

fn fill_missing_top_level(value: &mut Value, defaults: &Value) -> bool {
    let Some(root) = value.as_object_mut() else {
        return false;
    };
    let Some(default_root) = defaults.as_object() else {
        return false;
    };
    let mut changed = false;
    for (key, default_value) in default_root {
        if !root.contains_key(key) {
            root.insert(key.clone(), default_value.clone());
            changed = true;
        }
    }
    changed
}

fn validate_object_section(value: &Value, key: &str) -> Result<(), ConfigError> {
    let Some(root) = value.as_object() else {
        return Ok(());
    };
    if root.get(key).is_some_and(|section| !section.is_object()) {
        return Err(ConfigError::Validation(format!(
            "config.{key} must be a JSON object"
        )));
    }
    Ok(())
}

fn validate_nested_object(value: &Value, parent: &str, key: &str) -> Result<(), ConfigError> {
    let Some(root) = value.as_object() else {
        return Ok(());
    };
    let Some(parent_value) = root.get(parent).and_then(Value::as_object) else {
        return Ok(());
    };
    if parent_value
        .get(key)
        .is_some_and(|nested_value| !nested_value.is_object())
    {
        return Err(ConfigError::Validation(format!(
            "config.{parent}.{key} must be a JSON object"
        )));
    }
    Ok(())
}

fn validate_streamer_override_shapes(value: &Value) -> Result<(), ConfigError> {
    let Some(root) = value.as_object() else {
        return Ok(());
    };
    let Some(overrides) = root.get("streamer_overrides").and_then(Value::as_object) else {
        return Ok(());
    };
    for (login, override_value) in overrides {
        if !override_value.is_object() {
            return Err(ConfigError::Validation(format!(
                "config.streamer_overrides.{login} must be a JSON object"
            )));
        }
        let Some(override_object) = override_value.as_object() else {
            continue;
        };
        if override_object
            .get("bet")
            .is_some_and(|bet_value| !bet_value.is_object())
        {
            return Err(ConfigError::Validation(format!(
                "config.streamer_overrides.{login}.bet must be a JSON object"
            )));
        }
        if let Some(bet) = override_object.get("bet").and_then(Value::as_object) {
            if bet
                .get("filter_condition")
                .is_some_and(|filter| !filter.is_object())
            {
                return Err(ConfigError::Validation(format!(
                    "config.streamer_overrides.{login}.bet.filter_condition must be a JSON object"
                )));
            }
        }
    }
    Ok(())
}

fn ensure_object_section(value: &mut Value, key: &str) -> bool {
    let Some(root) = value.as_object_mut() else {
        return false;
    };
    if let Some(Value::Object(_)) = root.get(key) {
        false
    } else {
        root.insert(key.to_string(), Value::Object(Map::new()));
        true
    }
}

fn ensure_object_key(value: &mut Value, key: &str, default_value: Value) -> bool {
    let Some(root) = value.as_object_mut() else {
        return false;
    };
    if let Some(Value::Object(_)) = root.get(key) {
        false
    } else {
        root.insert(key.to_string(), default_value);
        true
    }
}

fn ensure_nested_defaults(value: &mut Value, key: &str, defaults: &Value) -> bool {
    let Some(root) = value.as_object_mut() else {
        return false;
    };
    let Some(target) = root.get_mut(key).and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(default_object) = defaults.as_object() else {
        return false;
    };
    let mut changed = false;
    for (nested_key, nested_value) in default_object {
        if !target.contains_key(nested_key) {
            target.insert(nested_key.clone(), nested_value.clone());
            changed = true;
        }
    }
    changed
}

fn ensure_streamer_override_defaults(
    value: &mut Value,
    bet_defaults: &Value,
    filter_condition_defaults: &Value,
) -> bool {
    let Some(root) = value.as_object_mut() else {
        return false;
    };
    let Some(overrides) = root
        .get_mut("streamer_overrides")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };

    let mut changed = false;
    for override_value in overrides.values_mut() {
        if !override_value.is_object() {
            *override_value = Value::Object(Map::new());
            changed = true;
        }

        changed |= ensure_streamer_override_fields(override_value, bet_defaults);
        let Some(override_object) = override_value.as_object_mut() else {
            continue;
        };
        let Some(bet_value) = override_object.get_mut("bet") else {
            continue;
        };
        changed |= ensure_object_key(
            bet_value,
            "filter_condition",
            filter_condition_defaults.clone(),
        );
        changed |= ensure_nested_defaults(bet_value, "filter_condition", filter_condition_defaults);
    }

    changed
}

fn ensure_streamer_override_fields(value: &mut Value, bet_defaults: &Value) -> bool {
    let Some(override_object) = value.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    for key in [
        "make_predictions",
        "follow_raid",
        "farm_drops",
        "claim_drops",
        "watch_one_stream_when_drops_active",
        "claim_moments",
        "watch_streak",
        "watch_streak_vod_recovery",
        "community_goals",
        "chat_presence",
    ] {
        if !override_object.contains_key(key) {
            override_object.insert(key.to_string(), Value::Null);
            changed = true;
        }
    }

    if !matches!(override_object.get("bet"), Some(Value::Object(_))) {
        override_object.insert("bet".to_string(), bet_defaults.clone());
        changed = true;
    } else if let Some(bet_value) = override_object.get_mut("bet") {
        changed |= fill_missing_top_level(bet_value, bet_defaults);
    }

    changed
}

fn privacy_defaults() -> Value {
    json!({ "anonymize_logs": false })
}

fn discord_defaults() -> Value {
    json!({ "webhook_api": "", "events": [] })
}

fn bet_defaults() -> Value {
    json!({
        "strategy": Value::Null,
        "percentage": Value::Null,
        "percentage_gap": Value::Null,
        "max_points": Value::Null,
        "stealth_mode": Value::Null,
        "deduct_stake_on_place": true,
        "delay_mode": Value::Null,
        "delay": Value::Null,
        "minimum_points": Value::Null,
        "filter_condition": filter_condition_defaults()
    })
}

fn filter_condition_defaults() -> Value {
    json!({
        "by": Value::Null,
        "where": Value::Null,
        "value": Value::Null
    })
}

fn absolutize(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn is_go_run_executable(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_lowercase();
    let temp_dir = env::temp_dir().to_string_lossy().to_lowercase();
    lower.contains("go-build") || lower.starts_with(&temp_dir)
}

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
mod tests;
