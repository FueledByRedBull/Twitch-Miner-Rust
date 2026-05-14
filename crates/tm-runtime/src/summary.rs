use tm_domain::{format_channel_points, format_duration, Streamer};

use crate::types::{SessionSummary, StreamerSummary};

pub fn apply_pubsub_gain(streamer: &mut Streamer, earned: i64, reason: &str, balance: i64) -> i64 {
    let previous = streamer.channel_points;
    let expected = previous + earned;
    let mut new_balance = expected;
    if earned == 0 && balance != 0 {
        new_balance = balance;
    }
    if new_balance < 0 {
        new_balance = 0;
    }
    if earned >= 0 && new_balance < previous {
        new_balance = previous;
    }

    streamer.channel_points = new_balance;
    streamer.points_init = true;

    let delta = if earned == 0 {
        streamer.channel_points - previous
    } else {
        earned
    };

    update_history(streamer, reason, earned);
    delta
}

pub fn update_history(streamer: &mut Streamer, reason: &str, amount: i64) {
    if reason.is_empty() {
        return;
    }
    let entry = streamer.history.entry(reason.to_string()).or_default();
    entry.count += 1;
    entry.amount += amount;
    if reason == "WATCH_STREAK" {
        if let Some(stream) = streamer.stream.as_mut() {
            stream.watch_streak_missing = false;
        }
    }
}

#[must_use]
pub fn build_session_summary(
    streamers: &[Streamer],
    initial_points: &[(&str, i64)],
    anonymize: bool,
    duration: std::time::Duration,
) -> SessionSummary {
    let initial_points: std::collections::HashMap<&str, i64> =
        initial_points.iter().copied().collect();
    let total_points_change: i64 = streamers
        .iter()
        .map(|streamer| {
            streamer.channel_points
                - initial_points
                    .get(streamer.username.as_str())
                    .copied()
                    .unwrap_or_default()
        })
        .sum();

    let total_points_line = if anonymize {
        "Total Points gained: [hidden]".to_string()
    } else {
        let sign = if total_points_change < 0 { "-" } else { "+" };
        format!("Total Points gained: {sign}{}", total_points_change.abs())
    };

    let streamers = streamers
        .iter()
        .filter_map(|streamer| {
            let initial = initial_points
                .get(streamer.username.as_str())
                .copied()
                .unwrap_or_default();
            let total = streamer.channel_points - initial;
            if total == 0 && streamer.history.is_empty() {
                return None;
            }

            let total_points_line = if anonymize {
                "Total Points [hidden]".to_string()
            } else {
                let sign = if total < 0 { "-" } else { "+" };
                format!("Total Points {sign}{}", total.abs())
            };
            let history_lines = streamer
                .history
                .iter()
                .map(|(reason, entry)| {
                    if anonymize {
                        format!("{reason} ({} times, [hidden])", entry.count)
                    } else {
                        format!("{reason} ({} times, {} gained)", entry.count, entry.amount)
                    }
                })
                .collect();

            Some(StreamerSummary {
                username: streamer.username.clone(),
                current_points: if anonymize {
                    "[hidden]".to_string()
                } else {
                    format_channel_points(streamer.channel_points)
                },
                total_points_line,
                history_lines,
            })
        })
        .collect();

    SessionSummary {
        duration: format_duration(duration),
        total_points_line,
        streamers,
    }
}
