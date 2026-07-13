use std::collections::VecDeque;

use tm_domain::{format_channel_points, BetSettings, PredictionEvent, Strategy, Streamer};

use crate::types::{PredictionSummary, SessionSummary, StreamerSummary};

pub fn apply_pubsub_gain(streamer: &mut Streamer, earned: i64, reason: &str, balance: i64) -> i64 {
    // A replay key is valid only while no other point event has been applied. This avoids
    // suppressing a legitimate later equal gain after a prediction stake or other balance move.
    streamer.processed_point_event_keys.clear();
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
    completed_predictions: &VecDeque<PredictionEvent>,
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
        .enumerate()
        .filter_map(|(index, streamer)| {
            let initial = initial_points
                .get(streamer.username.as_str())
                .copied()
                .unwrap_or_default();
            let total = streamer.channel_points - initial;
            if total == 0 && streamer.history.is_empty() {
                return None;
            }

            let total_points_line = if anonymize {
                "Total points gained (after farming - before farming): [hidden]".to_string()
            } else {
                let sign = if total < 0 { "-" } else { "+" };
                format!(
                    "Total points gained (after farming - before farming): {sign}{}",
                    total.abs()
                )
            };
            let mut history_lines = streamer
                .history
                .iter()
                .map(|(reason, entry)| {
                    if anonymize {
                        format!("{reason} ({} times, [hidden])", entry.count)
                    } else {
                        format!("{reason} ({} times, {} gained)", entry.count, entry.amount)
                    }
                })
                .collect::<Vec<_>>();
            history_lines.sort();

            Some(StreamerSummary {
                channel_id: if anonymize {
                    String::from("[hidden]")
                } else {
                    streamer.channel_id.clone()
                },
                username: if anonymize {
                    format!("streamer-{}", index + 1)
                } else {
                    streamer.username.clone()
                },
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

    let predictions = completed_predictions
        .iter()
        .map(|event| prediction_summary(event, anonymize))
        .collect();

    SessionSummary {
        duration: format_session_duration(duration),
        total_points_line,
        streamers,
        predictions,
    }
}

fn prediction_summary(event: &PredictionEvent, anonymize: bool) -> PredictionSummary {
    let event_id = if anonymize {
        "[hidden]"
    } else {
        event.event_id.as_str()
    };
    let title = if anonymize {
        "[hidden]".to_string()
    } else {
        one_line(&event.title)
    };
    let username = if anonymize {
        "streamer"
    } else {
        event.streamer.username.as_str()
    };
    let channel_id = if anonymize {
        "[hidden]"
    } else {
        event.streamer.channel_id.as_str()
    };
    let points = if anonymize {
        "[hidden]".to_string()
    } else {
        event.streamer.channel_points.to_string()
    };
    let choice = if anonymize {
        String::from("[hidden]")
    } else {
        event
            .decision
            .choice
            .map_or_else(|| String::from("unknown"), choice_label)
    };
    let outcome_id = if anonymize {
        "[hidden]"
    } else {
        event.decision.outcome_id.as_str()
    };
    let amount = if anonymize {
        "[hidden]".to_string()
    } else {
        event.decision.amount.to_string()
    };
    let outcome_lines = prediction_outcome_lines(event, anonymize);
    let result_type = if event.result_type.is_empty() {
        "UNKNOWN"
    } else {
        event.result_type.as_str()
    };
    let result_details = if anonymize {
        "[hidden]".to_string()
    } else if event.result_string.is_empty() {
        String::from("pending")
    } else {
        one_line(&event.result_string)
    };

    PredictionSummary {
        event_line: format!("EventPrediction(event_id={event_id}, title=\"{title}\")"),
        streamer_line: format!(
            "Streamer(username={username}, channel_id={channel_id}, channel_points={points})"
        ),
        bet_settings_line: format_bet_settings(&event.streamer.settings.bet),
        bet_line: prediction_bet_line(event, anonymize, &choice, &amount, outcome_id),
        outcome_lines,
        result_line: format!("Result: {{'type': '{result_type}', 'details': '{result_details}'}}"),
    }
}

fn prediction_bet_line(
    event: &PredictionEvent,
    anonymize: bool,
    choice: &str,
    amount: &str,
    outcome_id: &str,
) -> String {
    if anonymize {
        return format!(
            "Bet(TotalUsers=[hidden], TotalPoints=[hidden]), Decision={{'choice': '{choice}', 'amount': {amount}, 'id': '{outcome_id}'}}"
        );
    }
    let total_users: i64 = event
        .outcomes
        .iter()
        .map(|outcome| outcome.total_users)
        .sum();
    let total_points: i64 = event
        .outcomes
        .iter()
        .map(|outcome| outcome.total_points)
        .sum();
    format!(
        "Bet(TotalUsers={}, TotalPoints={}), Decision={{'choice': '{choice}', 'amount': {amount}, 'id': '{outcome_id}'}}",
        format_channel_points(total_users),
        format_channel_points(total_points)
    )
}

fn prediction_outcome_lines(event: &PredictionEvent, anonymize: bool) -> Vec<String> {
    event
        .outcomes
        .iter()
        .enumerate()
        .map(|(index, outcome)| {
            if anonymize {
                return format!("Outcome{index}([hidden])");
            }
            format!(
                "Outcome{index}({} ({}) Points: {}, Users: {} ({:.2}%), Odds: {:.2} ({:.2}%))",
                one_line(&outcome.title),
                outcome.color.to_uppercase(),
                format_channel_points(outcome.total_points),
                format_channel_points(outcome.total_users),
                outcome.percentage_users,
                outcome.odds,
                outcome.odds_percentage
            )
        })
        .collect()
}

fn format_bet_settings(settings: &BetSettings) -> String {
    format!(
        "BetSettings(Strategy={}, Percentage={}, PercentageGap={}, MaxPoints={})",
        strategy_name(settings.strategy),
        settings.percentage.unwrap_or_default(),
        settings.percentage_gap.unwrap_or_default(),
        settings.max_points.unwrap_or_default()
    )
}

const fn strategy_name(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::MostVoted => "MOST_VOTED",
        Strategy::HighOdds => "HIGH_ODDS",
        Strategy::Percentage => "PERCENTAGE",
        Strategy::SmartMoney => "SMART_MONEY",
        Strategy::Smart => "SMART",
        Strategy::Number1 => "NUMBER_1",
        Strategy::Number2 => "NUMBER_2",
        Strategy::Number3 => "NUMBER_3",
        Strategy::Number4 => "NUMBER_4",
        Strategy::Number5 => "NUMBER_5",
        Strategy::Number6 => "NUMBER_6",
        Strategy::Number7 => "NUMBER_7",
        Strategy::Number8 => "NUMBER_8",
    }
}

fn choice_label(index: usize) -> String {
    u8::try_from(index)
        .ok()
        .and_then(|value| b'A'.checked_add(value))
        .map_or_else(|| index.to_string(), |value| char::from(value).to_string())
}

fn one_line(value: &str) -> String {
    value
        .replace(['\r', '\n', '\"', '\''], " ")
        .trim()
        .to_string()
}

fn format_session_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    format!(
        "{hours:02}:{minutes:02}:{seconds:02}.{:06}",
        duration.subsec_micros()
    )
}
