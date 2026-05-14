use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

#[must_use]
pub fn generate_device_id() -> String {
    generate_hex_id(32)
}

#[must_use]
pub fn generate_client_session_id() -> String {
    generate_hex_id(16)
}

#[must_use]
pub fn generate_transaction_id() -> String {
    generate_hex_id(32)
}

fn generate_hex_id(len: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = format!("{nanos:032x}{counter:016x}");
    while seed.len() < len {
        seed.push('0');
    }
    seed[..len].to_string()
}
