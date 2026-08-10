use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use time::format_description::well_known::Rfc2822;
use time::OffsetDateTime;
use tm_domain::Stream;

use crate::contracts::{extract_build_id, extract_settings_script_url, extract_spade_url};
use crate::cookies::{claim_bonus_cookie_header, is_twitch_cookie_url, merge_cookie_headers};
use crate::hls::{lowest_bandwidth_variant_url, media_segment_url};
use crate::ids::{generate_client_session_id, generate_device_id, generate_transaction_id};
use crate::parsers::minute_watched_request;
use crate::responses::{
    archived_videos_from_typed, available_drop_campaign_ids_from_typed,
    channel_points_context_from_typed, decode_gql_data, followers_page_from_typed,
    inventory_snapshot_from_typed, recent_clips_from_typed, stream_info_from_typed,
    user_contributions_from_typed, validate_typed_claim_bonus_response,
    validate_typed_claim_drop_response, validate_typed_community_goal_response,
    watch_streak_milestone_from_typed,
};
use crate::types::{
    ArchivedVideo, ArchivedVideosData, AvailableDropsData, ChannelPointsContext, ClaimBonusData,
    ClaimBonusOutcome, ClaimDropData, ClaimDropOutcome, CommunityGoalContributionData,
    EmptyMutationData, FollowersData, GqlPersistedOperation, InventoryData, InventoryDrop,
    InventorySnapshot, LiveStatusData, PlaybackAccessTokenData, RecentClip, RecentClipsData,
    RewardListData, StreamInfo, StreamInfoData, TwitchClientError, TwitchEndpoints,
    UserContributionData, UserIdData, UserLoginData, ViewerDropsDashboard, WatchStreakMilestone,
};
use crate::{operations, CLIENT_ID};

const MAX_READ_ATTEMPTS: usize = 3;
const READ_RETRY_BASE: Duration = Duration::from_millis(250);
const MAX_READ_RETRY_DELAY: Duration = Duration::from_secs(30);
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Typed Twitch client with bounded read retries and non-replayed mutations.
///
/// Credentials and tokenized playback URLs remain private and never appear in
/// diagnostic errors.
#[derive(Debug)]
pub struct TwitchClient {
    client: reqwest::Client,
    /// Client for URLs supplied by Twitch documents. Its resolver validates
    /// every address returned by DNS and its redirect policy is disabled so a
    /// response cannot silently move a credential-bearing request elsewhere.
    remote_client: Result<reqwest::Client, ()>,
    allow_loopback_remote_endpoints: bool,
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
    value: Option<String>,
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
            .redirect(reqwest::redirect::Policy::none())
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
        let allow_loopback_remote_endpoints = endpoints_include_loopback_http(&endpoints);
        let remote_client = hardened_remote_client(allow_loopback_remote_endpoints);
        Self {
            client,
            remote_client,
            allow_loopback_remote_endpoints,
            auth_token: auth_token.into().trim().to_string(),
            default_cookie_header: default_cookie_header
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            client_session: generate_client_session_id(),
            device_id: generate_device_id(),
            user_agent: user_agent.into().trim().to_string(),
            client_version: Mutex::new(CachedClientVersion {
                value: None,
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
            if let Some(value) = cache.value.as_ref().filter(|_| {
                cache
                    .fetched_at
                    .is_some_and(|fetched_at| fetched_at.elapsed() < cache.ttl)
            }) {
                return Ok(value.clone());
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
        cache.value = Some(build_id.clone());
        cache.fetched_at = Some(Instant::now());
        Ok(build_id)
    }

    pub async fn fetch_settings_script_url(
        &self,
        page_url: &str,
    ) -> Result<String, TwitchClientError> {
        let parsed_page_url = reqwest::Url::parse(page_url)
            .map_err(|_| TwitchClientError::InvalidField("settings page URL"))?;
        validate_remote_endpoint(
            &parsed_page_url,
            "settings page URL",
            self.allow_loopback_remote_endpoints,
        )?;
        let remote_client = self.remote_client("settings page")?;
        let cookie = self.request_cookie_header(page_url, None);
        let response = self
            .send_read_request(
                || {
                    let mut request = remote_client
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
        let channel_login = normalize_channel_login(channel_login)
            .ok_or(TwitchClientError::InvalidField("channel_login"))?;
        let page_url = format!(
            "{}/{}",
            self.endpoints.twitch_url.trim_end_matches('/'),
            channel_login
        );
        let settings_url = self.fetch_settings_script_url(&page_url).await?;
        let parsed_settings_url = reqwest::Url::parse(&settings_url)
            .map_err(|_| TwitchClientError::InvalidField("settings script URL"))?;
        validate_remote_endpoint(
            &parsed_settings_url,
            "settings script URL",
            self.allow_loopback_remote_endpoints,
        )?;
        let remote_client = self.remote_client("settings script")?;
        let cookie = self.request_cookie_header(&settings_url, None);
        let response = self
            .send_read_request(
                || {
                    let mut request = remote_client
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
        let spade_url = extract_spade_url(&response.text().await?)?;
        // The settings script is fetched over TLS through the validated resolver,
        // but the spade value inside it is still unconstrained text and the
        // payload carries account identifiers. Hold it to the same bar as playback.
        let parsed = reqwest::Url::parse(&spade_url)
            .map_err(|_| TwitchClientError::InvalidField("spade_url"))?;
        validate_remote_endpoint(&parsed, "spade_url", self.allow_loopback_remote_endpoints)?;
        Ok(spade_url)
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

    pub async fn prime_live_playback(&self, channel_login: &str) -> Result<(), TwitchClientError> {
        let channel_login = normalize_channel_login(channel_login)
            .ok_or(TwitchClientError::InvalidField("channel_login"))?;

        let response: PlaybackAccessTokenData = self
            .post_gql_typed(&operations::playback_access_token(&channel_login))
            .await?;
        let token =
            response
                .stream_playback_access_token
                .ok_or(TwitchClientError::MissingField(
                    "data.streamPlaybackAccessToken",
                ))?;
        if token.signature.trim().is_empty() || token.value.trim().is_empty() {
            return Err(TwitchClientError::InvalidField(
                "data.streamPlaybackAccessToken",
            ));
        }

        let mut master_url = reqwest::Url::parse(&self.endpoints.playback_url)
            .map_err(|_| TwitchClientError::InvalidField("playback_url"))?
            .join(&format!("{channel_login}.m3u8"))
            .map_err(|_| TwitchClientError::InvalidField("channel_login"))?;
        master_url
            .query_pairs_mut()
            .append_pair("sig", &token.signature)
            .append_pair("token", &token.value);
        validate_remote_endpoint(
            &master_url,
            "playback_url",
            self.allow_loopback_remote_endpoints,
        )?;
        let remote_client = self.remote_client("master playlist")?;

        let master = remote_client
            .get(master_url)
            .header("User-Agent", self.user_agent())
            .send()
            .await
            .map_err(|error| sanitize_playback_error(&error, "master playlist"))?;
        if !master.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: master.status(),
                context: "fetch playback master playlist",
            });
        }
        let master_url = master.url().clone();
        let master_body = master
            .text()
            .await
            .map_err(|error| sanitize_playback_error(&error, "master playlist"))?;
        let variant_url = lowest_bandwidth_variant_url(&master_url, &master_body)?;
        validate_remote_endpoint(
            &variant_url,
            "master playlist",
            self.allow_loopback_remote_endpoints,
        )?;

        let variant = remote_client
            .get(variant_url)
            .header("User-Agent", self.user_agent())
            .send()
            .await
            .map_err(|error| sanitize_playback_error(&error, "media playlist"))?;
        if !variant.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: variant.status(),
                context: "fetch playback media playlist",
            });
        }
        let variant_url = variant.url().clone();
        let variant_body = variant
            .text()
            .await
            .map_err(|error| sanitize_playback_error(&error, "media playlist"))?;
        let segment_url = media_segment_url(&variant_url, &variant_body)?;
        validate_remote_endpoint(
            &segment_url,
            "media playlist",
            self.allow_loopback_remote_endpoints,
        )?;

        let segment = remote_client
            .head(segment_url)
            .header("User-Agent", self.user_agent())
            .send()
            .await
            .map_err(|error| sanitize_playback_error(&error, "media segment"))?;
        if !segment.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: segment.status(),
                context: "prime playback media segment",
            });
        }
        Ok(())
    }

    pub async fn fetch_watch_streak_achievement(
        &self,
        channel_id: &str,
    ) -> Result<Option<OffsetDateTime>, TwitchClientError> {
        Ok(self
            .fetch_watch_streak_milestone(channel_id)
            .await?
            .map(|milestone| milestone.achievement_timestamp))
    }

    pub async fn fetch_watch_streak_milestone(
        &self,
        channel_id: &str,
    ) -> Result<Option<WatchStreakMilestone>, TwitchClientError> {
        let response: RewardListData = self
            .post_gql_typed(&operations::reward_list(channel_id))
            .await?;
        watch_streak_milestone_from_typed(response)
    }

    pub async fn fetch_recent_archived_videos(
        &self,
        channel_login: &str,
    ) -> Result<Vec<ArchivedVideo>, TwitchClientError> {
        let response: ArchivedVideosData = self
            .post_gql_typed(&operations::recent_archived_videos(channel_login))
            .await?;
        archived_videos_from_typed(response)
    }

    pub async fn fetch_recent_clips(
        &self,
        channel_login: &str,
    ) -> Result<Vec<RecentClip>, TwitchClientError> {
        let response: RecentClipsData = self
            .post_gql_typed(&operations::recent_clips(channel_login))
            .await?;
        recent_clips_from_typed(response)
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
                let Some(variables) = operation.variables.as_object_mut() else {
                    return Err(TwitchClientError::InvalidField("channel follows variables"));
                };
                variables.insert(
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

    pub async fn fetch_inventory_typed(&self) -> Result<Vec<InventoryDrop>, TwitchClientError> {
        Ok(self.fetch_inventory_snapshot_typed().await?.drops)
    }

    pub async fn fetch_inventory_snapshot_typed(
        &self,
    ) -> Result<InventorySnapshot, TwitchClientError> {
        let response: InventoryData = self.post_gql_typed(&operations::inventory()).await?;
        inventory_snapshot_from_typed(response)
    }

    pub async fn fetch_claimable_drops(&self) -> Result<Vec<InventoryDrop>, TwitchClientError> {
        self.fetch_inventory_typed().await
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

    pub async fn fetch_available_drop_campaigns_typed(
        &self,
        channel_id: &str,
    ) -> Result<Vec<String>, TwitchClientError> {
        let response: AvailableDropsData = self
            .post_gql_typed(&operations::drops_highlight_service_available(channel_id))
            .await?;
        available_drop_campaign_ids_from_typed(response)
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
        let parsed_url = reqwest::Url::parse(&request.url)
            .map_err(|_| TwitchClientError::InvalidField("spade_url"))?;
        validate_remote_endpoint(
            &parsed_url,
            "spade_url",
            self.allow_loopback_remote_endpoints,
        )?;
        let remote_client = self.remote_client("spade")?;
        let response = remote_client
            .post(parsed_url)
            .header("Content-Type", request.content_type)
            .header("User-Agent", request.user_agent)
            .body(request.body)
            .send()
            .await
            .map_err(|error| sanitize_playback_error(&error, "spade"))?;
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
        if retry_read_only {
            return self.send_read_gql_value(build_request, "post gql").await;
        }
        let response = build_request().send().await?;
        if !response.status().is_success() {
            return Err(TwitchClientError::UnexpectedStatus {
                status: response.status(),
                context: "post gql",
            });
        }
        Ok(response.json().await?)
    }

    async fn send_read_gql_value<F>(
        &self,
        mut build_request: F,
        context: &'static str,
    ) -> Result<serde_json::Value, TwitchClientError>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        for attempt in 0..MAX_READ_ATTEMPTS {
            match build_request().send().await {
                Ok(response) => {
                    let status = response.status();
                    if is_retryable_read_status(status) && attempt + 1 < MAX_READ_ATTEMPTS {
                        let delay = retry_delay(&response, attempt);
                        drop(response);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    if !status.is_success() {
                        return Err(TwitchClientError::UnexpectedStatus { status, context });
                    }
                    let payload = response.json().await?;
                    if gql_has_only_transient_service_errors(&payload)
                        && attempt + 1 < MAX_READ_ATTEMPTS
                    {
                        tokio::time::sleep(read_backoff(attempt)).await;
                        continue;
                    }
                    return Ok(payload);
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
                        // Remote-document URLs can contain signed query data supplied by
                        // Twitch. Preserve the failure class without retaining the
                        // reqwest error, whose source chain can include the full URL.
                        return Err(sanitize_remote_error(&error, context));
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

    fn remote_client(&self, context: &'static str) -> Result<&reqwest::Client, TwitchClientError> {
        self.remote_client
            .as_ref()
            .map_err(|()| TwitchClientError::PlaybackRequest {
                context,
                failure: crate::types::TwitchFailureClass::Other,
            })
    }
}

fn is_retryable_read_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn gql_has_only_transient_service_errors(payload: &serde_json::Value) -> bool {
    let Some(errors) = payload.get("errors").and_then(serde_json::Value::as_array) else {
        return false;
    };
    !errors.is_empty()
        && errors.iter().all(|error| {
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|message| message == "service error")
        })
}

fn invalid_mutation(context: &'static str, detail: &'static str) -> TwitchClientError {
    TwitchClientError::MutationRejected {
        context: context.to_string(),
        detail: detail.to_string(),
    }
}

/// Guards every request target that Twitch hands us inside a document rather
/// than one we compile in: settings scripts, playback playlists/segments, and
/// the spade endpoint. DNS addresses are checked again by
/// [`ValidatedDnsResolver`] immediately before a connection is opened.
/// Loopback HTTP stays allowed only for an explicit loopback IP literal so
/// tests can serve these locally without turning a public hostname into an
/// SSRF escape hatch.
pub(crate) fn validate_remote_endpoint(
    url: &reqwest::Url,
    field: &'static str,
    allow_loopback_http: bool,
) -> Result<(), TwitchClientError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(TwitchClientError::InvalidField(field));
    }
    let host = url
        .host_str()
        .ok_or(TwitchClientError::InvalidField(field))?;
    let address = parse_ip_literal(host);
    let is_loopback_literal = address.as_ref().is_ok_and(IpAddr::is_loopback);
    let is_hostname = address.is_err();
    let normalized_host = host.trim_end_matches('.').to_ascii_lowercase();
    let is_local_hostname = normalized_host
        .rsplit_once('.')
        .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("local"));

    let accepted = match url.scheme() {
        "https" if is_hostname => {
            !normalized_host.is_empty() && normalized_host != "localhost" && !is_local_hostname
        }
        "https" => address.is_ok_and(is_public_ip),
        "http" => allow_loopback_http && is_loopback_literal,
        _ => false,
    };

    accepted
        .then_some(())
        .ok_or(TwitchClientError::InvalidField(field))
}

/// Checks the complete result of one DNS lookup. A hostname is accepted only
/// when every address is public; accepting the first public address would let
/// a mixed DNS response (or a rebinding response) reach a private destination.
pub(crate) fn validate_resolved_addresses(
    host: &str,
    addresses: &[SocketAddr],
    allow_loopback_http: bool,
) -> Result<(), DnsPolicyError> {
    if addresses.is_empty() {
        return Err(DnsPolicyError::NoAddresses);
    }

    let loopback_literal = parse_ip_literal(host).is_ok_and(|address| address.is_loopback());
    if allow_loopback_http && loopback_literal {
        return addresses
            .iter()
            .all(|address| address.ip().is_loopback())
            .then_some(())
            .ok_or(DnsPolicyError::LoopbackOnly);
    }

    addresses
        .iter()
        .all(|address| is_public_ip(address.ip()))
        .then_some(())
        .ok_or(DnsPolicyError::NonPublicAddress)
}

/// A reqwest resolver that validates and then pins the exact address set
/// returned for this connection. Hyper uses the returned iterator directly,
/// so a second resolution cannot silently bypass the policy between checking
/// and connecting.
#[derive(Debug, Default)]
pub(crate) struct ValidatedDnsResolver {
    pub(crate) allow_loopback_http: bool,
}

impl Resolve for ValidatedDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().trim_end_matches('.').to_owned();
        let allow_loopback_http = self.allow_loopback_http;
        Box::pin(async move {
            let addresses = if let Ok(address) = parse_ip_literal(&host) {
                vec![SocketAddr::new(address, 0)]
            } else {
                tokio::net::lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?
                    .collect::<Vec<_>>()
            };
            validate_resolved_addresses(&host, &addresses, allow_loopback_http)
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            let addrs: Addrs = Box::new(addresses.into_iter());
            Ok(addrs)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DnsPolicyError {
    NoAddresses,
    LoopbackOnly,
    NonPublicAddress,
}

impl fmt::Display for DnsPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NoAddresses => "DNS returned no addresses",
            Self::LoopbackOnly => "loopback test endpoint resolved to a non-loopback address",
            Self::NonPublicAddress => "remote endpoint resolved to a non-public address",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for DnsPolicyError {}

fn hardened_remote_client(allow_loopback_http: bool) -> Result<reqwest::Client, ()> {
    reqwest::Client::builder()
        .timeout(REMOTE_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(Arc::new(ValidatedDnsResolver {
            allow_loopback_http,
        }))
        .build()
        .map_err(|_| ())
}

pub(crate) fn endpoints_include_loopback_http(endpoints: &TwitchEndpoints) -> bool {
    [
        endpoints.twitch_url.as_str(),
        endpoints.gql_url.as_str(),
        endpoints.playback_url.as_str(),
    ]
    .into_iter()
    .filter_map(|value| reqwest::Url::parse(value).ok())
    .any(|url| {
        url.scheme() == "http"
            && url
                .host_str()
                .and_then(|host| parse_ip_literal(host).ok())
                .is_some_and(|address| address.is_loopback())
    })
}

fn parse_ip_literal(host: &str) -> Result<IpAddr, std::net::AddrParseError> {
    host.trim_start_matches('[').trim_end_matches(']').parse()
}

pub(crate) fn normalize_channel_login(channel_login: &str) -> Option<String> {
    let channel_login = channel_login.trim().to_ascii_lowercase();
    (!channel_login.is_empty()
        && channel_login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(channel_login)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

pub(crate) fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    let first = octets[0];
    let second = octets[1];
    let third = octets[2];
    let is_documentation = (first == 192 && second == 0 && (third == 0 || third == 2))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113);
    let is_benchmarking = first == 198 && (18..=19).contains(&second);
    let is_shared = first == 100 && (64..=127).contains(&second);
    let is_deprecated_6to4_relay = first == 192 && second == 88 && third == 99;
    let is_reserved = first >= 240;

    first != 0
        && !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_unspecified()
        && !address.is_broadcast()
        && !address.is_multicast()
        && !is_documentation
        && !is_benchmarking
        && !is_shared
        && !is_deprecated_6to4_relay
        && !is_reserved
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let first = segments[0];
    let second = segments[1];
    let is_global_unicast = (0x2000..=0x3fff).contains(&first);
    // IANA marks 2001::/23 non-global except for narrow protocol-specific
    // allocations. This endpoint policy does not admit protocol-specific
    // anycast/tunnel ranges, so deny the parent block rather than maintaining a
    // permissive exception list that can age into an SSRF bypass.
    let is_ietf_protocol_assignment = first == 0x2001 && second <= 0x01ff;
    let is_documentation =
        (first == 0x2001 && second == 0x0db8) || (first == 0x3fff && (second & 0xf000) == 0);
    let is_6to4 = first == 0x2002;

    is_global_unicast
        && !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_unique_local()
        && !address.is_unicast_link_local()
        && !address.is_multicast()
        && !is_ietf_protocol_assignment
        && !is_documentation
        && !is_6to4
}

fn sanitize_playback_error(error: &reqwest::Error, context: &'static str) -> TwitchClientError {
    TwitchClientError::PlaybackRequest {
        context,
        failure: request_failure_class(error),
    }
}

fn sanitize_remote_error(error: &reqwest::Error, context: &'static str) -> TwitchClientError {
    TwitchClientError::RemoteRequest {
        context,
        failure: request_failure_class(error),
    }
}

fn request_failure_class(error: &reqwest::Error) -> crate::types::TwitchFailureClass {
    if error.is_timeout() {
        crate::types::TwitchFailureClass::Timeout
    } else if error.is_connect() || error.is_request() {
        crate::types::TwitchFailureClass::ConnectionReset
    } else {
        crate::types::TwitchFailureClass::Other
    }
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
