#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::formatting::trim_trailing_zeros;
use crate::types::{BetSettings, Condition, OutcomeKey, Strategy, Streamer};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PredictionOutcome {
    pub id: String,
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
    pub outcome_id: String,
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
            .sum();
        let total_points: i64 = self
            .outcomes
            .iter()
            .map(|outcome| outcome.total_points)
            .sum();

        for outcome in &mut self.outcomes {
            outcome.percentage_users = if total_users > 0 {
                outcome.total_users as f64 * 100.0 / total_users as f64
            } else {
                0.0
            };
            outcome.odds = if outcome.total_points > 0 {
                total_points as f64 / outcome.total_points as f64
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
        let elapsed = (now - self.created_at).whole_milliseconds() as f64 / 1_000.0;
        std::time::Duration::from_secs_f64((self.window_seconds - elapsed).max(0.0))
    }

    pub fn decide(&mut self, balance: i64) -> PredictionDecision {
        let Some(choice) = select_outcome(&self.outcomes, &self.streamer.settings.bet) else {
            self.decision = PredictionDecision::default();
            return self.decision.clone();
        };

        let settings = &self.streamer.settings.bet;
        let percentage = f64::from(settings.percentage.unwrap_or(5)) / 100.0;
        let mut amount = ((balance as f64) * percentage) as i64;

        if let Some(max_points) = settings.max_points {
            amount = amount.min(i64::from(max_points));
        }
        amount = amount.min(balance);

        if settings.stealth_mode.unwrap_or(false) {
            let top_points = self.outcomes[choice].top_points;
            if top_points > 0 && amount >= top_points {
                amount = (top_points - 1).max(1);
            }
        }

        if amount < 10 {
            amount = match settings.max_points {
                Some(max_points) if max_points < 10 => i64::from(max_points),
                _ if balance >= 10 => 10,
                _ => amount,
            };
        }

        self.decision = PredictionDecision {
            choice: Some(choice),
            outcome_id: self.outcomes[choice].id.clone(),
            amount,
        };
        self.decision.clone()
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
                .map(|outcome| outcome.total_users as f64)
                .sum(),
            OutcomeKey::TotalPoints => self
                .outcomes
                .iter()
                .map(|outcome| outcome.total_points as f64)
                .sum(),
            OutcomeKey::DecisionUsers => match by_choice(|outcome| outcome.total_users as f64) {
                Ok(value) => value,
                Err(reason) => return (true, 0.0, reason),
            },
            OutcomeKey::DecisionPoints => match by_choice(|outcome| outcome.total_points as f64) {
                Ok(value) => value,
                Err(reason) => return (true, 0.0, reason),
            },
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
            OutcomeKey::TopPoints => match by_choice(|outcome| outcome.top_points as f64) {
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
            .map_or_else(|| self.decision.outcome_id.clone(), ToString::to_string)
    }

    #[must_use]
    pub fn decision_label(&self) -> String {
        let Some(outcome) = self.decision_outcome() else {
            if self.decision.outcome_id.is_empty() {
                return String::new();
            }
            return self.decision.choice.map_or_else(
                || self.decision.outcome_id.clone(),
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
        Strategy::MostVoted => max_index(outcomes, |outcome| outcome.total_users as f64),
        Strategy::HighOdds => max_index(outcomes, |outcome| outcome.odds),
        Strategy::Percentage => max_index(outcomes, |outcome| outcome.odds_percentage),
        Strategy::SmartMoney => max_index(outcomes, |outcome| outcome.top_points as f64),
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
                max_index(outcomes, |outcome| outcome.total_users as f64)
            }
        }
    }
}

fn max_index(
    outcomes: &[PredictionOutcome],
    scorer: impl Fn(&PredictionOutcome) -> f64,
) -> Option<usize> {
    outcomes
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| {
            scorer(left)
                .partial_cmp(&scorer(right))
                .unwrap_or(Ordering::Equal)
        })
        .map(|(index, _)| index)
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
    format!("#{}", choice + 1)
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
                    id: String::from("a"),
                    total_users: 10,
                    total_points: 100,
                    odds: 2.0,
                    odds_percentage: 50.0,
                    ..PredictionOutcome::default()
                },
                PredictionOutcome {
                    id: String::from("b"),
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
        assert_eq!(decision.outcome_id, "b");
        assert_eq!(decision.amount, 500);
    }

    #[test]
    fn decide_applies_stealth_mode_below_top_points() {
        let mut event = PredictionEvent {
            streamer: Streamer::default(),
            event_id: String::new(),
            title: String::new(),
            status: String::from("ACTIVE"),
            created_at: datetime!(2026-03-27 06:00 UTC),
            window_seconds: 10.0,
            outcomes: vec![
                PredictionOutcome {
                    id: String::from("a"),
                    total_users: 10,
                    total_points: 100,
                    top_points: 150,
                    odds: 2.0,
                    odds_percentage: 50.0,
                    ..PredictionOutcome::default()
                },
                PredictionOutcome {
                    id: String::from("b"),
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
        event.streamer.settings.bet.strategy = Strategy::HighOdds;
        event.streamer.settings.bet.percentage = Some(5);
        event.streamer.settings.bet.stealth_mode = Some(true);

        let decision = event.decide(2_000);
        assert_eq!(decision.outcome_id, "b");
        assert_eq!(decision.amount, 79);
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
                id: String::from("a"),
                title: String::from("Alpha"),
                color: String::from("blue"),
                ..PredictionOutcome::default()
            }],
            decision: PredictionDecision {
                choice: Some(0),
                outcome_id: String::from("a"),
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
                id: String::from("a"),
                odds: 1.5,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: String::from("b"),
                odds: 4.0,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: String::from("c"),
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
