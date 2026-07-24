#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(raw) = std::str::from_utf8(data) else {
        return;
    };
    let streamers = [];
    let _ = tm_pubsub::parse_transport_message(raw, &streamers);
    let _ = tm_pubsub::parse_message(raw, &streamers);
    let _ = tm_pubsub::parse_eventsub_message(raw, &streamers);
});
