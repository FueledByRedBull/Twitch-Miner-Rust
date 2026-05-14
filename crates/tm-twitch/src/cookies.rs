use std::collections::HashMap;

#[must_use]
pub fn claim_bonus_cookie_header(auth_token: &str, user_id: &str) -> Option<String> {
    match (auth_token.trim(), user_id.trim()) {
        ("", _) => None,
        (token, "") => Some(format!("auth-token={token}")),
        (token, persistent) => Some(format!("auth-token={token}; persistent={persistent}")),
    }
}

pub(crate) fn is_twitch_cookie_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(is_twitch_cookie_host))
        .unwrap_or(false)
}

pub(crate) fn is_twitch_cookie_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('.').to_lowercase();
    host == "twitch.tv" || host.ends_with(".twitch.tv")
}

pub(crate) fn merge_cookie_headers(
    default_cookie: Option<&str>,
    cookie: Option<&str>,
) -> Option<String> {
    let mut order = Vec::new();
    let mut values = HashMap::new();

    for source in [default_cookie, cookie] {
        for segment in source.into_iter().flat_map(|value| value.split(';')) {
            let Some((name, value)) = segment.trim().split_once('=') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            if name.is_empty() || value.is_empty() {
                continue;
            }
            if !values.contains_key(name) {
                order.push(name.to_string());
            }
            values.insert(name.to_string(), value.to_string());
        }
    }

    (!order.is_empty()).then(|| {
        order
            .into_iter()
            .filter_map(|name| values.get(&name).map(|value| format!("{name}={value}")))
            .collect::<Vec<_>>()
            .join("; ")
    })
}
