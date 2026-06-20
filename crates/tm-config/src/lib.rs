#![recursion_limit = "256"]

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use thiserror::Error;
use tm_domain::{
    BetSettings, Condition, DelayMode, FilterCondition, IrcMode, OutcomeKey, Strategy,
    StreamerSettings,
};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid config: {0}")]
    InvalidConfig(#[from] serde_json::Error),
    #[error("config io error: {0}")]
    Io(#[from] io::Error),
    #[error("config validation failed: {0}")]
    Validation(String),
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
    pub claim_drops: Option<bool>,
    pub claim_moments: Option<bool>,
    pub watch_streak: Option<bool>,
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
#[allow(clippy::struct_excessive_bools)]
pub struct ConfigFile {
    pub username: String,
    #[serde(default)]
    pub password: String,
    pub auto_update: bool,
    pub debug: bool,
    pub debug_deep: bool,
    pub watch_queue_logging: bool,
    pub smart_logging: bool,
    pub disable_ssl_cert_verification: bool,
    pub show_seconds: bool,
    pub claim_drops_startup: bool,
    pub claim_drops: bool,
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
    pub bet: BetConfig,
    pub timezone: Option<String>,
    pub privacy: PrivacyConfig,
    pub discord: DiscordConfig,
    pub streamer_overrides: HashMap<String, StreamerSettingsOverride>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        serde_json::from_value(default_config_value()).expect("default config must deserialize")
    }
}

#[must_use]
pub fn default_config_value() -> Value {
    json!({
        "username": "your-twitch-username",
        "auto_update": false,
        "debug": false,
        "debug_deep": false,
        "watch_queue_logging": false,
        "smart_logging": true,
        "disable_ssl_cert_verification": false,
        "show_seconds": false,
        "claim_drops_startup": true,
        "claim_drops": true,
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
    let mut changed = false;
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
        .expect("bet object must exist");
    changed |= ensure_object_key(
        bet_value,
        "filter_condition",
        filter_condition_defaults.clone(),
    );
    changed |= ensure_nested_defaults(bet_value, "filter_condition", &filter_condition_defaults);
    changed |=
        ensure_streamer_override_defaults(&mut value, &bet_defaults, &filter_condition_defaults);

    if changed {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    }

    Ok(serde_json::from_value(value)?)
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
    Ok(())
}

pub fn resolve_app_paths(input: &ResolveAppPathsInput) -> io::Result<AppPaths> {
    if let Some(data_dir_flag) = input.data_dir_flag.as_ref() {
        let work_dir = absolutize(data_dir_flag, &input.cwd);
        return Ok(AppPaths {
            config_path: work_dir.join("config.json"),
            work_dir,
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

#[must_use]
pub fn parse_chat_presence(mode: &str, fallback: IrcMode) -> IrcMode {
    match mode.trim().to_uppercase().as_str() {
        "ALWAYS" => IrcMode::Always,
        "NEVER" => IrcMode::Never,
        "ONLINE" => IrcMode::Online,
        "OFFLINE" => IrcMode::Offline,
        _ => fallback,
    }
}

#[must_use]
pub fn build_base_streamer_settings(config: &ConfigFile) -> StreamerSettings {
    StreamerSettings {
        make_predictions: config.betting_make_predictions,
        follow_raid: config.follow_raid,
        claim_drops: config.claim_drops,
        claim_moments: true,
        watch_streak: true,
        community_goals: config.community_goals,
        bet: merge_bet_settings(&BetSettings::default(), &config.bet),
        irc_mode: parse_chat_presence(&config.chat_presence, IrcMode::Online),
    }
}

#[must_use]
pub fn build_override_settings<S: std::hash::BuildHasher>(
    base: &StreamerSettings,
    overrides: &HashMap<String, StreamerSettingsOverride, S>,
) -> HashMap<String, StreamerSettings> {
    overrides
        .iter()
        .filter_map(|(login, override_settings)| {
            let key = login.trim().to_lowercase();
            if key.is_empty() {
                return None;
            }
            Some((key, merge_streamer_settings(base, override_settings)))
        })
        .collect()
}

fn merge_streamer_settings(
    base: &StreamerSettings,
    override_settings: &StreamerSettingsOverride,
) -> StreamerSettings {
    let mut settings = base.clone();
    if let Some(value) = override_settings.make_predictions {
        settings.make_predictions = value;
    }
    if let Some(value) = override_settings.follow_raid {
        settings.follow_raid = value;
    }
    if let Some(value) = override_settings.claim_drops {
        settings.claim_drops = value;
    }
    if let Some(value) = override_settings.claim_moments {
        settings.claim_moments = value;
    }
    if let Some(value) = override_settings.watch_streak {
        settings.watch_streak = value;
    }
    if let Some(value) = override_settings.community_goals {
        settings.community_goals = value;
    }
    settings.bet = merge_bet_settings(&settings.bet, &override_settings.bet);
    if let Some(chat_presence) = override_settings.chat_presence.as_deref() {
        settings.irc_mode = parse_chat_presence(chat_presence, settings.irc_mode);
    }
    settings
}

fn merge_bet_settings(base: &BetSettings, override_settings: &BetConfig) -> BetSettings {
    let mut bet = base.clone();
    if let Some(strategy) = override_settings.strategy.as_deref() {
        bet.strategy = parse_strategy(strategy).unwrap_or(bet.strategy);
    }
    if let Some(value) = override_settings.percentage {
        bet.percentage = Some(value);
    }
    if let Some(value) = override_settings.percentage_gap {
        bet.percentage_gap = Some(value);
    }
    if let Some(value) = override_settings.max_points {
        bet.max_points = Some(value);
    }
    if let Some(value) = override_settings.minimum_points {
        bet.minimum_points = Some(value);
    }
    if let Some(value) = override_settings.stealth_mode {
        bet.stealth_mode = Some(value);
    }
    if let Some(value) = override_settings.deduct_stake_on_place {
        bet.deduct_stake_on_place = Some(value);
    }
    if let Some(value) = override_settings.delay {
        bet.delay = Some(value);
    }
    if let Some(delay_mode) = override_settings.delay_mode.as_deref() {
        bet.delay_mode = parse_delay_mode(delay_mode).unwrap_or(bet.delay_mode);
    }
    if let Some(filter_condition) = override_settings.filter_condition.as_ref() {
        let mut current = bet.filter_condition.clone().unwrap_or(FilterCondition {
            by: OutcomeKey::TotalUsers,
            condition: Condition::Gte,
            value: None,
        });
        if let Some(by) = filter_condition.by.as_deref() {
            current.by = parse_outcome_key(by).unwrap_or(current.by);
        }
        if let Some(condition) = filter_condition.condition.as_deref() {
            current.condition = parse_condition(condition).unwrap_or(current.condition);
        }
        if filter_condition.value.is_some() {
            current.value = filter_condition.value;
        }
        if current.value.is_some() {
            bet.filter_condition = Some(current);
        }
    }
    bet
}

fn parse_strategy(raw: &str) -> Option<Strategy> {
    match raw.trim().to_uppercase().as_str() {
        "MOST_VOTED" => Some(Strategy::MostVoted),
        "HIGH_ODDS" => Some(Strategy::HighOdds),
        "PERCENTAGE" => Some(Strategy::Percentage),
        "SMART_MONEY" => Some(Strategy::SmartMoney),
        "SMART" => Some(Strategy::Smart),
        "NUMBER_1" => Some(Strategy::Number1),
        "NUMBER_2" => Some(Strategy::Number2),
        "NUMBER_3" => Some(Strategy::Number3),
        "NUMBER_4" => Some(Strategy::Number4),
        "NUMBER_5" => Some(Strategy::Number5),
        "NUMBER_6" => Some(Strategy::Number6),
        "NUMBER_7" => Some(Strategy::Number7),
        "NUMBER_8" => Some(Strategy::Number8),
        _ => None,
    }
}

fn parse_delay_mode(raw: &str) -> Option<DelayMode> {
    match raw.trim().to_uppercase().as_str() {
        "FROM_START" => Some(DelayMode::FromStart),
        "FROM_END" => Some(DelayMode::FromEnd),
        "PERCENTAGE" => Some(DelayMode::Percentage),
        _ => None,
    }
}

fn parse_outcome_key(raw: &str) -> Option<OutcomeKey> {
    match raw.trim().to_uppercase().as_str() {
        "PERCENTAGE_USERS" => Some(OutcomeKey::PercentageUsers),
        "ODDS" => Some(OutcomeKey::Odds),
        "ODDS_PERCENTAGE" => Some(OutcomeKey::OddsPercentage),
        "TOP_POINTS" => Some(OutcomeKey::TopPoints),
        "TOTAL_USERS" => Some(OutcomeKey::TotalUsers),
        "TOTAL_POINTS" => Some(OutcomeKey::TotalPoints),
        "DECISION_USERS" => Some(OutcomeKey::DecisionUsers),
        "DECISION_POINTS" => Some(OutcomeKey::DecisionPoints),
        _ => None,
    }
}

fn parse_condition(raw: &str) -> Option<Condition> {
    match raw.trim().to_uppercase().as_str() {
        "GT" => Some(Condition::Gt),
        "LT" => Some(Condition::Lt),
        "GTE" => Some(Condition::Gte),
        "LTE" => Some(Condition::Lte),
        _ => None,
    }
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
        "claim_drops",
        "claim_moments",
        "watch_streak",
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
mod tests {
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
        assert!(!config.auto_update);
        assert!(path.exists());
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(value.get("password").is_none());
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
                claim_drops: Some(false),
                ..StreamerSettingsOverride::default()
            },
        )]);
        let merged = build_override_settings(&base, &overrides);
        let override_settings = merged.get("somestreamer").unwrap();
        assert!(!override_settings.claim_drops);
        assert_eq!(override_settings.irc_mode, base.irc_mode);
    }
}
