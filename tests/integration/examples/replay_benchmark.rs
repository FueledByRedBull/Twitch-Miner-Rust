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

fn percentile_micros(samples: &[Duration], percentile: usize) -> u128 {
    let mut ordered = samples.iter().map(Duration::as_micros).collect::<Vec<_>>();
    ordered.sort_unstable();
    let numerator = ordered.len().saturating_mul(percentile).saturating_add(99);
    let index = numerator
        .checked_div(100)
        .unwrap_or_default()
        .saturating_sub(1)
        .min(ordered.len().saturating_sub(1));
    ordered.get(index).copied().unwrap_or_default()
}

fn latency_summary(samples: &[Duration]) -> Value {
    json!({
        "p50_micros": percentile_micros(samples, 50),
        "p95_micros": percentile_micros(samples, 95),
        "p99_micros": percentile_micros(samples, 99),
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

fn metric_summary(metrics: RuntimeMetricsSnapshot) -> Value {
    let mean_wait = metrics
        .total_command_wait_micros
        .checked_div(metrics.processed_events.max(1))
        .unwrap_or_default();
    json!({
        "processed_events": metrics.processed_events,
        "mean_actor_wait_micros": mean_wait,
        "max_queue_depth": metrics.max_queue_depth,
        "transport_events": metrics.transport_events,
        "mean_transport_latency_micros": metrics
            .total_transport_latency_micros
            .checked_div(metrics.transport_events.max(1))
            .unwrap_or_default(),
    })
}

async fn run_workload(streamer_count: usize) -> Result<Value, tm_runtime::RuntimeError> {
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

    Ok(json!({
        "streamers": streamer_count,
        "latency": latency_summary(&latencies),
        "throughput_commands_per_second": total_commands as f64 / elapsed.as_secs_f64(),
        "recovery_snapshot_micros": recovery_latency.as_micros(),
        "campaign_target_count": campaign_targets.len(),
        "campaign_pin_present": campaign_targets.first().is_some_and(|login| login == "streamer-0"),
        "metrics": metric_summary(metrics),
    }))
}

async fn profile_snapshots(streamer_count: usize) -> Result<Value, tm_runtime::RuntimeError> {
    let runtime = tm_runtime::spawn_runtime_state(benchmark_state(streamer_count));
    let mut samples = Vec::with_capacity(50);
    for _ in 0..50 {
        let started = Instant::now();
        let snapshot = runtime.state_snapshot().await?;
        std::hint::black_box(snapshot);
        samples.push(started.elapsed());
    }
    runtime.shutdown(true, timestamp(60)).await?;
    Ok(json!({
        "streamers": streamer_count,
        "clone_latency": latency_summary(&samples),
    }))
}

async fn benchmark_report() -> Result<Value, tm_runtime::RuntimeError> {
    let repetitions = std::env::var("TM_REPLAY_REPETITIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 100);
    let mut workloads = Vec::new();
    let mut snapshots = Vec::new();
    for _ in 0..repetitions {
        workloads.clear();
        workloads.reserve(WORKLOAD_SIZES.len());
        for streamer_count in WORKLOAD_SIZES {
            workloads.push(run_workload(streamer_count).await?);
        }
        snapshots.clear();
        snapshots.reserve(SNAPSHOT_SIZES.len());
        for streamer_count in SNAPSHOT_SIZES {
            snapshots.push(profile_snapshots(streamer_count).await?);
        }
    }
    Ok(json!({
        "schema": 1,
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
