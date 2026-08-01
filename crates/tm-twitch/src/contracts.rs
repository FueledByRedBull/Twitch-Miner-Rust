use std::sync::LazyLock;

use regex::Regex;

use crate::types::TwitchContractError;

// These fixed literals are compiled once and covered by extraction tests. A compile failure is a
// source defect, not an error reachable from Twitch or user input.
#[allow(clippy::expect_used)]
static BUILD_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"window\.__twilightBuildID\s*=\s*"([0-9a-fA-F\-]{36})""#)
        .expect("build id regex must compile")
});
#[allow(clippy::expect_used)]
static SETTINGS_SCRIPT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(https://static\.twitchcdn\.net/config/settings.*?\.js|https://assets\.twitch\.tv/config/settings.*?\.js)",
    )
    .expect("settings script regex must compile")
});
#[allow(clippy::expect_used)]
static SPADE_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""spade_url"\s*:\s*("(?:\\.|[^"\\])*")"#).expect("spade url regex must compile")
});

pub fn extract_build_id(html: &str) -> Result<String, TwitchContractError> {
    BUILD_ID_REGEX
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
        .ok_or(TwitchContractError::BuildIdNotFound)
}

pub fn extract_settings_script_url(html: &str) -> Result<String, TwitchContractError> {
    SETTINGS_SCRIPT_REGEX
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
        .ok_or(TwitchContractError::SettingsScriptNotFound)
}

pub fn extract_spade_url(settings_js: &str) -> Result<String, TwitchContractError> {
    let encoded = SPADE_URL_REGEX
        .captures(settings_js)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str())
        .ok_or(TwitchContractError::SpadeUrlNotFound)?;
    serde_json::from_str(encoded).map_err(|_| TwitchContractError::InvalidSpadeUrlString)
}
