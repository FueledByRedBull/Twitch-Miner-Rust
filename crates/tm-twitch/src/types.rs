use std::collections::BTreeMap;

use reqwest::StatusCode;
use serde::Serialize;
use thiserror::Error;
use tm_domain::{ActiveMultiplier, CommunityGoal};

use crate::{GQL_URL, TWITCH_URL};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TwitchContractError {
    #[error("build id not found")]
    BuildIdNotFound,
    #[error("settings script not found")]
    SettingsScriptNotFound,
    #[error("spade url not found")]
    SpadeUrlNotFound,
}

#[derive(Debug, Error)]
pub enum TwitchClientError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("twitch contract error: {0}")]
    Contract(#[from] TwitchContractError),
    #[error("unexpected status {status} for {context}")]
    UnexpectedStatus {
        status: StatusCode,
        context: &'static str,
    },
    #[error("missing response field: {0}")]
    MissingField(&'static str),
    #[error("graphql errors for {context}: {errors}")]
    GqlErrors { context: String, errors: String },
    #[error("mutation rejected for {context}: {detail}")]
    MutationRejected { context: String, detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GqlPersistedQuery {
    pub version: u8,
    #[serde(rename = "sha256Hash")]
    pub sha256_hash: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GqlPersistedExtensions {
    #[serde(rename = "persistedQuery")]
    pub persisted_query: GqlPersistedQuery,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GqlPersistedOperation {
    #[serde(rename = "operationName")]
    pub operation_name: &'static str,
    pub variables: serde_json::Value,
    pub extensions: GqlPersistedExtensions,
}

impl GqlPersistedOperation {
    #[must_use]
    pub fn new(
        operation_name: &'static str,
        sha256_hash: &'static str,
        variables: serde_json::Value,
    ) -> Self {
        Self {
            operation_name,
            variables,
            extensions: GqlPersistedExtensions {
                persisted_query: GqlPersistedQuery {
                    version: 1,
                    sha256_hash,
                },
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinuteWatchedRequest {
    pub url: String,
    pub content_type: String,
    pub user_agent: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GqlRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelPointsContext {
    pub balance: i64,
    pub claim_id: Option<String>,
    pub active_multiplier_count: usize,
    pub active_multipliers: Vec<ActiveMultiplier>,
    pub community_goals: Vec<CommunityGoal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub id: String,
    pub title: String,
    pub game_name: String,
    pub game_id: Option<String>,
    pub viewers_count: u32,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowersPage {
    pub logins: Vec<String>,
    pub has_next_page: bool,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryDrop {
    pub drop_instance_id: String,
    pub reward_name: String,
    pub campaign_name: String,
    pub current_minutes_watched: i64,
    pub required_minutes_watched: i64,
    pub is_claimed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimBonusOutcome {
    Claimed,
    AlreadyClaimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDropOutcome {
    EligibleForAll,
    AlreadyClaimed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwitchEndpoints {
    pub twitch_url: String,
    pub gql_url: String,
}

impl Default for TwitchEndpoints {
    fn default() -> Self {
        Self {
            twitch_url: TWITCH_URL.to_string(),
            gql_url: GQL_URL.to_string(),
        }
    }
}
