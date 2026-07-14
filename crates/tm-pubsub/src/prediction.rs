use ::time::format_description::well_known::Rfc3339;
use serde_json::Value;
use tm_domain::{OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome, Streamer};

pub(crate) fn parse_prediction_event(
    streamer: &Streamer,
    raw_event: &Value,
    is_created: bool,
) -> Result<PredictionEvent, &'static str> {
    let created_at = if is_created {
        optional_timestamp(raw_event, "created_at")?
            .ok_or("prediction event created_at is missing")?
    } else {
        optional_timestamp(raw_event, "created_at")
            .ok()
            .flatten()
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
    };
    let raw_window = if is_created {
        optional_nonnegative_float(raw_event, "prediction_window_seconds")?
            .ok_or("prediction event window is missing")?
    } else {
        optional_nonnegative_float(raw_event, "prediction_window_seconds")
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    let outcomes = parse_prediction_outcomes(raw_event, is_created)?;
    if is_created && outcomes.len() < 2 {
        return Err("prediction event has fewer than two outcomes");
    }
    let event_id = required_text(raw_event, "id", "prediction event id is missing")?;
    let title = if is_created {
        required_text(raw_event, "title", "prediction event title is missing")?
    } else {
        raw_event
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
    };
    let status =
        required_text(raw_event, "status", "prediction event status is missing")?.to_uppercase();
    if !matches!(
        status.as_str(),
        "ACTIVE" | "LOCKED" | "RESOLVED" | "CANCELED" | "CANCELLED"
    ) {
        return Err("prediction event status is unsupported");
    }
    let mut event = PredictionEvent {
        streamer: streamer.clone(),
        event_id: event_id.to_string(),
        title: title.to_string(),
        status,
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
    Ok(event)
}

fn parse_prediction_outcomes(
    raw_event: &Value,
    is_created: bool,
) -> Result<Vec<PredictionOutcome>, &'static str> {
    let Some(raw_outcomes) = raw_event.get("outcomes") else {
        return Ok(Vec::new());
    };
    let Some(raw_outcomes) = raw_outcomes.as_array() else {
        return if is_created {
            Err("prediction event outcomes are invalid")
        } else {
            Ok(Vec::new())
        };
    };
    let parsed = raw_outcomes
        .iter()
        .map(parse_prediction_outcome)
        .collect::<Result<Vec<_>, _>>();
    if is_created {
        parsed
    } else {
        // PubSub event-updated payloads are incremental. If Twitch omits any
        // display or counter field, retain the runtime's last complete snapshot.
        Ok(parsed.unwrap_or_default())
    }
}

fn optional_timestamp(value: &Value, field: &str) -> Result<Option<OffsetDateTime>, &'static str> {
    let Some(raw) = value.get(field) else {
        return Ok(None);
    };
    raw.as_str()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .map(Some)
        .ok_or("prediction event has invalid created_at")
}

fn optional_nonnegative_float(value: &Value, field: &str) -> Result<Option<f64>, &'static str> {
    let Some(raw) = value.get(field) else {
        return Ok(None);
    };
    raw.as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(Some)
        .ok_or("prediction event has invalid window")
}

fn parse_prediction_outcome(raw: &Value) -> Result<PredictionOutcome, &'static str> {
    let total_users = nonnegative_alias(raw, "total_users", "users")
        .ok_or("prediction outcome users are missing or invalid")?;
    let total_points = nonnegative_alias(raw, "total_points", "channel_points")
        .ok_or("prediction outcome points are missing or invalid")?;
    Ok(PredictionOutcome {
        id: required_text(raw, "id", "prediction outcome id is missing")?.to_string(),
        title: required_text(raw, "title", "prediction outcome title is missing")?.to_string(),
        color: required_text(raw, "color", "prediction outcome color is missing")?.to_string(),
        total_users,
        total_points,
        top_points: raw
            .get("top_predictors")
            .and_then(Value::as_array)
            .and_then(|predictors| predictors.first())
            .and_then(|predictor| nonnegative_alias(predictor, "points", "channel_points_used"))
            .unwrap_or_default(),
        percentage_users: 0.0,
        odds: 0.0,
        odds_percentage: 0.0,
    })
}

fn required_text<'a>(
    value: &'a Value,
    field: &str,
    error: &'static str,
) -> Result<&'a str, &'static str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(error)
}

fn nonnegative_alias(value: &Value, primary: &str, alternate: &str) -> Option<i64> {
    value
        .get(primary)
        .or_else(|| value.get(alternate))
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
}

pub(crate) fn winning_outcome_id(raw_event: &Value) -> Option<String> {
    raw_event
        .get("winning_outcome_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
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
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
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
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string)
                        } else {
                            None
                        }
                    })
                })
        })
}
