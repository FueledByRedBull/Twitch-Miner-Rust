//! Typed Twitch HTTP, GraphQL, playback, and contract adapters.
//!
//! Read-only operations may use bounded retries for classified transient
//! failures. Points-changing mutations are never replayed after an uncertain
//! response, required financial/claim fields fail closed, and diagnostics expose
//! only redacted response shapes.

pub const TWITCH_URL: &str = "https://www.twitch.tv";
pub const GQL_URL: &str = "https://gql.twitch.tv/gql";
pub const PLAYBACK_URL: &str = "https://usher.ttvnw.net/api/channel/hls/";
pub const CLIENT_ID: &str = "ue6666qo983tsx6so1t0vnawi233wa";
pub const DROP_ID: &str = "c2542d6d-cd10-4532-919b-3d19f30a768b";

mod client;
mod contracts;
mod cookies;
mod gql;
mod ids;
pub mod operations;
mod parsers;
mod responses;
mod types;

pub use client::TwitchClient;
pub use contracts::{extract_build_id, extract_settings_script_url, extract_spade_url};
pub use cookies::claim_bonus_cookie_header;
pub use gql::{gql_batch_request, gql_headers, gql_request};
pub use ids::{generate_client_session_id, generate_device_id, generate_transaction_id};
pub use operations::{PersistedOperationContract, PERSISTED_OPERATION_CONTRACTS};
pub use parsers::{
    community_goal_contribution_amount, minute_watched_request, operation_names,
    parse_available_drop_campaign_ids, parse_channel_points_context, parse_followers_page,
    parse_inventory_drops, parse_live_status, parse_stream_info, parse_user_points_contributions,
    validate_claim_bonus_response, validate_claim_drop_response, validate_community_goal_response,
    validate_gql_mutation_response,
};
pub use types::{
    ArchivedVideo, ChannelPointsContext, ClaimBonusOutcome, ClaimDropOutcome, FollowersPage,
    GqlPersistedExtensions, GqlPersistedOperation, GqlPersistedQuery, GqlRequest, InventoryDrop,
    InventorySnapshot, MinuteWatchedRequest, RecentClip, StreamInfo, TwitchClientError,
    TwitchContractError, TwitchEndpoints, TwitchFailureClass, ViewerDropsDashboard,
    WatchStreakMilestone,
};

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
mod tests;
