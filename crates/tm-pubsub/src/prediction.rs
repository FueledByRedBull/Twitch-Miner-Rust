use ::time::format_description::well_known::Rfc3339;
use serde_json::Value;
use tm_domain::{OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome, Streamer};

pub(crate) fn parse_prediction_event(
    streamer: &Streamer,
    raw_event: &Value,
    _apply_streamer_delay: bool,
) -> PredictionEvent {
    let created_at = raw_event
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    let raw_window = raw_event
        .get("prediction_window_seconds")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let outcomes = raw_event
        .get("outcomes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(parse_prediction_outcome)
        .collect::<Vec<_>>();
    let mut event = PredictionEvent {
        streamer: streamer.clone(),
        event_id: raw_event
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: raw_event
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        status: raw_event
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_uppercase(),
        created_at,
        window_seconds: raw_window,
        outcomes,
        decision: PredictionDecision::default(),
        bet_placed: false,
        bet_confirmed: false,
        result_type: String::new(),
        result_string: String::new(),
    };
    event.update_outcomes();
    event
}

fn parse_prediction_outcome(raw: &Value) -> PredictionOutcome {
    PredictionOutcome {
        id: raw
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: raw
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        color: raw
            .get("color")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        total_users: raw
            .get("total_users")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        total_points: raw
            .get("total_points")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        top_points: raw
            .get("top_predictors")
            .and_then(Value::as_array)
            .and_then(|predictors| predictors.first())
            .and_then(|predictor| predictor.get("points"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        percentage_users: 0.0,
        odds: 0.0,
        odds_percentage: 0.0,
    }
}

pub(crate) fn winning_outcome_id(raw_event: &Value) -> Option<String> {
    raw_event
        .get("winning_outcome_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            raw_event
                .get("outcomes")
                .and_then(Value::as_array)
                .and_then(|outcomes| {
                    outcomes.iter().find_map(|outcome| {
                        if outcome
                            .get("is_winning_outcome")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            return outcome
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                        let state = outcome
                            .get("state")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_uppercase();
                        if matches!(state.as_str(), "RESOLVED" | "WINNER" | "WIN") {
                            outcome
                                .get("id")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        } else {
                            None
                        }
                    })
                })
        })
}
