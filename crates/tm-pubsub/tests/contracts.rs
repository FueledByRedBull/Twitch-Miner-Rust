use std::fs;
use std::path::{Path, PathBuf};

use tm_domain::Streamer;
use tm_pubsub::{
    CommunityGoalKind, MinerEvent, PlaybackType, PredictionChannelKind, PredictionUserKind,
};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

fn fixture_json(name: &str) -> String {
    fs::read_to_string(fixture_path(name)).unwrap()
}

fn streamer(channel_id: &str) -> Streamer {
    Streamer {
        username: String::from("alpha"),
        channel_id: channel_id.to_string(),
        ..Streamer::default()
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn pubsub_contract_fixtures_cover_each_topic_family() {
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.claim_available.json"), &[]).unwrap(),
        Some(MinerEvent::ClaimAvailable {
            channel_id: String::from("123"),
            claim_id: String::from("claim-1"),
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.points_earned.json"), &[]).unwrap(),
        Some(MinerEvent::PointsEarned {
            channel_id: String::from("123"),
            earned: 50,
            reason: String::from("WATCH"),
            balance: 1050,
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.playback_stream_up.json"), &[]).unwrap(),
        Some(MinerEvent::Playback {
            channel_id: String::from("123"),
            kind: PlaybackType::StreamUp,
        })
    );
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.playback_viewcount.json"), &[]).unwrap(),
        Some(MinerEvent::Playback {
            channel_id: String::from("123"),
            kind: PlaybackType::Viewcount,
        })
    );
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.playback_stream_down.json"), &[]).unwrap(),
        Some(MinerEvent::Playback {
            channel_id: String::from("123"),
            kind: PlaybackType::StreamDown,
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.raid.json"), &[]).unwrap(),
        Some(MinerEvent::Raid {
            channel_id: String::from("123"),
            raid_id: String::from("raid-1"),
            target_login: String::from("target"),
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.moment.json"), &[]).unwrap(),
        Some(MinerEvent::Moment {
            channel_id: String::from("123"),
            moment_id: String::from("moment-1"),
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.community_goal_created.json"), &[]).unwrap(),
        Some(MinerEvent::CommunityGoal {
            channel_id: String::from("123"),
            kind: CommunityGoalKind::Created,
            goal: Some(tm_domain::CommunityGoal {
                id: String::from("goal-1"),
                title: String::from("Goal"),
                is_in_stock: true,
                points_contributed: 100,
                amount_needed: 500,
                per_stream_user_maximum_contribution: 50,
                status: String::from("ACTIVE"),
            }),
            goal_id: Some(String::from("goal-1")),
        })
    );
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.community_goal_updated.json"), &[]).unwrap(),
        Some(MinerEvent::CommunityGoal {
            channel_id: String::from("123"),
            kind: CommunityGoalKind::Updated,
            goal: Some(tm_domain::CommunityGoal {
                id: String::from("goal-1"),
                title: String::from("Goal"),
                is_in_stock: true,
                points_contributed: 150,
                amount_needed: 500,
                per_stream_user_maximum_contribution: 50,
                status: String::from("ACTIVE"),
            }),
            goal_id: Some(String::from("goal-1")),
        })
    );
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.community_goal_deleted.json"), &[]).unwrap(),
        Some(MinerEvent::CommunityGoal {
            channel_id: String::from("123"),
            kind: CommunityGoalKind::Deleted,
            goal: None,
            goal_id: Some(String::from("goal-1")),
        })
    );

    let prediction_channel = tm_pubsub::parse_message(
        &fixture_json("pubsub.prediction_event_created.json"),
        &[streamer("123")],
    )
    .unwrap()
    .unwrap();
    let MinerEvent::PredictionChannel {
        kind,
        event,
        winning_outcome_id,
    } = prediction_channel
    else {
        panic!("expected prediction channel event");
    };
    assert_eq!(kind, PredictionChannelKind::EventCreated);
    assert_eq!(event.event_id, "event-1");
    assert_eq!(event.outcomes.len(), 2);
    assert_eq!(winning_outcome_id, None);

    let prediction_updated = tm_pubsub::parse_message(
        &fixture_json("pubsub.prediction_event_updated.json"),
        &[streamer("123")],
    )
    .unwrap()
    .unwrap();
    let MinerEvent::PredictionChannel {
        kind,
        event,
        winning_outcome_id,
    } = prediction_updated
    else {
        panic!("expected prediction update event");
    };
    assert_eq!(kind, PredictionChannelKind::EventUpdated);
    assert_eq!(event.status, "RESOLVED");
    assert_eq!(event.outcomes[0].total_users, 10);
    assert_eq!(event.outcomes[0].total_points, 100);
    assert_eq!(event.outcomes[0].top_points, 20);
    assert_eq!(winning_outcome_id.as_deref(), Some("a"));

    let partial_update = r#"{"type":"MESSAGE","data":{"topic":"predictions-channel-v1.123","message":"{\"type\":\"event-updated\",\"data\":{\"event\":{\"id\":\"event-1\",\"status\":\"RESOLVED\",\"winning_outcome_id\":\"a\"}}}"}}"#;
    let MinerEvent::PredictionChannel {
        event,
        winning_outcome_id,
        ..
    } = tm_pubsub::parse_message(partial_update, &[streamer("123")])
        .unwrap()
        .unwrap()
    else {
        panic!("expected partial prediction update");
    };
    assert!(event.outcomes.is_empty());
    assert_eq!(winning_outcome_id.as_deref(), Some("a"));

    let resolved_without_winner = r#"{"type":"MESSAGE","data":{"topic":"predictions-channel-v1.123","message":"{\"type\":\"event-updated\",\"data\":{\"event\":{\"id\":\"event-1\",\"status\":\"RESOLVED\"}}}"}}"#;
    let MinerEvent::PredictionChannel {
        event,
        winning_outcome_id,
        ..
    } = tm_pubsub::parse_message(resolved_without_winner, &[streamer("123")])
        .unwrap()
        .unwrap()
    else {
        panic!("expected winner-less prediction update");
    };
    assert_eq!(event.status, "RESOLVED");
    assert!(event.outcomes.is_empty());
    assert_eq!(winning_outcome_id, None);

    for pending_status in ["RESOLVE_PENDING", "CANCEL_PENDING"] {
        let pending_update = format!(
            r#"{{"type":"MESSAGE","data":{{"topic":"predictions-channel-v1.123","message":"{{\"type\":\"event-updated\",\"data\":{{\"event\":{{\"id\":\"event-1\",\"status\":\"{pending_status}\"}}}}}}"}}}}"#
        );
        let MinerEvent::PredictionChannel { event, .. } =
            tm_pubsub::parse_message(&pending_update, &[streamer("123")])
                .unwrap()
                .unwrap()
        else {
            panic!("expected pending prediction update");
        };
        assert_eq!(event.status, pending_status);
        assert!(event.outcomes.is_empty());
    }

    let canceled_partial_update = r#"{"type":"MESSAGE","data":{"topic":"predictions-channel-v1.123","message":"{\"type\":\"event-updated\",\"data\":{\"event\":{\"id\":\"event-1\",\"status\":\"CANCELED\",\"created_at\":17,\"prediction_window_seconds\":-1,\"outcomes\":[{\"id\":\"a\",\"state\":\"CANCELED\"}]}}}"}}"#;
    let MinerEvent::PredictionChannel { event, .. } =
        tm_pubsub::parse_message(canceled_partial_update, &[streamer("123")])
            .unwrap()
            .unwrap()
    else {
        panic!("expected partial cancellation update");
    };
    assert_eq!(event.status, "CANCELED");
    assert!(event.outcomes.is_empty());

    let incomplete_created = r#"{"type":"MESSAGE","data":{"topic":"predictions-channel-v1.123","message":"{\"type\":\"event-created\",\"data\":{\"event\":{\"id\":\"event-2\",\"title\":\"Fixture\",\"status\":\"ACTIVE\",\"created_at\":\"2026-01-01T00:00:00Z\",\"prediction_window_seconds\":30,\"outcomes\":[{\"id\":\"a\",\"title\":\"Yes\",\"color\":\"blue\"},{\"id\":\"b\",\"title\":\"No\",\"color\":\"pink\"}]}}}"}}"#;
    assert!(matches!(
        tm_pubsub::parse_message(incomplete_created, &[streamer("123")]),
        Err(tm_pubsub::PubSubError::Protocol(
            "prediction outcome users are missing or invalid"
        ))
    ));

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.prediction_made.json"), &[]).unwrap(),
        Some(MinerEvent::PredictionUser {
            event_id: String::from("event-1"),
            kind: PredictionUserKind::PredictionMade,
            result: None,
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.prediction_result.json"), &[]).unwrap(),
        Some(MinerEvent::PredictionUser {
            event_id: String::from("event-1"),
            kind: PredictionUserKind::PredictionResult,
            result: Some(serde_json::json!({ "type": "WIN", "points_won": 150 })),
        })
    );

    let malformed_result = r#"{"type":"MESSAGE","data":{"topic":"predictions-user-v1.user","message":"{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"event-1\",\"result\":{\"type\":\"MYSTERY\"}}}}"}}"#;
    assert!(matches!(
        tm_pubsub::parse_message(malformed_result, &[]),
        Err(tm_pubsub::PubSubError::Protocol(
            "viewer prediction result type is unsupported"
        ))
    ));
}
