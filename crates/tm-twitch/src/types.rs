use std::collections::BTreeMap;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tm_domain::{ActiveMultiplier, CommunityGoal, OffsetDateTime};

use crate::{GQL_URL, PLAYBACK_URL, TWITCH_URL};

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
    #[error("playback request failed for {context}: {failure:?}")]
    PlaybackRequest {
        context: &'static str,
        failure: TwitchFailureClass,
    },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol response decode failed for {context}: {detail} ({shape})")]
    ProtocolDecode {
        context: String,
        detail: String,
        shape: String,
    },
    #[error("twitch contract error: {0}")]
    Contract(#[from] TwitchContractError),
    #[error("unexpected status {status} for {context}")]
    UnexpectedStatus {
        status: StatusCode,
        context: &'static str,
    },
    #[error("missing response field: {0}")]
    MissingField(&'static str),
    #[error("invalid response field: {0}")]
    InvalidField(&'static str),
    #[error("graphql errors for {context}: {errors}")]
    GqlErrors { context: String, errors: String },
    #[error("mutation rejected for {context}: {detail}")]
    MutationRejected { context: String, detail: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwitchFailureClass {
    Unauthorized,
    RateLimited,
    ServerError,
    Timeout,
    ConnectionReset,
    Other,
}

impl TwitchClientError {
    #[must_use]
    pub fn failure_class(&self) -> TwitchFailureClass {
        match self {
            Self::UnexpectedStatus { status, .. } if *status == StatusCode::UNAUTHORIZED => {
                TwitchFailureClass::Unauthorized
            }
            Self::UnexpectedStatus { status, .. } if *status == StatusCode::TOO_MANY_REQUESTS => {
                TwitchFailureClass::RateLimited
            }
            Self::UnexpectedStatus { status, .. } if status.is_server_error() => {
                TwitchFailureClass::ServerError
            }
            Self::Http(error) if error.is_timeout() => TwitchFailureClass::Timeout,
            Self::Http(error) if error.is_connect() || error.is_request() => {
                TwitchFailureClass::ConnectionReset
            }
            Self::PlaybackRequest { failure, .. } => *failure,
            _ => TwitchFailureClass::Other,
        }
    }
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
    pub created_at: Option<tm_domain::OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchStreakMilestone {
    pub value: Option<u32>,
    pub achievement_timestamp: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedVideo {
    pub id: String,
    pub broadcast_id: Option<String>,
    pub length_seconds: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecentClip {
    pub id: String,
    pub slug: String,
    pub url: String,
    pub duration_seconds: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GqlResponse<T> {
    pub(crate) data: Option<T>,
    #[serde(default)]
    pub(crate) errors: Option<Vec<GqlError>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GqlError {
    #[serde(rename = "message")]
    pub(crate) _message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserIdData {
    pub(crate) user: Option<UserIdUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserIdUser {
    pub(crate) id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserLoginData {
    pub(crate) user: Option<UserLoginUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserLoginUser {
    pub(crate) id: Option<String>,
    pub(crate) login: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LiveStatusData {
    pub(crate) user: Option<LiveStatusUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LiveStatusUser {
    pub(crate) stream: Option<LiveStatusStream>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LiveStatusStream {
    pub(crate) _id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChannelPointsData {
    pub(crate) community: Option<CommunityData>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommunityData {
    pub(crate) channel: Option<ChannelPointsChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChannelPointsChannel {
    #[serde(rename = "self")]
    pub(crate) self_data: Option<ChannelSelfData>,
    #[serde(rename = "communityPointsSettings")]
    pub(crate) settings: Option<CommunityPointsSettings>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ChannelSelfData {
    #[serde(rename = "communityPoints")]
    pub(crate) points: Option<CommunityPointsData>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommunityPointsData {
    pub(crate) balance: Option<i64>,
    #[serde(rename = "availableClaim")]
    pub(crate) available_claim: Option<AvailableClaim>,
    #[serde(rename = "activeMultipliers", default)]
    pub(crate) active_multipliers: Vec<ActiveMultiplier>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AvailableClaim {
    pub(crate) id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommunityPointsSettings {
    #[serde(default)]
    pub(crate) goals: Vec<ProtocolCommunityGoal>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProtocolCommunityGoal {
    pub(crate) id: Option<String>,
    pub(crate) title: Option<String>,
    #[serde(alias = "isInStock", default)]
    pub(crate) is_in_stock: bool,
    #[serde(alias = "pointsContributed")]
    pub(crate) points_contributed: Option<i64>,
    #[serde(alias = "amountNeeded", alias = "goal_amount")]
    pub(crate) amount_needed: Option<i64>,
    #[serde(
        alias = "perStreamUserMaximumContribution",
        alias = "per_stream_maximum_user_contribution"
    )]
    pub(crate) per_stream_user_maximum_contribution: Option<i64>,
    pub(crate) status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StreamInfoData {
    pub(crate) user: Option<StreamInfoUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PlaybackAccessTokenData {
    #[serde(rename = "streamPlaybackAccessToken")]
    pub(crate) stream_playback_access_token: Option<PlaybackAccessToken>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PlaybackAccessToken {
    pub(crate) signature: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StreamInfoUser {
    #[serde(rename = "broadcastSettings")]
    pub(crate) broadcast_settings: Option<BroadcastSettings>,
    pub(crate) stream: Option<ProtocolStream>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BroadcastSettings {
    pub(crate) title: Option<String>,
    pub(crate) game: Option<ProtocolGame>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProtocolGame {
    pub(crate) id: Option<String>,
    #[serde(rename = "displayName")]
    pub(crate) display_name: Option<String>,
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProtocolStream {
    pub(crate) id: Option<String>,
    #[serde(rename = "createdAt")]
    pub(crate) created_at: Option<String>,
    #[serde(rename = "viewersCount")]
    pub(crate) viewers_count: Option<u64>,
    #[serde(default)]
    pub(crate) tags: Vec<ProtocolTag>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RewardListData {
    pub(crate) channel: Option<RewardListChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RewardListChannel {
    #[serde(rename = "self")]
    pub(crate) self_data: Option<RewardListSelf>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RewardListSelf {
    #[serde(rename = "watchStreakMilestone")]
    pub(crate) watch_streak_milestone: Option<WatchStreakMilestoneEnvelope>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WatchStreakMilestoneEnvelope {
    #[serde(rename = "watchStreakMilestone")]
    pub(crate) milestone: Option<WatchStreakMilestoneData>,
    #[serde(rename = "expiresAt")]
    pub(crate) expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WatchStreakMilestoneData {
    pub(crate) value: Option<String>,
    #[serde(rename = "achievementTimestamp")]
    pub(crate) achievement_timestamp: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ArchivedVideosData {
    pub(crate) user: Option<ArchivedVideosUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ArchivedVideosUser {
    pub(crate) videos: Option<ArchivedVideosConnection>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ArchivedVideosConnection {
    #[serde(default)]
    pub(crate) edges: Vec<ArchivedVideoEdge>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ArchivedVideoEdge {
    pub(crate) node: Option<ArchivedVideoNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ArchivedVideoNode {
    pub(crate) id: Option<String>,
    #[serde(rename = "broadcastIdentifier")]
    pub(crate) broadcast_identifier: Option<BroadcastIdentifier>,
    #[serde(rename = "lengthSeconds")]
    pub(crate) length_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BroadcastIdentifier {
    pub(crate) id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecentClipsData {
    pub(crate) user: Option<RecentClipsUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecentClipsUser {
    pub(crate) clips: Option<RecentClipsConnection>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecentClipsConnection {
    #[serde(default)]
    pub(crate) edges: Vec<RecentClipEdge>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecentClipEdge {
    pub(crate) node: Option<RecentClipNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecentClipNode {
    pub(crate) id: Option<String>,
    pub(crate) slug: Option<String>,
    pub(crate) url: Option<String>,
    #[serde(rename = "durationSeconds")]
    pub(crate) duration_seconds: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProtocolTag {
    pub(crate) id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FollowersData {
    pub(crate) user: Option<FollowersUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FollowersUser {
    pub(crate) follows: Option<FollowersConnection>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FollowersConnection {
    pub(crate) edges: Option<Vec<FollowerEdge>>,
    #[serde(rename = "pageInfo")]
    pub(crate) page_info: Option<PageInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FollowerEdge {
    pub(crate) cursor: Option<String>,
    pub(crate) node: Option<FollowerNode>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FollowerNode {
    pub(crate) login: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PageInfo {
    #[serde(rename = "hasNextPage", default)]
    pub(crate) has_next_page: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InventoryData {
    #[serde(rename = "currentUser")]
    pub(crate) current_user: Option<InventoryUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InventoryUser {
    pub(crate) inventory: Option<InventoryState>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InventoryState {
    #[serde(rename = "dropCampaignsInProgress")]
    pub(crate) campaigns: Option<Vec<InventoryCampaign>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InventoryCampaign {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(rename = "displayName", default)]
    pub(crate) display_name: Option<String>,
    #[serde(rename = "timeBasedDrops", default)]
    pub(crate) drops: Vec<InventoryTimeDrop>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InventoryTimeDrop {
    #[serde(default)]
    pub(crate) name: Option<String>,
    pub(crate) benefit: Option<InventoryBenefit>,
    #[serde(rename = "self")]
    pub(crate) self_data: Option<InventoryDropSelf>,
    #[serde(rename = "requiredMinutesWatched", default)]
    pub(crate) required_minutes_watched: Option<i64>,
    #[serde(rename = "requiredProgress", default)]
    pub(crate) required_progress: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InventoryBenefit {
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InventoryDropSelf {
    #[serde(rename = "dropInstanceID")]
    pub(crate) drop_instance_id: Option<String>,
    #[serde(rename = "currentMinutesWatched", default)]
    pub(crate) current_minutes_watched: Option<i64>,
    #[serde(rename = "currentProgress", default)]
    pub(crate) current_progress: Option<i64>,
    #[serde(rename = "isClaimed")]
    pub(crate) is_claimed: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AvailableDropsData {
    pub(crate) channel: Option<AvailableDropsChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AvailableDropsChannel {
    #[serde(rename = "viewerDropCampaigns")]
    pub(crate) campaigns: Option<Vec<AvailableDropCampaign>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AvailableDropCampaign {
    pub(crate) id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ViewerDropsDashboard {
    /// Twitch changes this experimental dashboard shape frequently. The
    /// envelope is typed while unknown fields are intentionally retained for
    /// forward compatibility and are never logged.
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserContributionData {
    pub(crate) user: Option<UserContributionUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserContributionUser {
    pub(crate) channel: Option<UserContributionChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserContributionChannel {
    #[serde(rename = "self")]
    pub(crate) self_data: Option<UserContributionSelf>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserContributionSelf {
    #[serde(rename = "communityPoints")]
    pub(crate) community_points: Option<UserContributionPoints>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserContributionPoints {
    #[serde(rename = "goalContributions", default)]
    pub(crate) contributions: Vec<UserContribution>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserContribution {
    pub(crate) goal: Option<UserContributionGoal>,
    #[serde(rename = "userPointsContributedThisStream")]
    pub(crate) points: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserContributionGoal {
    pub(crate) id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct EmptyMutationData {}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClaimBonusData {
    #[serde(rename = "claimCommunityPoints")]
    pub(crate) claim: Option<ClaimBonusMutation>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClaimBonusMutation {
    pub(crate) status: Option<String>,
    pub(crate) error: Option<MutationError>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClaimDropData {
    #[serde(rename = "claimDropRewards")]
    pub(crate) claim: Option<ClaimDropMutation>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClaimDropMutation {
    pub(crate) status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommunityGoalContributionData {
    #[serde(rename = "contributeCommunityPointsCommunityGoal")]
    pub(crate) contribution: Option<CommunityGoalContributionMutation>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommunityGoalContributionMutation {
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MutationError {
    pub(crate) message: Option<String>,
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
    pub playback_url: String,
}

impl Default for TwitchEndpoints {
    fn default() -> Self {
        Self {
            twitch_url: TWITCH_URL.to_string(),
            gql_url: GQL_URL.to_string(),
            playback_url: PLAYBACK_URL.to_string(),
        }
    }
}
