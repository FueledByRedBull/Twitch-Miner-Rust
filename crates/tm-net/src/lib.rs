//! Outbound connection policy: the one place that decides how the miner
//! reaches the network.
//!
//! A configured proxy carries every connection and there is no direct
//! fallback, so an unavailable proxy fails the connection instead of exposing
//! the host's address. Environment proxy variables (`HTTP_PROXY`,
//! `HTTPS_PROXY`, `ALL_PROXY`) are never consulted; only the `proxy` setting
//! counts.
//!
//! Other crates build HTTP clients, TCP streams and `WebSocket`s through
//! [`NetworkPolicy`] and do not open sockets themselves. A source scan in this
//! crate's tests enforces that.

mod policy;
mod proxy;
mod tunnel;

pub use policy::NetworkPolicy;
pub use proxy::{ProxyConfig, ProxyParseError, ProxyScheme};
pub use tunnel::ProxyFailure;

#[cfg(test)]
#[path = "../tests/unit/proxy_tests.rs"]
mod proxy_tests;

#[cfg(test)]
#[path = "../tests/unit/policy_tests.rs"]
mod policy_tests;
