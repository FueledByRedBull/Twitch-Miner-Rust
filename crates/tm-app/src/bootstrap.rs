use std::env;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tm_auth::{AuthSession, AuthSessionError, TwitchAuthClient};
use tm_config::{
    default_user_config_dir, load_or_create_config, validate_config, AppPaths, ConfigError,
    ConfigFile,
};
use tm_twitch::generate_device_id;

use crate::Cli;

pub(crate) const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";
pub(crate) const READ_ONLY_FILE_SYSTEM_ERROR: i32 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TimezoneValidation {
    Valid(String),
    Invalid(String),
}

pub(crate) struct LoadedConfig {
    pub(crate) config: ConfigFile,
    pub(crate) active_paths: AppPaths,
}

pub(crate) fn has_override(cli: &Cli) -> bool {
    cli.config.is_some()
        || cli.data_dir.is_some()
        || env_has_value("TCPM_CONFIG")
        || env_has_value("TCPM_DATA_DIR")
}

pub(crate) fn env_has_value(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

pub(crate) fn load_config_with_fallback(
    paths: &AppPaths,
    has_override: bool,
) -> Result<LoadedConfig, ConfigError> {
    load_config_with_fallback_using(paths, has_override, default_user_config_dir, load_or_create_config)
}

pub(crate) fn load_config_with_fallback_using<D, F>(
    paths: &AppPaths,
    has_override: bool,
    fallback_dir: D,
    load: F,
) -> Result<LoadedConfig, ConfigError>
where
    D: Fn() -> Option<PathBuf>,
    F: Fn(&Path) -> Result<ConfigFile, ConfigError>,
{
    match load(&paths.config_path) {
        Ok(config) => {
            validate_config(&config)?;
            Ok(LoadedConfig {
                config,
                active_paths: paths.clone(),
            })
        }
        Err(ConfigError::Io(error)) if !has_override && should_fallback_to_user_config(&error) => {
            let fallback_dir = fallback_dir().ok_or(error)?;
            std::fs::create_dir_all(&fallback_dir)?;
            let fallback_path = fallback_dir.join("config.json");
            let config = load(&fallback_path)?;
            validate_config(&config)?;
            Ok(LoadedConfig {
                config,
                active_paths: AppPaths {
                    work_dir: fallback_dir,
                    config_path: fallback_path,
                },
            })
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn prepare_work_dir(paths: &AppPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.work_dir)?;
    env::set_current_dir(&paths.work_dir)?;
    Ok(())
}

pub(crate) fn build_http_client(disable_ssl_cert_verification: bool) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .danger_accept_invalid_certs(disable_ssl_cert_verification)
        .build()
        .context("build http client")
}

pub(crate) async fn load_or_login_session(
    config: &ConfigFile,
    base_dir: &Path,
    client: reqwest::Client,
) -> Result<AuthSession> {
    let auth_client = TwitchAuthClient::with_client(client);
    load_or_login_session_with_auth_client(config, base_dir, &auth_client).await
}

pub(crate) async fn load_or_login_session_with_auth_client(
    config: &ConfigFile,
    base_dir: &Path,
    auth_client: &TwitchAuthClient,
) -> Result<AuthSession> {
    let username = normalized_username(&config.username)?;
    let device_id = generate_device_id();

    match AuthSession::load_from_dir(base_dir, &username) {
        Ok(mut session) => {
            if let Some(auth_token) = session.auth_token().map(str::to_string) {
                match auth_client
                    .validate_login(&auth_token, &device_id, &username, DEFAULT_USER_AGENT)
                    .await
                {
                    Ok(user_id) => {
                        session.set_user_id(user_id);
                        session.save_to_dir(base_dir)?;
                        tracing::debug!(username = %username, "loaded cookies from disk");
                        return Ok(session);
                    }
                    Err(error) => {
                        tracing::warn!(
                            username = %username,
                            %error,
                            "saved cookies are invalid; starting device login"
                        );
                    }
                }
            } else {
                tracing::warn!(
                    username = %username,
                    "saved cookies missing auth-token; starting device login"
                );
            }
        }
        Err(AuthSessionError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                username = %username,
                %error,
                "unable to read saved cookies; starting device login"
            );
        }
    }

    let prompt = auth_client.request_device_code(&device_id).await?;
    tracing::info!(
        verification_uri = %prompt.verification_uri,
        user_code = %prompt.user_code,
        expires_in_seconds = prompt.expires_in.as_secs(),
        "complete Twitch device login"
    );
    let started = tokio::time::Instant::now();
    let auth_token = loop {
        if started.elapsed() >= prompt.expires_in {
            return Err(anyhow!("device code expired before authorization"));
        }
        match auth_client
            .poll_access_token(&device_id, &prompt.device_code)
            .await?
        {
            Some(token) => break token,
            None => tokio::time::sleep(prompt.interval).await,
        }
    };

    let user_id = auth_client
        .validate_login(&auth_token, &device_id, &username, DEFAULT_USER_AGENT)
        .await?;
    let mut session = AuthSession::new(&username, tm_auth::CookieStore::new());
    session.set_auth_token(auth_token);
    session.set_user_id(user_id);
    session.save_to_dir(base_dir)?;
    tracing::info!(username = %username, "device login completed");
    Ok(session)
}

pub(crate) fn normalized_username(username: &str) -> Result<String> {
    let username = username.trim().to_lowercase();
    if username.is_empty() || username == "your-twitch-username" {
        return Err(anyhow!("config.username must be set to a Twitch username"));
    }
    Ok(username)
}

pub(crate) fn validate_timezone_override(raw: Option<&str>) -> Option<TimezoneValidation> {
    let zone = raw
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))?;
    match zone.parse::<chrono_tz::Tz>() {
        Ok(_) => Some(TimezoneValidation::Valid(zone.to_string())),
        Err(_) => Some(TimezoneValidation::Invalid(zone.to_string())),
    }
}

pub(crate) fn log_timezone_validation(validation: Option<&TimezoneValidation>) {
    match validation {
        Some(TimezoneValidation::Valid(zone)) => {
            tracing::info!(timezone = %zone, "using configured timezone");
        }
        Some(TimezoneValidation::Invalid(zone)) => {
            tracing::warn!(
                timezone = %zone,
                "timezone override ignored; falling back to system time"
            );
        }
        None => {}
    }
}

pub(crate) fn should_fallback_to_user_config(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(READ_ONLY_FILE_SYSTEM_ERROR)
}
