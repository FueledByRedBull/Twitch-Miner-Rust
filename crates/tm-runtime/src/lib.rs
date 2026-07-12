pub use tm_domain::OffsetDateTime as RuntimeTime;

mod actor;
mod effect;
mod error;
mod prediction;
mod pubsub;
mod state;
mod summary;
mod types;

pub use actor::{RuntimeHandle, RuntimeMetrics, RuntimeMetricsSnapshot};
pub use effect::RuntimeEffect;
pub use error::{Result, RuntimeError};
pub use summary::{apply_pubsub_gain, build_session_summary, update_history};
pub use types::{
    ContextUpdate, RuntimeSession, RuntimeState, RuntimeSummary, SessionSummary, StreamUpdate,
    StreamerSummary,
};

#[allow(clippy::unused_async)]
pub async fn run(config: &tm_config::ConfigFile) -> RuntimeSession {
    bootstrap(config, tm_domain::OffsetDateTime::now_utc())
}

#[must_use]
pub fn bootstrap(
    config: &tm_config::ConfigFile,
    started_at: tm_domain::OffsetDateTime,
) -> RuntimeSession {
    RuntimeSession::from_state(RuntimeState::from_config(config, started_at))
}

#[must_use]
pub fn spawn_runtime(
    config: &tm_config::ConfigFile,
    started_at: tm_domain::OffsetDateTime,
) -> RuntimeHandle {
    actor::spawn_runtime_session(bootstrap(config, started_at))
}

#[must_use]
pub fn spawn_runtime_state(state: RuntimeState) -> RuntimeHandle {
    actor::spawn_runtime_session(RuntimeSession::from_state(state))
}

#[must_use]
pub fn spawn_runtime_now(config: &tm_config::ConfigFile) -> RuntimeHandle {
    spawn_runtime(config, tm_domain::OffsetDateTime::now_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tm_config::ConfigFile;
    use tm_domain::{
        parse_watch_priorities, CommunityGoal, HistoryEntry, IrcMode, OffsetDateTime,
        PredictionDecision, PredictionEvent, PredictionOutcome, Stream, Streamer, StreamerSettings,
        WatchPriority,
    };
    use tm_pubsub::{CommunityGoalKind, PlaybackType, PredictionChannelKind, PubSubEvent};

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

        let summary = build_session_summary(
            &[streamer],
            &[("tester", 1_500)],
            true,
            std::time::Duration::from_secs(45),
        );

        assert_eq!(summary.duration, "45s");
        assert_eq!(summary.total_points_line, "Total Points gained: [hidden]");
        assert_eq!(summary.streamers[0].current_points, "[hidden]");
        assert_eq!(
            summary.streamers[0].total_points_line,
            "Total Points [hidden]"
        );
        assert_eq!(
            summary.streamers[0].history_lines[0],
            "WATCH (2 times, [hidden])"
        );
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
        assert!(!state.streamers[1].settings.claim_drops);
    }

    #[test]
    fn bootstraps_runtime_session_and_captures_initial_points() {
        let config = ConfigFile {
            streamers: vec!["StreamerOne".into(), "ignored".into()],
            streamers_exclude: vec!["ignored".into()],
            ..ConfigFile::default()
        };

        let session = bootstrap(&config, ts(1_000));
        assert!(!session.summary.follower_mode);
        assert_eq!(session.summary.configured_streamers, 1);
        assert_eq!(session.state.streamers.len(), 1);
        assert_eq!(
            session.state.initial_points.get("streamerone"),
            Some(&session.state.streamers[0].channel_points)
        );
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
        };

        state.apply_pubsub_event(
            &PubSubEvent::Playback {
                channel_id: "123".into(),
                kind: PlaybackType::StreamUp,
            },
            ts(100),
        );
        assert_eq!(state.desired_chat_logins(), vec!["tester"]);
        assert!(state.watch_target_logins(ts(120)).is_empty());
        assert_eq!(state.watch_target_logins(ts(131)), vec!["tester"]);

        state.apply_pubsub_event(
            &PubSubEvent::Playback {
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
        };

        state.apply_pubsub_event(
            &PubSubEvent::Playback {
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
        };

        state.apply_stream_update(
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
        );

        let stream = state.streamers[0].stream.as_ref().unwrap();
        assert_eq!(stream.broadcast_id, "new-broadcast");
        assert_f64_eq(stream.minute_watched, 0.0);
        assert!(stream.last_minute_update.is_none());
        assert!(stream.watch_streak_missing);
        assert_eq!(stream.stream_up_at, Some(ts(120)));
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
        };

        let effects = state.apply_context_update(&ContextUpdate {
            channel_id: "123".into(),
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
        };

        let raid_effects = state.apply_pubsub_event(
            &PubSubEvent::Raid {
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
            .apply_pubsub_event(
                &PubSubEvent::Raid {
                    channel_id: "123".into(),
                    raid_id: "raid-1".into(),
                    target_login: "target".into(),
                },
                ts(101),
            )
            .is_empty());

        let moment_effects = state.apply_pubsub_event(
            &PubSubEvent::Moment {
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

        let goal_effects = state.apply_pubsub_event(
            &PubSubEvent::CommunityGoal {
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

        let prediction_effects = state.apply_pubsub_event(
            &PubSubEvent::PredictionChannel {
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

        let prediction_result = tm_pubsub::parse_message(
            r#"{"type":"MESSAGE","data":{"topic":"predictions-user-v1.user","message":"{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"event-1\",\"result\":{\"type\":\"WIN\"}}}}"}}"#,
            &[],
        )
        .unwrap()
        .unwrap();
        let settled = state.apply_pubsub_event(&prediction_result, ts(105));
        assert_eq!(
            settled,
            vec![RuntimeEffect::PredictionSettled {
                event_id: "event-1".into(),
                streamer_username: "tester".into(),
                title: "Prediction".into(),
                decision_label: String::new(),
                result_type: "WIN".into(),
                result_string: "WIN, Gained: +0".into(),
            }]
        );
        assert!(!state.predictions.contains_key("event-1"));
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
        };

        state.capture_initial_points();
        state.streamers[0].channel_points = 1_250;
        update_history(&mut state.streamers[0], "WATCH", 250);

        let summary = state.session_summary(false, ts(70));
        assert_eq!(summary.duration, "01m 00s");
        assert_eq!(summary.total_points_line, "Total Points gained: +250");
        assert_eq!(summary.streamers[0].current_points, "1.25k");
    }

    #[tokio::test]
    async fn spawned_runtime_is_single_writer_for_pubsub_and_shutdown() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };
        let runtime = spawn_runtime(&config, ts(10));
        let summary = runtime.runtime_summary().await.unwrap();
        assert_eq!(summary.configured_streamers, 1);

        runtime
            .apply_pubsub_event(
                PubSubEvent::PointsEarned {
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
        assert_eq!(summary.duration, "01m 00s");
    }

    #[tokio::test]
    async fn spawned_runtime_notifies_state_change_subscribers() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };
        let runtime = spawn_runtime(&config, ts(10));
        let mut changes = runtime.subscribe_state_changes();

        runtime.set_presence("100", true, ts(20)).await.unwrap();

        changes.changed().await.unwrap();
        assert_eq!(*changes.borrow(), 1);
    }

    #[tokio::test]
    async fn runtime_metrics_capture_event_processing_and_queue_depth() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };
        let runtime = spawn_runtime(&config, ts(10));
        runtime
            .apply_event(
                PubSubEvent::Playback {
                    channel_id: String::from("missing"),
                    kind: PlaybackType::Viewcount,
                },
                ts(11),
            )
            .await
            .unwrap();
        let metrics = runtime.metrics();
        assert_eq!(metrics.processed_events, 1);
        assert!(metrics.max_queue_depth >= 1);
        assert!(metrics.max_queue_depth <= 64);
    }

    #[tokio::test]
    async fn queued_commands_and_dropped_callers_do_not_block_orderly_shutdown() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };
        let runtime = spawn_runtime(&config, ts(10));
        let mut queued = Vec::new();
        for index in 0_i64..64 {
            let handle = runtime.clone();
            queued.push(tokio::spawn(async move {
                handle
                    .apply_event(
                        PubSubEvent::PointsEarned {
                            channel_id: String::new(),
                            earned: 1,
                            reason: String::from("WATCH"),
                            balance: index,
                        },
                        ts(20 + index),
                    )
                    .await
            }));
        }
        tokio::task::yield_now().await;
        let dropped = tokio::spawn({
            let handle = runtime.clone();
            async move {
                let _ = handle
                    .apply_event(
                        PubSubEvent::Playback {
                            channel_id: String::new(),
                            kind: PlaybackType::Viewcount,
                        },
                        ts(100),
                    )
                    .await;
            }
        });
        dropped.abort();

        let shutdown = tokio::spawn({
            let handle = runtime.clone();
            async move { handle.shutdown(false, ts(200)).await }
        });
        for task in queued {
            let _ = task.await;
        }
        let summary = shutdown.await.unwrap().unwrap();
        assert_eq!(summary.streamers.len(), 1);
        assert!(runtime.metrics().processed_events <= 65);
        assert!(runtime.metrics().max_queue_depth > 0);
    }

    #[tokio::test]
    async fn run_bootstraps_session_directly() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };

        let session = run(&config).await;

        assert_eq!(session.summary.configured_streamers, 1);
    }

    #[tokio::test]
    async fn runtime_handle_returns_typed_actor_closed_error_after_shutdown() {
        let config = ConfigFile {
            streamers: vec!["tester".into()],
            ..ConfigFile::default()
        };
        let runtime = spawn_runtime(&config, ts(10));

        let _ = runtime.shutdown(false, ts(70)).await.unwrap();
        let error = runtime.state_snapshot().await.unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::SendFailed {
                command: "StateSnapshot"
            } | RuntimeError::ActorClosed {
                command: "StateSnapshot"
            } | RuntimeError::CallerDropped {
                command: "StateSnapshot"
            }
        ));
    }
}
