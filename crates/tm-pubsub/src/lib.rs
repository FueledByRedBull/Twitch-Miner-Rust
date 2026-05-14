pub const WEBSOCKET_URL: &str = "wss://pubsub-edge.twitch.tv";

mod client;
mod errors;
mod parse;
mod prediction;
mod topics;
mod types;

pub use client::{PubSubClient, PubSubClientSettings, PubSubConnectionEvent};
pub use errors::PubSubError;
pub use parse::{
    bad_auth_cookie_file, channel_id_from_payload, parse_message, parse_transport_message,
};
pub use topics::{
    build_topic_batches, build_topics, chunk_topics, listen_payload, listen_payload_with_nonce,
    listen_payloads, ping_payload, topic_requires_auth,
};
pub use types::{
    CommunityGoalKind, IncomingTransportMessage, PlaybackType, PredictionChannelKind,
    PredictionUserKind, PubSubEvent,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::randomized_ping_delay;
    use serde_json::json;
    use std::time::Duration;
    use tm_domain::{CommunityGoal, Streamer};
    use tm_domain::{IrcMode, StreamerSettings};

    fn streamer(id: &str) -> Streamer {
        Streamer {
            channel_id: id.to_string(),
            settings: StreamerSettings {
                make_predictions: true,
                follow_raid: true,
                claim_moments: true,
                community_goals: true,
                claim_drops: true,
                irc_mode: IrcMode::Online,
                ..StreamerSettings::default()
            },
            ..Streamer::default()
        }
    }

    #[test]
    fn builds_topics_from_streamer_settings() {
        let topics = build_topics("user-1", &[streamer("100"), streamer("200")]).unwrap();
        assert!(topics.contains(&"community-points-user-v1.user-1".to_string()));
        assert!(topics.contains(&"predictions-user-v1.user-1".to_string()));
        assert!(topics.contains(&"video-playback-by-id.100".to_string()));
        assert!(topics.contains(&"raid.100".to_string()));
        assert!(topics.contains(&"predictions-channel-v1.100".to_string()));
        assert!(topics.contains(&"community-moments-channel-v1.100".to_string()));
        assert!(topics.contains(&"community-points-channel-v1.100".to_string()));
    }

    #[test]
    fn builds_bonus_claim_topic_even_without_community_goals() {
        let topics = build_topics(
            "user-1",
            &[Streamer {
                channel_id: String::from("100"),
                settings: StreamerSettings {
                    community_goals: false,
                    ..StreamerSettings::default()
                },
                ..Streamer::default()
            }],
        )
        .unwrap();

        assert!(topics.contains(&"community-points-channel-v1.100".to_string()));
    }

    #[test]
    fn builds_topic_batches_with_auth_topic_and_fifty_max_topics() {
        let streamers = (0..51)
            .map(|index| Streamer {
                channel_id: format!("channel-{index}"),
                ..Streamer::default()
            })
            .collect::<Vec<_>>();

        let batches = build_topic_batches("user-1", &streamers).unwrap();
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 50);
        assert_eq!(batches[1].len(), 50);
        assert_eq!(batches[2].len(), 3);
        assert_eq!(batches[0][0], "community-points-user-v1.user-1");
        assert!(!batches[2]
            .iter()
            .any(|topic| topic == "community-points-user-v1.user-1"));
    }

    #[test]
    fn chunks_topics_at_fifty() {
        let topics: Vec<String> = (0..101).map(|value| format!("topic-{value}")).collect();
        let chunks = chunk_topics(&topics);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 50);
        assert_eq!(chunks[1].len(), 50);
        assert_eq!(chunks[2].len(), 1);
    }

    #[test]
    fn listen_payload_only_adds_auth_for_user_topics() {
        let auth_payload = listen_payload("community-points-user-v1.user", "secret");
        assert_eq!(auth_payload["data"]["auth_token"], "secret");

        let no_auth_payload = listen_payload("video-playback-by-id.123", "secret");
        assert!(no_auth_payload["data"].get("auth_token").is_none());
    }

    #[test]
    fn listen_payloads_generate_unique_nonces_and_ping_payload() {
        let payloads = listen_payloads(
            &[
                "community-points-user-v1.user".to_string(),
                "video-playback-by-id.123".to_string(),
            ],
            "secret",
        );
        assert_eq!(payloads.len(), 2);
        assert_ne!(payloads[0]["nonce"], payloads[1]["nonce"]);
        assert_eq!(ping_payload()["type"], "PING");
    }

    #[test]
    fn channel_id_falls_back_across_payload_shapes() {
        let payload = json!({
            "data": {
                "prediction": { "channel_id": "prediction-id" }
            }
        });
        assert_eq!(
            channel_id_from_payload(&payload, "topic.ignore"),
            "prediction-id"
        );

        let payload = json!({
            "data": {
                "claim": { "channel_id": "claim-id" }
            }
        });
        assert_eq!(
            channel_id_from_payload(&payload, "topic.ignore"),
            "claim-id"
        );

        let payload = json!({
            "data": {
                "balance": { "channel_id": "balance-id" }
            }
        });
        assert_eq!(
            channel_id_from_payload(&payload, "topic.ignore"),
            "balance-id"
        );

        let payload = json!({});
        assert_eq!(
            channel_id_from_payload(&payload, "video-playback-by-id.topic-suffix"),
            "topic-suffix"
        );
    }

    #[test]
    fn parses_claim_available_with_single_streamer_fallback() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "",
                "message": "{\"type\":\"claim-available\",\"data\":{\"claim\":{\"id\":\"claim-1\"}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[streamer("fallback-channel")]).unwrap();
        assert_eq!(
            parsed,
            Some(PubSubEvent::ClaimAvailable {
                channel_id: String::from("fallback-channel"),
                claim_id: String::from("claim-1"),
            })
        );
    }

    #[test]
    fn parses_prediction_result_without_prediction_made() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "predictions-user-v1.user",
                "message": "{\"type\":\"prediction-result\",\"data\":{\"prediction\":{\"event_id\":\"event-1\",\"result\":{\"type\":\"WIN\"}}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[]).unwrap();
        assert_eq!(
            parsed,
            Some(PubSubEvent::PredictionUser {
                event_id: String::from("event-1"),
                kind: PredictionUserKind::PredictionResult,
                result: Some(json!({ "type": "WIN" })),
            })
        );
    }

    #[test]
    fn parses_prediction_channel_event_with_outcomes() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "predictions-channel-v1.123",
                "message": "{\"type\":\"event-created\",\"data\":{\"event\":{\"id\":\"event-1\",\"title\":\"Will it happen?\",\"status\":\"ACTIVE\",\"created_at\":\"2026-03-27T06:00:00Z\",\"prediction_window_seconds\":120,\"outcomes\":[{\"id\":\"a\",\"title\":\"Yes\",\"color\":\"blue\",\"total_users\":10,\"total_points\":100,\"top_predictors\":[{\"points\":30}]},{\"id\":\"b\",\"title\":\"No\",\"color\":\"pink\",\"total_users\":5,\"total_points\":50,\"top_predictors\":[{\"points\":25}]}]}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[streamer("123")]).unwrap();
        let Some(PubSubEvent::PredictionChannel {
            kind,
            event,
            winning_outcome_id,
        }) = parsed
        else {
            panic!("expected prediction channel event");
        };
        assert_eq!(kind, PredictionChannelKind::EventCreated);
        assert_eq!(event.event_id, "event-1");
        assert_eq!(event.streamer.channel_id, "123");
        assert_eq!(event.outcomes[0].top_points, 30);
        assert!((event.outcomes[0].odds - 1.5).abs() < f64::EPSILON);
        assert!((event.outcomes[1].odds_percentage - 33.333_333_333_333_336).abs() < f64::EPSILON);
        assert_eq!(winning_outcome_id, None);
    }

    #[test]
    fn parses_community_goal_message() {
        let raw = json!({
            "type": "MESSAGE",
            "data": {
                "topic": "community-points-channel-v1.123",
                "message": "{\"type\":\"community-goal-created\",\"data\":{\"community_goal\":{\"id\":\"goal-1\",\"title\":\"Goal\",\"is_in_stock\":true,\"points_contributed\":100,\"goal_amount\":500,\"per_stream_maximum_user_contribution\":50,\"status\":\"ACTIVE\"}}}"
            }
        })
        .to_string();
        let parsed = parse_message(&raw, &[]).unwrap();
        assert_eq!(
            parsed,
            Some(PubSubEvent::CommunityGoal {
                channel_id: String::from("123"),
                kind: CommunityGoalKind::Created,
                goal: Some(CommunityGoal {
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
    }

    #[test]
    fn parse_transport_message_handles_control_frames_and_bad_auth() {
        assert_eq!(
            parse_transport_message(r#"{"type":"PONG"}"#, &[]).unwrap(),
            IncomingTransportMessage::Pong
        );
        assert_eq!(
            parse_transport_message(r#"{"type":"RECONNECT"}"#, &[]).unwrap(),
            IncomingTransportMessage::Reconnect
        );
        assert_eq!(
            parse_transport_message(
                r#"{"type":"RESPONSE","error":"ERR_BADAUTH bad token","nonce":"abc"}"#,
                &[]
            )
            .unwrap(),
            IncomingTransportMessage::ResponseError {
                error: "ERR_BADAUTH bad token".into(),
                nonce: Some("abc".into()),
                is_bad_auth: true,
            }
        );
    }

    #[test]
    fn bad_auth_cookie_file_matches_go_shape() {
        assert_eq!(bad_auth_cookie_file(Some("alice")), "cookies/alice.json");
        assert_eq!(bad_auth_cookie_file(None), "cookies/<username>.json");
    }

    #[test]
    fn randomized_ping_delay_stays_in_expected_range() {
        let base = Duration::from_secs(25);
        let delay = randomized_ping_delay(base);
        assert!((Duration::from_secs(25)..=Duration::from_secs(30)).contains(&delay));
    }
}
