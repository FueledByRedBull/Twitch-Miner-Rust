use std::collections::HashMap;

use tm_domain::{
    CommunityGoal, IrcMode, OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome,
    Stream, Streamer, StreamerSettings, WatchPriority,
};
use tm_pubsub::{
    parse_message, CommunityGoalKind, PlaybackType, PredictionChannelKind, PredictionUserKind,
    PubSubEvent,
};
use tm_runtime::{RuntimeEffect, RuntimeState};

fn ts(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

#[test]
fn runtime_state_applies_pubsub_sequence_end_to_end() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![
            Streamer {
                username: "alpha".into(),
                channel_id: "100".into(),
                channel_points: 1_000,
                settings: StreamerSettings {
                    irc_mode: IrcMode::Online,
                    follow_raid: true,
                    claim_moments: true,
                    community_goals: true,
                    make_predictions: true,
                    ..StreamerSettings::default()
                },
                stream: Some(Stream::default()),
                ..Streamer::default()
            },
            Streamer {
                username: "beta".into(),
                channel_id: "200".into(),
                channel_points: 900,
                settings: StreamerSettings {
                    irc_mode: IrcMode::Offline,
                    ..StreamerSettings::default()
                },
                stream: Some(Stream::default()),
                ..Streamer::default()
            },
        ],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
    };

    state.capture_initial_points();

    state.apply_pubsub_event(
        &PubSubEvent::Playback {
            channel_id: "100".into(),
            kind: PlaybackType::StreamUp,
        },
        ts(10),
    );
    state.apply_pubsub_event(
        &PubSubEvent::Playback {
            channel_id: "200".into(),
            kind: PlaybackType::StreamUp,
        },
        ts(11),
    );
    assert_eq!(state.desired_chat_logins(), vec!["alpha"]);
    assert!(state.watch_target_logins(ts(20)).is_empty());
    assert_eq!(state.watch_target_logins(ts(50)), vec!["alpha", "beta"]);

    let claim = state.apply_pubsub_event(
        &PubSubEvent::ClaimAvailable {
            channel_id: "100".into(),
            claim_id: "claim-1".into(),
        },
        ts(51),
    );
    assert_eq!(
        claim,
        vec![RuntimeEffect::ClaimBonus {
            channel_id: "100".into(),
            claim_id: "claim-1".into(),
        }]
    );

    state.apply_pubsub_event(
        &PubSubEvent::PointsEarned {
            channel_id: "100".into(),
            earned: 50,
            reason: "WATCH".into(),
            balance: 1_050,
        },
        ts(52),
    );
    assert_eq!(state.streamers[0].channel_points, 1_050);

    let goal_effects = state.apply_pubsub_event(
        &PubSubEvent::CommunityGoal {
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
        ts(53),
    );
    assert_eq!(
        goal_effects,
        vec![RuntimeEffect::ContributeCommunityGoals {
            channel_id: "100".into(),
        }]
    );

    let raid_effects = state.apply_pubsub_event(
        &PubSubEvent::Raid {
            channel_id: "100".into(),
            raid_id: "raid-1".into(),
            target_login: "target".into(),
        },
        ts(54),
    );
    assert_eq!(
        raid_effects,
        vec![RuntimeEffect::JoinRaid {
            channel_id: "100".into(),
            raid_id: "raid-1".into(),
            target_login: "target".into(),
        }]
    );

    let prediction_created = state.apply_pubsub_event(
        &PubSubEvent::PredictionChannel {
            kind: PredictionChannelKind::EventCreated,
            event: Box::new(PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: "event-1".into(),
                title: "Prediction".into(),
                status: "ACTIVE".into(),
                created_at: ts(55),
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
        ts(55),
    );
    assert_eq!(
        prediction_created,
        vec![RuntimeEffect::EvaluatePrediction {
            event_id: "event-1".into(),
        }]
    );

    state.apply_pubsub_event(
        &PubSubEvent::PredictionUser {
            event_id: "event-1".into(),
            kind: PredictionUserKind::PredictionMade,
            result: None,
        },
        ts(56),
    );
    let prediction_result = parse_message(
        r#"{"type":"MESSAGE","data":{"topic":"predictions-user-v1.user","message":"{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"event-1\",\"result\":{\"type\":\"WIN\"}}}}"}}"#,
        &[],
    )
    .unwrap()
    .unwrap();
    let settled = state.apply_pubsub_event(&prediction_result, ts(57));
    assert_eq!(
        settled,
        vec![RuntimeEffect::PredictionSettled {
            event_id: "event-1".into(),
            streamer_username: "alpha".into(),
            title: "Prediction".into(),
            decision_label: String::new(),
            result_type: "WIN".into(),
            result_string: "WIN, Gained: +0".into(),
        }]
    );

    let summary = state.session_summary(false, ts(120));
    assert_eq!(summary.total_points_line, "Total Points gained: +50");
    assert_eq!(summary.streamers.len(), 1);
    assert_eq!(summary.streamers[0].username, "alpha");
}
