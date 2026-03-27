use tm_config::ConfigFile;
use tm_integration_tests::{fixture_path, ts};
use tm_pubsub::{PlaybackType, PredictionUserKind, PubSubEvent};
use tm_runtime::{RuntimeEffect, RuntimeState};

#[tokio::test]
async fn fixture_config_runtime_flow_stays_consistent_across_pubsub_events() {
    let config: ConfigFile =
        serde_json::from_slice(&std::fs::read(fixture_path("config.full.json")).unwrap()).unwrap();
    let mut state = RuntimeState::from_config(&config, ts(0));
    assert_eq!(state.streamers.len(), 2);

    state.streamers[0].channel_id = String::from("100");
    state.streamers[1].channel_id = String::from("200");
    state.streamers[0].channel_points = 1_000;
    state.streamers[1].channel_points = 900;
    state.capture_initial_points();

    state.apply_pubsub_event(
        &PubSubEvent::Playback {
            channel_id: String::from("100"),
            kind: PlaybackType::StreamUp,
        },
        ts(10),
    );
    state.apply_pubsub_event(
        &PubSubEvent::PointsEarned {
            channel_id: String::from("100"),
            earned: 50,
            reason: String::from("WATCH"),
            balance: 1_050,
        },
        ts(11),
    );
    let claim_effects = state.apply_pubsub_event(
        &PubSubEvent::ClaimAvailable {
            channel_id: String::from("100"),
            claim_id: String::from("claim-1"),
        },
        ts(12),
    );
    assert_eq!(
        claim_effects,
        vec![RuntimeEffect::ClaimBonus {
            channel_id: String::from("100"),
            claim_id: String::from("claim-1"),
        }]
    );

    state.predictions.insert(
        String::from("event-1"),
        tm_domain::PredictionEvent {
            streamer: state.streamers[0].clone(),
            event_id: String::from("event-1"),
            title: String::from("Prediction"),
            status: String::from("ACTIVE"),
            created_at: ts(13),
            window_seconds: 30.0,
            outcomes: Vec::new(),
            decision: tm_domain::PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        },
    );
    let settled = state.apply_pubsub_event(
        &PubSubEvent::PredictionUser {
            event_id: String::from("event-1"),
            kind: PredictionUserKind::PredictionResult,
            result: Some(serde_json::json!({ "type": "WIN" })),
        },
        ts(14),
    );
    assert_eq!(
        settled,
        vec![RuntimeEffect::PredictionSettled {
            event_id: String::from("event-1"),
            streamer_username: String::from("alice"),
            title: String::from("Prediction"),
            decision_label: String::new(),
            result_type: String::from("WIN"),
            result_string: String::from("WIN, Gained: +0"),
        }]
    );

    let summary = state.session_summary(false, ts(120));
    assert_eq!(summary.total_points_line, "Total Points gained: +50");
    assert_eq!(summary.streamers.len(), 1);
    assert_eq!(summary.streamers[0].username, "alice");
}
