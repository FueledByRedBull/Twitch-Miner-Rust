use tm_integration_tests::{base_runtime_state, fixture_json, ts};
use tm_runtime::{RuntimeEffect, RuntimeState};

#[test]
fn runtime_applies_root_fixture_sequence_end_to_end() {
    let mut state: RuntimeState = base_runtime_state();
    state.capture_initial_points();

    let stream_up = tm_pubsub::parse_message(&fixture_json("pubsub.playback_stream_up.json"), &[])
        .unwrap()
        .unwrap();
    assert!(state.apply_pubsub_event(&stream_up, ts(10)).is_empty());
    assert_eq!(state.desired_chat_logins(), vec!["alpha"]);
    assert_eq!(state.watch_target_logins(ts(41)), vec!["alpha"]);

    let claim = tm_pubsub::parse_message(&fixture_json("pubsub.claim_available.json"), &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        state.apply_pubsub_event(&claim, ts(42)),
        vec![RuntimeEffect::ClaimBonus {
            channel_id: String::from("123"),
            claim_id: String::from("claim-1"),
        }]
    );

    let points = tm_pubsub::parse_message(&fixture_json("pubsub.points_earned.json"), &[])
        .unwrap()
        .unwrap();
    assert!(state.apply_pubsub_event(&points, ts(43)).is_empty());
    assert_eq!(state.streamers[0].channel_points, 1_050);

    let raid = tm_pubsub::parse_message(&fixture_json("pubsub.raid.json"), &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        state.apply_pubsub_event(&raid, ts(44)),
        vec![RuntimeEffect::JoinRaid {
            channel_id: String::from("123"),
            raid_id: String::from("raid-1"),
            target_login: String::from("target"),
        }]
    );

    let moment = tm_pubsub::parse_message(&fixture_json("pubsub.moment.json"), &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        state.apply_pubsub_event(&moment, ts(45)),
        vec![RuntimeEffect::ClaimMoment {
            channel_id: String::from("123"),
            moment_id: String::from("moment-1"),
        }]
    );

    let goal = tm_pubsub::parse_message(&fixture_json("pubsub.community_goal_created.json"), &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        state.apply_pubsub_event(&goal, ts(46)),
        vec![RuntimeEffect::ContributeCommunityGoals {
            channel_id: String::from("123"),
        }]
    );

    let prediction_created = tm_pubsub::parse_message(
        &fixture_json("pubsub.prediction_event_created.json"),
        &state.streamers,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        state.apply_pubsub_event(&prediction_created, ts(47)),
        vec![RuntimeEffect::EvaluatePrediction {
            event_id: String::from("event-1"),
        }]
    );
    assert!(state.predictions.contains_key("event-1"));

    let prediction_result =
        tm_pubsub::parse_message(&fixture_json("pubsub.prediction_result.json"), &[])
            .unwrap()
            .unwrap();
    assert_eq!(
        state.apply_pubsub_event(&prediction_result, ts(48)),
        vec![RuntimeEffect::PredictionSettled {
            event_id: String::from("event-1"),
            streamer_username: String::from("alpha"),
            title: String::from("Will it happen?"),
            decision_label: String::new(),
            result_type: String::from("WIN"),
            result_string: String::from("WIN, Gained: +150"),
        }]
    );

    let summary = state.session_summary(false, ts(90));
    assert_eq!(summary.total_points_line, "Total Points gained: +50");
    assert_eq!(summary.streamers.len(), 1);
    assert_eq!(summary.streamers[0].username, "alpha");
}
