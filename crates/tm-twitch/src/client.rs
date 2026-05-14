use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use tm_domain::Stream;

use crate::contracts::{extract_build_id, extract_settings_script_url, extract_spade_url};
use crate::cookies::{claim_bonus_cookie_header, is_twitch_cookie_url, merge_cookie_headers};
use crate::ids::{generate_client_session_id, generate_device_id, generate_transaction_id};
use crate::parsers::{
    minute_watched_request, operation_name, parse_available_drop_campaign_ids,
    parse_channel_points_context, parse_followers_page, parse_inventory_drops, parse_live_status,
    parse_stream_info, validate_claim_bonus_response, validate_claim_drop_response,
    validate_community_goal_response, validate_gql_mutation_response,
};
use crate::types::{
    ChannelPointsContext, ClaimBonusOutcome, ClaimDropOutcome, GqlPersistedOperation,
    InventoryDrop, StreamInfo, TwitchClientError, TwitchEndpoints,
};
use crate::{operations, CLIENT_ID, DEFAULT_CLIENT_VERSION};

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

    pub async fn claim_moment(&self, moment_id: &str) -> Result<(), TwitchClientError> {
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

    pub(crate) fn request_cookie_header(&self, url: &str, cookie: Option<&str>) -> Option<String> {
        let default_cookie = is_twitch_cookie_url(url)
            .then_some(self.default_cookie_header.as_deref())
            .flatten();
        merge_cookie_headers(default_cookie, cookie)
    }
}
