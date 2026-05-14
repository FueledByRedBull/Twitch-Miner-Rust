use tm_domain::{ActiveMultiplier, CommunityGoal, Stream};

use crate::types::{
    ChannelPointsContext, ClaimBonusOutcome, ClaimDropOutcome, FollowersPage, InventoryDrop,
    MinuteWatchedRequest, StreamInfo, TwitchClientError,
};

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

pub(crate) fn operation_name(value: &serde_json::Value) -> Option<String> {
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
