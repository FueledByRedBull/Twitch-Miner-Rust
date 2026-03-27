use tm_domain::{
    CommunityGoal, PredictionDecision, PredictionEvent, PredictionOutcome,
};
use tm_integration_tests::{base_runtime_state, ts};
use tm_pubsub::{CommunityGoalKind, PlaybackType, PredictionChannelKind, PubSubEvent};
use tm_runtime::RuntimeEffect;

#[tokio::test]
async fn spawned_runtime_applies_stream_presence_goal_and_prediction_flow() {
    let mut state = base_runtime_state();
    state.streamers[0].channel_id = String::from("100");
    let runtime = tm_runtime::spawn_runtime_state(state);

    runtime
        .apply_pubsub_event(
            PubSubEvent::Playback {
                channel_id: "100".into(),
                kind: PlaybackType::StreamUp,
            },
            ts(10),
        )
        .await
        .unwrap();

    let snapshot = runtime.state_snapshot().await.unwrap();
    assert_eq!(snapshot.desired_chat_logins(), vec!["alpha"]);

    let goal_effects = runtime
        .apply_pubsub_event(
            PubSubEvent::CommunityGoal {
                channel_id: "100".into(),
                kind: CommunityGoalKind::Updated,
                goal: Some(CommunityGoal {
                    id: "goal-1".into(),
                    title: "Goal".into(),
                    is_in_stock: true,
                    points_contributed: 30,
                    amount_needed: 100,
                    per_stream_user_maximum_contribution: 50,
                    status: "ACTIVE".into(),
                }),
                goal_id: Some("goal-1".into()),
            },
            ts(11),
        )
        .await
        .unwrap();
    assert_eq!(
        goal_effects,
        vec![RuntimeEffect::ContributeCommunityGoals {
            channel_id: "100".into()
        }]
    );

    let prediction_effects = runtime
        .apply_pubsub_event(
            PubSubEvent::PredictionChannel {
                kind: PredictionChannelKind::EventCreated,
                event: Box::new(PredictionEvent {
                    streamer: snapshot.streamers[0].clone(),
                    event_id: "event-1".into(),
                    title: "Prediction".into(),
                    status: "ACTIVE".into(),
                    created_at: ts(12),
                    window_seconds: 30.0,
                    outcomes: vec![
                        PredictionOutcome {
                            id: "a".into(),
                            title: "Yes".into(),
                            color: "blue".into(),
                            total_users: 10,
                            total_points: 100,
                            top_points: 20,
                            percentage_users: 66.66666666666667,
                            odds: 1.5,
                            odds_percentage: 66.66666666666667,
                        },
                        PredictionOutcome {
                            id: "b".into(),
                            title: "No".into(),
                            color: "pink".into(),
                            total_users: 5,
                            total_points: 50,
                            top_points: 10,
                            percentage_users: 33.333333333333336,
                            odds: 3.0,
                            odds_percentage: 33.333333333333336,
                        },
                    ],
                    decision: PredictionDecision::default(),
                    bet_placed: false,
                    bet_confirmed: false,
                    result_type: String::new(),
                    result_string: String::new(),
                }),
                winning_outcome_id: None,
            },
            ts(12),
        )
        .await
        .unwrap();
    assert_eq!(
        prediction_effects,
        vec![RuntimeEffect::EvaluatePrediction {
            event_id: "event-1".into()
        }]
    );
}
