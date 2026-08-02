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
                .any(|outcome| outcome.id.as_ref() == winning_outcome_id)
            {
                return None;
            }
            if event.decision.outcome_id.as_ref() == winning_outcome_id {
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
    if decision.amount <= 0 || decision.outcome_id.as_ref() != winning_outcome_id {
        return 0;
    }

    let total_points = outcomes.iter().fold(0_i128, |total, outcome| {
        total.saturating_add(i128::from(outcome.total_points))
    });
    let winning_points = outcomes
        .iter()
        .find(|outcome| outcome.id.as_ref() == winning_outcome_id)
        .map(|outcome| i128::from(outcome.total_points))
        .unwrap_or_default();
    if total_points <= 0 || winning_points <= 0 {
        return decision.amount;
    }

    let numerator = i128::from(decision.amount).saturating_mul(total_points);
    let payout = i64::try_from(
        numerator
            .saturating_add(winning_points / 2)
            .checked_div(winning_points)
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX);
    payout.max(decision.amount)
}

#[cfg(test)]
mod tests {
    use tm_domain::{OffsetDateTime, PredictionOutcome, Streamer};

    use super::*;

    fn prediction(amount: i64, left_points: i64, right_points: i64) -> PredictionEvent {
        PredictionEvent {
            streamer: Streamer {
                username: String::from("tester"),
                ..Streamer::default()
            },
            event_id: String::from("prediction-property"),
            title: String::from("Fixture"),
            status: String::from("RESOLVED"),
            created_at: OffsetDateTime::UNIX_EPOCH,
            window_seconds: 30.0,
            outcomes: vec![
                PredictionOutcome {
                    id: "a".into(),
                    title: String::from("A"),
                    total_points: left_points,
                    ..PredictionOutcome::default()
                },
                PredictionOutcome {
                    id: "b".into(),
                    title: String::from("B"),
                    total_points: right_points,
                    ..PredictionOutcome::default()
                },
            ],
            decision: PredictionDecision {
                choice: Some(0),
                outcome_id: "a".into(),
                amount,
            },
            bet_placed: true,
            bet_confirmed: true,
            result_type: String::new(),
            result_string: String::new(),
        }
    }

    #[test]
    fn settlement_properties_hold_across_extreme_valid_pools() {
        for amount in [1, 100, i64::MAX] {
            for left_points in [0, 1, 100, i64::MAX] {
                for right_points in [0, 1, 100, i64::MAX] {
                    for winner in ["a", "b"] {
                        let mut event = prediction(amount, left_points, right_points);
                        let effect = build_prediction_settlement_effect(&mut event, Some(winner))
                            .expect("known winner must settle a resolved prediction");
                        let expected_result = if winner == "a" { "WIN" } else { "LOSE" };

                        assert_eq!(event.result_type, expected_result);
                        assert!(event.result_string.starts_with(expected_result));
                        assert!(matches!(
                            effect,
                            RuntimeEffect::PredictionSettled {
                                ref result_type,
                                ..
                            } if result_type == expected_result
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn settlement_requires_a_known_winner_and_refunds_both_cancel_spellings() {
        let mut unresolved = prediction(100, 100, 100);
        assert!(build_prediction_settlement_effect(&mut unresolved, None).is_none());
        assert!(build_prediction_settlement_effect(&mut unresolved, Some("unknown")).is_none());

        for status in ["CANCELED", "CANCELLED"] {
            let mut canceled = prediction(100, i64::MAX, i64::MAX);
            canceled.status = String::from(status);
            let effect = build_prediction_settlement_effect(&mut canceled, None);

            assert!(effect.is_some());
            assert_eq!(canceled.result_type, "REFUND");
            assert_eq!(canceled.result_string, "REFUND, Refunded: +0");
        }
    }

    #[test]
    fn resolved_status_classifier_accepts_only_terminal_statuses() {
        for status in ["RESOLVED", "CANCELED", "CANCELLED"] {
            assert!(prediction_status_is_resolved(status));
        }
        for status in ["", "ACTIVE", "LOCKED", "resolved", "CANCEL"] {
            assert!(!prediction_status_is_resolved(status));
        }
    }
}
