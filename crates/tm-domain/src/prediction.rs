use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::formatting::trim_trailing_zeros;
use crate::types::{BetSettings, Condition, OutcomeKey, Strategy, Streamer};

const MIN_STEALTH_OFFSET: u8 = 1;
const MAX_STEALTH_OFFSET: u8 = 5;

fn safe_duration_from_seconds(seconds: f64) -> std::time::Duration {
    if !seconds.is_finite() || seconds <= 0.0 {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::try_from_secs_f64(seconds).unwrap_or(std::time::Duration::MAX)
}

fn i64_as_f64(value: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let converted = value as f64;
    converted
}

fn percentage_of_balance(balance: i64, percentage: u32) -> i64 {
    let amount = i128::from(balance.max(0)) * i128::from(percentage) / 100;
    i64::try_from(amount).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PredictionOutcome {
    /// Immutable identity shared with owned decisions sent across the runtime actor.
    pub id: Arc<str>,
    pub title: String,
    pub color: String,
    pub total_users: i64,
    pub total_points: i64,
    pub top_points: i64,
    pub percentage_users: f64,
    pub odds: f64,
    pub odds_percentage: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PredictionDecision {
    pub choice: Option<usize>,
    /// Shares the selected outcome allocation while keeping the decision self-contained.
    pub outcome_id: Arc<str>,
    pub amount: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PredictionSettlement {
    pub gained: i64,
    pub placed: i64,
    pub won: i64,
    pub result_type: String,
    pub result_string: String,
    pub decision_label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictionEvent {
    pub streamer: Streamer,
    pub event_id: String,
    pub title: String,
    pub status: String,
    pub created_at: OffsetDateTime,
    pub window_seconds: f64,
    pub outcomes: Vec<PredictionOutcome>,
    pub decision: PredictionDecision,
    pub bet_placed: bool,
    pub bet_confirmed: bool,
    pub result_type: String,
    pub result_string: String,
}

impl PredictionEvent {
    pub fn update_outcomes(&mut self) {
        let total_users: i64 = self
            .outcomes
            .iter()
            .map(|outcome| outcome.total_users)
            .fold(0_i64, i64::saturating_add);
        let total_points: i64 = self
            .outcomes
            .iter()
            .map(|outcome| outcome.total_points)
            .fold(0_i64, i64::saturating_add);

        for outcome in &mut self.outcomes {
            outcome.percentage_users = if total_users > 0 {
                i64_as_f64(outcome.total_users) * 100.0 / i64_as_f64(total_users)
            } else {
                0.0
            };
            outcome.odds = if outcome.total_points > 0 {
                i64_as_f64(total_points) / i64_as_f64(outcome.total_points)
            } else {
                0.0
            };
            outcome.odds_percentage = if outcome.odds > 0.0 {
                100.0 / outcome.odds
            } else {
                0.0
            };
        }
    }

    #[must_use]
    pub fn closing_after(&self, now: OffsetDateTime) -> std::time::Duration {
        let elapsed = (now - self.created_at).as_seconds_f64();
        safe_duration_from_seconds(self.window_seconds - elapsed)
    }

    pub fn decide(&mut self, balance: i64) -> PredictionDecision {
        self.decide_with_stealth_offset(balance, MIN_STEALTH_OFFSET)
    }

    /// Evaluates one prediction with an application-selected small amount
    /// variation. This is not an anti-detection guarantee; keeping the offset
    /// injected leaves strategy and arithmetic deterministic under test.
    pub fn decide_with_stealth_offset(
        &mut self,
        balance: i64,
        stealth_offset: u8,
    ) -> PredictionDecision {
        let Some(choice) = select_outcome(&self.outcomes, &self.streamer.settings.bet) else {
            self.decision = PredictionDecision::default();
            return self.decision.clone();
        };

        let balance = balance.max(0);
        let settings = &self.streamer.settings.bet;
        let mut amount = percentage_of_balance(balance, settings.percentage.unwrap_or(5));

        if let Some(max_points) = settings.max_points {
            amount = amount.min(i64::from(max_points));
        }
        amount = amount.min(balance);

        if amount < 10 {
            amount = match settings.max_points {
                Some(max_points) if max_points < 10 => i64::from(max_points).min(balance),
                _ if balance >= 10 => 10,
                _ => amount,
            };
        }

        if settings.stealth_mode.unwrap_or(false) {
            let top_points = self.outcomes[choice].top_points;
            if top_points > 1 && amount >= top_points {
                let offset =
                    i64::from(stealth_offset.clamp(MIN_STEALTH_OFFSET, MAX_STEALTH_OFFSET));
                let effective_minimum = amount.min(10);
                amount = top_points.saturating_sub(offset).max(effective_minimum);
            }
        }

        let decision = PredictionDecision {
            choice: Some(choice),
            outcome_id: Arc::clone(&self.outcomes[choice].id),
            amount,
        };
        self.decision.clone_from(&decision);
        decision
    }

    #[must_use]
    pub fn should_skip_by_filter(&self) -> (bool, f64, String) {
        let Some(filter_condition) = self.streamer.settings.bet.filter_condition.as_ref() else {
            return (false, 0.0, String::new());
        };
        let Some(expected) = filter_condition.value else {
            return (false, 0.0, String::new());
        };

        let by_choice = |selector: fn(&PredictionOutcome) -> f64| -> Result<f64, String> {
            let choice = self
                .decision
                .choice
                .ok_or_else(|| String::from("filter_condition requires a decision outcome"))?;
            self.outcomes
                .get(choice)
                .map(selector)
                .ok_or_else(|| String::from("filter_condition requires a decision outcome"))
        };

        let compared = match filter_condition.by {
            OutcomeKey::TotalUsers => self
                .outcomes
                .iter()
                .map(|outcome| i64_as_f64(outcome.total_users))
                .sum(),
            OutcomeKey::TotalPoints => self
                .outcomes
                .iter()
                .map(|outcome| i64_as_f64(outcome.total_points))
                .sum(),
            OutcomeKey::DecisionUsers => match by_choice(|outcome| i64_as_f64(outcome.total_users))
            {
                Ok(value) => value,
                Err(reason) => return (true, 0.0, reason),
            },
            OutcomeKey::DecisionPoints => {
                match by_choice(|outcome| i64_as_f64(outcome.total_points)) {
                    Ok(value) => value,
                    Err(reason) => return (true, 0.0, reason),
                }
            }
            OutcomeKey::PercentageUsers => match by_choice(|outcome| outcome.percentage_users) {
                Ok(value) => value,
                Err(reason) => return (true, 0.0, reason),
            },
            OutcomeKey::Odds => match by_choice(|outcome| outcome.odds) {
                Ok(value) => value,
                Err(reason) => return (true, 0.0, reason),
            },
            OutcomeKey::OddsPercentage => match by_choice(|outcome| outcome.odds_percentage) {
                Ok(value) => value,
                Err(reason) => return (true, 0.0, reason),
            },
            OutcomeKey::TopPoints => match by_choice(|outcome| i64_as_f64(outcome.top_points)) {
                Ok(value) => value,
                Err(reason) => return (true, 0.0, reason),
            },
        };

        let pass = match filter_condition.condition {
            Condition::Gt => compared > expected,
            Condition::Lt => compared < expected,
            Condition::Gte => compared >= expected,
            Condition::Lte => compared <= expected,
        };

        if pass {
            (false, compared, String::new())
        } else {
            (
                true,
                compared,
                format!(
                    "filter_condition {:?} {:?} {} not met (current {})",
                    filter_condition.by,
                    filter_condition.condition,
                    format_float(expected),
                    format_float(compared)
                ),
            )
        }
    }

    pub fn parse_result(&mut self, result_type: &str, points_won: i64) -> PredictionSettlement {
        let result_type = result_type.trim().to_uppercase();
        let mut placed = self.decision.amount;
        let mut won = points_won.max(0);
        if result_type == "REFUND" {
            placed = 0;
            won = 0;
        }
        let gained = won - placed;
        self.result_type.clone_from(&result_type);

        let action = match result_type.as_str() {
            "LOSE" => "Lost",
            "REFUND" => "Refunded",
            _ => "Gained",
        };
        let sign = if gained >= 0 { "+" } else { "" };
        let result_string = format!(
            "{result_type}, {action}: {sign}{}",
            crate::format_channel_points(gained)
        );
        self.result_string.clone_from(&result_string);

        PredictionSettlement {
            gained,
            placed,
            won,
            result_type,
            result_string,
            decision_label: self.decision_label(),
        }
    }

    #[must_use]
    pub fn decision_outcome(&self) -> Option<&PredictionOutcome> {
        if let Some(choice) = self.decision.choice {
            if let Some(outcome) = self.outcomes.get(choice) {
                return Some(outcome);
            }
        }
        self.outcomes
            .iter()
            .find(|outcome| outcome.id == self.decision.outcome_id)
    }

    #[must_use]
    pub fn decision_outcome_string(&self) -> String {
        self.decision_outcome()
            .map_or_else(|| self.decision.outcome_id.to_string(), ToString::to_string)
    }

    #[must_use]
    pub fn decision_label(&self) -> String {
        let Some(outcome) = self.decision_outcome() else {
            if self.decision.outcome_id.is_empty() {
                return String::new();
            }
            return self.decision.choice.map_or_else(
                || self.decision.outcome_id.to_string(),
                |choice| format!("{}: {}", choice_label(choice), self.decision.outcome_id),
            );
        };
        let choice = self.decision.choice.or_else(|| {
            self.outcomes
                .iter()
                .position(|candidate| candidate.id == outcome.id)
        });
        let Some(choice) = choice else {
            return format!("{} ({})", outcome.title, outcome.color.to_uppercase());
        };
        format!(
            "{}: {} ({})",
            choice_label(choice),
            outcome.title,
            outcome.color.to_uppercase()
        )
    }
}

impl fmt::Display for PredictionOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}), Points: {}, Users: {} ({:.2}%), Odds: {} ({}%)",
            self.title.trim(),
            self.color.to_uppercase(),
            crate::format_channel_points(self.total_points),
            crate::format_channel_points(self.total_users),
            self.percentage_users,
            format_float(self.odds),
            format_float(self.odds_percentage)
        )
    }
}

#[must_use]
pub fn select_outcome(outcomes: &[PredictionOutcome], settings: &BetSettings) -> Option<usize> {
    if outcomes.is_empty() {
        return None;
    }

    let number_choice = |index: usize| {
        outcomes
            .get(index)
            .map(|_| index)
            .or_else(|| max_index(outcomes, |outcome| outcome.odds))
    };

    match settings.strategy {
        Strategy::MostVoted => max_index_by_key(outcomes, |outcome| outcome.total_users),
        Strategy::HighOdds => max_index(outcomes, |outcome| outcome.odds),
        Strategy::Percentage => max_index(outcomes, |outcome| outcome.odds_percentage),
        Strategy::SmartMoney => max_index_by_key(outcomes, |outcome| outcome.top_points),
        Strategy::Number1 => Some(0),
        Strategy::Number2 => number_choice(1),
        Strategy::Number3 => number_choice(2),
        Strategy::Number4 => number_choice(3),
        Strategy::Number5 => number_choice(4),
        Strategy::Number6 => number_choice(5),
        Strategy::Number7 => number_choice(6),
        Strategy::Number8 => number_choice(7),
        Strategy::Smart => {
            let gap = f64::from(settings.percentage_gap.unwrap_or(20));
            let mut sorted = outcomes.to_vec();
            sorted.sort_by(|left, right| {
                right
                    .percentage_users
                    .partial_cmp(&left.percentage_users)
                    .unwrap_or(Ordering::Equal)
            });
            if sorted.len() >= 2
                && (sorted[0].percentage_users - sorted[1].percentage_users).abs() < gap
            {
                max_index(outcomes, |outcome| outcome.odds)
            } else {
                max_index_by_key(outcomes, |outcome| outcome.total_users)
            }
        }
    }
}

fn max_index(
    outcomes: &[PredictionOutcome],
    scorer: impl Fn(&PredictionOutcome) -> f64,
) -> Option<usize> {
    let mut outcomes = outcomes.iter().enumerate();
    let (mut best_index, first) = outcomes.next()?;
    let mut best_score = scorer(first);
    for (index, outcome) in outcomes {
        let score = scorer(outcome);
        if score.partial_cmp(&best_score) == Some(Ordering::Greater) {
            best_index = index;
            best_score = score;
        }
    }
    Some(best_index)
}

fn max_index_by_key<T: Ord>(
    outcomes: &[PredictionOutcome],
    key: impl Fn(&PredictionOutcome) -> T,
) -> Option<usize> {
    let mut outcomes = outcomes.iter().enumerate();
    let (mut best_index, first) = outcomes.next()?;
    let mut best_key = key(first);
    for (index, outcome) in outcomes {
        let candidate_key = key(outcome);
        if candidate_key > best_key {
            best_index = index;
            best_key = candidate_key;
        }
    }
    Some(best_index)
}

fn format_float(value: f64) -> String {
    trim_trailing_zeros(&format!("{value:.2}"))
}

fn choice_label(choice: usize) -> String {
    if choice < 26 {
        return char::from_u32(u32::from(b'A') + u32::try_from(choice).unwrap_or_default())
            .unwrap_or('#')
            .to_string();
    }
    format!("#{}", choice.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::types::{Condition, FilterCondition, OutcomeKey};

    fn assert_f64_eq(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn percentage_amount_is_exact_across_integer_boundaries() {
        for balance in [-1, 0, 1, 10, 9_007_199_254_740_993, i64::MAX] {
            for percentage in [0, 1, 5, 33, 100, 101, u32::MAX] {
                let expected = i128::from(balance.max(0)) * i128::from(percentage) / 100;
                let expected = i64::try_from(expected).unwrap_or(i64::MAX);
                assert_eq!(
                    percentage_of_balance(balance, percentage),
                    expected,
                    "balance={balance}, percentage={percentage}"
                );
            }
        }
    }

    #[test]
    fn integer_strategies_distinguish_values_above_f64_precision() {
        let outcomes = vec![
            PredictionOutcome {
                total_users: 9_007_199_254_740_993,
                top_points: 9_007_199_254_740_993,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                total_users: 9_007_199_254_740_992,
                top_points: 9_007_199_254_740_992,
                ..PredictionOutcome::default()
            },
        ];
        let mut settings = BetSettings {
            strategy: Strategy::MostVoted,
            ..BetSettings::default()
        };
        assert_eq!(select_outcome(&outcomes, &settings), Some(0));
        settings.strategy = Strategy::SmartMoney;
        assert_eq!(select_outcome(&outcomes, &settings), Some(0));
    }

    #[test]
    fn prediction_strategy_ties_select_the_first_outcome() {
        let outcomes = vec![
            PredictionOutcome {
                id: Arc::from("first"),
                total_users: 10,
                top_points: 20,
                odds: 2.0,
                odds_percentage: 50.0,
                percentage_users: 50.0,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: Arc::from("second"),
                total_users: 10,
                top_points: 20,
                odds: 2.0,
                odds_percentage: 50.0,
                percentage_users: 50.0,
                ..PredictionOutcome::default()
            },
        ];
        for strategy in [
            Strategy::MostVoted,
            Strategy::HighOdds,
            Strategy::Percentage,
            Strategy::SmartMoney,
            Strategy::Smart,
        ] {
            assert_eq!(
                select_outcome(
                    &outcomes,
                    &BetSettings {
                        strategy,
                        ..BetSettings::default()
                    }
                ),
                Some(0),
                "strategy={strategy:?}"
            );
        }
    }

    #[test]
    fn closing_after_is_safe_for_invalid_prediction_windows() {
        for window_seconds in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let event = PredictionEvent {
                streamer: Streamer::default(),
                event_id: String::new(),
                title: String::new(),
                status: String::new(),
                created_at: datetime!(2026-03-27 06:00 UTC),
                window_seconds,
                outcomes: Vec::new(),
                decision: PredictionDecision::default(),
                bet_placed: false,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            };
            assert_eq!(
                event.closing_after(datetime!(2026-03-27 06:00 UTC)),
                std::time::Duration::ZERO
            );
        }
    }

    #[test]
    fn filter_condition_totals() {
        let mut event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::new(),
            title: String::new(),
            status: String::from("ACTIVE"),
            created_at: datetime!(2026-03-27 06:00 UTC),
            window_seconds: 10.0,
            outcomes: vec![
                PredictionOutcome {
                    total_users: 10,
                    total_points: 100,
                    ..PredictionOutcome::default()
                },
                PredictionOutcome {
                    total_users: 15,
                    total_points: 200,
                    ..PredictionOutcome::default()
                },
            ],
            decision: PredictionDecision {
                choice: Some(1),
                ..PredictionDecision::default()
            },
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };
        event.streamer.settings.bet.filter_condition = Some(FilterCondition {
            by: OutcomeKey::TotalUsers,
            condition: Condition::Gte,
            value: Some(20.0),
        });
        let (skip, compared, _) = event.should_skip_by_filter();
        assert!(!skip);
        assert_f64_eq(compared, 25.0);

        event
            .streamer
            .settings
            .bet
            .filter_condition
            .as_mut()
            .unwrap()
            .value = Some(30.0);
        let (skip, compared, _) = event.should_skip_by_filter();
        assert!(skip);
        assert_f64_eq(compared, 25.0);
    }

    #[test]
    fn filter_condition_decision_users() {
        let mut event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::new(),
            title: String::new(),
            status: String::from("ACTIVE"),
            created_at: datetime!(2026-03-27 06:00 UTC),
            window_seconds: 10.0,
            outcomes: vec![
                PredictionOutcome {
                    total_users: 5,
                    total_points: 10,
                    ..PredictionOutcome::default()
                },
                PredictionOutcome {
                    total_users: 50,
                    total_points: 20,
                    ..PredictionOutcome::default()
                },
            ],
            decision: PredictionDecision {
                choice: Some(1),
                ..PredictionDecision::default()
            },
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };
        event.streamer.settings.bet.filter_condition = Some(FilterCondition {
            by: OutcomeKey::DecisionUsers,
            condition: Condition::Lt,
            value: Some(40.0),
        });
        let (skip, compared, _) = event.should_skip_by_filter();
        assert!(skip);
        assert_f64_eq(compared, 50.0);

        event.streamer.settings.bet.filter_condition = Some(FilterCondition {
            by: OutcomeKey::DecisionUsers,
            condition: Condition::Gte,
            value: Some(50.0),
        });
        let (skip, _, reason) = event.should_skip_by_filter();
        assert!(!skip, "{reason}");
    }

    #[test]
    fn decide_respects_limits() {
        let mut event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::new(),
            title: String::new(),
            status: String::from("ACTIVE"),
            created_at: datetime!(2026-03-27 06:00 UTC),
            window_seconds: 10.0,
            outcomes: vec![
                PredictionOutcome {
                    id: Arc::from("a"),
                    total_users: 10,
                    total_points: 100,
                    odds: 2.0,
                    odds_percentage: 50.0,
                    ..PredictionOutcome::default()
                },
                PredictionOutcome {
                    id: Arc::from("b"),
                    total_users: 20,
                    total_points: 50,
                    odds: 4.0,
                    odds_percentage: 25.0,
                    ..PredictionOutcome::default()
                },
            ],
            decision: PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };
        event.streamer.settings.bet.strategy = Strategy::HighOdds;
        event.streamer.settings.bet.percentage = Some(5);
        event.streamer.settings.bet.max_points = Some(500);

        let decision = event.decide(20_000);
        assert_eq!(decision.outcome_id.as_ref(), "b");
        assert_eq!(decision.amount, 500);
        assert!(Arc::ptr_eq(&decision.outcome_id, &event.outcomes[1].id));
        assert!(Arc::ptr_eq(
            &decision.outcome_id,
            &event.decision.outcome_id
        ));

        let wire = serde_json::to_value(&decision).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"choice": 1, "outcome_id": "b", "amount": 500})
        );
        let decoded: PredictionDecision = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded, decision);
    }

    #[test]
    fn decide_applies_every_bounded_stealth_offset() {
        let event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::new(),
            title: String::new(),
            status: String::from("ACTIVE"),
            created_at: datetime!(2026-03-27 06:00 UTC),
            window_seconds: 10.0,
            outcomes: vec![
                PredictionOutcome {
                    id: Arc::from("a"),
                    total_users: 10,
                    total_points: 100,
                    top_points: 150,
                    odds: 2.0,
                    odds_percentage: 50.0,
                    ..PredictionOutcome::default()
                },
                PredictionOutcome {
                    id: Arc::from("b"),
                    total_users: 20,
                    total_points: 50,
                    top_points: 80,
                    odds: 4.0,
                    odds_percentage: 25.0,
                    ..PredictionOutcome::default()
                },
            ],
            decision: PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };
        let mut event = event;
        event.streamer.settings.bet.strategy = Strategy::HighOdds;
        event.streamer.settings.bet.percentage = Some(5);
        event.streamer.settings.bet.stealth_mode = Some(true);

        for offset in MIN_STEALTH_OFFSET..=MAX_STEALTH_OFFSET {
            let mut event = event.clone();
            let decision = event.decide_with_stealth_offset(2_000, offset);
            assert_eq!(decision.outcome_id.as_ref(), "b");
            assert_eq!(decision.amount, 80 - i64::from(offset));
        }
    }

    #[test]
    fn stealth_variation_preserves_top_minimum_balance_and_maximum_bounds() {
        let mut event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::new(),
            title: String::new(),
            status: String::from("ACTIVE"),
            created_at: datetime!(2026-03-27 06:00 UTC),
            window_seconds: 10.0,
            outcomes: vec![PredictionOutcome {
                id: Arc::from("a"),
                total_users: 10,
                top_points: 1,
                ..PredictionOutcome::default()
            }],
            decision: PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        };
        event.streamer.settings.bet.strategy = Strategy::MostVoted;
        event.streamer.settings.bet.percentage = Some(100);
        event.streamer.settings.bet.max_points = None;
        event.streamer.settings.bet.stealth_mode = Some(true);

        assert_eq!(event.decide_with_stealth_offset(100, 5).amount, 100);

        event.outcomes[0].top_points = 10;
        assert_eq!(event.decide_with_stealth_offset(100, 5).amount, 10);

        event.outcomes[0].top_points = 80;
        event.streamer.settings.bet.max_points = Some(70);
        assert_eq!(event.decide_with_stealth_offset(100, 5).amount, 70);

        event.streamer.settings.bet.max_points = None;
        assert_eq!(event.decide_with_stealth_offset(50, 5).amount, 50);

        event.outcomes[0].top_points = 12;
        assert_eq!(event.decide_with_stealth_offset(100, 3).amount, 10);

        event.outcomes[0].top_points = 80;
        assert_eq!(event.decide_with_stealth_offset(5, 5).amount, 5);

        event.streamer.settings.bet.max_points = Some(5);
        assert_eq!(event.decide_with_stealth_offset(100, 5).amount, 5);

        event.streamer.settings.bet.max_points = Some(8);
        assert_eq!(event.decide_with_stealth_offset(5, 5).amount, 5);
    }

    #[test]
    fn parse_result_matches_go_shape() {
        let mut event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::new(),
            title: String::new(),
            status: String::from("RESOLVED"),
            created_at: datetime!(2026-03-27 06:00 UTC),
            window_seconds: 10.0,
            outcomes: vec![PredictionOutcome {
                id: Arc::from("a"),
                title: String::from("Alpha"),
                color: String::from("blue"),
                ..PredictionOutcome::default()
            }],
            decision: PredictionDecision {
                choice: Some(0),
                outcome_id: Arc::from("a"),
                amount: 125,
            },
            bet_placed: true,
            bet_confirmed: true,
            result_type: String::new(),
            result_string: String::new(),
        };

        let settlement = event.parse_result("WIN", 300);
        assert_eq!(settlement.gained, 175);
        assert_eq!(settlement.result_type, "WIN");
        assert_eq!(settlement.result_string, "WIN, Gained: +175");
        assert_eq!(settlement.decision_label, "A: Alpha (BLUE)");
    }

    #[test]
    fn numbered_strategy_falls_back_to_high_odds_when_outcome_is_missing() {
        let outcomes = vec![
            PredictionOutcome {
                id: Arc::from("a"),
                odds: 1.5,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: Arc::from("b"),
                odds: 4.0,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: Arc::from("c"),
                odds: 2.0,
                ..PredictionOutcome::default()
            },
        ];
        let settings = BetSettings {
            strategy: Strategy::Number5,
            ..BetSettings::default()
        };

        assert_eq!(select_outcome(&outcomes, &settings), Some(1));
    }
}
