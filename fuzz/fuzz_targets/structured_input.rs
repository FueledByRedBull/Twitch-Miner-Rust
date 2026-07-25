#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tm_auth::decode_cookie_store(data);
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };

    let _ = tm_twitch::parse_channel_points_context(&value);
    let _ = tm_twitch::parse_stream_info(&value);
    let _ = tm_twitch::parse_followers_page(&value);
    let _ = tm_twitch::parse_inventory_drops(&value);
    let _ = tm_twitch::parse_available_drop_campaign_ids(&value);
    let _ = tm_twitch::parse_user_points_contributions(&value);
    let _ = tm_twitch::validate_gql_mutation_response("fuzz", &value);
    let _ = tm_twitch::validate_claim_bonus_response(&value);
    let _ = tm_twitch::validate_claim_drop_response(&value);
    let _ = tm_twitch::validate_community_goal_response(&value);

    if let Ok(config) = serde_json::from_value::<tm_config::ConfigFile>(value) {
        let _ = tm_config::validate_config(&config);
    }
});
