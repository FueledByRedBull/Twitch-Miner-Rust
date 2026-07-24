use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tm_domain::{
    OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome, Stream, Streamer,
    StreamerSettings, WatchPriority,
};
use tm_pubsub::{PlaybackType, PredictionChannelKind, PubSubEvent};
use tm_runtime::{RuntimeMetricsSnapshot, RuntimeState};

const WORKLOAD_SIZES: [usize; 4] = [1, 10, 50, 200];
const SNAPSHOT_SIZES: [usize; 3] = [17, 100, 1_000];

#[derive(Debug)]
struct WorkloadRun {
    latencies: Vec<Duration>,
    throughput_commands_per_second: f64,
    recovery_snapshot_latency: Duration,
    campaign_target_count: usize,
    campaign_pin_present: bool,
    metrics: RuntimeMetricsSnapshot,
}

#[derive(Debug)]
struct SnapshotRun {
    samples: Vec<Duration>,
}

fn timestamp(seconds: u64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn benchmark_state(streamer_count: usize) -> RuntimeState {
    let streamers = (0..streamer_count)
        .map(|index| Streamer {
            username: format!("streamer-{index}"),
            channel_id: format!("channel-{index}"),
            channel_points: 1_000,
            presence_known: true,
            is_online: true,
            settings: StreamerSettings {
                make_predictions: true,
                farm_drops: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                broadcast_id: format!("broadcast-{index}"),
                drops_tags: index == 0,
                drop_campaign_eligible: Some(index == 0),
                stream_up_at: Some(timestamp(1)),
                ..Stream::default()
            }),
            ..Streamer::default()
        })
        .collect();
    RuntimeState {
        started_at: timestamp(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Drops, WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers,
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: VecDeque::new(),
        completed_predictions: VecDeque::new(),
    }
}

fn percentile_index(sample_count: usize, percentile: usize) -> usize {
    let numerator = sample_count.saturating_mul(percentile).saturating_add(99);
    numerator
        .checked_div(100)
        .unwrap_or_default()
        .saturating_sub(1)
        .min(sample_count.saturating_sub(1))
}

fn percentile_micros(samples: &[Duration], percentile: usize) -> u128 {
    let mut ordered = samples.iter().map(Duration::as_micros).collect::<Vec<_>>();
    ordered.sort_unstable();
    let index = percentile_index(ordered.len(), percentile);
    ordered.get(index).copied().unwrap_or_default()
}

fn latency_summary(samples: &[Duration]) -> Value {
    json!({
        "p50_micros": percentile_micros(samples, 50),
        "p95_micros": percentile_micros(samples, 95),
        "p99_micros": percentile_micros(samples, 99),
    })
}

fn numeric_summary(samples: &[f64]) -> Value {
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    let percentile = |value| {
        ordered
            .get(percentile_index(ordered.len(), value))
            .copied()
            .unwrap_or_default()
    };
    json!({
        "p50": percentile(50),
        "p95": percentile(95),
        "p99": percentile(99),
    })
}

async fn apply_queue_pressure(
    runtime: &tm_runtime::RuntimeHandle,
    streamer_count: usize,
) -> Result<Vec<Duration>, tm_runtime::RuntimeError> {
    let event_count = streamer_count.saturating_mul(20).max(200);
    let mut tasks = Vec::with_capacity(event_count);
    for index in 0..event_count {
        let runtime = runtime.clone();
        tasks.push(tokio::spawn(async move {
            let channel_index = index % streamer_count;
            let sequence = index / streamer_count;
            let event = PubSubEvent::PointsEarned {
                channel_id: format!("channel-{channel_index}"),
                earned: 12,
                reason: String::from("WATCH"),
                balance: 1_012_i64.saturating_add(
                    i64::try_from(sequence)
                        .unwrap_or(i64::MAX)
                        .saturating_mul(12),
                ),
            };
            let started = Instant::now();
            runtime.apply_event(event, timestamp(10)).await?;
            Ok::<Duration, tm_runtime::RuntimeError>(started.elapsed())
        }));
    }
    let mut latencies = Vec::with_capacity(tasks.len());
    for task in tasks {
        match task.await {
            Ok(result) => latencies.push(result?),
            Err(_) => {
                return Err(tm_runtime::RuntimeError::ActorClosed {
                    command: "ReplayQueueTask",
                });
            }
        }
    }
    Ok(latencies)
}

async fn replay_presence_storm(
    runtime: &tm_runtime::RuntimeHandle,
    streamer_count: usize,
    latencies: &mut Vec<Duration>,
) -> Result<(), tm_runtime::RuntimeError> {
    for index in 0..streamer_count.min(20) {
        for kind in [
            PlaybackType::StreamDown,
            PlaybackType::StreamDown,
            PlaybackType::StreamUp,
            PlaybackType::StreamUp,
        ] {
            let started = Instant::now();
            runtime
                .apply_event(
                    PubSubEvent::Playback {
                        channel_id: format!("channel-{index}"),
                        kind,
                    },
                    timestamp(20),
                )
                .await?;
            latencies.push(started.elapsed());
        }
    }
    Ok(())
}

fn prediction_fixture(streamer: Streamer, index: usize) -> PredictionEvent {
    PredictionEvent {
        streamer,
        event_id: format!("prediction-{index}"),
        title: String::from("Sanitized replay prediction"),
        status: String::from("ACTIVE"),
        created_at: timestamp(30),
        window_seconds: 30.0,
        outcomes: vec![
            PredictionOutcome {
                id: format!("outcome-{index}-a"),
                title: String::from("A"),
                total_points: 1_000,
                total_users: 10,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: format!("outcome-{index}-b"),
                title: String::from("B"),
                total_points: 500,
                total_users: 5,
                ..PredictionOutcome::default()
            },
        ],
        decision: PredictionDecision::default(),
        bet_placed: false,
        bet_confirmed: false,
        result_type: String::new(),
        result_string: String::new(),
    }
}

async fn replay_predictions(
    runtime: &tm_runtime::RuntimeHandle,
    streamer_count: usize,
    latencies: &mut Vec<Duration>,
) -> Result<(), tm_runtime::RuntimeError> {
    let snapshot = runtime.state_snapshot().await?;
    for index in 0..streamer_count.min(16) {
        let mut event = prediction_fixture(snapshot.streamers[index].clone(), index);
        let started = Instant::now();
        runtime
            .apply_event(
                PubSubEvent::PredictionChannel {
                    kind: PredictionChannelKind::EventCreated,
                    event: Box::new(event.clone()),
                    winning_outcome_id: None,
                },
                timestamp(30),
            )
            .await?;
        runtime
            .record_prediction_placed(
                event.event_id.clone(),
                PredictionDecision {
                    choice: Some(0),
                    outcome_id: event.outcomes[0].id.clone(),
                    amount: 10,
                },
                true,
            )
            .await?;
        event.status = String::from("RESOLVED");
        runtime
            .apply_event(
                PubSubEvent::PredictionChannel {
                    kind: PredictionChannelKind::EventUpdated,
                    winning_outcome_id: Some(event.outcomes[0].id.clone()),
                    event: Box::new(event),
                },
                timestamp(31),
            )
            .await?;
        latencies.push(started.elapsed());
    }
    Ok(())
}

fn metric_summary(runs: &[RuntimeMetricsSnapshot]) -> Value {
    let totals = runs
        .iter()
        .fold(RuntimeMetricsSnapshot::default(), |mut total, metrics| {
            total.processed_events = total
                .processed_events
                .saturating_add(metrics.processed_events);
            total.total_command_wait_micros = total
                .total_command_wait_micros
                .saturating_add(metrics.total_command_wait_micros);
            total.max_queue_depth = total.max_queue_depth.max(metrics.max_queue_depth);
            total.transport_events = total
                .transport_events
                .saturating_add(metrics.transport_events);
            total.total_transport_latency_micros = total
                .total_transport_latency_micros
                .saturating_add(metrics.total_transport_latency_micros);
            total
        });
    let mean_wait = totals
        .total_command_wait_micros
        .checked_div(totals.processed_events.max(1))
        .unwrap_or_default();
    json!({
        "processed_events": totals.processed_events,
        "mean_actor_wait_micros": mean_wait,
        "max_queue_depth": totals.max_queue_depth,
        "transport_events": totals.transport_events,
        "mean_transport_latency_micros": totals
            .total_transport_latency_micros
            .checked_div(totals.transport_events.max(1))
            .unwrap_or_default(),
    })
}

async fn run_workload(streamer_count: usize) -> Result<WorkloadRun, tm_runtime::RuntimeError> {
    let runtime = tm_runtime::spawn_runtime_state(benchmark_state(streamer_count));
    let started = Instant::now();
    let mut latencies = apply_queue_pressure(&runtime, streamer_count).await?;
    replay_presence_storm(&runtime, streamer_count, &mut latencies).await?;
    replay_predictions(&runtime, streamer_count, &mut latencies).await?;
    let recovery_started = Instant::now();
    let snapshot = runtime.state_snapshot().await?;
    let recovery_latency = recovery_started.elapsed();
    let elapsed = started.elapsed();
    let metrics = runtime.metrics();
    let campaign_targets = snapshot.campaign_watch_logins(timestamp(40));
    let total_commands = metrics.processed_events;
    runtime.shutdown(true, timestamp(60)).await?;

    Ok(WorkloadRun {
        latencies,
        throughput_commands_per_second: total_commands as f64 / elapsed.as_secs_f64(),
        recovery_snapshot_latency: recovery_latency,
        campaign_target_count: campaign_targets.len(),
        campaign_pin_present: campaign_targets
            .first()
            .is_some_and(|login| login == "streamer-0"),
        metrics,
    })
}

fn workload_summary(streamer_count: usize, runs: &[WorkloadRun]) -> Value {
    let latencies = runs
        .iter()
        .flat_map(|run| run.latencies.iter().copied())
        .collect::<Vec<_>>();
    let throughputs = runs
        .iter()
        .map(|run| run.throughput_commands_per_second)
        .collect::<Vec<_>>();
    let recovery_latencies = runs
        .iter()
        .map(|run| run.recovery_snapshot_latency)
        .collect::<Vec<_>>();
    let metrics = runs.iter().map(|run| run.metrics).collect::<Vec<_>>();
    let campaign_target_count = runs.first().map_or(0, |run| run.campaign_target_count);

    json!({
        "streamers": streamer_count,
        "run_count": runs.len(),
        "latency_sample_count": latencies.len(),
        "latency": latency_summary(&latencies),
        "throughput_commands_per_second": numeric_summary(&throughputs),
        "recovery_snapshot_latency": latency_summary(&recovery_latencies),
        "campaign_target_count": campaign_target_count,
        "campaign_target_count_consistent": runs
            .iter()
            .all(|run| run.campaign_target_count == campaign_target_count),
        "campaign_pin_present": runs.iter().all(|run| run.campaign_pin_present),
        "metrics": metric_summary(&metrics),
    })
}

async fn profile_snapshots(streamer_count: usize) -> Result<SnapshotRun, tm_runtime::RuntimeError> {
    let runtime = tm_runtime::spawn_runtime_state(benchmark_state(streamer_count));
    let mut samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let started = Instant::now();
        let snapshot = runtime.state_snapshot().await?;
        std::hint::black_box(snapshot);
        samples.push(started.elapsed());
    }
    runtime.shutdown(true, timestamp(60)).await?;
    Ok(SnapshotRun { samples })
}

fn snapshot_summary(streamer_count: usize, runs: &[SnapshotRun]) -> Value {
    let samples = runs
        .iter()
        .flat_map(|run| run.samples.iter().copied())
        .collect::<Vec<_>>();
    json!({
        "streamers": streamer_count,
        "run_count": runs.len(),
        "sample_count": samples.len(),
        "clone_latency": latency_summary(&samples),
    })
}

async fn benchmark_report() -> Result<Value, tm_runtime::RuntimeError> {
    let repetitions = std::env::var("TM_REPLAY_REPETITIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 100);
    let mut workload_runs = WORKLOAD_SIZES.map(|_| Vec::with_capacity(repetitions));
    let mut snapshot_runs = SNAPSHOT_SIZES.map(|_| Vec::with_capacity(repetitions));
    for _ in 0..repetitions {
        for (index, streamer_count) in WORKLOAD_SIZES.into_iter().enumerate() {
            workload_runs[index].push(run_workload(streamer_count).await?);
        }
        for (index, streamer_count) in SNAPSHOT_SIZES.into_iter().enumerate() {
            snapshot_runs[index].push(profile_snapshots(streamer_count).await?);
        }
    }
    let workloads = WORKLOAD_SIZES
        .into_iter()
        .zip(workload_runs.iter())
        .map(|(streamer_count, runs)| workload_summary(streamer_count, runs))
        .collect::<Vec<_>>();
    let snapshots = SNAPSHOT_SIZES
        .into_iter()
        .zip(snapshot_runs.iter())
        .map(|(streamer_count, runs)| snapshot_summary(streamer_count, runs))
        .collect::<Vec<_>>();
    Ok(json!({
        "schema": 2,
        "revision": option_env!("BUILD_REVISION").unwrap_or("development"),
        "workload": "sanitized-twitch-free-replay",
        "repetitions": repetitions,
        "workloads": workloads,
        "snapshot_profiles": snapshots,
        "allocation_measurement": "not instrumented; no unsafe allocator shim or benchmark-only allocator dependency",
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "{}",
        serde_json::to_string_pretty(&benchmark_report().await?)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload_run(latencies: &[u64], throughput: f64) -> WorkloadRun {
        WorkloadRun {
            latencies: latencies
                .iter()
                .copied()
                .map(Duration::from_micros)
                .collect(),
            throughput_commands_per_second: throughput,
            recovery_snapshot_latency: Duration::from_micros(10),
            campaign_target_count: 1,
            campaign_pin_present: true,
            metrics: RuntimeMetricsSnapshot {
                processed_events: 2,
                total_command_wait_micros: 4,
                max_queue_depth: 64,
                transport_events: 1,
                total_transport_latency_micros: 3,
            },
        }
    }

    #[test]
    fn workload_summary_aggregates_every_repetition() {
        let report = workload_summary(
            10,
            &[
                workload_run(&[10, 20], 100.0),
                workload_run(&[30, 40], 200.0),
            ],
        );

        assert_eq!(report["run_count"], 2);
        assert_eq!(report["latency_sample_count"], 4);
        assert_eq!(report["latency"]["p95_micros"], 40);
        assert_eq!(report["throughput_commands_per_second"]["p50"], 100.0);
        assert_eq!(report["throughput_commands_per_second"]["p95"], 200.0);
        assert_eq!(report["metrics"]["processed_events"], 4);
    }

    #[test]
    fn snapshot_summary_aggregates_every_repetition() {
        let report = snapshot_summary(
            1_000,
            &[
                SnapshotRun {
                    samples: vec![Duration::from_micros(10), Duration::from_micros(20)],
                },
                SnapshotRun {
                    samples: vec![Duration::from_micros(30), Duration::from_micros(40)],
                },
            ],
        );

        assert_eq!(report["run_count"], 2);
        assert_eq!(report["sample_count"], 4);
        assert_eq!(report["clone_latency"]["p99_micros"], 40);
    }
}
