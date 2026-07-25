use std::hint::black_box;
use std::time::Instant;

use serde_json::json;
use tm_domain::{
    BetSettings, OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome, Strategy,
    Streamer, StreamerSettings,
};

const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

fn benchmark_event() -> PredictionEvent {
    let mut event = PredictionEvent {
        streamer: Streamer {
            channel_points: 123_456,
            settings: StreamerSettings {
                bet: BetSettings {
                    strategy: Strategy::MostVoted,
                    percentage: Some(7),
                    max_points: None,
                    stealth_mode: Some(false),
                    ..BetSettings::default()
                },
                ..StreamerSettings::default()
            },
            ..Streamer::default()
        },
        event_id: String::from("language-comparison"),
        title: String::from("Sanitized prediction"),
        status: String::from("ACTIVE"),
        created_at: OffsetDateTime::UNIX_EPOCH,
        window_seconds: 120.0,
        outcomes: vec![
            PredictionOutcome {
                id: String::from("alpha"),
                title: String::from("Alpha"),
                total_users: 641,
                total_points: 7_000_000,
                top_points: 25_000,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: String::from("bravo"),
                title: String::from("Bravo"),
                total_users: 455,
                total_points: 4_000_000,
                top_points: 18_000,
                ..PredictionOutcome::default()
            },
        ],
        decision: PredictionDecision::default(),
        bet_placed: false,
        bet_confirmed: false,
        result_type: String::new(),
        result_string: String::new(),
    };
    event.update_outcomes();
    event
}

fn percentile(samples: &[f64], percent: usize) -> f64 {
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    let index = ordered
        .len()
        .saturating_mul(percent)
        .saturating_add(99)
        .checked_div(100)
        .unwrap_or_default()
        .saturating_sub(1)
        .min(ordered.len().saturating_sub(1));
    ordered.get(index).copied().unwrap_or_default()
}

fn benchmark_balance(index: u32) -> i64 {
    123_456_i64 + i64::from(index & 7)
}

fn operation_checksum(decision: &PredictionDecision) -> i64 {
    decision
        .amount
        .wrapping_add(i64::try_from(decision.choice.unwrap_or_default()).unwrap_or_default())
        .wrapping_add(i64::try_from(decision.outcome_id.len()).unwrap_or_default())
}

fn update_semantic_checksum(hash: u64, decision: &PredictionDecision) -> u64 {
    let choice = decision.choice.map_or(u64::MAX, |value| value as u64);
    let bytes = choice
        .to_le_bytes()
        .into_iter()
        .chain(decision.amount.to_le_bytes())
        .chain(decision.outcome_id.bytes());
    bytes.fold(hash, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

fn semantic_sequence_checksum(event: &mut PredictionEvent, iterations: u32) -> String {
    let hash = (0..iterations).fold(FNV_OFFSET, |hash, index| {
        let decision = event.decide(benchmark_balance(index));
        update_semantic_checksum(hash, &decision)
    });
    format!("{hash:016x}")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iterations = std::env::var("TM_LANGUAGE_BENCHMARK_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(2_000_000)
        .clamp(100_000, 100_000_000);
    let runs = std::env::var("TM_LANGUAGE_BENCHMARK_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5)
        .clamp(3, 25);
    let mut event = benchmark_event();
    let mut last_decision = PredictionDecision::default();
    for index in 0..10_000 {
        let decision = event.decide(benchmark_balance(index));
        black_box(&decision);
        last_decision = decision;
    }

    let mut throughput = Vec::with_capacity(runs);
    let mut checksum = 0_i64;
    for _ in 0..runs {
        let started = Instant::now();
        for index in 0..iterations {
            let decision = event.decide(benchmark_balance(index));
            checksum = checksum.wrapping_add(operation_checksum(&decision));
            black_box(&decision);
            last_decision = decision;
        }
        throughput.push(f64::from(iterations) / started.elapsed().as_secs_f64());
    }
    let semantic_checksum = semantic_sequence_checksum(&mut event, iterations);

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": 3,
            "implementation": "rust",
            "revision": option_env!("BUILD_REVISION").unwrap_or("development"),
            "workload": "complete-production-prediction-decision",
            "iterations_per_run": iterations,
            "runs": runs,
            "operations_per_second": {
                "median": percentile(&throughput, 50),
                "p95": percentile(&throughput, 95),
            },
            "checksum": checksum,
            "semantic_checksum": semantic_checksum,
            "decision_output": {
                "choice": last_decision.choice,
                "outcome_id": last_decision.outcome_id,
                "amount": last_decision.amount,
            },
            "measurement": {
                "build_profile": "cargo-release-opt-z-lto",
                "percentage_math": "exact-i128-integer",
                "outcome_id": "owned-string",
                "output_consumption": "complete-decision-black-box",
                "semantic_verification": "all-decisions-separate-pass",
            },
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{update_semantic_checksum, FNV_OFFSET};
    use tm_domain::PredictionDecision;

    #[test]
    fn semantic_sequence_distinguishes_equal_length_intermediate_outcome_ids() {
        let final_decision = PredictionDecision {
            choice: Some(0),
            outcome_id: String::from("final"),
            amount: 100,
        };
        let first_sequence = update_semantic_checksum(
            update_semantic_checksum(
                FNV_OFFSET,
                &PredictionDecision {
                    choice: Some(1),
                    outcome_id: String::from("alpha"),
                    amount: 50,
                },
            ),
            &final_decision,
        );
        let second_sequence = update_semantic_checksum(
            update_semantic_checksum(
                FNV_OFFSET,
                &PredictionDecision {
                    choice: Some(1),
                    outcome_id: String::from("bravo"),
                    amount: 50,
                },
            ),
            &final_decision,
        );

        assert_ne!(first_sequence, second_sequence);
    }
}
