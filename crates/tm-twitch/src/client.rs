use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use time::format_description::well_known::{Rfc2822, Rfc3339};
use time::OffsetDateTime;
use tm_domain::Stream;

use crate::contracts::{extract_build_id, extract_settings_script_url, extract_spade_url};
use crate::cookies::{claim_bonus_cookie_header, is_twitch_cookie_url, merge_cookie_headers};
use crate::ids::{generate_client_session_id, generate_device_id, generate_transaction_id};
use crate::parsers::minute_watched_request;
use crate::types::{
    AvailableDropsData, ChannelPointsContext, ClaimBonusData, ClaimBonusOutcome, ClaimDropData,
    ClaimDropOutcome, CommunityGoalContributionData, EmptyMutationData, FollowersData,
    GqlPersistedOperation, GqlResponse, InventoryData, InventoryDrop, LiveStatusData,
    RewardListData, StreamInfo, StreamInfoData, TwitchClientError, TwitchEndpoints,
    UserContributionData, UserIdData, UserLoginData, ViewerDropsDashboard,
};
use crate::{operations, CLIENT_ID, DEFAULT_CLIENT_VERSION};

const MAX_READ_ATTEMPTS: usize = 3;
const READ_RETRY_BASE: Duration = Duration::from_millis(250);
const MAX_READ_RETRY_DELAY: Duration = Duration::from_secs(30);

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
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cache
                .fetched_at
                .is_some_and(|fetched_at| fetched_at.elapsed() < cache.ttl)
            {
                return Ok(cache.value.clone());
            }
        }

        let cookie = self.request_cookie_header(&self.endpoints.twitch_url, None);
        let response = self
            .send_read_request(
                || {
                    let mut request = self
                        .client
                        .get(&self.endpoints.twitch_url)
                        .header("User-Agent", self.user_agent());
                    if let Some(cookie) = cookie.as_deref() {
                        request = request.header("Cookie", cookie);
                    }
                    request
                },
                "fetch homepage",
            )
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
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.value.clone_from(&build_id);
        cache.fetched_at = Some(Instant::now());
        Ok(build_id)
    }

    pub async fn fetch_settings_script_url(
        &self,
        page_url: &str,
    ) -> Result<String, TwitchClientError> {
        let cookie = self.request_cookie_header(page_url, None);
        let response = self
            .send_read_request(
                || {
                    let mut request = self
                        .client
                        .get(page_url)
                        .header("User-Agent", self.user_agent());
                    if let Some(cookie) = cookie.as_deref() {
                        request = request.header("Cookie", cookie);
                    }
                    request
                },
                "fetch settings page",
            )
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
        let cookie = self.request_cookie_header(&settings_url, None);
        let response = self
            .send_read_request(
                || {
                    let mut request = self
                        .client
                        .get(&settings_url)
                        .header("User-Agent", self.user_agent());
                    if let Some(cookie) = cookie.as_deref() {
                        request = request.header("Cookie", cookie);
                    }
                    request
                },
                "fetch settings script",
            )
            .await?;
        if !response.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: response.status(),
                context: "fetch settings script",
            });
        }
        Ok(extract_spade_url(&response.text().await?)?)
    }

    pub async fn fetch_channel_id(&self, login: &str) -> Result<String, TwitchClientError> {
        let response: UserIdData = self
            .post_gql_typed(&operations::get_id_from_login(login))
            .await?;
        response
            .user
            .and_then(|user| user.id)
            .filter(|id| !id.trim().is_empty())
            .ok_or(TwitchClientError::MissingField("data.user.id"))
    }

    pub async fn fetch_channel_login_by_id(
        &self,
        channel_id: &str,
    ) -> Result<String, TwitchClientError> {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            return Err(TwitchClientError::InvalidField("channel_id"));
        }
        let operation = serde_json::json!({
            "operationName": "ResolveLoginById",
            "query": "query ResolveLoginById($id: ID!) { user(id: $id) { id login } }",
            "variables": { "id": channel_id }
        });
        let payload = self.post_gql_value(operation).await?;
        let response: UserLoginData = decode_gql_data(&payload, "ResolveLoginById")?;
        let user = response
            .user
            .ok_or(TwitchClientError::MissingField("data.user"))?;
        if user.id.as_deref() != Some(channel_id) {
            return Err(TwitchClientError::InvalidField("data.user.id"));
        }
        user.login
            .map(|login| login.trim().to_ascii_lowercase())
            .filter(|login| !login.is_empty())
            .ok_or(TwitchClientError::MissingField("data.user.login"))
    }

    /// Low-level compatibility escape hatch for protocol experiments.
    /// High-value runtime paths use typed methods below.
    pub async fn post_gql(
        &self,
        operation: &GqlPersistedOperation,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql_value_with_cookie(
            serde_json::to_value(operation)?,
            None,
            operation_is_read_only(operation.operation_name),
        )
        .await
    }

    /// Low-level compatibility escape hatch for protocol experiments.
    pub async fn post_gql_batch(
        &self,
        operations: &[GqlPersistedOperation],
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql_value_with_cookie(
            serde_json::to_value(operations)?,
            None,
            !operations.is_empty()
                && operations
                    .iter()
                    .all(|operation| operation_is_read_only(operation.operation_name)),
        )
        .await
    }

    pub async fn fetch_channel_points_context(
        &self,
        channel_login: &str,
    ) -> Result<ChannelPointsContext, TwitchClientError> {
        let response: crate::types::ChannelPointsData = self
            .post_gql_typed(&operations::channel_points_context(channel_login))
            .await?;
        channel_points_context_from_typed(response)
    }

    pub async fn is_stream_live(&self, channel_id: &str) -> Result<bool, TwitchClientError> {
        let response: LiveStatusData = self
            .post_gql_typed(&operations::is_stream_live(channel_id))
            .await?;
        Ok(response.user.and_then(|user| user.stream).is_some())
    }

    pub async fn fetch_stream_info(
        &self,
        channel_login: &str,
    ) -> Result<StreamInfo, TwitchClientError> {
        let response: StreamInfoData = self
            .post_gql_typed(&operations::stream_info_overlay(channel_login))
            .await?;
        stream_info_from_typed(response)
    }

    pub async fn fetch_watch_streak_achievement(
        &self,
        channel_id: &str,
    ) -> Result<Option<OffsetDateTime>, TwitchClientError> {
        let response: RewardListData = self
            .post_gql_typed(&operations::reward_list(channel_id))
            .await?;
        watch_streak_achievement_from_typed(response)
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

            let response: FollowersData = self.post_gql_typed(&operation).await?;
            let page = followers_page_from_typed(response)?;
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
        if channel_id.trim().is_empty() || claim_id.trim().is_empty() {
            return Err(invalid_mutation(
                "ClaimCommunityPoints",
                "channel_id and claim_id are required",
            ));
        }
        let cookie = claim_bonus_cookie_header(&self.auth_token, user_id.unwrap_or_default());
        let response: ClaimBonusData = self
            .post_mutation_typed_value(
                serde_json::to_value(operations::claim_community_points(channel_id, claim_id))?,
                cookie.as_deref(),
            )
            .await?;
        validate_typed_claim_bonus_response(response)
    }

    pub async fn claim_moment(&self, moment_id: &str) -> Result<(), TwitchClientError> {
        if moment_id.trim().is_empty() {
            return Err(invalid_mutation(
                "CommunityMomentCallout_Claim",
                "moment_id is required",
            ));
        }
        let _: EmptyMutationData = self
            .post_mutation_typed(&operations::community_moment_claim(moment_id))
            .await?;
        Ok(())
    }

    pub async fn join_raid(&self, raid_id: &str) -> Result<(), TwitchClientError> {
        if raid_id.trim().is_empty() {
            return Err(invalid_mutation("JoinRaid", "raid_id is required"));
        }
        let _: EmptyMutationData = self
            .post_mutation_typed(&operations::join_raid(raid_id))
            .await?;
        Ok(())
    }

    pub async fn make_prediction(
        &self,
        event_id: &str,
        outcome_id: &str,
        points: i64,
    ) -> Result<(), TwitchClientError> {
        if event_id.trim().is_empty() || outcome_id.trim().is_empty() || points < 10 {
            return Err(invalid_mutation(
                "MakePrediction",
                "event_id, outcome_id, and points >= 10 are required",
            ));
        }
        let _: EmptyMutationData = self
            .post_mutation_typed(&operations::make_prediction(
                event_id,
                outcome_id,
                points,
                &generate_transaction_id(),
            ))
            .await?;
        Ok(())
    }

    /// Compatibility facade for callers that need to inspect the raw response.
    /// Runtime code should use `fetch_inventory_typed` or `fetch_claimable_drops`.
    pub async fn fetch_inventory(&self) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::inventory()).await
    }

    pub async fn fetch_inventory_typed(&self) -> Result<Vec<InventoryDrop>, TwitchClientError> {
        let response: InventoryData = self.post_gql_typed(&operations::inventory()).await?;
        inventory_drops_from_typed(response)
    }

    pub async fn fetch_claimable_drops(&self) -> Result<Vec<InventoryDrop>, TwitchClientError> {
        self.fetch_inventory_typed().await
    }

    /// Compatibility facade for callers that need the raw experimental dashboard shape.
    pub async fn fetch_viewer_drops_dashboard(
        &self,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::viewer_drops_dashboard()).await
    }

    pub async fn fetch_viewer_drops_dashboard_typed(
        &self,
    ) -> Result<ViewerDropsDashboard, TwitchClientError> {
        self.post_gql_typed(&operations::viewer_drops_dashboard())
            .await
    }

    pub async fn claim_drop(
        &self,
        drop_instance_id: &str,
    ) -> Result<ClaimDropOutcome, TwitchClientError> {
        if drop_instance_id.trim().is_empty() {
            return Err(invalid_mutation(
                "DropsPage_ClaimDropRewards",
                "drop_instance_id is required",
            ));
        }
        let response: ClaimDropData = self
            .post_mutation_typed(&operations::claim_drop_rewards(drop_instance_id))
            .await?;
        validate_typed_claim_drop_response(response)
    }

    pub async fn fetch_available_drop_campaigns(
        &self,
        channel_id: &str,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::drops_highlight_service_available(channel_id))
            .await
    }

    pub async fn fetch_available_drop_campaigns_typed(
        &self,
        channel_id: &str,
    ) -> Result<Vec<String>, TwitchClientError> {
        let response: AvailableDropsData = self
            .post_gql_typed(&operations::drops_highlight_service_available(channel_id))
            .await?;
        available_drop_campaign_ids_from_typed(response)
    }

    pub async fn fetch_available_drop_campaign_ids(
        &self,
        channel_id: &str,
    ) -> Result<Vec<String>, TwitchClientError> {
        self.fetch_available_drop_campaigns_typed(channel_id).await
    }

    /// Compatibility facade for callers that need the raw contribution response.
    pub async fn fetch_user_points_contribution(
        &self,
        channel_login: &str,
    ) -> Result<serde_json::Value, TwitchClientError> {
        self.post_gql(&operations::user_points_contribution(channel_login))
            .await
    }

    pub async fn fetch_user_points_contribution_typed(
        &self,
        channel_login: &str,
    ) -> Result<Vec<(String, i64)>, TwitchClientError> {
        let response: UserContributionData = self
            .post_gql_typed(&operations::user_points_contribution(channel_login))
            .await?;
        user_contributions_from_typed(response)
    }

    pub async fn contribute_community_goal(
        &self,
        amount: i64,
        channel_id: &str,
        goal_id: &str,
    ) -> Result<(), TwitchClientError> {
        if amount <= 0 || channel_id.trim().is_empty() || goal_id.trim().is_empty() {
            return Err(invalid_mutation(
                "ContributeCommunityPointsCommunityGoal",
                "amount > 0, channel_id, and goal_id are required",
            ));
        }
        let response: CommunityGoalContributionData = self
            .post_mutation_typed(&operations::contribute_community_goal(
                amount,
                channel_id,
                goal_id,
                &generate_transaction_id(),
            ))
            .await?;
        validate_typed_community_goal_response(response)
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
        self.post_gql_value_with_cookie(payload, None, true).await
    }

    async fn post_gql_typed<T>(
        &self,
        operation: &GqlPersistedOperation,
    ) -> Result<T, TwitchClientError>
    where
        T: DeserializeOwned,
    {
        let payload = self
            .post_gql_value(serde_json::to_value(operation)?)
            .await?;
        decode_gql_data(&payload, operation.operation_name)
    }

    async fn post_gql_value_with_cookie(
        &self,
        payload: serde_json::Value,
        cookie: Option<&str>,
        retry_read_only: bool,
    ) -> Result<serde_json::Value, TwitchClientError> {
        if !retry_read_only && self.auth_token.trim().is_empty() {
            return Err(invalid_mutation("mutation", "auth_token is required"));
        }
        let client_version = self.update_client_version().await?;
        let cookie = self.request_cookie_header(&self.endpoints.gql_url, cookie);
        let build_request = || {
            let mut request = self
                .client
                .post(&self.endpoints.gql_url)
                .header("Authorization", format!("OAuth {}", self.auth_token()))
                .header("Client-Id", CLIENT_ID)
                .header("Client-Session-Id", self.client_session_id())
                .header("Client-Version", &client_version)
                .header("User-Agent", self.user_agent())
                .header("X-Device-Id", self.device_id())
                .header("Content-Type", "application/json")
                .json(&payload);
            if let Some(cookie) = cookie.as_deref() {
                request = request.header("Cookie", cookie);
            }
            request
        };
        let response = if retry_read_only {
            self.send_read_request(build_request, "post gql").await?
        } else {
            build_request().send().await?
        };
        if !response.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: response.status(),
                context: "post gql",
            });
        }
        Ok(response.json().await?)
    }

    async fn post_mutation_typed<T>(
        &self,
        operation: &GqlPersistedOperation,
    ) -> Result<T, TwitchClientError>
    where
        T: DeserializeOwned,
    {
        self.post_mutation_typed_value(serde_json::to_value(operation)?, None)
            .await
    }

    async fn post_mutation_typed_value<T>(
        &self,
        payload: serde_json::Value,
        cookie: Option<&str>,
    ) -> Result<T, TwitchClientError>
    where
        T: DeserializeOwned,
    {
        let payload = self
            .post_gql_value_with_cookie(payload, cookie, false)
            .await?;
        decode_gql_data(&payload, "mutation")
    }

    async fn send_read_request<F>(
        &self,
        mut build_request: F,
        context: &'static str,
    ) -> Result<reqwest::Response, TwitchClientError>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        for attempt in 0..MAX_READ_ATTEMPTS {
            match build_request().send().await {
                Ok(response) => {
                    let status = response.status();
                    if !is_retryable_read_status(status) || attempt + 1 == MAX_READ_ATTEMPTS {
                        return Ok(response);
                    }
                    let delay = retry_delay(&response, attempt);
                    drop(response);
                    tokio::time::sleep(delay).await;
                }
                Err(error) => {
                    if !is_retryable_read_error(&error) || attempt + 1 == MAX_READ_ATTEMPTS {
                        return Err(TwitchClientError::Http(error));
                    }
                    tokio::time::sleep(read_backoff(attempt)).await;
                }
            }
        }
        Err(TwitchClientError::UnexpectedStatus {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            context,
        })
    }

    pub(crate) fn request_cookie_header(&self, url: &str, cookie: Option<&str>) -> Option<String> {
        let default_cookie = is_twitch_cookie_url(url)
            .then_some(self.default_cookie_header.as_deref())
            .flatten();
        merge_cookie_headers(default_cookie, cookie)
    }
}

fn is_retryable_read_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn invalid_mutation(context: &'static str, detail: &'static str) -> TwitchClientError {
    TwitchClientError::MutationRejected {
        context: context.to_string(),
        detail: detail.to_string(),
    }
}

fn operation_is_read_only(operation_name: &str) -> bool {
    crate::operations::PERSISTED_OPERATION_CONTRACTS
        .iter()
        .find(|contract| contract.operation_name == operation_name)
        .is_some_and(|contract| contract.read_only)
}

fn decode_gql_data<T>(
    payload: &serde_json::Value,
    context: &'static str,
) -> Result<T, TwitchClientError>
where
    T: DeserializeOwned,
{
    let response: GqlResponse<T> = serde_json::from_value(payload.clone()).map_err(|error| {
        TwitchClientError::ProtocolDecode {
            context: context.to_string(),
            detail: error.to_string(),
            shape: redacted_response_shape(payload),
        }
    })?;
    if let Some(errors) = response.errors.filter(|errors| !errors.is_empty()) {
        return Err(TwitchClientError::GqlErrors {
            context: context.to_string(),
            errors: format!("{} error(s)", errors.len()),
        });
    }
    response.data.ok_or(TwitchClientError::MissingField("data"))
}

fn validate_typed_claim_bonus_response(
    response: crate::types::ClaimBonusData,
) -> Result<ClaimBonusOutcome, TwitchClientError> {
    let claim = response
        .claim
        .ok_or(TwitchClientError::MissingField("data.claimCommunityPoints"))?;
    if claim
        .error
        .as_ref()
        .and_then(|error| error.message.as_deref())
        .is_some_and(|message| !message.trim().is_empty())
    {
        return Err(TwitchClientError::MutationRejected {
            context: String::from("ClaimCommunityPoints"),
            detail: String::from("claim bonus response reported an error"),
        });
    }
    match claim
        .status
        .as_deref()
        .map(|status| status.trim().to_uppercase())
        .as_deref()
    {
        Some("SUCCESS") | None => Ok(ClaimBonusOutcome::Claimed),
        Some("ALREADY_CLAIMED") => Ok(ClaimBonusOutcome::AlreadyClaimed),
        Some(_) => Err(TwitchClientError::MutationRejected {
            context: String::from("ClaimCommunityPoints"),
            detail: String::from("unexpected claim bonus status"),
        }),
    }
}

fn validate_typed_claim_drop_response(
    response: crate::types::ClaimDropData,
) -> Result<ClaimDropOutcome, TwitchClientError> {
    let status =
        response
            .claim
            .and_then(|claim| claim.status)
            .ok_or(TwitchClientError::MissingField(
                "data.claimDropRewards.status",
            ))?;
    match status.trim().to_uppercase().as_str() {
        "ELIGIBLE_FOR_ALL" => Ok(ClaimDropOutcome::EligibleForAll),
        "DROP_INSTANCE_ALREADY_CLAIMED" => Ok(ClaimDropOutcome::AlreadyClaimed),
        status => Err(TwitchClientError::MutationRejected {
            context: String::from("DropsPage_ClaimDropRewards"),
            // The status is a bounded protocol enum, not user data or a response payload.
            // Include it to make operator diagnostics actionable without leaking raw JSON.
            detail: format!("unexpected drop claim status {status}"),
        }),
    }
}

fn validate_typed_community_goal_response(
    response: CommunityGoalContributionData,
) -> Result<(), TwitchClientError> {
    let contribution = response
        .contribution
        .ok_or(TwitchClientError::MissingField(
            "data.contributeCommunityPointsCommunityGoal",
        ))?;
    if contribution
        .error
        .is_some_and(|error| !error.trim().is_empty())
    {
        return Err(TwitchClientError::MutationRejected {
            context: String::from("ContributeCommunityPointsCommunityGoal"),
            detail: String::from("community goal response reported an error"),
        });
    }
    Ok(())
}

fn redacted_response_shape(payload: &serde_json::Value) -> String {
    match payload {
        serde_json::Value::Object(fields) => format!(
            "object(fields={}, has_data={}, has_errors={})",
            fields.len(),
            fields.contains_key("data"),
            fields.contains_key("errors")
        ),
        serde_json::Value::Array(items) => format!("array(length={})", items.len()),
        serde_json::Value::Null => String::from("null"),
        serde_json::Value::Bool(_) => String::from("boolean"),
        serde_json::Value::Number(_) => String::from("number"),
        serde_json::Value::String(_) => String::from("string"),
    }
}

pub(crate) fn channel_points_context_from_typed(
    data: crate::types::ChannelPointsData,
) -> Result<ChannelPointsContext, TwitchClientError> {
    let channel = data
        .community
        .ok_or(TwitchClientError::MissingField("data.community"))?
        .channel
        .ok_or(TwitchClientError::MissingField("data.community.channel"))?;
    let points = channel
        .self_data
        .ok_or(TwitchClientError::MissingField(
            "data.community.channel.self",
        ))?
        .points
        .ok_or(TwitchClientError::MissingField(
            "data.community.channel.self.communityPoints",
        ))?;
    let balance = points.balance.ok_or(TwitchClientError::MissingField(
        "data.community.channel.self.communityPoints.balance",
    ))?;
    let claim_id = points.available_claim.and_then(|claim| claim.id);
    let active_multipliers = points.active_multipliers;
    let community_goals = channel
        .settings
        .map(|settings| {
            settings
                .goals
                .into_iter()
                .map(|goal| {
                    let id = goal.id.filter(|id| !id.trim().is_empty()).ok_or(
                        TwitchClientError::MissingField(
                            "data.community.channel.communityPointsSettings.goals.id",
                        ),
                    )?;
                    let points_contributed = goal.points_contributed.ok_or(
                        TwitchClientError::MissingField(
                            "data.community.channel.communityPointsSettings.goals.pointsContributed",
                        ),
                    )?;
                    let amount_needed = goal.amount_needed.ok_or(
                        TwitchClientError::MissingField(
                            "data.community.channel.communityPointsSettings.goals.amountNeeded",
                        ),
                    )?;
                    let per_stream_user_maximum_contribution = goal
                        .per_stream_user_maximum_contribution
                        .ok_or(TwitchClientError::MissingField(
                            "data.community.channel.communityPointsSettings.goals.perStreamUserMaximumContribution",
                        ))?;
                    if points_contributed < 0 {
                        return Err(TwitchClientError::InvalidField(
                            "data.community.channel.communityPointsSettings.goals.pointsContributed",
                        ));
                    }
                    if amount_needed < 0 {
                        return Err(TwitchClientError::InvalidField(
                            "data.community.channel.communityPointsSettings.goals.amountNeeded",
                        ));
                    }
                    if per_stream_user_maximum_contribution < 0 {
                        return Err(TwitchClientError::InvalidField(
                            "data.community.channel.communityPointsSettings.goals.perStreamUserMaximumContribution",
                        ));
                    }
                    Ok(tm_domain::CommunityGoal {
                        id,
                        title: goal.title.unwrap_or_default(),
                        is_in_stock: goal.is_in_stock,
                        points_contributed,
                        amount_needed,
                        per_stream_user_maximum_contribution,
                        status: goal.status.unwrap_or_default(),
                    })
                })
                .collect::<Result<Vec<_>, TwitchClientError>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(ChannelPointsContext {
        balance,
        claim_id,
        active_multiplier_count: active_multipliers.len(),
        active_multipliers,
        community_goals,
    })
}

fn stream_info_from_typed(data: StreamInfoData) -> Result<StreamInfo, TwitchClientError> {
    let user = data
        .user
        .ok_or(TwitchClientError::MissingField("data.user"))?;
    let stream = user
        .stream
        .ok_or(TwitchClientError::MissingField("data.user.stream"))?;
    let id = stream
        .id
        .ok_or(TwitchClientError::MissingField("data.user.stream.id"))?;
    let settings = user.broadcast_settings;
    let title = settings
        .as_ref()
        .and_then(|settings| settings.title.clone())
        .unwrap_or_default();
    let game = settings.and_then(|settings| settings.game);
    let game_name = game
        .as_ref()
        .and_then(|game| game.display_name.as_ref().or(game.name.as_ref()))
        .cloned()
        .unwrap_or_default();
    let game_id = game.and_then(|game| game.id);
    let tags = stream.tags.into_iter().filter_map(|tag| tag.id).collect();
    let created_at = stream
        .created_at
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());

    Ok(StreamInfo {
        id,
        title,
        game_name,
        game_id,
        viewers_count: u32::try_from(stream.viewers_count.unwrap_or_default()).unwrap_or(u32::MAX),
        tags,
        created_at,
    })
}

pub(crate) fn watch_streak_achievement_from_typed(
    data: RewardListData,
) -> Result<Option<OffsetDateTime>, TwitchClientError> {
    let timestamp = data
        .channel
        .and_then(|channel| channel.self_data)
        .and_then(|self_data| self_data.watch_streak_milestone)
        .and_then(|envelope| envelope.milestone)
        .and_then(|milestone| milestone.achievement_timestamp);
    timestamp
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
                TwitchClientError::InvalidField(
                    "data.channel.self.watchStreakMilestone.watchStreakMilestone.achievementTimestamp",
                )
            })
        })
        .transpose()
}

pub(crate) fn available_drop_campaign_ids_from_typed(
    data: AvailableDropsData,
) -> Result<Vec<String>, TwitchClientError> {
    // Twitch returns a null channel/campaign list when the channel has no
    // currently available drops. The Go reference treats that shape as an
    // empty result, while entries in a present list still require valid IDs.
    let Some(channel) = data.channel else {
        return Ok(Vec::new());
    };
    let Some(campaigns) = channel.campaigns else {
        return Ok(Vec::new());
    };
    campaigns
        .into_iter()
        .map(|campaign| {
            campaign
                .id
                .filter(|id| !id.trim().is_empty())
                .ok_or(TwitchClientError::MissingField(
                    "data.channel.viewerDropCampaigns.id",
                ))
        })
        .collect()
}

pub(crate) fn user_contributions_from_typed(
    data: UserContributionData,
) -> Result<Vec<(String, i64)>, TwitchClientError> {
    let contributions = data
        .user
        .ok_or(TwitchClientError::MissingField("data.user"))?
        .channel
        .ok_or(TwitchClientError::MissingField("data.user.channel"))?
        .self_data
        .ok_or(TwitchClientError::MissingField("data.user.channel.self"))?
        .community_points
        .ok_or(TwitchClientError::MissingField(
            "data.user.channel.self.communityPoints",
        ))?
        .contributions;
    contributions
        .into_iter()
        .map(|item| {
            let goal = item.goal.ok_or(TwitchClientError::MissingField(
                "data.user.channel.self.communityPoints.goalContributions.goal",
            ))?;
            let id = goal.id.filter(|id| !id.trim().is_empty()).ok_or(
                TwitchClientError::MissingField(
                    "data.user.channel.self.communityPoints.goalContributions.goal.id",
                ),
            )?;
            let points = item.points.ok_or(TwitchClientError::MissingField(
                "data.user.channel.self.communityPoints.goalContributions.userPointsContributedThisStream",
            ))?;
            if points < 0 {
                return Err(TwitchClientError::InvalidField(
                    "data.user.channel.self.communityPoints.goalContributions.userPointsContributedThisStream",
                ));
            }
            Ok((id, points))
        })
        .collect()
}

fn followers_page_from_typed(
    data: FollowersData,
) -> Result<crate::types::FollowersPage, TwitchClientError> {
    let follows = data
        .user
        .ok_or(TwitchClientError::MissingField("data.user"))?
        .follows
        .ok_or(TwitchClientError::MissingField("data.user.follows"))?;
    let edges = follows
        .edges
        .ok_or(TwitchClientError::MissingField("data.user.follows.edges"))?;
    let cursor = edges.last().and_then(|edge| edge.cursor.clone());
    let logins = edges
        .into_iter()
        .filter_map(|edge| edge.node.and_then(|node| node.login))
        .map(|login| login.to_lowercase())
        .collect();
    let page_info = follows.page_info.ok_or(TwitchClientError::MissingField(
        "data.user.follows.pageInfo",
    ))?;
    Ok(crate::types::FollowersPage {
        logins,
        has_next_page: page_info.has_next_page,
        cursor,
    })
}

pub(crate) fn inventory_drops_from_typed(
    data: InventoryData,
) -> Result<Vec<InventoryDrop>, TwitchClientError> {
    let campaigns = data
        .current_user
        .ok_or(TwitchClientError::MissingField("data.currentUser"))?
        .inventory
        .ok_or(TwitchClientError::MissingField(
            "data.currentUser.inventory",
        ))?
        .campaigns
        .ok_or(TwitchClientError::MissingField(
            "data.currentUser.inventory.dropCampaignsInProgress",
        ))?;
    let mut drops = Vec::new();
    for campaign in campaigns {
        let campaign_name = campaign.name.or(campaign.display_name).unwrap_or_default();
        for drop in campaign.drops {
            let Some(self_data) = drop.self_data else {
                continue;
            };
            let Some(drop_instance_id) = self_data.drop_instance_id else {
                continue;
            };
            let required_minutes_watched = drop
                .required_minutes_watched
                .or(drop.required_progress)
                .ok_or(TwitchClientError::MissingField(
                    "data.currentUser.inventory.timeBasedDrops.requiredMinutesWatched",
                ))?;
            let is_claimed = self_data.is_claimed.ok_or(TwitchClientError::MissingField(
                "data.currentUser.inventory.timeBasedDrops.self.isClaimed",
            ))?;
            drops.push(InventoryDrop {
                drop_instance_id,
                reward_name: drop
                    .name
                    .or_else(|| drop.benefit.and_then(|benefit| benefit.name))
                    .unwrap_or_default(),
                campaign_name: campaign_name.clone(),
                current_minutes_watched: self_data
                    .current_minutes_watched
                    .or(self_data.current_progress)
                    .unwrap_or_default(),
                required_minutes_watched,
                is_claimed,
            });
        }
    }
    Ok(drops)
}

fn is_retryable_read_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn retry_delay(response: &reqwest::Response, attempt: usize) -> Duration {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(retry_after_duration)
        .or_else(|| {
            response
                .headers()
                .get("ratelimit-reset")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
                .and_then(|reset| {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    (reset > now).then_some(Duration::from_secs(reset - now))
                })
        })
        .unwrap_or_else(|| read_backoff(attempt))
        .min(MAX_READ_RETRY_DELAY)
}

pub(crate) fn retry_after_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let normalized = value
        .strip_suffix(" GMT")
        .map_or_else(|| value.to_string(), |prefix| format!("{prefix} +0000"));
    let target = OffsetDateTime::parse(&normalized, &Rfc2822).ok()?;
    let seconds = (target - OffsetDateTime::now_utc()).whole_seconds();
    if seconds <= 0 {
        return None;
    }
    u64::try_from(seconds).ok().map(Duration::from_secs)
}

fn read_backoff(attempt: usize) -> Duration {
    let multiplier = 1_u32 << attempt.min(4);
    READ_RETRY_BASE
        .checked_mul(multiplier)
        .unwrap_or(MAX_READ_RETRY_DELAY)
        .min(MAX_READ_RETRY_DELAY)
}
