use serde::Deserialize;

use crate::types::TwitchContractError;

pub fn extract_build_id(html: &str) -> Result<String, TwitchContractError> {
    let marker = "window.__twilightBuildID";
    html.match_indices(marker)
        .find_map(|(start, _)| {
            html[start + marker.len()..]
                .trim_start()
                .strip_prefix('=')
                .and_then(|rest| rest.trim_start().strip_prefix('"'))
                .and_then(|rest| rest.split_once('"').map(|(value, _)| value))
                .filter(|value| {
                    value.len() == 36
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
                })
                .map(str::to_owned)
        })
        .ok_or(TwitchContractError::BuildIdNotFound)
}

pub fn extract_settings_script_url(html: &str) -> Result<String, TwitchContractError> {
    const PREFIXES: [&str; 2] = [
        "https://static.twitchcdn.net/config/settings",
        "https://assets.twitch.tv/config/settings",
    ];
    PREFIXES
        .iter()
        .filter_map(|prefix| {
            html.find(prefix).and_then(|start| {
                html[start..]
                    .find(".js")
                    .map(|end| (start, html[start..start + end + 3].to_string()))
            })
        })
        .min_by_key(|(start, _)| *start)
        .map(|(_, url)| url)
        .ok_or(TwitchContractError::SettingsScriptNotFound)
}

pub fn extract_spade_url(settings_js: &str) -> Result<String, TwitchContractError> {
    let marker = "\"spade_url\"";
    let mut found = false;
    let value = settings_js.match_indices(marker).find_map(|(start, _)| {
        found = true;
        let encoded = settings_js[start + marker.len()..]
            .trim_start()
            .strip_prefix(':')?
            .trim_start();
        let mut parser = serde_json::Deserializer::from_str(encoded);
        String::deserialize(&mut parser).ok()
    });
    value.ok_or(if found {
        TwitchContractError::InvalidSpadeUrlString
    } else {
        TwitchContractError::SpadeUrlNotFound
    })
}
