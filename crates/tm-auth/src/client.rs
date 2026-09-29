use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use thiserror::Error;

use crate::device_flow::{
    build_device_code_request_with_scope, build_token_poll_request, build_validate_login_request,
    DeviceFlowState, DEVICE_URL, TOKEN_URL, VALIDATE_URL,
};

pub const ACTIVATE_URL: &str = "https://www.twitch.tv/activate";

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCodePrompt {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
    pub expires_in: Duration,
}

impl fmt::Debug for DeviceCodePrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceCodePrompt")
            .field("device_code", &"<redacted>")
            .field("user_code", &"<redacted>")
            .field("verification_uri", &self.verification_uri)
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginValidation {
    pub user_id: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum TokenPollOutcome {
    AccessToken(String),
    Pending,
    SlowDown,
}

impl TokenPollOutcome {
    #[must_use]
    pub fn next_interval(&self, current: Duration) -> Duration {
        if matches!(self, Self::SlowDown) {
            current.saturating_add(Duration::from_secs(5))
        } else {
            current
        }
    }
}

impl fmt::Debug for TokenPollOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccessToken(_) => formatter.write_str("AccessToken(<redacted>)"),
            Self::Pending => formatter.write_str("Pending"),
            Self::SlowDown => formatter.write_str("SlowDown"),
        }
    }
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
}

#[derive(Debug)]
pub struct TwitchAuthClient {
    client: reqwest::Client,
    endpoints: AuthEndpoints,
}

#[derive(Deserialize)]
struct TokenPollResponse {
    access_token: Option<String>,
    error: Option<String>,
    message: Option<String>,
}

impl fmt::Debug for TokenPollResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenPollResponse")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("error", &self.error)
            .field("message", &self.message)
            .finish()
    }
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

    pub async fn request_device_code_with_scope(
        &self,
        device_id: &str,
        scopes: &str,
    ) -> Result<DeviceCodePrompt, AuthClientError> {
        let mut request = build_device_code_request_with_scope(device_id, scopes);
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
    ) -> Result<TokenPollOutcome, AuthClientError> {
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
            return token
                .map(TokenPollOutcome::AccessToken)
                .ok_or(AuthClientError::MissingAccessToken);
        }

        if status == StatusCode::BAD_REQUEST {
            let body = response
                .json::<TokenPollResponse>()
                .await
                .map_err(AuthClientError::Http)?;
            match body.error.as_deref().or(body.message.as_deref()) {
                Some("authorization_pending") => return Ok(TokenPollOutcome::Pending),
                Some("slow_down") => return Ok(TokenPollOutcome::SlowDown),
                _ => {}
            }
        }

        Err(AuthClientError::UnexpectedStatus {
            status,
            context: "poll access token",
        })
    }

    pub async fn validate_login_details(
        &self,
        auth_token: &str,
        device_id: &str,
        username: &str,
        user_agent: &str,
    ) -> Result<LoginValidation, AuthClientError> {
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
        login_validation_from_payload(&payload, username)
    }
}

fn login_validation_from_payload(
    payload: &serde_json::Value,
    username: &str,
) -> Result<LoginValidation, AuthClientError> {
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
    let user_id = payload
        .get("user_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or(AuthClientError::MissingUserId)?;
    let scopes = payload
        .get("scopes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect();
    Ok(LoginValidation { user_id, scopes })
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

    #[test]
    fn debug_redacts_device_code_prompt() {
        let prompt = DeviceCodePrompt {
            device_code: "secret-device-code".into(),
            user_code: "secret-user-code".into(),
            verification_uri: ACTIVATE_URL.into(),
            interval: Duration::from_secs(5),
            expires_in: Duration::from_secs(900),
        };
        let output = format!("{prompt:?}");
        assert!(!output.contains("secret-device-code"));
        assert!(!output.contains("secret-user-code"));
        assert!(output.contains("<redacted>"));
    }

    #[test]
    fn repeated_slow_down_responses_increase_all_subsequent_poll_intervals() {
        let mut interval = Duration::from_secs(2);
        interval = TokenPollOutcome::SlowDown.next_interval(interval);
        assert_eq!(interval, Duration::from_secs(7));
        interval = TokenPollOutcome::Pending.next_interval(interval);
        assert_eq!(interval, Duration::from_secs(7));
        interval = TokenPollOutcome::SlowDown.next_interval(interval);
        assert_eq!(interval, Duration::from_secs(12));
        assert!(
            !format!("{:?}", TokenPollOutcome::AccessToken("secret".into())).contains("secret")
        );
    }

    #[test]
    fn validation_retains_scopes_without_requiring_them() {
        let validation = login_validation_from_payload(
            &serde_json::json!({
                "login": "Tester",
                "user_id": "user-123",
                "scopes": ["chat:read", "channel:read:predictions"]
            }),
            "tester",
        )
        .unwrap();
        assert_eq!(validation.user_id, "user-123");
        assert_eq!(validation.scopes, ["chat:read", "channel:read:predictions"]);

        let validation = login_validation_from_payload(
            &serde_json::json!({ "login": "tester", "user_id": "user-123" }),
            "tester",
        )
        .unwrap();
        assert!(validation.scopes.is_empty());
    }
}
