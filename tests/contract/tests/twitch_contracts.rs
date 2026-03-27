use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use tm_domain::Stream;
use tm_twitch::{
    extract_build_id, extract_settings_script_url, extract_spade_url, gql_headers,
    minute_watched_request, operation_names, operations, parse_channel_points_context,
    parse_inventory_drops, parse_stream_info,
};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name)
}

#[test]
fn gql_headers_and_operation_names_match_expected_contract() {
    let operation = operations::get_id_from_login("tester");
    let payload = serde_json::to_value(&operation).unwrap();
    let headers = gql_headers("token", "session", "build-id", "ua", "device");

    assert_eq!(headers["Authorization"], "OAuth token");
    assert_eq!(headers["Client-Id"], tm_twitch::CLIENT_ID);
    assert_eq!(headers["Client-Session-Id"], "session");
    assert_eq!(headers["Client-Version"], "build-id");
    assert_eq!(headers["User-Agent"], "ua");
    assert_eq!(headers["X-Device-Id"], "device");
    assert_eq!(operation_names(&payload), vec!["GetIDFromLogin"]);
}

#[test]
fn minute_watched_payload_contract_is_form_url_encoded() {
    let stream = Stream {
        payload: vec![json!({
            "event": "minute-watched",
            "properties": {"channel": "alpha"}
        })],
        ..Stream::default()
    };

    let request = minute_watched_request("ua", "https://spade.example/submit", &stream).unwrap();
    assert_eq!(request.url, "https://spade.example/submit");
    assert_eq!(request.content_type, "application/x-www-form-urlencoded");
    assert_eq!(request.user_agent, "ua");
    assert!(request.body.starts_with("data="));
}

#[test]
fn fixture_backed_contracts_cover_homepage_settings_and_core_gql_shapes() {
    let homepage = fs::read_to_string(fixture_path("twitch.homepage.html")).unwrap();
    assert_eq!(
        extract_build_id(&homepage).unwrap(),
        "ef928475-9403-42f2-8a34-55784bd08e16"
    );
    assert_eq!(
        extract_settings_script_url(&homepage).unwrap(),
        "https://static.twitchcdn.net/config/settings.123.js"
    );

    let settings = fs::read_to_string(fixture_path("twitch.settings.js")).unwrap();
    assert_eq!(
        extract_spade_url(&settings).unwrap(),
        "https://spade.example/submit"
    );

    let context_payload =
        serde_json::from_slice(&fs::read(fixture_path("twitch.channel_points_context.json")).unwrap())
            .unwrap();
    let context = parse_channel_points_context(&context_payload).unwrap();
    assert_eq!(context.balance, 1234);
    assert_eq!(context.claim_id.as_deref(), Some("claim-1"));

    let stream_payload =
        serde_json::from_slice(&fs::read(fixture_path("twitch.stream_info.json")).unwrap()).unwrap();
    let stream_info = parse_stream_info(&stream_payload).unwrap();
    assert_eq!(stream_info.id, "stream-1");
    assert_eq!(stream_info.game_name, "Game Name");

    let inventory_payload =
        serde_json::from_slice(&fs::read(fixture_path("twitch.inventory.json")).unwrap()).unwrap();
    let drops = parse_inventory_drops(&inventory_payload);
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].drop_instance_id, "drop-1");
}
