use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tm_auth::{AuthClientError, AuthSession, AuthSessionError, LoginValidation, TwitchAuthClient};
use tm_config::{
    default_user_config_dir, load_or_create_config, preview_config, validate_config, AppPaths,
    ConfigError, ConfigFile,
};
use tm_twitch::generate_device_id;

use crate::Cli;

pub(crate) const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";
pub(crate) const READ_ONLY_FILE_SYSTEM_ERROR: i32 = 30;
const SAVED_SESSION_RETRY_BASE: Duration = Duration::from_secs(5);
const SAVED_SESSION_RETRY_MAX: Duration = Duration::from_secs(5 * 60);

enum SavedSessionValidation {
    Valid(LoginValidation),
    Reauthorize,
}

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
    load_config_with_fallback_using(
        paths,
        has_override,
        default_user_config_dir,
        load_or_create_config,
    )
}

pub(crate) fn preview_config_with_fallback(
    paths: &AppPaths,
    has_override: bool,
) -> Result<LoadedConfig, ConfigError> {
    load_config_with_fallback_using(paths, has_override, default_user_config_dir, |path| {
        preview_config(path).map(|preview| preview.config)
    })
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
    if disable_ssl_cert_verification {
        return Err(anyhow!(
            "config.disable_ssl_cert_verification is no longer supported because it disables TLS certificate verification"
        ));
    }
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
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

pub(crate) async fn load_and_validate_existing_session(
    config: &ConfigFile,
    base_dir: &Path,
    client: reqwest::Client,
) -> Result<AuthSession> {
    let username = normalized_username(&config.username)?;
    let mut session =
        AuthSession::load_from_dir(base_dir, &username).context("load existing canary session")?;
    let auth_token = session
        .auth_token()
        .ok_or_else(|| anyhow!("existing canary session has no auth token"))?
        .to_string();
    let auth_client = TwitchAuthClient::with_client(client);
    let validation = auth_client
        .validate_login_details(
            &auth_token,
            &generate_device_id(),
            &username,
            DEFAULT_USER_AGENT,
        )
        .await
        .context("validate existing canary session")?;
    session.set_user_id(validation.user_id);
    session.set_scopes(validation.scopes);
    Ok(session)
}

pub(crate) async fn load_or_login_session_with_auth_client(
    config: &ConfigFile,
    base_dir: &Path,
    auth_client: &TwitchAuthClient,
) -> Result<AuthSession> {
    load_or_login_session_with_auth_client_and_retry(
        config,
        base_dir,
        auth_client,
        SAVED_SESSION_RETRY_BASE,
        SAVED_SESSION_RETRY_MAX,
    )
    .await
}

pub(crate) async fn load_or_login_session_with_auth_client_and_retry(
    config: &ConfigFile,
    base_dir: &Path,
    auth_client: &TwitchAuthClient,
    retry_base: Duration,
    retry_max: Duration,
) -> Result<AuthSession> {
    let username = normalized_username(&config.username)?;
    let device_id = generate_device_id();

    match AuthSession::load_from_dir(base_dir, &username) {
        Ok(mut session) => {
            if let Some(auth_token) = session.auth_token().map(str::to_string) {
                match validate_saved_session_with_retry(
                    auth_client,
                    &auth_token,
                    &device_id,
                    &username,
                    retry_base,
                    retry_max,
                )
                .await?
                {
                    SavedSessionValidation::Valid(validation) => {
                        session.set_user_id(validation.user_id);
                        session.set_scopes(validation.scopes);
                        session.save_to_dir(base_dir)?;
                        tracing::debug!(username = %username, "loaded cookies from disk");
                        return Ok(session);
                    }
                    SavedSessionValidation::Reauthorize => {
                        tracing::warn!(
                            username = %username,
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

    let scopes = tm_auth::device_flow_scope_for_eventsub(config.betting_make_predictions);
    let prompt = auth_client
        .request_device_code_with_scope(&device_id, &scopes)
        .await?;
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

    let validation = auth_client
        .validate_login_details(&auth_token, &device_id, &username, DEFAULT_USER_AGENT)
        .await?;
    let mut session = AuthSession::new(&username, tm_auth::CookieStore::new());
    session.set_auth_token(auth_token);
    session.set_user_id(validation.user_id);
    session.set_scopes(validation.scopes);
    session.save_to_dir(base_dir)?;
    tracing::info!(username = %username, "device login completed");
    Ok(session)
}

async fn validate_saved_session_with_retry(
    auth_client: &TwitchAuthClient,
    auth_token: &str,
    device_id: &str,
    username: &str,
    retry_base: Duration,
    retry_max: Duration,
) -> Result<SavedSessionValidation> {
    let mut failure_attempt = 0_u32;
    loop {
        match auth_client
            .validate_login_details(auth_token, device_id, username, DEFAULT_USER_AGENT)
            .await
        {
            Ok(validation) => return Ok(SavedSessionValidation::Valid(validation)),
            Err(error) if saved_session_requires_reauthorization(&error) => {
                return Ok(SavedSessionValidation::Reauthorize);
            }
            Err(error) => {
                let Some(error_class) = saved_session_retry_class(&error) else {
                    return Err(error).context("validate saved Twitch session");
                };
                failure_attempt = failure_attempt.saturating_add(1);
                let delay = saved_session_retry_delay(failure_attempt, retry_base, retry_max);
                tracing::warn!(
                    operation = "auth",
                    error_class,
                    failure_attempt,
                    retry_delay_seconds = delay.as_secs(),
                    "saved session validation failed transiently; retrying"
                );
                if delay.is_zero() {
                    continue;
                }
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    result = crate::shutdown::wait_for_shutdown_signal() => {
                        result?;
                        return Err(anyhow!("shutdown requested while retrying saved session validation"));
                    }
                }
            }
        }
    }
}

fn saved_session_retry_class(error: &AuthClientError) -> Option<&'static str> {
    match error {
        AuthClientError::Http(error) if error.is_timeout() => Some("timeout"),
        AuthClientError::Http(error)
            if error.is_connect()
                || (error.is_request() && !error.is_decode() && !error.is_body()) =>
        {
            Some("connection-reset")
        }
        AuthClientError::UnexpectedStatus { status, .. }
            if *status == reqwest::StatusCode::TOO_MANY_REQUESTS =>
        {
            Some("rate-limited")
        }
        AuthClientError::UnexpectedStatus { status, .. } if status.is_server_error() => {
            Some("server-error")
        }
        _ => None,
    }
}

fn saved_session_retry_delay(
    failure_attempt: u32,
    retry_base: Duration,
    retry_max: Duration,
) -> Duration {
    let exponent = failure_attempt.saturating_sub(1).min(6);
    retry_base
        .checked_mul(1_u32 << exponent)
        .unwrap_or(retry_max)
        .min(retry_max)
}

fn saved_session_requires_reauthorization(error: &AuthClientError) -> bool {
    matches!(
        error,
        AuthClientError::UnexpectedStatus { status, .. }
            if matches!(
                *status,
                reqwest::StatusCode::BAD_REQUEST
                    | reqwest::StatusCode::UNAUTHORIZED
                    | reqwest::StatusCode::FORBIDDEN
            )
    ) || matches!(error, AuthClientError::LoginMismatch { .. })
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        build_http_client, saved_session_requires_reauthorization, saved_session_retry_class,
        saved_session_retry_delay,
    };
    use tm_auth::AuthClientError;

    #[test]
    fn build_http_client_rejects_insecure_tls_toggle() {
        match build_http_client(true) {
            Ok(_) => panic!("expected insecure TLS toggle to be rejected"),
            Err(error) => assert!(error.to_string().contains("disable_ssl_cert_verification")),
        }
    }

    #[test]
    fn build_http_client_accepts_secure_default() {
        assert!(build_http_client(false).is_ok());
    }

    #[test]
    fn saved_session_reauthorization_requires_definitive_auth_rejection() {
        assert!(saved_session_requires_reauthorization(
            &AuthClientError::UnexpectedStatus {
                status: reqwest::StatusCode::UNAUTHORIZED,
                context: "validate login",
            }
        ));
        assert!(saved_session_requires_reauthorization(
            &AuthClientError::LoginMismatch {
                expected_login: String::from("expected"),
                actual_login: String::from("other"),
            }
        ));
        assert!(!saved_session_requires_reauthorization(
            &AuthClientError::UnexpectedStatus {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                context: "validate login",
            }
        ));
        assert!(!saved_session_requires_reauthorization(
            &AuthClientError::MissingUserId
        ));
    }

    #[test]
    fn saved_session_retry_policy_is_transient_and_bounded() {
        assert_eq!(
            saved_session_retry_class(&AuthClientError::UnexpectedStatus {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                context: "validate login",
            }),
            Some("server-error")
        );
        assert_eq!(
            saved_session_retry_class(&AuthClientError::UnexpectedStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                context: "validate login",
            }),
            Some("rate-limited")
        );
        assert_eq!(
            saved_session_retry_class(&AuthClientError::MissingUserId),
            None
        );
        assert_eq!(
            saved_session_retry_delay(1, Duration::from_secs(5), Duration::from_secs(300)),
            Duration::from_secs(5)
        );
        assert_eq!(
            saved_session_retry_delay(20, Duration::from_secs(5), Duration::from_secs(300)),
            Duration::from_secs(300)
        );
    }
}
