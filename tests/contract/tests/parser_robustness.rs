use tm_domain::Streamer;

fn generated_inputs(count: usize) -> impl Iterator<Item = Vec<u8>> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..count).map(move |index| {
        let length = index % 1024;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    })
}

#[test]
fn untrusted_protocol_parsers_do_not_panic_on_arbitrary_bytes() {
    let streamers = [Streamer::default()];
    for bytes in generated_inputs(2_000) {
        let text = String::from_utf8_lossy(&bytes);

        let _ = tm_auth::decode_cookie_store(&bytes);
        let _ = serde_json::from_slice::<tm_config::ConfigFile>(&bytes);
        let _ = tm_irc::parse_line(&text);
        let _ = tm_pubsub::parse_message(&text, &streamers);
        let _ = tm_pubsub::parse_transport_message(&text, &streamers);
        let _ = tm_twitch::extract_build_id(&text);
        let _ = tm_twitch::extract_settings_script_url(&text);
        let _ = tm_twitch::extract_spade_url(&text);

        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let _ = tm_twitch::operation_names(&value);
            let _ = tm_twitch::parse_channel_points_context(&value);
            let _ = tm_twitch::parse_stream_info(&value);
            let _ = tm_twitch::parse_live_status(&value);
            let _ = tm_twitch::parse_followers_page(&value);
            let _ = tm_twitch::parse_inventory_drops(&value);
            let _ = tm_twitch::parse_available_drop_campaign_ids(&value);
            let _ = tm_twitch::parse_user_points_contributions(&value);
        }
    }
}
