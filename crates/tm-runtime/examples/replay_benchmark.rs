use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde_json::json;
use tm_domain::{
    MinerEvent, OffsetDateTime, PlaybackType, PredictionChannelKind, PredictionDecision,
    PredictionEvent, PredictionOutcome, Stream, Streamer, StreamerSettings, WatchPriority,
};
use tm_runtime::RuntimeState;

const STREAMER_COUNT: usize = 200;
const EVENTS_PER_STREAMER: usize = 20;

fn timestamp(seconds: u64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn benchmark_state() -> RuntimeState {
    let streamers = (0..STREAMER_COUNT)
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

async fn apply_concurrent_events(
    runtime: &tm_runtime::RuntimeHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_count = STREAMER_COUNT * EVENTS_PER_STREAMER;
    let mut tasks = Vec::with_capacity(event_count);
    for index in 0..event_count {
        let runtime = runtime.clone();
        tasks.push(tokio::spawn(async move {
            let channel_index = index % STREAMER_COUNT;
            let sequence = index / STREAMER_COUNT;
            runtime
                .apply_event(
                    MinerEvent::PointsEarned {
                        channel_id: format!("channel-{channel_index}"),
                        earned: 12,
                        reason: String::from("WATCH"),
                        balance: 1_012_i64.saturating_add(
                            i64::try_from(sequence)
                                .unwrap_or(i64::MAX)
                                .saturating_mul(12),
                        ),
                    },
                    timestamp(10),
                )
                .await
        }));
    }
    for task in tasks {
        task.await??;
    }
    Ok(())
}

async fn replay_presence_storm(
    runtime: &tm_runtime::RuntimeHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    for index in 0..20 {
        for kind in [
            PlaybackType::StreamDown,
            PlaybackType::StreamDown,
            PlaybackType::StreamUp,
            PlaybackType::StreamUp,
        ] {
            runtime
                .apply_event(
                    MinerEvent::Playback {
                        channel_id: format!("channel-{index}"),
                        kind,
                    },
                    timestamp(20),
                )
                .await?;
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
                id: format!("outcome-{index}-a").into(),
                title: String::from("A"),
                total_points: 1_000,
                total_users: 10,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: format!("outcome-{index}-b").into(),
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
) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = runtime.state_snapshot().await?;
    for index in 0..16 {
        let mut event = prediction_fixture(snapshot.streamers[index].clone(), index);
        runtime
            .apply_event(
                MinerEvent::PredictionChannel {
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
                MinerEvent::PredictionChannel {
                    kind: PredictionChannelKind::EventUpdated,
                    winning_outcome_id: Some(event.outcomes[0].id.to_string()),
                    event: Box::new(event),
                },
                timestamp(31),
            )
            .await?;
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
async fn benchmark_report() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let runtime = tm_runtime::spawn_runtime_state(benchmark_state());
    let started = Instant::now();
    apply_concurrent_events(&runtime).await?;
    replay_presence_storm(&runtime).await?;
    replay_predictions(&runtime).await?;
    let snapshot = runtime.state_snapshot().await?;
    let elapsed_micros = started.elapsed().as_micros();
    let metrics = runtime.metrics();
    let campaign_targets = snapshot.campaign_watch_logins(timestamp(40));
    runtime.shutdown(true, timestamp(60)).await?;

    Ok(json!({
        "schema": 1,
        "revision": option_env!("BUILD_REVISION").unwrap_or("development"),
        "host": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "workload": "sanitized-twitch-free-replay-200-streamers",
        "streamers": STREAMER_COUNT,
        "elapsed_micros": elapsed_micros,
        "processed_events": metrics.processed_events,
        "throughput_events_per_second":
            (metrics.processed_events as f64) / (elapsed_micros.max(1) as f64 / 1_000_000.0),
        "campaign_target_count": campaign_targets.len(),
        "campaign_pin_present": campaign_targets.first().is_some_and(|login| login == "streamer-0"),
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string(&benchmark_report().await?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_workload_has_expected_shape() {
        let state = benchmark_state();
        assert_eq!(state.streamers.len(), STREAMER_COUNT);
        assert_eq!(state.streamers[0].channel_id, "channel-0");
        assert!(state.streamers[0].stream.as_ref().unwrap().drops_tags);
    }
}
