use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use reqwest::StatusCode;
use serde::Serialize;
use thiserror::Error;
use tm_domain::{ActiveMultiplier, CommunityGoal, Stream};

pub const TWITCH_URL: &str = "https://www.twitch.tv";
pub const GQL_URL: &str = "https://gql.twitch.tv/gql";
pub const CLIENT_ID: &str = "ue6666qo983tsx6so1t0vnawi233wa";
pub const DROP_ID: &str = "c2542d6d-cd10-4532-919b-3d19f30a768b";
pub const DEFAULT_CLIENT_VERSION: &str = "ef928475-9403-42f2-8a34-55784bd08e16";
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
static BUILD_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"window\.__twilightBuildID\s*=\s*"([0-9a-fA-F\-]{36})""#)
        .expect("build id regex must compile")
});
static SETTINGS_SCRIPT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(https://static\.twitchcdn\.net/config/settings.*?\.js|https://assets\.twitch\.tv/config/settings.*?\.js)",
    )
    .expect("settings script regex must compile")
});
static SPADE_URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""spade_url":"(.*?)""#).expect("spade url regex must compile"));

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
    GqlErrors {
        context: String,
        errors: String,
    },
    #[error("mutation rejected for {context}: {detail}")]
    MutationRejected {
        context: String,
        detail: String,
    },
}

#[derive(Debug)]
pub struct TwitchClient {
    client: reqwest::Client,
    auth_token: String,
    default_cookie_header: Option<String>,
    client_session: String,
    device_id: String,
    user_agent: String,
    client_version: Mutex<CachedClientVersion>,
    endpoints: TwitchEndpoints,
}

#[derive(Debug, Clone)]
struct CachedClientVersion {
    value: String,
    fetched_at: Option<Instant>,
    ttl: Duration,
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

impl TwitchClient {
    pub fn new(
        auth_token: impl Into<String>,
        user_agent: impl Into<String>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self::with_client(client, auth_token, user_agent))
    }

    #[must_use]
    pub fn with_client(
        client: reqwest::Client,
        auth_token: impl Into<String>,
        user_agent: impl Into<String>,
    ) -> Self {
        Self::with_client_and_cookie_header_and_endpoints(
            client,
            auth_token,
            user_agent,
            None,
            TwitchEndpoints::default(),
        )
    }

    #[must_use]
    pub fn with_client_and_cookie_header(
        client: reqwest::Client,
        auth_token: impl Into<String>,
        user_agent: impl Into<String>,
        default_cookie_header: Option<String>,
    ) -> Self {
        Self::with_client_and_cookie_header_and_endpoints(
            client,
            auth_token,
            user_agent,
            default_cookie_header,
            TwitchEndpoints::default(),
        )
    }

    #[must_use]
    pub fn with_client_and_endpoints(
        client: reqwest::Client,
        auth_token: impl Into<String>,
        user_agent: impl Into<String>,
        endpoints: TwitchEndpoints,
    ) -> Self {
        Self::with_client_and_cookie_header_and_endpoints(
            client, auth_token, user_agent, None, endpoints,
        )
    }

    #[must_use]
    pub fn with_client_and_cookie_header_and_endpoints(
        client: reqwest::Client,
        auth_token: impl Into<String>,
        user_agent: impl Into<String>,
        default_cookie_header: Option<String>,
        endpoints: TwitchEndpoints,
    ) -> Self {
        Self {
            client,
            auth_token: auth_token.into().trim().to_string(),
            default_cookie_header: default_cookie_header
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            client_session: generate_client_session_id(),
            device_id: generate_device_id(),
            user_agent: user_agent.into().trim().to_string(),
            client_version: Mutex::new(CachedClientVersion {
                value: DEFAULT_CLIENT_VERSION.to_string(),
                fetched_at: None,
                ttl: Duration::from_secs(10 * 60 * 60),
            }),
            endpoints,
        }
    }

    #[must_use]
    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    #[must_use]
    pub fn client_session_id(&self) -> &str {
        &self.client_session
    }

    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    #[must_use]
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub async fn update_client_version(&self) -> Result<String, TwitchClientError> {
        {
            let cache = self
                .client_version
                .lock()
                .expect("client version lock poisoned");
            if cache
                .fetched_at
                .is_some_and(|fetched_at| fetched_at.elapsed() < cache.ttl)
            {
                return Ok(cache.value.clone());
            }
        }

        let response = self
            .client
            .get(&self.endpoints.twitch_url)
            .header("User-Agent", self.user_agent());
        let response =
            if let Some(cookie) = self.request_cookie_header(&self.endpoints.twitch_url, None) {
                response.header("Cookie", cookie).send()
            } else {
                response.send()
            }
            .await?;
        if !response.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: response.status(),
                context: "fetch homepage",
            });
        }
        let build_id = extract_build_id(&response.text().await?)?;
        let mut cache = self
            .client_version
            .lock()
            .expect("client version lock poisoned");
        cache.value.clone_from(&build_id);
        cache.fetched_at = Some(Instant::now());
        Ok(build_id)
    }

    pub async fn fetch_settings_script_url(
        &self,
        page_url: &str,
    ) -> Result<String, TwitchClientError> {
        let response = self
            .client
            .get(page_url)
            .header("User-Agent", self.user_agent());
        let response = if let Some(cookie) = self.request_cookie_header(page_url, None) {
            response.header("Cookie", cookie).send()
        } else {
            response.send()
        }
        .await?;
        if !response.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: response.status(),
                context: "fetch settings page",
            });
        }
        Ok(extract_settings_script_url(&response.text().await?)?)
    }

    pub async fn fetch_spade_url(&self, channel_login: &str) -> Result<String, TwitchClientError> {
        let page_url = format!(
            "{}/{}",
            self.endpoints.twitch_url.trim_end_matches('/'),
            channel_login.trim().to_lowercase()
        );
        let settings_url = self.fetch_settings_script_url(&page_url).await?;
        let response = self
            .client
            .get(&settings_url)
            .header("User-Agent", self.user_agent());
        let response = if let Some(cookie) = self.request_cookie_header(&settings_url, None) {
            response.header("Cookie", cookie).send()
        } else {
            response.send()
        }
        .await?;
        if !response.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: response.status(),
                context: "fetch settings script",
            });
        }
        Ok(extract_spade_url(&response.text().await?)?)
    }

    pub async fn post_gql(
        &self,
        operation: &GqlPersistedOperation,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql_value(serde_json::to_value(operation)?).await
    }

    pub async fn post_gql_batch(
        &self,
        operations: &[GqlPersistedOperation],
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql_value(serde_json::to_value(operations)?).await
    }

    pub async fn fetch_channel_id(&self, login: &str) -> Result<String, TwitchClientError> {
        let response = self.post_gql(&operations::get_id_from_login(login)).await?;
        response
            .pointer("/data/user/id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or(TwitchClientError::MissingField("data.user.id"))
    }

    pub async fn fetch_channel_points_context(
        &self,
        channel_login: &str,
    ) -> Result<ChannelPointsContext, TwitchClientError> {
        let response = self
            .post_gql(&operations::channel_points_context(channel_login))
            .await?;
        parse_channel_points_context(&response)
    }

    pub async fn is_stream_live(&self, channel_id: &str) -> Result<bool, TwitchClientError> {
        let response = self
            .post_gql(&operations::is_stream_live(channel_id))
            .await?;
        Ok(parse_live_status(&response))
    }

    pub async fn fetch_stream_info(
        &self,
        channel_login: &str,
    ) -> Result<StreamInfo, TwitchClientError> {
        let response = self
            .post_gql(&operations::stream_info_overlay(channel_login))
            .await?;
        parse_stream_info(&response)
    }

    pub async fn fetch_followers(
        &self,
        limit: u32,
        order: &str,
    ) -> Result<Vec<String>, TwitchClientError> {
        let mut cursor = None::<String>;
        let mut followers = Vec::new();

        loop {
            let mut operation = operations::channel_follows(limit, order);
            if let Some(cursor) = cursor.as_ref() {
                operation
                    .variables
                    .as_object_mut()
                    .expect("channel follows variables must be an object")
                    .insert(
                        "cursor".to_string(),
                        serde_json::Value::String(cursor.clone()),
                    );
            }

            let response = self.post_gql(&operation).await?;
            let page = parse_followers_page(&response)?;
            followers.extend(page.logins);
            let has_next = page.has_next_page;
            cursor = page.cursor;
            if !has_next || cursor.is_none() {
                break;
            }
        }

        Ok(followers)
    }

    pub async fn claim_bonus(
        &self,
        channel_id: &str,
        claim_id: &str,
        user_id: Option<&str>,
    ) -> Result<ClaimBonusOutcome, TwitchClientError> {
        let cookie = claim_bonus_cookie_header(&self.auth_token, user_id.unwrap_or_default());
        let response = self
            .post_validated_mutation_value_with_cookie(
                serde_json::to_value(operations::claim_community_points(channel_id, claim_id))?,
                cookie.as_deref(),
            )
            .await?;
        validate_claim_bonus_response(&response)
    }

    pub async fn claim_moment(
        &self,
        moment_id: &str,
    ) -> Result<(), TwitchClientError> {
        self.post_validated_mutation(&operations::community_moment_claim(moment_id))
            .await?;
        Ok(())
    }

    pub async fn join_raid(&self, raid_id: &str) -> Result<(), TwitchClientError> {
        self.post_validated_mutation(&operations::join_raid(raid_id))
            .await?;
        Ok(())
    }

    pub async fn make_prediction(
        &self,
        event_id: &str,
        outcome_id: &str,
        points: i64,
    ) -> Result<(), TwitchClientError> {
        self.post_validated_mutation(&operations::make_prediction(
            event_id,
            outcome_id,
            points,
            &generate_transaction_id(),
        ))
        .await?;
        Ok(())
    }

    pub async fn fetch_inventory(&self) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::inventory()).await
    }

    pub async fn fetch_claimable_drops(&self) -> Result<Vec<InventoryDrop>, TwitchClientError> {
        let response = self.fetch_inventory().await?;
        Ok(parse_inventory_drops(&response))
    }

    pub async fn fetch_viewer_drops_dashboard(
        &self,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::viewer_drops_dashboard()).await
    }

    pub async fn claim_drop(
        &self,
        drop_instance_id: &str,
    ) -> Result<ClaimDropOutcome, TwitchClientError> {
        let response = self
            .post_validated_mutation(&operations::claim_drop_rewards(drop_instance_id))
            .await?;
        validate_claim_drop_response(&response)
    }

    pub async fn fetch_available_drop_campaigns(
        &self,
        channel_id: &str,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::drops_highlight_service_available(channel_id))
            .await
    }

    pub async fn fetch_available_drop_campaign_ids(
        &self,
        channel_id: &str,
    ) -> Result<Vec<String>, TwitchClientError> {
        let response = self.fetch_available_drop_campaigns(channel_id).await?;
        Ok(parse_available_drop_campaign_ids(&response))
    }

    pub async fn fetch_user_points_contribution(
        &self,
        channel_login: &str,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::user_points_contribution(channel_login))
            .await
    }

    pub async fn contribute_community_goal(
        &self,
        amount: i64,
        channel_id: &str,
        goal_id: &str,
    ) -> Result<(), TwitchClientError> {
        let response = self
            .post_validated_mutation(&operations::contribute_community_goal(
                amount,
                channel_id,
                goal_id,
                &generate_transaction_id(),
            ))
            .await?;
        validate_community_goal_response(&response)
    }

    pub async fn send_minute_watched(
        &self,
        spade_url: &str,
        stream: &Stream,
    ) -> Result<StatusCode, TwitchClientError> {
        let request = minute_watched_request(self.user_agent(), spade_url, stream)?;
        let response = self
            .client
            .post(request.url)
            .header("Content-Type", request.content_type)
            .header("User-Agent", request.user_agent)
            .body(request.body)
            .send()
            .await?;
        Ok(response.status())
    }

    async fn post_gql_value(
        &self,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql_value_with_cookie(payload, None).await
    }

    async fn post_gql_value_with_cookie(
        &self,
        payload: serde_json::Value,
        cookie: Option<&str>,
    ) -> Result<serde_json::Value, TwitchClientError> {
        let client_version = self.update_client_version().await?;
        let mut request = self
            .client
            .post(&self.endpoints.gql_url)
            .header("Authorization", format!("OAuth {}", self.auth_token()))
            .header("Client-Id", CLIENT_ID)
            .header("Client-Session-Id", self.client_session_id())
            .header("Client-Version", client_version)
            .header("User-Agent", self.user_agent())
            .header("X-Device-Id", self.device_id())
            .header("Content-Type", "application/json")
            .json(&payload);
        if let Some(cookie) = self.request_cookie_header(&self.endpoints.gql_url, cookie) {
            request = request.header("Cookie", cookie);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: response.status(),
                context: "post gql",
            });
        }
        Ok(response.json().await?)
    }

    async fn post_validated_mutation(
        &self,
        operation: &GqlPersistedOperation,
    ) -> Result<serde_json::Value, TwitchClientError> {
        let payload = self.post_gql(operation).await?;
        validate_gql_mutation_response(operation.operation_name, &payload)?;
        Ok(payload)
    }

    async fn post_validated_mutation_value_with_cookie(
        &self,
        payload: serde_json::Value,
        cookie: Option<&str>,
    ) -> Result<serde_json::Value, TwitchClientError> {
        let operation_name = operation_name(&payload).unwrap_or_else(|| String::from("mutation"));
        let payload = self.post_gql_value_with_cookie(payload, cookie).await?;
        validate_gql_mutation_response(&operation_name, &payload)?;
        Ok(payload)
    }

    fn request_cookie_header(&self, url: &str, cookie: Option<&str>) -> Option<String> {
        let default_cookie = is_twitch_cookie_url(url)
            .then_some(self.default_cookie_header.as_deref())
            .flatten();
        merge_cookie_headers(default_cookie, cookie)
    }
}

pub mod operations {
    use serde_json::json;

    use super::GqlPersistedOperation;

    #[must_use]
    pub fn get_id_from_login(login: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "GetIDFromLogin",
            "94e82a7b1e3c21e186daa73ee2afc4b8f23bade1fbbff6fe8ac133f50a2f58ca",
            json!({ "login": login }),
        )
    }

    #[must_use]
    pub fn channel_follows(limit: u32, order: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "ChannelFollows",
            "eecf815273d3d949e5cf0085cc5084cd8a1b5b7b6f7990cf43cb0beadf546907",
            json!({ "limit": limit, "order": order }),
        )
    }

    #[must_use]
    pub fn channel_points_context(channel_login: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "ChannelPointsContext",
            "374314de591e69925fce3ddc2bcf085796f56ebb8cad67a0daa3165c03adc345",
            json!({ "channelLogin": channel_login }),
        )
    }

    #[must_use]
    pub fn is_stream_live(channel_id: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "WithIsStreamLiveQuery",
            "04e46329a6786ff3a81c01c50bfa5d725902507a0deb83b0edbf7abe7a3716ea",
            json!({ "id": channel_id }),
        )
    }

    #[must_use]
    pub fn stream_info_overlay(channel_login: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "VideoPlayerStreamInfoOverlayChannel",
            "e785b65ff71ad7b363b34878335f27dd9372869ad0c5740a130b9268bcdbe7e7",
            json!({ "channel": channel_login.to_lowercase() }),
        )
    }

    #[must_use]
    pub fn claim_community_points(channel_id: &str, claim_id: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "ClaimCommunityPoints",
            "46aaeebe02c99afdf4fc97c7c0cba964124bf6b0af229395f1f6d1feed05b3d0",
            json!({ "input": { "channelID": channel_id, "claimID": claim_id } }),
        )
    }

    #[must_use]
    pub fn community_moment_claim(moment_id: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "CommunityMomentCallout_Claim",
            "e2d67415aead910f7f9ceb45a77b750a1e1d9622c936d832328a0689e054db62",
            json!({ "input": { "momentID": moment_id } }),
        )
    }

    #[must_use]
    pub fn join_raid(raid_id: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "JoinRaid",
            "c6a332a86d1087fbbb1a8623aa01bd1313d2386e7c63be60fdb2d1901f01a4ae",
            json!({ "input": { "raidID": raid_id } }),
        )
    }

    #[must_use]
    pub fn make_prediction(
        event_id: &str,
        outcome_id: &str,
        points: i64,
        transaction_id: &str,
    ) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "MakePrediction",
            "b44682ecc88358817009f20e69d75081b1e58825bb40aa53d5dbadcc17c881d8",
            json!({
                "input": {
                    "eventID": event_id,
                    "outcomeID": outcome_id,
                    "points": points,
                    "transactionID": transaction_id
                }
            }),
        )
    }

    #[must_use]
    pub fn inventory() -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "Inventory",
            "d86775d0ef16a63a33ad52e80eaff963b2d5b72fada7c991504a57496e1d8e4b",
            json!({ "fetchRewardCampaigns": true }),
        )
    }

    #[must_use]
    pub fn viewer_drops_dashboard() -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "ViewerDropsDashboard",
            "5a4da2ab3d5b47c9f9ce864e727b2cb346af1e3ea8b897fe8f704a97ff017619",
            json!({ "fetchRewardCampaigns": true }),
        )
    }

    #[must_use]
    pub fn claim_drop_rewards(drop_instance_id: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "DropsPage_ClaimDropRewards",
            "a455deea71bdc9015b78eb49f4acfbce8baa7ccbedd28e549bb025bd0f751930",
            json!({ "input": { "dropInstanceID": drop_instance_id } }),
        )
    }

    #[must_use]
    pub fn drops_highlight_service_available(channel_id: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "DropsHighlightService_AvailableDrops",
            "782dad0f032942260171d2d80a654f88bdd0c5a9dddc392e9bc92218a0f42d20",
            json!({ "channelID": channel_id }),
        )
    }

    #[must_use]
    pub fn user_points_contribution(channel_login: &str) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "UserPointsContribution",
            "23ff2c2d60708379131178742327ead913b93b1bd6f665517a6d9085b73f661f",
            json!({ "channelLogin": channel_login }),
        )
    }

    #[must_use]
    pub fn contribute_community_goal(
        amount: i64,
        channel_id: &str,
        goal_id: &str,
        transaction_id: &str,
    ) -> GqlPersistedOperation {
        GqlPersistedOperation::new(
            "ContributeCommunityPointsCommunityGoal",
            "5774f0ea5d89587d73021a2e03c3c44777d903840c608754a1be519f51e37bb6",
            json!({
                "input": {
                    "amount": amount,
                    "channelID": channel_id,
                    "goalID": goal_id,
                    "transactionID": transaction_id
                }
            }),
        )
    }
}

#[must_use]
pub fn gql_headers(
    auth_token: &str,
    client_session: &str,
    client_version: &str,
    user_agent: &str,
    device_id: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".into(), format!("OAuth {auth_token}")),
        ("Client-Id".into(), CLIENT_ID.into()),
        ("Client-Session-Id".into(), client_session.into()),
        ("Client-Version".into(), client_version.into()),
        ("User-Agent".into(), user_agent.into()),
        ("X-Device-Id".into(), device_id.into()),
        ("Content-Type".into(), "application/json".into()),
    ])
}

pub fn gql_request(
    auth_token: &str,
    client_session: &str,
    client_version: &str,
    user_agent: &str,
    device_id: &str,
    operation: &GqlPersistedOperation,
) -> Result<GqlRequest, serde_json::Error> {
    Ok(GqlRequest {
        url: GQL_URL.to_string(),
        headers: gql_headers(
            auth_token,
            client_session,
            client_version,
            user_agent,
            device_id,
        ),
        body: serde_json::to_string(operation)?,
    })
}

pub fn gql_batch_request(
    auth_token: &str,
    client_session: &str,
    client_version: &str,
    user_agent: &str,
    device_id: &str,
    operations: &[GqlPersistedOperation],
) -> Result<GqlRequest, serde_json::Error> {
    Ok(GqlRequest {
        url: GQL_URL.to_string(),
        headers: gql_headers(
            auth_token,
            client_session,
            client_version,
            user_agent,
            device_id,
        ),
        body: serde_json::to_string(operations)?,
    })
}

#[must_use]
pub fn claim_bonus_cookie_header(auth_token: &str, user_id: &str) -> Option<String> {
    match (auth_token.trim(), user_id.trim()) {
        ("", _) => None,
        (token, "") => Some(format!("auth-token={token}")),
        (token, persistent) => Some(format!("auth-token={token}; persistent={persistent}")),
    }
}

fn is_twitch_cookie_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(is_twitch_cookie_host))
        .unwrap_or(false)
}

fn is_twitch_cookie_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('.').to_lowercase();
    host == "twitch.tv" || host.ends_with(".twitch.tv")
}

fn merge_cookie_headers(default_cookie: Option<&str>, cookie: Option<&str>) -> Option<String> {
    let mut order = Vec::new();
    let mut values = HashMap::new();

    for source in [default_cookie, cookie] {
        for segment in source.into_iter().flat_map(|value| value.split(';')) {
            let Some((name, value)) = segment.trim().split_once('=') else {
                continue;
            };
            let name = name.trim();
            let value = value.trim();
            if name.is_empty() || value.is_empty() {
                continue;
            }
            if !values.contains_key(name) {
                order.push(name.to_string());
            }
            values.insert(name.to_string(), value.to_string());
        }
    }

    (!order.is_empty()).then(|| {
        order
            .into_iter()
            .filter_map(|name| values.get(&name).map(|value| format!("{name}={value}")))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

pub fn extract_build_id(html: &str) -> Result<String, TwitchContractError> {
    BUILD_ID_REGEX
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
        .ok_or(TwitchContractError::BuildIdNotFound)
}

pub fn extract_settings_script_url(html: &str) -> Result<String, TwitchContractError> {
    SETTINGS_SCRIPT_REGEX
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
        .ok_or(TwitchContractError::SettingsScriptNotFound)
}

pub fn extract_spade_url(settings_js: &str) -> Result<String, TwitchContractError> {
    SPADE_URL_REGEX
        .captures(settings_js)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
        .ok_or(TwitchContractError::SpadeUrlNotFound)
}

pub fn minute_watched_request(
    user_agent: &str,
    spade_url: &str,
    stream: &Stream,
) -> Result<MinuteWatchedRequest, serde_json::Error> {
    let encoded = stream.encode_payload()?;
    Ok(MinuteWatchedRequest {
        url: spade_url.to_string(),
        content_type: "application/x-www-form-urlencoded".to_string(),
        user_agent: user_agent.to_string(),
        body: format!("data={}", url_encode(&encoded["data"])),
    })
}

#[must_use]
pub fn community_goal_contribution_amount(
    goal: &CommunityGoal,
    user_points_this_stream: i64,
    local_channel_points: i64,
) -> i64 {
    let user_left = goal.per_stream_user_maximum_contribution - user_points_this_stream;
    [goal.amount_left(), user_left, local_channel_points]
        .into_iter()
        .min()
        .unwrap_or_default()
        .max(0)
}

pub fn operation_names(payload: &serde_json::Value) -> Vec<String> {
    match payload {
        serde_json::Value::Array(items) => items.iter().filter_map(operation_name).collect(),
        _ => operation_name(payload).into_iter().collect(),
    }
}

pub fn parse_channel_points_context(
    payload: &serde_json::Value,
) -> Result<ChannelPointsContext, TwitchClientError> {
    let channel = payload
        .pointer("/data/community/channel")
        .ok_or(TwitchClientError::MissingField("data.community.channel"))?;
    let balance = channel
        .pointer("/self/communityPoints/balance")
        .and_then(serde_json::Value::as_i64)
        .ok_or(TwitchClientError::MissingField(
            "data.community.channel.self.communityPoints.balance",
        ))?;
    let claim_id = channel
        .pointer("/self/communityPoints/availableClaim/id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let active_multiplier_count = channel
        .pointer("/self/communityPoints/activeMultipliers")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let active_multipliers = channel
        .pointer("/self/communityPoints/activeMultipliers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|multiplier| {
            serde_json::from_value::<ActiveMultiplier>(multiplier.clone()).ok()
        })
        .collect();
    let community_goals = channel
        .pointer("/communityPointsSettings/goals")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|goal| serde_json::from_value::<CommunityGoal>(normalize_goal_value(goal)).ok())
        .collect();

    Ok(ChannelPointsContext {
        balance,
        claim_id,
        active_multiplier_count,
        active_multipliers,
        community_goals,
    })
}

pub fn parse_stream_info(payload: &serde_json::Value) -> Result<StreamInfo, TwitchClientError> {
    let user = payload
        .pointer("/data/user")
        .ok_or(TwitchClientError::MissingField("data.user"))?;
    let stream = user
        .get("stream")
        .ok_or(TwitchClientError::MissingField("data.user.stream"))?;
    let title = user
        .pointer("/broadcastSettings/title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let game = user.pointer("/broadcastSettings/game");
    let tags = stream
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tag| {
            tag.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();

    Ok(StreamInfo {
        id: stream
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or(TwitchClientError::MissingField("data.user.stream.id"))?
            .to_string(),
        title,
        game_name: game
            .and_then(|game| game.get("displayName").or_else(|| game.get("name")))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        game_id: game
            .and_then(|game| game.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        viewers_count: u32::try_from(
            stream
                .get("viewersCount")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
        )
        .unwrap_or(u32::MAX),
        tags,
    })
}

#[must_use]
pub fn parse_live_status(payload: &serde_json::Value) -> bool {
    payload
        .pointer("/data/user/stream")
        .is_some_and(|value| !value.is_null())
}

pub fn parse_followers_page(
    payload: &serde_json::Value,
) -> Result<FollowersPage, TwitchClientError> {
    let follows = payload
        .pointer("/data/user/follows")
        .ok_or(TwitchClientError::MissingField("data.user.follows"))?;
    let edges = follows
        .get("edges")
        .and_then(serde_json::Value::as_array)
        .ok_or(TwitchClientError::MissingField("data.user.follows.edges"))?;

    Ok(FollowersPage {
        logins: edges
            .iter()
            .filter_map(|edge| {
                edge.pointer("/node/login")
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_lowercase)
            .collect(),
        has_next_page: follows
            .pointer("/pageInfo/hasNextPage")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        cursor: edges
            .last()
            .and_then(|edge| edge.get("cursor"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

pub fn parse_inventory_drops(payload: &serde_json::Value) -> Vec<InventoryDrop> {
    payload
        .pointer("/data/currentUser/inventory/dropCampaignsInProgress")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|campaign| {
            let campaign_name = campaign
                .get("name")
                .or_else(|| campaign.get("displayName"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            campaign
                .get("timeBasedDrops")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |drop| {
                    let drop_instance_id = drop
                        .pointer("/self/dropInstanceID")
                        .and_then(serde_json::Value::as_str)?
                        .to_string();
                    Some(InventoryDrop {
                        drop_instance_id,
                        reward_name: drop
                            .get("name")
                            .or_else(|| drop.pointer("/benefit/name"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        campaign_name: campaign_name.clone(),
                        current_minutes_watched: drop
                            .pointer("/self/currentMinutesWatched")
                            .or_else(|| drop.pointer("/self/currentProgress"))
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or_default(),
                        required_minutes_watched: drop
                            .get("requiredMinutesWatched")
                            .or_else(|| drop.get("requiredProgress"))
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or_default(),
                        is_claimed: drop
                            .pointer("/self/isClaimed")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn parse_available_drop_campaign_ids(payload: &serde_json::Value) -> Vec<String> {
    payload
        .pointer("/data/channel/viewerDropCampaigns")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|campaign| {
            campaign
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

pub fn parse_user_points_contributions(payload: &serde_json::Value) -> Vec<(String, i64)> {
    payload
        .pointer("/data/user/channel/self/communityPoints/goalContributions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let goal_id = item
                .pointer("/goal/id")
                .and_then(serde_json::Value::as_str)?;
            Some((
                goal_id.to_string(),
                item.get("userPointsContributedThisStream")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default(),
            ))
        })
        .collect()
}

pub fn validate_gql_mutation_response(
    context: &str,
    payload: &serde_json::Value,
) -> Result<(), TwitchClientError> {
    let Some(errors) = payload.get("errors") else {
        return Ok(());
    };
    if matches!(errors, serde_json::Value::Null) {
        return Ok(());
    }
    if errors.as_array().is_some_and(Vec::is_empty) {
        return Ok(());
    }
    Err(TwitchClientError::GqlErrors {
        context: context.to_string(),
        errors: errors.to_string(),
    })
}

pub fn validate_claim_bonus_response(
    payload: &serde_json::Value,
) -> Result<ClaimBonusOutcome, TwitchClientError> {
    let claim = payload
        .pointer("/data/claimCommunityPoints")
        .ok_or(TwitchClientError::MissingField("data.claimCommunityPoints"))?;
    if let Some(message) = payload
        .pointer("/data/claimCommunityPoints/error/message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Err(TwitchClientError::MutationRejected {
            context: String::from("ClaimCommunityPoints"),
            detail: format!("claim bonus error: {message}"),
        });
    }
    match claim
        .get("status")
        .and_then(serde_json::Value::as_str)
        .map(|status| status.trim().to_uppercase())
        .as_deref()
    {
        Some("SUCCESS") | None => Ok(ClaimBonusOutcome::Claimed),
        Some("ALREADY_CLAIMED") => Ok(ClaimBonusOutcome::AlreadyClaimed),
        Some(status) => Err(TwitchClientError::MutationRejected {
            context: String::from("ClaimCommunityPoints"),
            detail: format!("unexpected claim bonus status {status}"),
        }),
    }
}

pub fn validate_claim_drop_response(
    payload: &serde_json::Value,
) -> Result<ClaimDropOutcome, TwitchClientError> {
    match payload
        .pointer("/data/claimDropRewards/status")
        .and_then(serde_json::Value::as_str)
        .map(|status| status.trim().to_uppercase())
        .as_deref()
    {
        Some("ELIGIBLE_FOR_ALL") => Ok(ClaimDropOutcome::EligibleForAll),
        Some("DROP_INSTANCE_ALREADY_CLAIMED") => Ok(ClaimDropOutcome::AlreadyClaimed),
        Some(status) => Err(TwitchClientError::MutationRejected {
            context: String::from("DropsPage_ClaimDropRewards"),
            detail: format!("unexpected drop claim status {status}"),
        }),
        None => Err(TwitchClientError::MissingField(
            "data.claimDropRewards.status",
        )),
    }
}

pub fn validate_community_goal_response(
    payload: &serde_json::Value,
) -> Result<(), TwitchClientError> {
    if let Some(error) = payload
        .pointer("/data/contributeCommunityPointsCommunityGoal/error")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        return Err(TwitchClientError::MutationRejected {
            context: String::from("ContributeCommunityPointsCommunityGoal"),
            detail: format!("community goal error: {error}"),
        });
    }
    Ok(())
}

fn normalize_goal_value(goal: &serde_json::Value) -> serde_json::Value {
    let Some(goal) = goal.as_object() else {
        return goal.clone();
    };

    let mut normalized = serde_json::Map::with_capacity(goal.len());
    for (key, value) in goal {
        let normalized_key = match key.as_str() {
            "isInStock" => "is_in_stock",
            "pointsContributed" => "points_contributed",
            "amountNeeded" | "goal_amount" => "amount_needed",
            "perStreamUserMaximumContribution" | "per_stream_maximum_user_contribution" => {
                "per_stream_user_maximum_contribution"
            }
            other => other,
        };
        normalized.insert(normalized_key.to_string(), value.clone());
    }
    serde_json::Value::Object(normalized)
}

fn operation_name(value: &serde_json::Value) -> Option<String> {
    value
        .get("operationName")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_build_id_from_homepage() {
        let html =
            r#"<script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#;
        assert_eq!(
            extract_build_id(html).unwrap(),
            "ef928475-9403-42f2-8a34-55784bd08e16"
        );
    }

    #[test]
    fn extracts_settings_script_and_spade_url() {
        let html = r#"<script src="https://static.twitchcdn.net/config/settings.123.js"></script>"#;
        assert_eq!(
            extract_settings_script_url(html).unwrap(),
            "https://static.twitchcdn.net/config/settings.123.js"
        );

        let settings = r#"window.__settings={"spade_url":"https://spade.example/submit"}"#;
        assert_eq!(
            extract_spade_url(settings).unwrap(),
            "https://spade.example/submit"
        );
    }

    #[test]
    fn builds_gql_headers_like_go() {
        let headers = gql_headers("token", "session", "version", "ua", "device");
        assert_eq!(headers["Authorization"], "OAuth token");
        assert_eq!(headers["Client-Id"], CLIENT_ID);
        assert_eq!(headers["Client-Session-Id"], "session");
        assert_eq!(headers["Client-Version"], "version");
        assert_eq!(headers["User-Agent"], "ua");
        assert_eq!(headers["X-Device-Id"], "device");
    }

    #[test]
    fn builds_single_and_batch_gql_requests() {
        let request = gql_request(
            "token",
            "session",
            "version",
            "ua",
            "device",
            &operations::get_id_from_login("tester"),
        )
        .unwrap();
        assert_eq!(request.url, GQL_URL);
        assert_eq!(request.headers["Authorization"], "OAuth token");
        assert!(request
            .body
            .contains("\"operationName\":\"GetIDFromLogin\""));

        let batch = gql_batch_request(
            "token",
            "session",
            "version",
            "ua",
            "device",
            &[
                operations::inventory(),
                operations::viewer_drops_dashboard(),
                operations::claim_drop_rewards("drop-1"),
            ],
        )
        .unwrap();
        assert!(batch.body.starts_with('['));
        assert!(batch.body.contains("ViewerDropsDashboard"));
        assert!(batch.body.contains("DropsPage_ClaimDropRewards"));
    }

    #[test]
    fn builds_claim_bonus_cookie_header() {
        assert_eq!(
            claim_bonus_cookie_header("token", "user").unwrap(),
            "auth-token=token; persistent=user"
        );
        assert_eq!(
            claim_bonus_cookie_header("token", "").unwrap(),
            "auth-token=token"
        );
        assert!(claim_bonus_cookie_header("", "").is_none());
    }

    #[test]
    fn merges_default_and_explicit_cookie_headers() {
        assert_eq!(
            merge_cookie_headers(
                Some("session=abc; auth-token=token"),
                Some("auth-token=override; persistent=user")
            )
            .unwrap(),
            "session=abc; auth-token=override; persistent=user"
        );
        assert_eq!(
            merge_cookie_headers(Some("session=abc"), None).unwrap(),
            "session=abc"
        );
        assert!(merge_cookie_headers(None, None).is_none());
    }

    #[test]
    fn cookie_urls_match_twitch_hosts_only() {
        assert!(is_twitch_cookie_url("https://gql.twitch.tv/gql"));
        assert!(is_twitch_cookie_url("https://www.twitch.tv/some-channel"));
        assert!(!is_twitch_cookie_url(
            "https://static.twitchcdn.net/config/settings.js"
        ));
        assert!(!is_twitch_cookie_url("not-a-url"));
    }

    #[test]
    fn builds_minute_watched_form_request() {
        let stream = Stream {
            payload: vec![serde_json::json!({
                "event": "minute-watched",
                "properties": { "broadcast_id": "123" }
            })],
            ..Stream::default()
        };
        let request = minute_watched_request("ua", "https://spade.example", &stream).unwrap();
        assert_eq!(request.url, "https://spade.example");
        assert_eq!(request.content_type, "application/x-www-form-urlencoded");
        assert_eq!(request.user_agent, "ua");
        assert!(request.body.starts_with("data="));
    }

    #[test]
    fn calculates_community_goal_contribution_amount() {
        let goal = CommunityGoal {
            amount_needed: 500,
            points_contributed: 100,
            per_stream_user_maximum_contribution: 50,
            ..CommunityGoal::default()
        };
        assert_eq!(community_goal_contribution_amount(&goal, 10, 1_000), 40);
        assert_eq!(community_goal_contribution_amount(&goal, 60, 1_000), 0);
        assert_eq!(community_goal_contribution_amount(&goal, 0, 20), 20);
    }

    #[test]
    fn persisted_operations_match_expected_names() {
        assert_eq!(
            operations::get_id_from_login("abc").operation_name,
            "GetIDFromLogin"
        );
        assert_eq!(
            operations::channel_follows(100, "ASC").operation_name,
            "ChannelFollows"
        );
        assert_eq!(
            operations::claim_community_points("1", "2").operation_name,
            "ClaimCommunityPoints"
        );
        assert_eq!(operations::join_raid("raid").operation_name, "JoinRaid");
        assert_eq!(
            operations::make_prediction("e", "o", 10, "tx").operation_name,
            "MakePrediction"
        );
        assert_eq!(
            operations::is_stream_live("1").operation_name,
            "WithIsStreamLiveQuery"
        );
        assert_eq!(
            operations::stream_info_overlay("abc").operation_name,
            "VideoPlayerStreamInfoOverlayChannel"
        );
        assert_eq!(
            operations::claim_drop_rewards("drop").operation_name,
            "DropsPage_ClaimDropRewards"
        );
        assert_eq!(
            operations::viewer_drops_dashboard().operation_name,
            "ViewerDropsDashboard"
        );
        assert_eq!(
            operations::drops_highlight_service_available("1").operation_name,
            "DropsHighlightService_AvailableDrops"
        );
        assert_eq!(operations::inventory().operation_name, "Inventory");
    }

    #[test]
    fn operation_names_extracts_single_and_batch_payloads() {
        let single =
            serde_json::to_value(operations::get_id_from_login("tester")).expect("serialize");
        assert_eq!(operation_names(&single), vec!["GetIDFromLogin"]);

        let batch = serde_json::json!([
            operations::inventory(),
            operations::viewer_drops_dashboard()
        ]);
        assert_eq!(
            operation_names(&batch),
            vec!["Inventory", "ViewerDropsDashboard"]
        );
    }

    #[test]
    fn generated_ids_match_expected_lengths() {
        assert_eq!(generate_device_id().len(), 32);
        assert_eq!(generate_client_session_id().len(), 16);
        assert_eq!(generate_transaction_id().len(), 32);
    }

    #[test]
    fn gql_mutation_validation_rejects_top_level_errors() {
        let error = validate_gql_mutation_response(
            "ClaimCommunityPoints",
            &serde_json::json!({
                "errors": [{ "message": "boom" }]
            }),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            TwitchClientError::GqlErrors { context, .. } if context == "ClaimCommunityPoints"
        ));
    }

    #[test]
    fn claim_bonus_validation_accepts_expected_statuses() {
        assert_eq!(
            validate_claim_bonus_response(&serde_json::json!({
                "data": {
                    "claimCommunityPoints": {
                        "status": "SUCCESS",
                        "error": { "message": "" }
                    }
                }
            }))
            .unwrap(),
            ClaimBonusOutcome::Claimed
        );
        assert_eq!(
            validate_claim_bonus_response(&serde_json::json!({
                "data": {
                    "claimCommunityPoints": {
                        "status": "already_claimed",
                        "error": { "message": null }
                    }
                }
            }))
            .unwrap(),
            ClaimBonusOutcome::AlreadyClaimed
        );
        assert_eq!(
            validate_claim_bonus_response(&serde_json::json!({
                "data": {
                    "claimCommunityPoints": {
                        "balance": 1550
                    }
                }
            }))
            .unwrap(),
            ClaimBonusOutcome::Claimed
        );
    }

    #[test]
    fn claim_bonus_validation_rejects_errors_and_unknown_statuses() {
        let error = validate_claim_bonus_response(&serde_json::json!({
            "data": {
                "claimCommunityPoints": {
                    "status": "SUCCESS",
                    "error": { "message": "already used" }
                }
            }
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            TwitchClientError::MutationRejected { context, .. } if context == "ClaimCommunityPoints"
        ));

        let error = validate_claim_bonus_response(&serde_json::json!({
            "data": {
                "claimCommunityPoints": {
                    "status": "DENIED",
                    "error": { "message": "" }
                }
            }
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            TwitchClientError::MutationRejected { context, .. } if context == "ClaimCommunityPoints"
        ));
    }

    #[test]
    fn claim_drop_validation_accepts_expected_statuses() {
        assert_eq!(
            validate_claim_drop_response(&serde_json::json!({
                "data": {
                    "claimDropRewards": {
                        "status": "ELIGIBLE_FOR_ALL"
                    }
                }
            }))
            .unwrap(),
            ClaimDropOutcome::EligibleForAll
        );
        assert_eq!(
            validate_claim_drop_response(&serde_json::json!({
                "data": {
                    "claimDropRewards": {
                        "status": "drop_instance_already_claimed"
                    }
                }
            }))
            .unwrap(),
            ClaimDropOutcome::AlreadyClaimed
        );
    }

    #[test]
    fn claim_drop_validation_rejects_unknown_statuses() {
        let error = validate_claim_drop_response(&serde_json::json!({
            "data": {
                "claimDropRewards": {
                    "status": "INELIGIBLE"
                }
            }
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            TwitchClientError::MutationRejected { context, .. }
                if context == "DropsPage_ClaimDropRewards"
        ));
    }

    #[test]
    fn community_goal_validation_rejects_error_field() {
        let error = validate_community_goal_response(&serde_json::json!({
            "data": {
                "contributeCommunityPointsCommunityGoal": {
                    "error": "goal closed"
                }
            }
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            TwitchClientError::MutationRejected { context, .. }
                if context == "ContributeCommunityPointsCommunityGoal"
        ));
    }

    #[test]
    fn twitch_client_defaults_are_initialized() {
        let client = TwitchClient::new("token", "ua").unwrap();
        assert_eq!(client.auth_token(), "token");
        assert_eq!(client.user_agent(), "ua");
        assert_eq!(client.client_session_id().len(), 16);
        assert_eq!(client.device_id().len(), 32);
    }

    #[test]
    fn twitch_client_accepts_cookie_header_and_endpoint_overrides() {
        let client = TwitchClient::with_client_and_cookie_header_and_endpoints(
            reqwest::Client::builder().build().unwrap(),
            "token",
            "ua",
            Some(String::from("session=abc")),
            TwitchEndpoints {
                twitch_url: String::from("http://127.0.0.1:1234"),
                gql_url: String::from("http://127.0.0.1:1234/gql"),
            },
        );
        assert_eq!(client.auth_token(), "token");
        assert_eq!(client.user_agent(), "ua");
        assert_eq!(
            client.request_cookie_header("http://127.0.0.1:1234/gql", None),
            None
        );
        assert_eq!(
            client.request_cookie_header("https://gql.twitch.tv/gql", None),
            Some(String::from("session=abc"))
        );
    }

    #[test]
    fn parses_channel_points_context_shape() {
        let payload = serde_json::json!({
            "data": {
                "community": {
                    "channel": {
                        "self": {
                            "communityPoints": {
                                "balance": 1234,
                                "availableClaim": { "id": "claim-1" },
                                "activeMultipliers": [{ "factor": 1.5 }, { "factor": 2.0 }]
                            }
                        },
                        "communityPointsSettings": {
                            "goals": [{
                                "id": "goal-1",
                                "title": "Goal",
                                "is_in_stock": true,
                                "points_contributed": 5,
                                "goal_amount": 10,
                                "per_stream_maximum_user_contribution": 5,
                                "status": "ACTIVE"
                            }]
                        }
                    }
                }
            }
        });
        let context = parse_channel_points_context(&payload).unwrap();
        assert_eq!(context.balance, 1234);
        assert_eq!(context.claim_id.as_deref(), Some("claim-1"));
        assert_eq!(context.active_multiplier_count, 2);
        assert_eq!(context.active_multipliers.len(), 2);
        assert!((context.active_multipliers[0].factor - 1.5).abs() < f64::EPSILON);
        assert_eq!(context.community_goals.len(), 1);
    }

    #[test]
    fn parses_stream_info_shape() {
        let payload = serde_json::json!({
            "data": {
                "user": {
                    "broadcastSettings": {
                        "title": "Test title",
                        "game": {
                            "id": "game-1",
                            "displayName": "Game Name"
                        }
                    },
                    "stream": {
                        "id": "stream-1",
                        "viewersCount": 42,
                        "tags": [{ "id": "tag-1" }, { "id": "tag-2" }]
                    }
                }
            }
        });
        let info = parse_stream_info(&payload).unwrap();
        assert_eq!(info.id, "stream-1");
        assert_eq!(info.title, "Test title");
        assert_eq!(info.game_name, "Game Name");
        assert_eq!(info.game_id.as_deref(), Some("game-1"));
        assert_eq!(info.viewers_count, 42);
        assert_eq!(info.tags, vec!["tag-1", "tag-2"]);
    }

    #[test]
    fn parses_inventory_drop_listing() {
        let payload = serde_json::json!({
            "data": {
                "currentUser": {
                    "inventory": {
                        "dropCampaignsInProgress": [{
                            "name": "Campaign",
                            "timeBasedDrops": [{
                                "name": "Reward",
                                "requiredMinutesWatched": 60,
                                "self": {
                                    "dropInstanceID": "drop-1",
                                    "currentMinutesWatched": 30,
                                    "isClaimed": false
                                }
                            }]
                        }]
                    }
                }
            }
        });
        let drops = parse_inventory_drops(&payload);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].campaign_name, "Campaign");
        assert_eq!(drops[0].reward_name, "Reward");
        assert_eq!(drops[0].drop_instance_id, "drop-1");
        assert_eq!(drops[0].current_minutes_watched, 30);
        assert_eq!(drops[0].required_minutes_watched, 60);
        assert!(!drops[0].is_claimed);
    }

    #[test]
    fn parses_available_drop_campaign_ids() {
        let payload = serde_json::json!({
            "data": {
                "channel": {
                    "viewerDropCampaigns": [
                        { "id": "campaign-1" },
                        { "id": "campaign-2" }
                    ]
                }
            }
        });
        assert_eq!(
            parse_available_drop_campaign_ids(&payload),
            vec!["campaign-1", "campaign-2"]
        );
    }
}
