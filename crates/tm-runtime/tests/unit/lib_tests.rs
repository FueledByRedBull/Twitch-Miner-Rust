use super::*;
use std::collections::HashMap;
use tm_config::ConfigFile;
use tm_domain::{
    parse_watch_priorities, should_prioritize_streak, CommunityGoal, CommunityGoalKind,
    HistoryEntry, IrcMode, MinerEvent, OffsetDateTime, PlaybackType, PredictionChannelKind,
    PredictionDecision, PredictionEvent, PredictionOutcome, Stream, Streamer, StreamerSettings,
    WatchPriority,
};
use tm_pubsub::parse_message;

fn assert_f64_eq(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < f64::EPSILON);
}

fn ts(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

#[test]
fn pubsub_gain_supports_prediction_stake_deduction() {
    let mut streamer = Streamer {
        username: "tester".into(),
        channel_points: 1_000_000,
        points_init: true,
        ..Streamer::default()
    };

    let delta = apply_pubsub_gain(&mut streamer, -250_000, "PREDICTION", 0);
    assert_eq!(delta, -250_000);
    assert_eq!(streamer.channel_points, 750_000);

    let delta = apply_pubsub_gain(&mut streamer, 256_827, "PREDICTION", 0);
    assert_eq!(delta, 256_827);
    assert_eq!(streamer.channel_points, 1_006_827);

    let entry = streamer.history.get("PREDICTION").unwrap();
    assert_eq!(entry.amount, 6_827);
    assert_eq!(entry.count, 2);
}

#[test]
fn positive_pubsub_gains_do_not_move_balance_backward() {
    let mut streamer = Streamer {
        username: "tester".into(),
        channel_points: 1_000,
        points_init: true,
        ..Streamer::default()
    };

    let delta = apply_pubsub_gain(&mut streamer, 10, "WATCH", 900);
    assert_eq!(delta, 10);
    assert_eq!(streamer.channel_points, 1_010);
}

#[test]
fn point_gain_properties_hold_across_extreme_values() {
    let starting_balances = [0, 1, 1_000, i64::MAX - 1, i64::MAX];
    let gains = [i64::MIN, -1_000, -1, 0, 1, 1_000, i64::MAX];

    for starting_balance in starting_balances {
        for earned in gains {
            let mut streamer = Streamer {
                username: String::from("tester"),
                channel_points: starting_balance,
                points_init: true,
                ..Streamer::default()
            };

            apply_pubsub_gain(&mut streamer, earned, "WATCH", 0);

            assert!(
                streamer.channel_points >= 0,
                "balance became negative for start={starting_balance}, earned={earned}"
            );
            if earned >= 0 {
                assert!(
                    streamer.channel_points >= starting_balance,
                    "positive gain moved backward for start={starting_balance}, earned={earned}"
                );
            }
            assert_eq!(streamer.history["WATCH"].count, 1);
            assert_eq!(streamer.history["WATCH"].amount, earned);
        }
    }
}

#[test]
fn point_history_saturates_instead_of_panicking_on_external_extremes() {
    let mut streamer = Streamer {
        history: HashMap::from([(
            String::from("WATCH"),
            HistoryEntry {
                count: u32::MAX,
                amount: i64::MAX,
            },
        )]),
        ..Streamer::default()
    };

    update_history(&mut streamer, "WATCH", i64::MAX);

    assert_eq!(streamer.history["WATCH"].count, u32::MAX);
    assert_eq!(streamer.history["WATCH"].amount, i64::MAX);
}

#[test]
fn zero_earned_pubsub_message_can_adopt_absolute_balance() {
    let mut streamer = Streamer {
        username: "tester".into(),
        channel_points: 1_000,
        points_init: true,
        ..Streamer::default()
    };

    let delta = apply_pubsub_gain(&mut streamer, 0, "WATCH", 1_200);
    assert_eq!(delta, 200);
    assert_eq!(streamer.channel_points, 1_200);
}

#[test]
fn watch_streak_history_clears_missing_state() {
    let mut streamer = Streamer {
        stream: Some(Stream {
            watch_streak_missing: true,
            ..Stream::default()
        }),
        settings: StreamerSettings::default(),
        ..Streamer::default()
    };

    update_history(&mut streamer, "WATCH_STREAK", 50);
    assert!(!streamer.stream.as_ref().unwrap().watch_streak_missing);
}

#[test]
fn watch_streak_event_records_resolution_time_for_warm_restart() {
    let config = ConfigFile {
        streamers: vec![String::from("tester")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].stream = Some(Stream::default());
    let event = MinerEvent::PointsEarned {
        channel_id: String::from("100"),
        earned: 50,
        reason: String::from("WATCH_STREAK"),
        balance: 50,
    };

    assert!(state.apply_event_with_outcome(&event, ts(42)).changed);
    let stream = state.streamers[0].stream.as_ref().unwrap();
    assert!(!stream.watch_streak_missing);
    assert_eq!(stream.watch_streak_resolved_at, Some(ts(42)));
}

#[test]
fn session_summary_hides_points_in_privacy_mode() {
    let streamer = Streamer {
        username: "tester".into(),
        channel_points: 2_000,
        history: std::collections::HashMap::from([(
            "WATCH".into(),
            HistoryEntry {
                count: 2,
                amount: 100,
            },
        )]),
        ..Streamer::default()
    };
    let completed = std::collections::VecDeque::from([PredictionEvent {
        streamer: streamer.clone(),
        event_id: String::from("private-event-id"),
        title: String::from("Private prediction title"),
        status: String::from("RESOLVED"),
        created_at: ts(1),
        window_seconds: 30.0,
        outcomes: vec![PredictionOutcome {
            id: "private-outcome-id".into(),
            title: String::from("Private outcome"),
            color: String::from("blue"),
            ..PredictionOutcome::default()
        }],
        decision: PredictionDecision {
            choice: Some(0),
            outcome_id: "private-outcome-id".into(),
            amount: 100,
        },
        bet_placed: true,
        bet_confirmed: true,
        result_type: String::from("WIN"),
        result_string: String::from("WIN, Gained: +100"),
    }]);

    let summary = build_session_summary(
        &[streamer],
        &[("tester", 1_500)],
        &completed,
        true,
        std::time::Duration::from_secs(45),
    );

    assert_eq!(summary.duration, "00:00:45.000000");
    assert_eq!(summary.total_points_line, "Total Points gained: [hidden]");
    assert_eq!(summary.streamers[0].current_points, "[hidden]");
    assert_eq!(summary.streamers[0].username, "streamer-1");
    assert_eq!(summary.streamers[0].channel_id, "[hidden]");
    assert_eq!(
        summary.streamers[0].total_points_line,
        "Total points gained (after farming - before farming): [hidden]"
    );
    assert_eq!(
        summary.streamers[0].history_lines[0],
        "WATCH (2 times, [hidden])"
    );
    let prediction = &summary.predictions[0];
    let rendered = format!("{prediction:?}");
    for private in [
        "private-event-id",
        "Private prediction title",
        "private-outcome-id",
        "Private outcome",
        "tester",
    ] {
        assert!(!rendered.contains(private));
    }
    assert!(prediction.bet_line.contains("TotalUsers=[hidden]"));
    assert!(prediction.bet_line.contains("'choice': '[hidden]'"));
    assert_eq!(prediction.outcome_lines, vec!["Outcome0([hidden])"]);
}

#[test]
fn runtime_state_builds_from_config_with_overrides() {
    let config = ConfigFile {
        streamers: vec!["StreamerOne".into(), "streamertwo".into(), "ignored".into()],
        streamers_exclude: vec!["ignored".into()],
        watch_priority: vec!["POINTS_ASC".into(), "DROPS".into()],
        game_priority: vec!["Valorant".into()],
        streamer_overrides: HashMap::from([(
            "streamertwo".into(),
            tm_config::StreamerSettingsOverride {
                claim_drops: Some(false),
                chat_presence: Some("OFFLINE".into()),
                ..tm_config::StreamerSettingsOverride::default()
            },
        )]),
        ..ConfigFile::default()
    };

    let state = RuntimeState::from_config(&config, ts(1000));
    assert!(!state.follower_mode);
    assert_eq!(state.streamers.len(), 2);
    assert_eq!(state.streamers[0].username, "streamerone");
    assert_eq!(state.streamers[1].username, "streamertwo");
    assert_eq!(
        state.watch_priorities,
        parse_watch_priorities(&config.watch_priority)
    );
    assert_eq!(state.game_priority, vec!["valorant"]);
    assert_eq!(state.streamers[1].settings.irc_mode, IrcMode::Offline);
    assert!(state.streamers[1].settings.farm_drops);
    assert!(!state.streamers[1].settings.claim_drops);
}

#[test]
fn playback_presence_drives_watch_and_chat_targets() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_id: "123".into(),
            settings: StreamerSettings {
                irc_mode: IrcMode::Online,
                ..StreamerSettings::default()
            },
            stream: Some(Stream::default()),
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    state.apply_event(
        &MinerEvent::Playback {
            channel_id: "123".into(),
            kind: PlaybackType::StreamUp,
        },
        ts(100),
    );
    assert_eq!(state.desired_chat_logins(), vec!["tester"]);
    assert_eq!(state.watch_target_logins(ts(100)), vec!["tester"]);

    state.apply_event(
        &MinerEvent::Playback {
            channel_id: "123".into(),
            kind: PlaybackType::StreamDown,
        },
        ts(200),
    );
    assert!(state.desired_chat_logins().is_empty());
    assert!(!state.streamers[0].is_online);
    assert_eq!(state.streamers[0].offline_at, Some(ts(200)));
    assert_f64_eq(
        state.streamers[0].stream.as_ref().unwrap().minute_watched,
        0.0,
    );
}

#[test]
fn viewcount_playback_does_not_promote_presence() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_id: "123".into(),
            settings: StreamerSettings {
                irc_mode: IrcMode::Online,
                ..StreamerSettings::default()
            },
            stream: Some(Stream::default()),
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    state.apply_event(
        &MinerEvent::Playback {
            channel_id: "123".into(),
            kind: PlaybackType::Viewcount,
        },
        ts(100),
    );

    assert!(!state.streamers[0].presence_known);
    assert!(!state.streamers[0].is_online);
    assert!(state.desired_chat_logins().is_empty());
    assert!(state.watch_target_logins(ts(131)).is_empty());
}

#[test]
fn stream_rollover_resets_watch_progress_and_marks_streak_missing() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_id: "123".into(),
            settings: StreamerSettings {
                watch_streak: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                broadcast_id: "old-broadcast".into(),
                title: "Old".into(),
                minute_watched: 17.5,
                last_minute_update: Some(ts(90)),
                watch_streak_missing: false,
                stream_up_at: Some(ts(10)),
                ..Stream::default()
            }),
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    let updated = state
        .apply_stream_update(
            &StreamUpdate {
                channel_id: "123".into(),
                id: "new-broadcast".into(),
                title: "New".into(),
                game_name: "Game".into(),
                game_id: Some("game-1".into()),
                tags: vec!["tag-1".into()],
                viewers_count: 42,
            },
            ts(120),
        )
        .unwrap();

    assert_eq!(updated, state.streamers[0]);

    let stream = state.streamers[0].stream.as_ref().unwrap();
    assert_eq!(stream.broadcast_id, "new-broadcast");
    assert_f64_eq(stream.minute_watched, 0.0);
    assert!(stream.last_minute_update.is_none());
    assert!(stream.watch_streak_missing);
    assert_eq!(stream.stream_up_at, Some(ts(120)));
    assert!(should_prioritize_streak(&state.streamers[0], ts(120)));
}

#[test]
fn short_restart_chains_preserve_resolved_streak_state() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Streak],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_id: "123".into(),
            is_online: true,
            presence_known: true,
            settings: StreamerSettings {
                watch_streak: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                broadcast_id: "segment-a".into(),
                watch_streak_missing: false,
                ..Stream::default()
            }),
            ..Streamer::default()
        }],
        initial_points: HashMap::from([(String::from("tester"), 10)]),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    for (offline_at, online_at, broadcast_id) in [(100, 110, "segment-b"), (200, 210, "segment-c")]
    {
        assert!(state.apply_presence("123", false, ts(offline_at)));
        assert!(state.apply_presence("123", true, ts(online_at)));
        assert_eq!(
            state.streamers[0].last_stream_ended_at,
            Some(ts(offline_at))
        );
        let _ = state.apply_stream_update(
            &StreamUpdate {
                channel_id: "123".into(),
                id: broadcast_id.into(),
                title: String::new(),
                game_name: String::new(),
                game_id: None,
                viewers_count: 0,
                tags: Vec::new(),
            },
            ts(online_at + 1),
        );
        assert!(
            !state.streamers[0]
                .stream
                .as_ref()
                .unwrap()
                .watch_streak_missing
        );
        assert!(!should_prioritize_streak(
            &state.streamers[0],
            ts(online_at + 1),
        ));
    }

    assert!(state.apply_presence("123", false, ts(300)));
    assert!(state.apply_presence("123", true, ts(2_101)));
    assert!(
        state.streamers[0]
            .stream
            .as_ref()
            .unwrap()
            .watch_streak_missing
    );
    assert!(should_prioritize_streak(&state.streamers[0], ts(2_101),));
}

#[test]
fn runtime_login_refresh_preserves_initial_balance_and_releases_suspension() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "old-login".into(),
            channel_id: "123".into(),
            watch_suspended_until: Some(ts(500)),
            ..Streamer::default()
        }],
        initial_points: HashMap::from([(String::from("old-login"), 99)]),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    assert!(state.update_streamer_login("123", " New-Login "));
    assert_eq!(state.streamers[0].username, "new-login");
    assert!(state.streamers[0].watch_suspended_until.is_none());
    assert_eq!(state.initial_points.get("new-login"), Some(&99));
    assert!(!state.initial_points.contains_key("old-login"));

    state.streamers[0].watch_suspended_until = Some(ts(600));
    assert!(state.update_streamer_login("123", "new-login"));
    assert!(state.streamers[0].watch_suspended_until.is_none());
    assert!(!state.update_streamer_login("123", "new-login"));
}

#[test]
fn stream_metadata_invalidates_campaign_only_when_identity_changes() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Drops],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_id: "123".into(),
            stream: Some(Stream {
                broadcast_id: "broadcast".into(),
                game: Some(tm_domain::Game::from_name("Game")),
                drop_campaign_eligible: Some(true),
                ..Stream::default()
            }),
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };
    let update = StreamUpdate {
        channel_id: "123".into(),
        id: "broadcast".into(),
        title: "Title".into(),
        game_name: "Game".into(),
        game_id: Some("game-1".into()),
        tags: Vec::new(),
        viewers_count: 42,
    };

    let _ = state.apply_stream_update(&update, ts(120));
    assert_eq!(
        state.streamers[0]
            .stream
            .as_ref()
            .unwrap()
            .drop_campaign_eligible,
        Some(true)
    );

    let mut changed = update;
    changed.game_name = "Different Game".into();
    let _ = state.apply_stream_update(&changed, ts(240));
    assert_eq!(
        state.streamers[0]
            .stream
            .as_ref()
            .unwrap()
            .drop_campaign_eligible,
        None
    );

    state.set_drop_campaign_eligibility("123", false);
    assert_eq!(
        state.streamers[0]
            .stream
            .as_ref()
            .unwrap()
            .drop_campaign_eligible,
        Some(false)
    );
}

#[test]
fn context_update_emits_goal_contribution_effect_for_active_goals() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_id: "123".into(),
            settings: StreamerSettings {
                community_goals: true,
                ..StreamerSettings::default()
            },
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    let effects = state.apply_context_update(&ContextUpdate {
        channel_id: "123".into(),
        channel_points_enabled: Some(true),
        balance: 500,
        active_multipliers: Vec::new(),
        community_goals: vec![CommunityGoal {
            id: "goal-1".into(),
            title: "Goal".into(),
            is_in_stock: true,
            points_contributed: 25,
            amount_needed: 100,
            per_stream_user_maximum_contribution: 50,
            status: "STARTED".into(),
        }],
    });

    assert_eq!(
        effects,
        vec![RuntimeEffect::ContributeCommunityGoals {
            channel_id: "123".into(),
        }]
    );
    assert_eq!(state.streamers[0].channel_points, 500);
    assert!(state.streamers[0].community_goals.contains_key("goal-1"));
}

#[allow(clippy::too_many_lines)]
#[test]
fn raid_moment_goal_and_prediction_events_emit_effects() {
    let mut state = RuntimeState {
        started_at: ts(0),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_id: "123".into(),
            settings: StreamerSettings {
                follow_raid: true,
                claim_moments: true,
                community_goals: true,
                make_predictions: true,
                ..StreamerSettings::default()
            },
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    let raid_effects = state.apply_event(
        &MinerEvent::Raid {
            channel_id: "123".into(),
            raid_id: "raid-1".into(),
            target_login: "target".into(),
        },
        ts(100),
    );
    assert_eq!(
        raid_effects,
        vec![RuntimeEffect::JoinRaid {
            channel_id: "123".into(),
            raid_id: "raid-1".into(),
            target_login: "target".into(),
        }]
    );
    assert!(state
        .apply_event(
            &MinerEvent::Raid {
                channel_id: "123".into(),
                raid_id: "raid-1".into(),
                target_login: "target".into(),
            },
            ts(101),
        )
        .is_empty());

    let moment_effects = state.apply_event(
        &MinerEvent::Moment {
            channel_id: "123".into(),
            moment_id: "moment-1".into(),
        },
        ts(102),
    );
    assert_eq!(
        moment_effects,
        vec![RuntimeEffect::ClaimMoment {
            channel_id: "123".into(),
            moment_id: "moment-1".into(),
        }]
    );
    assert!(state
        .apply_event(
            &MinerEvent::Moment {
                channel_id: "123".into(),
                moment_id: "moment-1".into(),
            },
            ts(102),
        )
        .is_empty());

    let claim = MinerEvent::ClaimAvailable {
        channel_id: "123".into(),
        claim_id: "claim-1".into(),
    };
    assert_eq!(
        state.apply_event(&claim, ts(102)),
        vec![RuntimeEffect::ClaimBonus {
            channel_id: "123".into(),
            claim_id: "claim-1".into(),
        }]
    );
    assert!(state.apply_event(&claim, ts(102)).is_empty());

    let goal_effects = state.apply_event(
        &MinerEvent::CommunityGoal {
            channel_id: "123".into(),
            kind: CommunityGoalKind::Created,
            goal: Some(CommunityGoal {
                id: "goal-1".into(),
                title: "Goal".into(),
                is_in_stock: true,
                points_contributed: 10,
                amount_needed: 100,
                per_stream_user_maximum_contribution: 50,
                status: "ACTIVE".into(),
            }),
            goal_id: Some("goal-1".into()),
        },
        ts(103),
    );
    assert_eq!(
        goal_effects,
        vec![RuntimeEffect::ContributeCommunityGoals {
            channel_id: "123".into(),
        }]
    );
    assert!(state.streamers[0].community_goals.contains_key("goal-1"));
    let unchanged_goal = state.streamers[0].community_goals["goal-1"].clone();
    let unchanged_application = state.apply_event_with_outcome(
        &MinerEvent::CommunityGoal {
            channel_id: "123".into(),
            kind: CommunityGoalKind::Updated,
            goal: Some(unchanged_goal),
            goal_id: Some("goal-1".into()),
        },
        ts(103),
    );
    assert!(!unchanged_application.changed);
    assert!(unchanged_application.effects.is_empty());

    let prediction_effects = state.apply_event(
        &MinerEvent::PredictionChannel {
            kind: PredictionChannelKind::EventCreated,
            event: Box::new(PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: "event-1".into(),
                title: "Prediction".into(),
                status: "ACTIVE".into(),
                created_at: ts(104),
                window_seconds: 30.0,
                outcomes: vec![
                    PredictionOutcome {
                        id: "a".into(),
                        title: "Alpha".into(),
                        color: "blue".into(),
                        total_users: 10,
                        total_points: 100,
                        top_points: 20,
                        percentage_users: 66.666_666_666_666_67,
                        odds: 1.5,
                        odds_percentage: 66.666_666_666_666_67,
                    },
                    PredictionOutcome {
                        id: "b".into(),
                        title: "Beta".into(),
                        color: "pink".into(),
                        total_users: 5,
                        total_points: 50,
                        top_points: 10,
                        percentage_users: 33.333_333_333_333_336,
                        odds: 3.0,
                        odds_percentage: 33.333_333_333_333_336,
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
        ts(104),
    );
    assert_eq!(
        prediction_effects,
        vec![RuntimeEffect::EvaluatePrediction {
            event_id: "event-1".into(),
        }]
    );
    assert!(state.predictions.contains_key("event-1"));
    let duplicate_prediction_effects = state.apply_event(
        &MinerEvent::PredictionChannel {
            kind: PredictionChannelKind::EventCreated,
            event: Box::new(state.predictions["event-1"].clone()),
            winning_outcome_id: None,
        },
        ts(104),
    );
    assert!(duplicate_prediction_effects.is_empty());

    let prediction_result = parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-user-v1.user","message":"{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"event-1\",\"result\":{\"type\":\"WIN\",\"points_won\":250}}}}"}}"#,
            &[],
        )
        .unwrap()
        .unwrap();
    let settled = state.apply_event(&prediction_result, ts(105));
    assert_eq!(
        settled,
        vec![RuntimeEffect::PredictionSettled {
            event_id: "event-1".into(),
            streamer_username: "tester".into(),
            title: "Prediction".into(),
            decision_label: String::new(),
            result_type: "WIN".into(),
            result_string: "WIN, Gained: +250".into(),
        }]
    );
    assert!(!state.predictions.contains_key("event-1"));
    assert_eq!(state.completed_predictions.len(), 1);
    let summary = state.session_summary(false, ts(106));
    assert_eq!(summary.predictions.len(), 1);
    assert_eq!(
        summary.predictions[0].event_line,
        "EventPrediction(event_id=event-1, title=\"Prediction\")"
    );
    assert!(summary.predictions[0]
        .outcome_lines
        .iter()
        .any(|line| line.contains("Alpha (BLUE)")));
    assert!(summary.predictions[0]
        .result_line
        .contains("WIN, Gained: +250"));
    let mut replay = state.completed_predictions[0].clone();
    replay.status = String::from("ACTIVE");
    replay.result_type.clear();
    replay.result_string.clear();
    assert!(state
        .apply_event(
            &MinerEvent::PredictionChannel {
                kind: PredictionChannelKind::EventCreated,
                event: Box::new(replay),
                winning_outcome_id: None,
            },
            ts(107),
        )
        .is_empty());
}

#[test]
fn runtime_session_summary_uses_captured_initial_points() {
    let mut state = RuntimeState {
        started_at: ts(10),
        follower_mode: false,
        watch_priorities: vec![WatchPriority::Order],
        game_priority: Vec::new(),
        game_exclusions: Vec::new(),
        streamers: vec![Streamer {
            username: "tester".into(),
            channel_points: 1_000,
            ..Streamer::default()
        }],
        initial_points: HashMap::new(),
        predictions: HashMap::new(),
        processed_prediction_ids: std::collections::VecDeque::new(),
        completed_predictions: std::collections::VecDeque::new(),
    };

    state.capture_initial_points();
    state.streamers[0].channel_points = 1_250;
    update_history(&mut state.streamers[0], "WATCH", 250);

    let summary = state.session_summary(false, ts(70));
    assert_eq!(summary.duration, "00:01:00.000000");
    assert_eq!(summary.total_points_line, "Total Points gained: +250");
    assert_eq!(summary.streamers[0].current_points, "1.25k");
}

#[tokio::test]
async fn spawned_runtime_is_single_writer_for_pubsub_and_shutdown() {
    let config = ConfigFile {
        streamers: vec!["tester".into()],
        ..ConfigFile::default()
    };
    let runtime = spawn_runtime_state(RuntimeState::from_config(&config, ts(10)));
    let summary = runtime.runtime_summary().await.unwrap();
    assert_eq!(summary.configured_streamers, 1);

    runtime
        .apply_event(
            MinerEvent::PointsEarned {
                channel_id: String::new(),
                earned: 100,
                reason: "WATCH".into(),
                balance: 100,
            },
            ts(20),
        )
        .await
        .unwrap();
    let summary = runtime.shutdown(false, ts(70)).await.unwrap();
    assert_eq!(summary.duration, "00:01:00.000000");
}

#[tokio::test]
async fn spawned_runtime_notifies_state_change_subscribers() {
    let config = ConfigFile {
        streamers: vec!["tester".into()],
        ..ConfigFile::default()
    };
    let runtime = spawn_runtime_state(RuntimeState::from_config(&config, ts(10)));
    let mut changes = runtime.subscribe_state_changes();

    runtime.set_presence("100", true, ts(20)).await.unwrap();

    changes.changed().await.unwrap();
    assert_eq!(*changes.borrow(), 1);
}

#[test]
fn duplicate_points_event_does_not_double_balance_or_history() {
    let config = ConfigFile {
        streamers: vec![String::from("tester")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].channel_points = 1_000;
    let event = MinerEvent::PointsEarned {
        channel_id: String::from("100"),
        earned: 50,
        reason: String::from("WATCH"),
        balance: 1_050,
    };

    let first = state.apply_event_with_outcome(&event, ts(1));
    let replay = state.apply_event_with_outcome(&event, ts(2));

    assert!(first.changed);
    assert!(!replay.changed);
    assert_eq!(state.streamers[0].channel_points, 1_050);
    assert_eq!(state.streamers[0].history["WATCH"].count, 1);
    assert_eq!(state.streamers[0].history["WATCH"].amount, 50);

    crate::summary::apply_pubsub_gain(&mut state.streamers[0], -50, "PREDICTION", 0);
    state.streamers[0].apply_channel_points_context(1_050, &[], &[]);
    let after_balance_change = state.apply_event_with_outcome(&event, ts(3));
    assert!(after_balance_change.changed);
    assert_eq!(state.streamers[0].channel_points, 1_100);
    assert_eq!(state.streamers[0].history["WATCH"].count, 2);
    assert_eq!(state.streamers[0].history["WATCH"].amount, 100);
}

#[test]
fn disabled_channel_ignores_point_events_and_goal_effects() {
    let config = ConfigFile {
        streamers: vec![String::from("disabled")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    let streamer = &mut state.streamers[0];
    streamer.channel_id = String::from("100");
    streamer.points_init = true;
    streamer.channel_points_enabled = Some(false);
    streamer.settings.community_goals = true;

    let points = MinerEvent::PointsEarned {
        channel_id: String::from("100"),
        earned: 50,
        reason: String::from("WATCH"),
        balance: 50,
    };
    assert!(!state.apply_event_with_outcome(&points, ts(1)).changed);
    assert_eq!(state.streamers[0].channel_points, 0);
    assert!(state
        .apply_context_update(&ContextUpdate {
            channel_id: String::from("100"),
            channel_points_enabled: Some(false),
            balance: 0,
            active_multipliers: Vec::new(),
            community_goals: vec![CommunityGoal {
                id: String::from("goal"),
                title: String::from("Goal"),
                is_in_stock: true,
                points_contributed: 0,
                amount_needed: 100,
                per_stream_user_maximum_contribution: 50,
                status: String::from("STARTED"),
            }],
        })
        .is_empty());
}

#[test]
fn skipped_point_effects_can_be_retried_after_channel_reenable() {
    let config = ConfigFile {
        streamers: vec![String::from("tester")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].channel_points_enabled = Some(true);
    state.streamers[0].settings.make_predictions = true;
    let claim = MinerEvent::ClaimAvailable {
        channel_id: String::from("100"),
        claim_id: String::from("claim-1"),
    };
    let prediction = MinerEvent::PredictionChannel {
        kind: PredictionChannelKind::EventCreated,
        event: Box::new(PredictionEvent {
            streamer: state.streamers[0].clone(),
            event_id: String::from("prediction-1"),
            title: String::from("Prediction"),
            status: String::from("ACTIVE"),
            created_at: ts(1),
            window_seconds: 30.0,
            outcomes: vec![PredictionOutcome {
                id: String::from("outcome-1").into(),
                title: String::from("Outcome"),
                ..PredictionOutcome::default()
            }],
            decision: PredictionDecision::default(),
            bet_placed: false,
            bet_confirmed: false,
            result_type: String::new(),
            result_string: String::new(),
        }),
        winning_outcome_id: None,
    };

    assert_eq!(state.apply_event(&claim, ts(1)).len(), 1);
    assert_eq!(state.apply_event(&prediction, ts(1)).len(), 1);
    state.apply_context_update(&ContextUpdate {
        channel_id: String::from("100"),
        channel_points_enabled: Some(false),
        balance: 0,
        active_multipliers: Vec::new(),
        community_goals: Vec::new(),
    });
    state.release_claim_bonus("100", "claim-1");
    state.release_prediction("prediction-1");
    state.apply_context_update(&ContextUpdate {
        channel_id: String::from("100"),
        channel_points_enabled: Some(true),
        balance: 100,
        active_multipliers: Vec::new(),
        community_goals: Vec::new(),
    });

    assert_eq!(state.apply_event(&claim, ts(2)).len(), 1);
    assert_eq!(state.apply_event(&prediction, ts(2)).len(), 1);
}

#[test]
fn points_replay_dedupe_property_tracks_post_application_balance() {
    for earned in [1, 12, 50, 1_000, i64::from(u32::MAX)] {
        for starting_balance in [0, 100, 1_000_000] {
            let config = ConfigFile {
                streamers: vec![String::from("tester")],
                ..ConfigFile::default()
            };
            let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
            state.streamers[0].channel_id = String::from("100");
            state.streamers[0].channel_points = starting_balance;
            let event = MinerEvent::PointsEarned {
                channel_id: String::from("100"),
                earned,
                reason: String::from("WATCH"),
                balance: starting_balance.saturating_add(earned),
            };

            assert!(state.apply_event_with_outcome(&event, ts(1)).changed);
            assert!(!state.apply_event_with_outcome(&event, ts(2)).changed);

            apply_pubsub_gain(&mut state.streamers[0], -earned, "PREDICTION", 0);
            assert!(state.apply_event_with_outcome(&event, ts(3)).changed);
            assert_eq!(state.streamers[0].history["WATCH"].count, 2);
        }
    }
}

#[test]
fn unknown_channel_winner_does_not_fabricate_a_loss() {
    let config = ConfigFile {
        streamers: vec![String::from("tester")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].settings.make_predictions = true;
    let mut active = PredictionEvent {
        streamer: state.streamers[0].clone(),
        event_id: String::from("prediction-unknown-winner"),
        title: String::from("Fixture prediction"),
        status: String::from("ACTIVE"),
        created_at: ts(1),
        window_seconds: 30.0,
        outcomes: vec![
            PredictionOutcome {
                id: "a".into(),
                title: String::from("Yes"),
                total_points: 100,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: "b".into(),
                title: String::from("No"),
                total_points: 100,
                ..PredictionOutcome::default()
            },
        ],
        decision: PredictionDecision {
            choice: Some(0),
            outcome_id: "a".into(),
            amount: 100,
        },
        bet_placed: true,
        bet_confirmed: true,
        result_type: String::new(),
        result_string: String::new(),
    };
    state
        .predictions
        .insert(active.event_id.clone(), active.clone());
    active.status = String::from("RESOLVED");

    let application = state.apply_event_with_outcome(
        &MinerEvent::PredictionChannel {
            kind: PredictionChannelKind::EventUpdated,
            event: Box::new(active),
            winning_outcome_id: Some(String::from("unknown")),
        },
        ts(2),
    );

    assert!(application.effects.is_empty());
    assert!(application.changed);
    assert!(state.predictions.contains_key("prediction-unknown-winner"));
    assert!(state.completed_predictions.is_empty());
    assert!(state.predictions["prediction-unknown-winner"]
        .result_type
        .is_empty());
}

#[test]
fn partial_settlement_update_keeps_connection_flow_for_viewer_result() {
    let config = ConfigFile {
        streamers: vec![String::from("tester")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].settings.make_predictions = true;
    let active = PredictionEvent {
        streamer: state.streamers[0].clone(),
        event_id: String::from("prediction-partial-settlement"),
        title: String::from("Fixture prediction"),
        status: String::from("ACTIVE"),
        created_at: ts(1),
        window_seconds: 30.0,
        outcomes: vec![
            PredictionOutcome {
                id: "a".into(),
                title: String::from("Yes"),
                total_points: 100,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: "b".into(),
                title: String::from("No"),
                total_points: 100,
                ..PredictionOutcome::default()
            },
        ],
        decision: PredictionDecision {
            choice: Some(0),
            outcome_id: "a".into(),
            amount: 100,
        },
        bet_placed: true,
        bet_confirmed: true,
        result_type: String::new(),
        result_string: String::new(),
    };
    state.predictions.insert(active.event_id.clone(), active);

    let pending_update = parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-channel-v1.100","message":"{\"type\":\"event-updated\",\"data\":{\"event\":{\"id\":\"prediction-partial-settlement\",\"status\":\"RESOLVE_PENDING\"}}}"}}"#,
            &state.streamers,
        )
        .unwrap()
        .unwrap();
    let pending_application = state.apply_event_with_outcome(&pending_update, ts(2));
    assert!(pending_application.changed);
    assert!(pending_application.effects.is_empty());
    assert_eq!(
        state.predictions["prediction-partial-settlement"].status,
        "RESOLVE_PENDING"
    );

    let channel_update = parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-channel-v1.100","message":"{\"type\":\"event-updated\",\"data\":{\"event\":{\"id\":\"prediction-partial-settlement\",\"status\":\"RESOLVED\"}}}"}}"#,
            &state.streamers,
        )
        .unwrap()
        .unwrap();
    let channel_application = state.apply_event_with_outcome(&channel_update, ts(3));
    assert!(channel_application.changed);
    assert!(channel_application.effects.is_empty());
    assert!(state
        .predictions
        .contains_key("prediction-partial-settlement"));

    let viewer_result = parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-user-v1.user","message":"{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"prediction-partial-settlement\",\"result\":{\"type\":\"WIN\",\"points_won\":250}}}}"}}"#,
            &[],
        )
        .unwrap()
        .unwrap();
    let viewer_effects = state.apply_event(&viewer_result, ts(4));

    assert_eq!(viewer_effects.len(), 1);
    assert!(!state
        .predictions
        .contains_key("prediction-partial-settlement"));
    assert_eq!(state.completed_predictions.len(), 1);
    assert_eq!(state.completed_predictions[0].result_type, "WIN");
    assert_eq!(
        state.completed_predictions[0].result_string,
        "WIN, Gained: +150"
    );
}

#[test]
fn partial_cancellation_update_refunds_placed_bet() {
    let config = ConfigFile {
        streamers: vec![String::from("tester")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].settings.make_predictions = true;
    let active = PredictionEvent {
        streamer: state.streamers[0].clone(),
        event_id: String::from("prediction-canceled"),
        title: String::from("Fixture prediction"),
        status: String::from("ACTIVE"),
        created_at: ts(1),
        window_seconds: 30.0,
        outcomes: vec![PredictionOutcome {
            id: "a".into(),
            title: String::from("Yes"),
            total_points: 100,
            ..PredictionOutcome::default()
        }],
        decision: PredictionDecision {
            choice: Some(0),
            outcome_id: "a".into(),
            amount: 100,
        },
        bet_placed: true,
        bet_confirmed: true,
        result_type: String::new(),
        result_string: String::new(),
    };
    state.predictions.insert(active.event_id.clone(), active);

    let cancellation = parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-channel-v1.100","message":"{\"type\":\"event-updated\",\"data\":{\"event\":{\"id\":\"prediction-canceled\",\"status\":\"CANCELED\",\"outcomes\":[{\"id\":\"a\",\"state\":\"CANCELED\"}]}}}"}}"#,
            &state.streamers,
        )
        .unwrap()
        .unwrap();
    let effects = state.apply_event(&cancellation, ts(2));

    assert_eq!(effects.len(), 1);
    assert_eq!(state.completed_predictions.len(), 1);
    assert_eq!(state.completed_predictions[0].result_type, "REFUND");
    assert_eq!(
        state.completed_predictions[0].result_string,
        "REFUND, Refunded: +0"
    );
}

#[test]
fn late_viewer_result_refines_channel_settlement_without_duplicate_effect() {
    let config = ConfigFile {
        streamers: vec![String::from("tester")],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(0));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].settings.make_predictions = true;
    let active = PredictionEvent {
        streamer: state.streamers[0].clone(),
        event_id: String::from("prediction-1"),
        title: String::from("Fixture prediction"),
        status: String::from("ACTIVE"),
        created_at: ts(1),
        window_seconds: 30.0,
        outcomes: vec![
            PredictionOutcome {
                id: "a".into(),
                title: String::from("Yes"),
                total_points: 100,
                ..PredictionOutcome::default()
            },
            PredictionOutcome {
                id: "b".into(),
                title: String::from("No"),
                total_points: 100,
                ..PredictionOutcome::default()
            },
        ],
        decision: PredictionDecision {
            choice: Some(0),
            outcome_id: "a".into(),
            amount: 100,
        },
        bet_placed: true,
        bet_confirmed: true,
        result_type: String::new(),
        result_string: String::new(),
    };
    state
        .predictions
        .insert(active.event_id.clone(), active.clone());
    state.apply_context_update(&ContextUpdate {
        channel_id: String::from("100"),
        channel_points_enabled: Some(false),
        balance: 0,
        active_multipliers: Vec::new(),
        community_goals: Vec::new(),
    });
    assert!(state.predictions.contains_key("prediction-1"));
    state.apply_context_update(&ContextUpdate {
        channel_id: String::from("100"),
        channel_points_enabled: Some(true),
        balance: 100,
        active_multipliers: Vec::new(),
        community_goals: Vec::new(),
    });
    let mut resolved = active;
    resolved.status = String::from("RESOLVED");
    resolved.outcomes.clear();

    let channel_effects = state.apply_event(
        &MinerEvent::PredictionChannel {
            kind: PredictionChannelKind::EventUpdated,
            event: Box::new(resolved),
            winning_outcome_id: Some(String::from("a")),
        },
        ts(2),
    );
    assert_eq!(channel_effects.len(), 1);
    assert_eq!(state.completed_predictions.len(), 1);
    assert_eq!(state.completed_predictions[0].outcomes.len(), 2);

    let viewer_result = parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-user-v1.user","message":"{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"prediction-1\",\"result\":{\"type\":\"WIN\",\"points_won\":250}}}}"}}"#,
            &[],
        )
        .unwrap()
        .unwrap();
    let viewer_effects = state.apply_event(&viewer_result, ts(3));

    assert!(viewer_effects.is_empty());
    assert_eq!(state.completed_predictions.len(), 1);
    assert_eq!(
        state.completed_predictions[0].result_string,
        "WIN, Gained: +150"
    );
}

#[tokio::test]
async fn checked_presence_update_reports_only_real_transitions() {
    let config = ConfigFile {
        streamers: vec!["tester".into()],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(10));
    state.streamers[0].channel_id = String::from("100");
    let runtime = spawn_runtime_state(state);

    assert!(runtime
        .set_presence_if_changed("100", true, ts(20))
        .await
        .unwrap());
    assert!(!runtime
        .set_presence_if_changed("100", true, ts(21))
        .await
        .unwrap());
    assert!(runtime
        .set_presence_if_changed("100", false, ts(22))
        .await
        .unwrap());
}

#[tokio::test]
async fn releasing_watch_slot_resets_only_that_channels_progress() {
    let config = ConfigFile {
        streamers: vec!["alpha".into(), "bravo".into()],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(10));
    for (index, channel_id) in ["100", "200"].into_iter().enumerate() {
        state.streamers[index].channel_id = channel_id.to_owned();
        state.streamers[index].settings.watch_streak = true;
        state.streamers[index].stream = Some(Stream {
            minute_watched: 12.0,
            last_minute_update: Some(ts(20)),
            ..Stream::default()
        });
    }
    let runtime = spawn_runtime_state(state);

    runtime.reset_watch_progress("100").await.unwrap();

    let snapshot = runtime.state_snapshot().await.unwrap();
    let released = snapshot.streamers[0].stream.as_ref().unwrap();
    let retained = snapshot.streamers[1].stream.as_ref().unwrap();
    assert_f64_eq(released.minute_watched, 0.0);
    assert!(released.last_minute_update.is_none());
    assert_f64_eq(retained.minute_watched, 12.0);
    assert_eq!(retained.last_minute_update, Some(ts(20)));
    assert!(should_prioritize_streak(&snapshot.streamers[0], ts(30)));
    assert!(!should_prioritize_streak(&snapshot.streamers[1], ts(30)));
}

#[test]
fn confirmed_watch_progress_rejects_stale_and_nonpositive_intervals() {
    let config = ConfigFile {
        streamers: vec!["alpha".into()],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(10));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].stream = Some(Stream {
        minute_watched: 2.0,
        last_minute_update: Some(ts(100)),
        ..Stream::default()
    });

    state.mark_minute_watched("100", ts(120));
    assert_f64_eq(
        state.streamers[0].stream.as_ref().unwrap().minute_watched,
        2.0 + 20.0 / 60.0,
    );

    // One failed nominal tick still proves a short continuous interval.
    state.mark_minute_watched("100", ts(160));
    assert_f64_eq(
        state.streamers[0].stream.as_ref().unwrap().minute_watched,
        3.0,
    );

    // The scheduler/request envelope allows at most 400 seconds.
    state.mark_minute_watched("100", ts(560));
    let confirmed = 3.0 + 400.0 / 60.0;
    assert_f64_eq(
        state.streamers[0].stream.as_ref().unwrap().minute_watched,
        confirmed,
    );

    state.mark_minute_watched("100", ts(961));
    assert_f64_eq(
        state.streamers[0].stream.as_ref().unwrap().minute_watched,
        confirmed,
    );
    assert_eq!(
        state.streamers[0]
            .stream
            .as_ref()
            .unwrap()
            .last_minute_update,
        Some(ts(961))
    );

    state.mark_minute_watched("100", ts(900));
    let stream = state.streamers[0].stream.as_ref().unwrap();
    assert_f64_eq(stream.minute_watched, confirmed);
    assert_eq!(stream.last_minute_update, Some(ts(900)));
}

#[test]
fn confirmed_watch_progress_stops_after_channel_points_are_disabled() {
    let config = ConfigFile {
        streamers: vec!["alpha".into()],
        ..ConfigFile::default()
    };
    let mut state = RuntimeState::from_targets(&config, &config.streamers, ts(10));
    state.streamers[0].channel_id = String::from("100");
    state.streamers[0].channel_points_enabled = Some(false);
    state.streamers[0].stream = Some(Stream {
        minute_watched: 2.0,
        last_minute_update: Some(ts(100)),
        ..Stream::default()
    });

    state.mark_minute_watched("100", ts(120));

    let stream = state.streamers[0].stream.as_ref().unwrap();
    assert_f64_eq(stream.minute_watched, 2.0);
    assert_eq!(stream.last_minute_update, Some(ts(100)));
}

#[tokio::test]
async fn runtime_metrics_capture_event_processing_and_compatibility_fields() {
    let config = ConfigFile {
        streamers: vec!["tester".into()],
        ..ConfigFile::default()
    };
    let runtime = spawn_runtime_state(RuntimeState::from_config(&config, ts(10)));
    runtime
        .apply_event(
            MinerEvent::Playback {
                channel_id: String::from("missing"),
                kind: PlaybackType::Viewcount,
            },
            ts(11),
        )
        .await
        .unwrap();
    let metrics = runtime.metrics();
    assert_eq!(metrics.processed_events, 1);
    assert_eq!(metrics.max_queue_depth, 0);
}

#[tokio::test]
async fn runtime_handle_returns_typed_closed_error_after_shutdown() {
    let config = ConfigFile {
        streamers: vec!["tester".into()],
        ..ConfigFile::default()
    };
    let runtime = spawn_runtime_state(RuntimeState::from_config(&config, ts(10)));

    let _ = runtime.shutdown(false, ts(70)).await.unwrap();
    let error = runtime.state_snapshot().await.unwrap_err();

    assert!(matches!(
        error,
        RuntimeError::RuntimeClosed {
            command: "StateSnapshot"
        }
    ));
}
