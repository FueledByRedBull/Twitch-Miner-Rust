use std::hint::black_box;
use std::time::Instant;

use serde_json::json;
use tm_domain::{
    BetSettings, OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome, Strategy,
    Streamer, StreamerSettings,
};

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iterations = std::env::var("TM_LANGUAGE_BENCHMARK_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(2_000_000)
        .clamp(10_000, 100_000_000);
    let runs = std::env::var("TM_LANGUAGE_BENCHMARK_RUNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5)
        .clamp(3, 25);
    let mut event = benchmark_event();
    for _ in 0..10_000 {
        black_box(event.decide(123_456));
    }

    let mut throughput = Vec::with_capacity(runs);
    let mut checksum = 0_i64;
    for _ in 0..runs {
        let started = Instant::now();
        for _ in 0..iterations {
            checksum = checksum.wrapping_add(black_box(event.decide(123_456).amount));
        }
        throughput.push(f64::from(iterations) / started.elapsed().as_secs_f64());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": 1,
            "implementation": "rust",
            "revision": option_env!("BUILD_REVISION").unwrap_or("development"),
            "workload": "production-prediction-decision",
            "iterations_per_run": iterations,
            "runs": runs,
            "operations_per_second": {
                "median": percentile(&throughput, 50),
                "p95": percentile(&throughput, 95),
            },
            "checksum": checksum,
        }))?
    );
    Ok(())
}
