use std::time::Duration;

use serde::{Deserialize, Serialize};
use tm_twitch::CLIENT_ID;

pub const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
pub const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
pub const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
pub const ANDROID_TV_ORIGIN: &str = "https://android.tv.twitch.tv";
pub const ANDROID_TV_REFERER: &str = "https://android.tv.twitch.tv/";
pub const ANDROID_TV_USER_AGENT: &str =
    "Dalvik/2.1.0 (Linux; U; Android 7.1.2; Android TV Build/NHG47K)";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub form: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFlowState {
    pub device_code: String,
    pub user_code: String,
    pub interval: Duration,
    pub expires_in: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginValidationRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[must_use]
pub fn device_flow_scope() -> &'static str {
    "channel_read chat:read user_blocks_edit user_blocks_read user_follows_edit user_read"
}

#[must_use]
pub fn build_device_code_request(device_id: &str) -> OAuthRequest {
    OAuthRequest {
        url: DEVICE_URL.to_string(),
        headers: device_headers(device_id),
        form: vec![
            ("client_id".into(), CLIENT_ID.into()),
            ("scopes".into(), device_flow_scope().into()),
        ],
    }
}

#[must_use]
pub fn build_token_poll_request(device_id: &str, device_code: &str) -> OAuthRequest {
    OAuthRequest {
        url: TOKEN_URL.to_string(),
        headers: device_headers(device_id),
        form: vec![
            ("client_id".into(), CLIENT_ID.into()),
            ("device_code".into(), device_code.into()),
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
        ],
    }
}

#[must_use]
pub fn build_validate_login_request(
    auth_token: &str,
    device_id: &str,
    user_agent: &str,
) -> LoginValidationRequest {
    LoginValidationRequest {
        url: VALIDATE_URL.to_string(),
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("Authorization".into(), format!("OAuth {auth_token}")),
            ("X-Device-Id".into(), device_id.into()),
            ("User-Agent".into(), user_agent.into()),
        ],
    }
}

impl From<DeviceCodeResponse> for DeviceFlowState {
    fn from(value: DeviceCodeResponse) -> Self {
        Self {
            device_code: value.device_code,
            user_code: value.user_code,
            interval: Duration::from_secs(value.interval),
            expires_in: Duration::from_secs(value.expires_in),
        }
    }
}

fn device_headers(device_id: &str) -> Vec<(String, String)> {
    vec![
        ("Accept".into(), "application/json".into()),
        ("Accept-Encoding".into(), "gzip".into()),
        ("Accept-Language".into(), "en-US".into()),
        ("Cache-Control".into(), "no-cache".into()),
        ("Client-Id".into(), CLIENT_ID.into()),
        ("Host".into(), "id.twitch.tv".into()),
        ("Origin".into(), ANDROID_TV_ORIGIN.into()),
        ("Pragma".into(), "no-cache".into()),
        ("Referer".into(), ANDROID_TV_REFERER.into()),
        ("User-Agent".into(), ANDROID_TV_USER_AGENT.into()),
        ("X-Device-Id".into(), device_id.into()),
        (
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_flow_scope_matches_go() {
        assert_eq!(
            device_flow_scope(),
            "channel_read chat:read user_blocks_edit user_blocks_read user_follows_edit user_read"
        );
    }

    #[test]
    fn builds_device_code_request() {
        let request = build_device_code_request("device-1");
        assert_eq!(request.url, DEVICE_URL);
        assert!(request
            .headers
            .contains(&("Client-Id".into(), CLIENT_ID.into())));
        assert!(request
            .headers
            .contains(&("X-Device-Id".into(), "device-1".into())));
        assert!(request
            .form
            .contains(&("scopes".into(), device_flow_scope().into())));
    }

    #[test]
    fn builds_token_poll_request() {
        let request = build_token_poll_request("device-1", "code-1");
        assert_eq!(request.url, TOKEN_URL);
        assert!(request
            .form
            .contains(&("device_code".into(), "code-1".into())));
        assert!(request.form.contains(&(
            "grant_type".into(),
            "urn:ietf:params:oauth:grant-type:device_code".into()
        )));
    }

    #[test]
    fn builds_validate_login_request() {
        let request = build_validate_login_request("token", "device", "ua");
        assert_eq!(request.url, VALIDATE_URL);
        assert!(request
            .headers
            .contains(&("Authorization".into(), "OAuth token".into())));
        assert!(request
            .headers
            .contains(&("X-Device-Id".into(), "device".into())));
    }

    #[test]
    fn converts_device_code_response_to_state() {
        let response = DeviceCodeResponse {
            device_code: "device-code".into(),
            user_code: "USER".into(),
            interval: 5,
            expires_in: 1800,
        };
        let state = DeviceFlowState::from(response);
        assert_eq!(state.interval, Duration::from_secs(5));
        assert_eq!(state.expires_in, Duration::from_secs(1800));
    }
}
