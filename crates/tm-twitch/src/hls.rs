use crate::types::TwitchClientError;

pub(crate) fn lowest_bandwidth_variant_url(
    base_url: &reqwest::Url,
    playlist: &str,
) -> Result<reqwest::Url, TwitchClientError> {
    validate_header(playlist, "master playlist")?;

    let mut pending_attributes = None;
    let mut selected: Option<(u64, reqwest::Url)> = None;
    for line in playlist.lines().map(str::trim) {
        if line.is_empty() || line == "#EXTM3U" {
            continue;
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if pending_attributes.is_some() {
                return Err(TwitchClientError::InvalidField("master playlist"));
            }
            pending_attributes = Some(parse_variant_attributes(attributes)?);
            continue;
        }
        if pending_attributes.is_none() {
            continue;
        }
        if line.starts_with('#') {
            if line.starts_with("#EXT") {
                return Err(TwitchClientError::InvalidField("master playlist"));
            }
            continue;
        }

        let attributes = pending_attributes
            .take()
            .ok_or(TwitchClientError::InvalidField("master playlist"))?;
        if attributes.has_video {
            let url = base_url
                .join(line)
                .map_err(|_| TwitchClientError::InvalidField("master playlist"))?;
            if selected
                .as_ref()
                .is_none_or(|(bandwidth, _)| attributes.bandwidth < *bandwidth)
            {
                selected = Some((attributes.bandwidth, url));
            }
        }
    }
    if pending_attributes.is_some() {
        return Err(TwitchClientError::InvalidField("master playlist"));
    }
    selected
        .map(|(_, url)| url)
        .ok_or(TwitchClientError::MissingField("master playlist"))
}

pub(crate) fn media_segment_url(
    base_url: &reqwest::Url,
    playlist: &str,
) -> Result<reqwest::Url, TwitchClientError> {
    validate_header(playlist, "media playlist")?;

    let mut expects_segment = false;
    let mut saw_stray_uri = false;
    let mut newest_segment = None;
    for line in playlist.lines().map(str::trim) {
        if line.is_empty() || line == "#EXTM3U" {
            continue;
        }
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            let duration = value
                .split_once(',')
                .map(|(duration, _)| duration)
                .ok_or(TwitchClientError::InvalidField("media playlist"))?
                .trim()
                .parse::<f64>()
                .map_err(|_| TwitchClientError::InvalidField("media playlist"))?;
            if !duration.is_finite() || duration < 0.0 || expects_segment {
                return Err(TwitchClientError::InvalidField("media playlist"));
            }
            if saw_stray_uri {
                return Err(TwitchClientError::InvalidField("media playlist"));
            }
            expects_segment = true;
            continue;
        }
        if line == "#EXTINF" {
            return Err(TwitchClientError::InvalidField("media playlist"));
        }
        if line.starts_with('#') {
            continue;
        }

        if !expects_segment {
            if newest_segment.is_some() {
                return Err(TwitchClientError::InvalidField("media playlist"));
            }
            saw_stray_uri = true;
            continue;
        }

        let url = base_url
            .join(line)
            .map_err(|_| TwitchClientError::InvalidField("media playlist"))?;
        newest_segment = Some(url);
        expects_segment = false;
    }
    if expects_segment {
        Err(TwitchClientError::InvalidField("media playlist"))
    } else {
        newest_segment.ok_or(TwitchClientError::MissingField("media playlist"))
    }
}

fn validate_header(playlist: &str, field: &'static str) -> Result<(), TwitchClientError> {
    if playlist
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        == Some("#EXTM3U")
    {
        Ok(())
    } else {
        Err(TwitchClientError::InvalidField(field))
    }
}

struct VariantAttributes {
    bandwidth: u64,
    has_video: bool,
}

fn parse_variant_attributes(input: &str) -> Result<VariantAttributes, TwitchClientError> {
    let attributes =
        split_attribute_list(input).ok_or(TwitchClientError::InvalidField("master playlist"))?;
    let bandwidth = attribute(&attributes, "BANDWIDTH")
        .ok_or(TwitchClientError::InvalidField("master playlist"))?
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(TwitchClientError::InvalidField("master playlist"))?;
    let has_resolution = attribute(&attributes, "RESOLUTION").is_some_and(|value| {
        value
            .split_once('x')
            .or_else(|| value.split_once('X'))
            .is_some_and(|(width, height)| {
                width.parse::<u32>().is_ok_and(|value| value > 0)
                    && height.parse::<u32>().is_ok_and(|value| value > 0)
            })
    });
    let has_video_codec = attribute(&attributes, "CODECS").is_some_and(|value| {
        value.split(',').map(str::trim).any(|codec| {
            [
                "avc1", "avc3", "hev1", "hvc1", "vp8", "vp09", "av01", "theora",
            ]
            .iter()
            .any(|prefix| codec.to_ascii_lowercase().starts_with(prefix))
        })
    });
    Ok(VariantAttributes {
        bandwidth,
        has_video: has_resolution || has_video_codec,
    })
}

fn split_attribute_list(input: &str) -> Option<Vec<(&str, &str)>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (index, character) in input.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted {
        return None;
    }
    parts.push(&input[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .map(|part| {
            let (key, value) = part.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            if key.is_empty() || value.is_empty() {
                return None;
            }
            let value = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                &value[1..value.len() - 1]
            } else if value.contains('"') {
                return None;
            } else {
                value
            };
            Some((key, value))
        })
        .collect()
}

fn attribute<'a>(attributes: &'a [(&str, &str)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(*value))
}
