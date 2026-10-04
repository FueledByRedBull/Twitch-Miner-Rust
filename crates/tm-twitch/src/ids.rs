use std::collections::hash_map::RandomState;
use std::fmt::Write;
use std::hash::BuildHasher;
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
    let mut id = String::with_capacity(len + 16);
    while id.len() < len {
        // Writing to a String cannot fail.
        let _ = write!(id, "{:016x}", random_u64());
    }
    id.truncate(len);
    id
}

// std's RandomState keys come from OS randomness, so identifiers differ across
// processes; the counter and clock only keep calls within a process distinct.
fn random_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    RandomState::new().hash_one((nanos, counter))
}
