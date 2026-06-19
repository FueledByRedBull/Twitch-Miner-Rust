use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use thiserror::Error;

use crate::device_flow::{
    build_device_code_request, build_token_poll_request, build_validate_login_request,
    DeviceFlowState, DEVICE_URL, TOKEN_URL, VALIDATE_URL,
};
use crate::session::AuthSession;
use crate::CookieStore;

pub const ACTIVATE_URL: &str = "https://www.twitch.tv/activate";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodePrompt {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
    pub expires_in: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginResult {
    pub session: AuthSession,
    pub prompt: DeviceCodePrompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEndpoints {
    pub device_code_url: String,
    pub token_url: String,
    pub validate_url: String,
}

#[derive(Debug, Error)]
pub enum AuthClientError {
    #[error("http client build failed: {0}")]
    BuildClient(#[from] reqwest::Error),
    #[error("http request failed: {0}")]
    Http(reqwest::Error),
    #[error("unexpected status {status} for {context}")]
    UnexpectedStatus {
        status: StatusCode,
        context: &'static str,
    },
    #[error("device flow expired before authorization")]
    DeviceFlowExpired,
    #[error("oauth token missing from response")]
    MissingAccessToken,
    #[error("login missing from validation response")]
    MissingLogin,
    #[error("user id missing from validation response")]
    MissingUserId,
    #[error(
        "validated token belongs to Twitch login '{actual_login}', expected '{expected_login}'"
    )]
    LoginMismatch {
        expected_login: String,
        actual_login: String,
    },
    #[error("session error: {0}")]
    Session(#[from] crate::AuthSessionError),
}

#[derive(Debug)]
pub struct TwitchAuthClient {
    client: reqwest::Client,
    endpoints: AuthEndpoints,
}

#[derive(Debug, Deserialize)]
struct TokenPollResponse {
    access_token: Option<String>,
    error: Option<String>,
    message: Option<String>,
}

impl TwitchAuthClient {
    pub fn new(timeout: Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder().timeout(timeout).build()?,
            endpoints: AuthEndpoints::default(),
        })
    }

    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        Self {
            client,
            endpoints: AuthEndpoints::default(),
        }
    }

    #[must_use]
    pub fn with_client_and_endpoints(client: reqwest::Client, endpoints: AuthEndpoints) -> Self {
        Self { client, endpoints }
    }

    pub async fn request_device_code(
        &self,
        device_id: &str,
    ) -> Result<DeviceCodePrompt, AuthClientError> {
        let mut request = build_device_code_request(device_id);
        request.url.clone_from(&self.endpoints.device_code_url);
        let response = self
            .client
            .post(request.url)
            .headers(headers_from_pairs(&request.headers)?)
            .form(&request.form)
            .send()
            .await
            .map_err(AuthClientError::Http)?;
        if !response.status().is_success() {
            return Err(AuthClientError::UnexpectedStatus {
                status: response.status(),
                context: "request device code",
            });
        }
        let state = DeviceFlowState::from(
            response
                .json::<crate::device_flow::DeviceCodeResponse>()
                .await
                .map_err(AuthClientError::Http)?,
        );
        Ok(DeviceCodePrompt {
            device_code: state.device_code,
            user_code: state.user_code,
            verification_uri: ACTIVATE_URL.to_string(),
            interval: state.interval,
            expires_in: state.expires_in,
        })
    }

    pub async fn poll_access_token(
        &self,
        device_id: &str,
        device_code: &str,
    ) -> Result<Option<String>, AuthClientError> {
        let mut request = build_token_poll_request(device_id, device_code);
        request.url.clone_from(&self.endpoints.token_url);
        let response = self
            .client
            .post(request.url)
            .headers(headers_from_pairs(&request.headers)?)
            .form(&request.form)
            .send()
            .await
            .map_err(AuthClientError::Http)?;
        let status = response.status();

        if status.is_success() {
            let token = response
                .json::<TokenPollResponse>()
                .await
                .map_err(AuthClientError::Http)?
                .access_token;
            return token.ok_or(AuthClientError::MissingAccessToken).map(Some);
        }

        if status == StatusCode::BAD_REQUEST {
            let body = response
                .json::<TokenPollResponse>()
                .await
                .map_err(AuthClientError::Http)?;
            let pending = matches!(
                body.error.as_deref().or(body.message.as_deref()),
                Some("authorization_pending" | "slow_down")
            );
            if pending {
                return Ok(None);
            }
        }

        Err(AuthClientError::UnexpectedStatus {
            status,
            context: "poll access token",
        })
    }

    pub async fn validate_login(
        &self,
        auth_token: &str,
        device_id: &str,
        username: &str,
        user_agent: &str,
    ) -> Result<String, AuthClientError> {
        let mut request = build_validate_login_request(auth_token, device_id, user_agent);
        request.url.clone_from(&self.endpoints.validate_url);
        let response = self
            .client
            .get(request.url)
            .headers(headers_from_pairs(&request.headers)?)
            .send()
            .await
            .map_err(AuthClientError::Http)?;
        if !response.status().is_success() {
            return Err(AuthClientError::UnexpectedStatus {
                status: response.status(),
                context: "validate login",
            });
        }
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(AuthClientError::Http)?;
        let login = payload
            .get("login")
            .and_then(serde_json::Value::as_str)
            .ok_or(AuthClientError::MissingLogin)?
            .trim()
            .to_lowercase();
        let expected_login = username.trim().to_lowercase();
        if login != expected_login {
            return Err(AuthClientError::LoginMismatch {
                expected_login,
                actual_login: login,
            });
        }
        payload
            .get("user_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or(AuthClientError::MissingUserId)
    }

    pub async fn login_with_device_flow(
        &self,
        username: &str,
        device_id: &str,
        user_agent: &str,
        base_dir: impl AsRef<Path>,
    ) -> Result<LoginResult, AuthClientError> {
        let prompt = self.request_device_code(device_id).await?;
        let started = std::time::Instant::now();
        let token = loop {
            if started.elapsed() >= prompt.expires_in {
                return Err(AuthClientError::DeviceFlowExpired);
            }
            match self
                .poll_access_token(device_id, &prompt.device_code)
                .await?
            {
                Some(token) => break token,
                None => sleep_without_blocking_runtime(prompt.interval).await,
            }
        };

        let user_id = self
            .validate_login(&token, device_id, username, user_agent)
            .await?;
        let mut session = AuthSession::new(username, CookieStore::new());
        session.set_auth_token(token);
        session.set_user_id(user_id);
        session.save_to_dir(base_dir)?;

        Ok(LoginResult { session, prompt })
    }
}

impl Default for AuthEndpoints {
    fn default() -> Self {
        Self {
            device_code_url: DEVICE_URL.to_string(),
            token_url: TOKEN_URL.to_string(),
            validate_url: VALIDATE_URL.to_string(),
        }
    }
}

fn sleep_without_blocking_runtime(duration: Duration) -> ThreadSleep {
    ThreadSleep {
        shared: Arc::new(ThreadSleepShared {
            done: AtomicBool::new(duration.is_zero()),
            started: AtomicBool::new(false),
            waker: Mutex::new(None),
            duration,
        }),
    }
}

fn headers_from_pairs(
    pairs: &[(String, String)],
) -> Result<reqwest::header::HeaderMap, AuthClientError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                AuthClientError::UnexpectedStatus {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    context: "build request headers",
                }
            })?,
            reqwest::header::HeaderValue::from_str(value).map_err(|_| {
                AuthClientError::UnexpectedStatus {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    context: "build request headers",
                }
            })?,
        );
    }
    Ok(headers)
}

struct ThreadSleep {
    shared: Arc<ThreadSleepShared>,
}

struct ThreadSleepShared {
    done: AtomicBool,
    started: AtomicBool,
    waker: Mutex<Option<Waker>>,
    duration: Duration,
}

impl std::future::Future for ThreadSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.shared.done.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }

        {
            let mut waker = self.shared.waker.lock().expect("sleep waker lock poisoned");
            *waker = Some(cx.waker().clone());
        }

        if !self.shared.started.swap(true, Ordering::SeqCst) {
            let shared = Arc::clone(&self.shared);
            std::thread::spawn(move || {
                std::thread::sleep(shared.duration);
                shared.done.store(true, Ordering::SeqCst);
                if let Some(waker) = shared
                    .waker
                    .lock()
                    .expect("sleep waker lock poisoned")
                    .take()
                {
                    waker.wake();
                }
            });
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_client_constructs() {
        let client = TwitchAuthClient::new(Duration::from_secs(30)).unwrap();
        let _ = client;
    }

    #[test]
    fn prompt_uses_activate_url() {
        let prompt = DeviceCodePrompt {
            device_code: "device-code".into(),
            user_code: "ABCD".into(),
            verification_uri: ACTIVATE_URL.into(),
            interval: Duration::from_secs(5),
            expires_in: Duration::from_secs(900),
        };
        assert_eq!(prompt.verification_uri, "https://www.twitch.tv/activate");
    }
}
