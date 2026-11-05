//! Immediate session commands, `--auth-status` and `--decrypt-session`. Neither
//! starts a login; only `--decrypt-session` writes, and only the session file
//! and its backup.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tm_auth::{
    cookie_file_path, is_encrypted_session, AuthSession, AuthSessionError, SessionKey,
    TwitchAuthClient, KEY_FILE_ENV,
};
use tm_config::AppPaths;
use tm_net::NetworkPolicy;
use tm_twitch::generate_device_id;

use crate::bootstrap::{
    build_http_client, load_session_key, normalized_username,
    saved_session_requires_reauthorization, saved_session_retry_class, validate_app_config,
    DEFAULT_USER_AGENT,
};
use crate::observability::format_resume_gap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Validation {
    Valid,
    Rejected,
    Unreachable,
    NotChecked,
}

/// What `--auth-status` prints. It carries no token of any kind.
#[derive(Debug, Serialize)]
pub(crate) struct AuthStatusReport {
    session_file: bool,
    encrypted: bool,
    login: String,
    validation: Validation,
    detail: Option<String>,
    scopes: Vec<String>,
    expires_in_seconds: Option<u64>,
    file_age_seconds: Option<u64>,
    refresh_token: Option<bool>,
}

impl AuthStatusReport {
    fn new(login: &str) -> Self {
        Self {
            session_file: false,
            encrypted: false,
            login: login.to_string(),
            validation: Validation::NotChecked,
            detail: None,
            scopes: Vec::new(),
            expires_in_seconds: None,
            file_age_seconds: None,
            refresh_token: None,
        }
    }

    fn render(&self) -> String {
        let file = match (self.session_file, self.encrypted) {
            (false, _) => "missing",
            (true, true) => "present, encrypted",
            (true, false) => "present, plaintext",
        };
        let validation = match self.validation {
            Validation::Valid => "valid",
            Validation::Rejected => "rejected",
            Validation::Unreachable => "unreachable",
            Validation::NotChecked => "not checked",
        };
        let mut lines = vec![
            format!("session file: {file}"),
            format!("login: {}", self.login),
            format!("token: {validation}"),
        ];
        if let Some(detail) = &self.detail {
            lines.push(format!("detail: {detail}"));
        }
        if self.validation == Validation::Valid {
            lines.push(if self.scopes.is_empty() {
                String::from("scopes: none")
            } else {
                format!("scopes: {}", self.scopes.join(", "))
            });
        }
        if let Some(seconds) = self.expires_in_seconds {
            lines.push(format!("expires in: {}", duration_text(seconds)));
        }
        if let Some(seconds) = self.file_age_seconds {
            lines.push(format!("session file age: {}", duration_text(seconds)));
        }
        if let Some(present) = self.refresh_token {
            lines.push(format!(
                "refresh token: {}",
                if present { "present" } else { "absent" }
            ));
        }
        lines.join("\n")
    }

    /// Success while the token is valid or Twitch could not be reached; the
    /// session is missing, rejected, or unusable otherwise.
    fn outcome(&self) -> Result<()> {
        match (self.validation, self.session_file) {
            (Validation::Valid | Validation::Unreachable, _) => Ok(()),
            (Validation::Rejected, _) => Err(anyhow!("Twitch rejected the saved session")),
            (Validation::NotChecked, false) => Err(anyhow!("no saved session")),
            (Validation::NotChecked, true) => {
                Err(anyhow!("the saved session could not be checked"))
            }
        }
    }
}

fn duration_text(seconds: u64) -> String {
    format_resume_gap(Duration::from_secs(seconds))
}

/// The configured login and network policy, read without writing the configuration.
fn configured_login(paths: &AppPaths) -> Result<(String, NetworkPolicy)> {
    let preview = tm_config::preview_config(&paths.config_path)?;
    validate_app_config(&preview.config)?;
    Ok((
        normalized_username(&preview.config.username)?,
        preview.config.proxy.network_policy()?,
    ))
}

fn configured_username(paths: &AppPaths) -> Result<String> {
    configured_login(paths).map(|(username, _)| username)
}

pub(crate) async fn print_auth_status(paths: &AppPaths, json: bool) -> Result<()> {
    let (username, network) = configured_login(paths)?;
    let key = load_session_key(|message| eprintln!("warning: {message}"))?;
    let auth_client = TwitchAuthClient::with_client(build_http_client(&network)?);
    let report =
        collect_auth_status(&paths.work_dir, &username, key.as_ref(), &auth_client).await?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("{}", report.render());
    }
    report.outcome()
}

/// Validates the saved session once and describes it. Nothing is written and
/// no login is started.
pub(crate) async fn collect_auth_status(
    work_dir: &Path,
    username: &str,
    key: Option<&SessionKey>,
    auth_client: &TwitchAuthClient,
) -> Result<AuthStatusReport> {
    let path = cookie_file_path(work_dir, username)?;
    let mut report = AuthStatusReport::new(username);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error).context("read the session file's metadata"),
    };
    if !metadata.is_file() {
        return Ok(report);
    }
    report.session_file = true;
    report.file_age_seconds = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|age| age.as_secs());
    let bytes = fs::read(&path).context("read the session file")?;
    report.encrypted = is_encrypted_session(&bytes);
    let session = match AuthSession::from_bytes(username, &bytes, key) {
        Ok(session) => session,
        Err(error) => {
            // Parser errors can quote file content, so they are never printed.
            report.detail = Some(match error {
                AuthSessionError::CookieStore(_)
                | AuthSessionError::CookieEncode(_)
                | AuthSessionError::Io(_) => {
                    String::from("the session file is not a valid session")
                }
                other => other.to_string(),
            });
            return Ok(report);
        }
    };
    report.refresh_token = Some(session.refresh_token().is_some());
    let Some(auth_token) = session.auth_token() else {
        report.detail = Some(String::from("the saved session has no auth token"));
        return Ok(report);
    };
    match auth_client
        .validate_login_details(
            auth_token,
            &generate_device_id(),
            username,
            DEFAULT_USER_AGENT,
        )
        .await
    {
        Ok(validation) => {
            report.validation = Validation::Valid;
            report.scopes = validation.scopes;
            report.expires_in_seconds = validation.expires_in.map(|expires| expires.as_secs());
        }
        Err(error) if saved_session_requires_reauthorization(&error) => {
            report.validation = Validation::Rejected;
            report.detail = Some(error.to_string());
        }
        Err(error) => {
            report.validation = Validation::Unreachable;
            report.detail = Some(
                saved_session_retry_class(&error)
                    .unwrap_or("unexpected-response")
                    .to_string(),
            );
        }
    }
    Ok(report)
}

/// Rewrites the encrypted session file and its backup as plaintext, so a
/// release that cannot read encrypted files can run again.
pub(crate) fn decrypt_session(paths: &AppPaths) -> Result<()> {
    let username = configured_username(paths)?;
    let key = load_session_key(|message| eprintln!("warning: {message}"))?.ok_or_else(|| {
        anyhow!("--decrypt-session needs {KEY_FILE_ENV} to name the file holding the session key")
    })?;
    decrypt_session_files(&paths.work_dir, &username, &key)
}

fn decrypt_session_files(work_dir: &Path, username: &str, key: &SessionKey) -> Result<()> {
    AuthSession::decrypt_session_files(work_dir, username, key)
        .context("decrypt the saved session")?;
    println!(
        "session files for {username} are plaintext again; unset {KEY_FILE_ENV} before starting \
         this release again, or its next save encrypts them"
    );
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/auth_commands_tests.rs"]
mod tests;
