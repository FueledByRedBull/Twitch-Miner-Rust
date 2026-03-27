use std::fs;
use std::path::{Path, PathBuf};

use tm_domain::Streamer;
use tm_pubsub::{
    CommunityGoalKind, PlaybackType, PredictionChannelKind, PredictionUserKind, PubSubEvent,
};
use tm_twitch::{
    extract_build_id, extract_settings_script_url, extract_spade_url,
    parse_available_drop_campaign_ids, parse_channel_points_context, parse_followers_page,
    parse_inventory_drops, parse_live_status, parse_stream_info,
    parse_user_points_contributions,
};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures").join(name)
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
fn twitch_contract_fixtures_cover_build_id_context_stream_info_inventory_campaigns_and_lookup_shapes()
{
    let homepage = fixture_json("twitch.homepage.html");
    assert_eq!(
        extract_build_id(&homepage).unwrap(),
        "ef928475-9403-42f2-8a34-55784bd08e16"
    );
    assert_eq!(
        extract_settings_script_url(&homepage).unwrap(),
        "https://static.twitchcdn.net/config/settings.123.js"
    );

    let settings = fixture_json("twitch.settings.js");
    assert_eq!(
        extract_spade_url(&settings).unwrap(),
        "https://spade.example/submit"
    );

    let context_payload =
        serde_json::from_slice::<serde_json::Value>(&fs::read(fixture_path("twitch.channel_points_context.json")).unwrap())
            .unwrap();
    let context = parse_channel_points_context(&context_payload).unwrap();
    assert_eq!(context.balance, 1234);
    assert_eq!(context.claim_id.as_deref(), Some("claim-1"));

    let stream_payload =
        serde_json::from_slice::<serde_json::Value>(&fs::read(fixture_path("twitch.stream_info.json")).unwrap())
            .unwrap();
    let stream = parse_stream_info(&stream_payload).unwrap();
    assert_eq!(stream.id, "stream-1");
    assert_eq!(stream.game_name, "Game Name");

    let live_offline_payload =
        serde_json::from_slice::<serde_json::Value>(&fs::read(fixture_path("twitch.stream_live.offline.json")).unwrap())
            .unwrap();
    assert!(!parse_live_status(&live_offline_payload));
    let live_online_payload =
        serde_json::from_slice::<serde_json::Value>(&fs::read(fixture_path("twitch.stream_live.online.json")).unwrap())
            .unwrap();
    assert!(parse_live_status(&live_online_payload));

    let followers_payload =
        serde_json::from_slice::<serde_json::Value>(&fs::read(fixture_path("twitch.followers.json")).unwrap())
            .unwrap();
    let followers = parse_followers_page(&followers_payload).unwrap();
    assert_eq!(followers.logins, vec!["alice", "bob"]);
    assert!(followers.has_next_page);
    assert_eq!(followers.cursor.as_deref(), Some("cursor-2"));

    let inventory_payload =
        serde_json::from_slice::<serde_json::Value>(&fs::read(fixture_path("twitch.inventory.json")).unwrap())
            .unwrap();
    let drops = parse_inventory_drops(&inventory_payload);
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].drop_instance_id, "drop-1");

    let campaign_payload = serde_json::from_slice::<serde_json::Value>(
        &fs::read(fixture_path("twitch.available_drop_campaigns.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parse_available_drop_campaign_ids(&campaign_payload),
        vec!["campaign-1", "campaign-2"]
    );

    let contributions_payload = serde_json::from_slice::<serde_json::Value>(
        &fs::read(fixture_path("twitch.user_points_contribution.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parse_user_points_contributions(&contributions_payload),
        vec![(String::from("goal-1"), 25), (String::from("goal-2"), 10)]
    );
}

#[test]
fn pubsub_contract_fixtures_cover_each_topic_family() {
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.claim_available.json"), &[]).unwrap(),
        Some(PubSubEvent::ClaimAvailable {
            channel_id: String::from("123"),
            claim_id: String::from("claim-1"),
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.points_earned.json"), &[]).unwrap(),
        Some(PubSubEvent::PointsEarned {
            channel_id: String::from("123"),
            earned: 50,
            reason: String::from("WATCH"),
            balance: 1050,
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.playback_stream_up.json"), &[]).unwrap(),
        Some(PubSubEvent::Playback {
            channel_id: String::from("123"),
            kind: PlaybackType::StreamUp,
        })
    );
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.playback_viewcount.json"), &[]).unwrap(),
        Some(PubSubEvent::Playback {
            channel_id: String::from("123"),
            kind: PlaybackType::Viewcount,
        })
    );
    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.playback_stream_down.json"), &[]).unwrap(),
        Some(PubSubEvent::Playback {
            channel_id: String::from("123"),
            kind: PlaybackType::StreamDown,
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.raid.json"), &[]).unwrap(),
        Some(PubSubEvent::Raid {
            channel_id: String::from("123"),
            raid_id: String::from("raid-1"),
            target_login: String::from("target"),
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.moment.json"), &[]).unwrap(),
        Some(PubSubEvent::Moment {
            channel_id: String::from("123"),
            moment_id: String::from("moment-1"),
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.community_goal_created.json"), &[]).unwrap(),
        Some(PubSubEvent::CommunityGoal {
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
        Some(PubSubEvent::CommunityGoal {
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
        Some(PubSubEvent::CommunityGoal {
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
    let PubSubEvent::PredictionChannel {
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
    let PubSubEvent::PredictionChannel {
        kind,
        event,
        winning_outcome_id,
    } = prediction_updated
    else {
        panic!("expected prediction update event");
    };
    assert_eq!(kind, PredictionChannelKind::EventUpdated);
    assert_eq!(event.status, "RESOLVED");
    assert_eq!(winning_outcome_id.as_deref(), Some("a"));

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.prediction_made.json"), &[]).unwrap(),
        Some(PubSubEvent::PredictionUser {
            event_id: String::from("event-1"),
            kind: PredictionUserKind::PredictionMade,
            result: None,
        })
    );

    assert_eq!(
        tm_pubsub::parse_message(&fixture_json("pubsub.prediction_result.json"), &[]).unwrap(),
        Some(PubSubEvent::PredictionUser {
            event_id: String::from("event-1"),
            kind: PredictionUserKind::PredictionResult,
            result: Some(serde_json::json!({ "type": "WIN" })),
        })
    );
}
