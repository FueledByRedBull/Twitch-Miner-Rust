use std::fs;
use std::path::{Path, PathBuf};

use tm_twitch::{
    extract_build_id, extract_settings_script_url, extract_spade_url,
    parse_available_drop_campaign_ids, parse_channel_points_context, parse_followers_page,
    parse_inventory_drops, parse_live_status, parse_stream_info, parse_user_points_contributions,
};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

#[test]
fn homepage_fixture_extracts_build_id_and_settings_url() {
    let html = fs::read_to_string(fixture_path("twitch.homepage.html")).unwrap();
    assert_eq!(
        extract_build_id(&html).unwrap(),
        "ef928475-9403-42f2-8a34-55784bd08e16"
    );
    assert_eq!(
        extract_settings_script_url(&html).unwrap(),
        "https://static.twitchcdn.net/config/settings.123.js"
    );
}

#[test]
fn settings_fixture_extracts_spade_url() {
    let settings = fs::read_to_string(fixture_path("twitch.settings.js")).unwrap();
    assert_eq!(
        extract_spade_url(&settings).unwrap(),
        "https://spade.example/submit"
    );
}

#[test]
fn channel_points_context_fixture_matches_expected_shape() {
    let payload = serde_json::from_slice(
        &fs::read(fixture_path("twitch.channel_points_context.json")).unwrap(),
    )
    .unwrap();
    let context = parse_channel_points_context(&payload).unwrap();
    assert_eq!(context.balance, 1234);
    assert_eq!(context.claim_id.as_deref(), Some("claim-1"));
    assert_eq!(context.active_multiplier_count, 2);
    assert_eq!(context.active_multipliers.len(), 2);
    assert_eq!(context.community_goals.len(), 1);
    assert_eq!(context.community_goals[0].id, "goal-1");
}

#[test]
fn stream_info_fixture_matches_expected_shape() {
    let payload =
        serde_json::from_slice(&fs::read(fixture_path("twitch.stream_info.json")).unwrap())
            .unwrap();
    let info = parse_stream_info(&payload).unwrap();
    assert_eq!(info.id, "stream-1");
    assert_eq!(info.title, "Test title");
    assert_eq!(info.game_name, "Game Name");
    assert_eq!(info.game_id.as_deref(), Some("game-1"));
    assert_eq!(info.viewers_count, 42);
    assert_eq!(info.tags, vec!["tag-1", "tag-2"]);
}

#[test]
fn live_status_fixture_covers_offline_shape() {
    let payload =
        serde_json::from_slice(&fs::read(fixture_path("twitch.stream_live.offline.json")).unwrap())
            .unwrap();
    assert!(!parse_live_status(&payload));
}

#[test]
fn followers_fixture_matches_expected_page_shape() {
    let payload =
        serde_json::from_slice(&fs::read(fixture_path("twitch.followers.json")).unwrap()).unwrap();
    let page = parse_followers_page(&payload).unwrap();
    assert_eq!(page.logins, vec!["alice", "bob"]);
    assert!(page.has_next_page);
    assert_eq!(page.cursor.as_deref(), Some("cursor-2"));
}

#[test]
fn inventory_fixture_matches_expected_drop_listing() {
    let payload =
        serde_json::from_slice(&fs::read(fixture_path("twitch.inventory.json")).unwrap()).unwrap();
    let drops = parse_inventory_drops(&payload);
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].campaign_name, "Campaign");
    assert_eq!(drops[0].reward_name, "Reward");
    assert_eq!(drops[0].drop_instance_id, "drop-1");
    assert_eq!(drops[0].current_minutes_watched, 30);
    assert_eq!(drops[0].required_minutes_watched, 60);
    assert!(!drops[0].is_claimed);
}

#[test]
fn available_drop_campaign_fixture_matches_expected_ids() {
    let payload = serde_json::from_slice(
        &fs::read(fixture_path("twitch.available_drop_campaigns.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parse_available_drop_campaign_ids(&payload),
        vec!["campaign-1", "campaign-2"]
    );
}

#[test]
fn contribution_fixture_matches_expected_lookup_shape() {
    let payload = serde_json::from_slice(
        &fs::read(fixture_path("twitch.user_points_contribution.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parse_user_points_contributions(&payload),
        vec![(String::from("goal-1"), 25), (String::from("goal-2"), 10)]
    );
}
