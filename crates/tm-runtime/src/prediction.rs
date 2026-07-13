use tm_domain::{PredictionDecision, PredictionEvent};

use crate::effect::RuntimeEffect;

pub(crate) fn build_prediction_settlement_effect(
    event: &mut PredictionEvent,
    winning_outcome_id: Option<&str>,
) -> Option<RuntimeEffect> {
    let settlement = match event.status.as_str() {
        "CANCELED" | "CANCELLED" => event.parse_result("REFUND", 0),
        "RESOLVED" => {
            let winning_outcome_id = winning_outcome_id?;
            if !event
                .outcomes
                .iter()
                .any(|outcome| outcome.id == winning_outcome_id)
            {
                return None;
            }
            if event.decision.outcome_id == winning_outcome_id {
                event.parse_result(
                    "WIN",
                    payout_for_outcome(&event.decision, &event.outcomes, winning_outcome_id),
                )
            } else {
                event.parse_result("LOSE", 0)
            }
        }
        _ => return None,
    };
    Some(RuntimeEffect::PredictionSettled {
        event_id: event.event_id.clone(),
        streamer_username: event.streamer.username.clone(),
        title: event.title.clone(),
        decision_label: settlement.decision_label,
        result_type: settlement.result_type,
        result_string: settlement.result_string,
    })
}

pub(crate) fn prediction_status_is_resolved(status: &str) -> bool {
    matches!(status, "RESOLVED" | "CANCELED" | "CANCELLED")
}

fn payout_for_outcome(
    decision: &PredictionDecision,
    outcomes: &[tm_domain::PredictionOutcome],
    winning_outcome_id: &str,
) -> i64 {
    if decision.amount <= 0 || decision.outcome_id != winning_outcome_id {
        return 0;
    }

    let total_points = outcomes
        .iter()
        .map(|outcome| outcome.total_points)
        .sum::<i64>();
    let winning_points = outcomes
        .iter()
        .find(|outcome| outcome.id == winning_outcome_id)
        .map(|outcome| outcome.total_points)
        .unwrap_or_default();
    if total_points <= 0 || winning_points <= 0 {
        return decision.amount;
    }

    let numerator = i128::from(decision.amount) * i128::from(total_points);
    let denominator = i128::from(winning_points);
    let payout = i64::try_from((numerator + (denominator / 2)) / denominator).unwrap_or(i64::MAX);
    payout.max(decision.amount)
}
