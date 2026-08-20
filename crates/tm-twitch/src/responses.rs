use serde::de::DeserializeOwned;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::types::{
    ArchivedVideo, ArchivedVideosData, AvailableDropsData, ChannelPointsContext, ClaimBonusOutcome,
    ClaimDropOutcome, CommunityGoalContributionData, FollowersData, GqlResponse, InventoryData,
    InventoryDrop, InventorySnapshot, RecentClip, RecentClipsData, RewardListData, StreamInfo,
    StreamInfoData, TwitchClientError, UserContributionData, WatchStreakMilestone,
};

pub(crate) fn is_persisted_query_not_found(payload: &serde_json::Value) -> bool {
    payload
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|errors| {
            errors.iter().any(|error| {
                error.get("message").and_then(serde_json::Value::as_str)
                    == Some("PersistedQueryNotFound")
            })
        })
}

pub(crate) fn decode_gql_data<T>(
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

pub(crate) fn validate_typed_claim_bonus_response(
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

pub(crate) fn validate_typed_claim_drop_response(
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

pub(crate) fn validate_typed_community_goal_response(
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
    let crate::types::ChannelPointsChannel {
        self_data,
        settings,
    } = channel;
    let channel_points_enabled = settings.as_ref().and_then(|settings| settings.is_enabled);
    let points = self_data
        .as_ref()
        .and_then(|self_data| self_data.points.as_ref());
    let balance = points
        .and_then(|points| points.balance)
        .or_else(|| (channel_points_enabled == Some(false)).then_some(0))
        .ok_or(TwitchClientError::MissingField(
            "data.community.channel.self.communityPoints.balance",
        ))?;
    let claim_id = points.and_then(|points| {
        points
            .available_claim
            .as_ref()
            .and_then(|claim| claim.id.clone())
    });
    let active_multipliers = points
        .map(|points| points.active_multipliers.clone())
        .unwrap_or_default();
    let community_goals = settings
        .filter(|_| channel_points_enabled != Some(false))
        .map(|settings| {
            settings
                .goals
                .iter()
                .cloned()
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
        channel_points_enabled,
        claim_id,
        active_multiplier_count: active_multipliers.len(),
        active_multipliers,
        community_goals,
    })
}

pub(crate) fn stream_info_from_typed(
    data: StreamInfoData,
) -> Result<StreamInfo, TwitchClientError> {
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

pub(crate) fn watch_streak_milestone_from_typed(
    data: RewardListData,
) -> Result<Option<WatchStreakMilestone>, TwitchClientError> {
    let Some(envelope) = data
        .channel
        .and_then(|channel| channel.self_data)
        .and_then(|self_data| self_data.watch_streak_milestone)
    else {
        return Ok(None);
    };
    let Some(milestone) = envelope.milestone else {
        return Ok(None);
    };
    let Some(timestamp) = milestone
        .achievement_timestamp
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let achievement_timestamp = OffsetDateTime::parse(timestamp, &Rfc3339).map_err(|_| {
        TwitchClientError::InvalidField(
            "data.channel.self.watchStreakMilestone.watchStreakMilestone.achievementTimestamp",
        )
    })?;
    let value = milestone
        .value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|_| {
            TwitchClientError::InvalidField(
                "data.channel.self.watchStreakMilestone.watchStreakMilestone.value",
            )
        })?;
    let expires_at = envelope
        .expires_at
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| OffsetDateTime::parse(value, &Rfc3339))
        .transpose()
        .map_err(|_| {
            TwitchClientError::InvalidField("data.channel.self.watchStreakMilestone.expiresAt")
        })?;
    let missed_broadcast_ids = envelope
        .missed_streams
        .map(|streams| {
            streams
                .into_iter()
                .flat_map(|stream| stream.broadcast_identifiers)
                .map(|identifier| {
                    required_text(
                        identifier.id,
                        "data.channel.self.watchStreakMilestone.missedStreams.broadcastIdentifiers.id",
                    )
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(Some(WatchStreakMilestone {
        value,
        achievement_timestamp,
        expires_at,
        missed_broadcast_ids,
    }))
}

pub(crate) fn archived_videos_from_typed(
    data: ArchivedVideosData,
) -> Result<Vec<ArchivedVideo>, TwitchClientError> {
    let Some(user) = data.user else {
        return Ok(Vec::new());
    };
    let Some(videos) = user.videos else {
        return Ok(Vec::new());
    };
    videos
        .edges
        .into_iter()
        // GraphQL can retain an edge with a null node when that individual VOD
        // is unavailable. Keep validating every present node strictly so an
        // actual field-contract drift still fails the canary.
        .filter_map(|edge| edge.node)
        .map(|node| {
            let id = required_text(node.id, "data.user.videos.edges.node.id")?;
            let length_seconds = u32::try_from(node.length_seconds.ok_or(
                TwitchClientError::MissingField("data.user.videos.edges.node.lengthSeconds"),
            )?)
            .map_err(|_| {
                TwitchClientError::InvalidField("data.user.videos.edges.node.lengthSeconds")
            })?;
            let broadcast_id = node
                .broadcast_identifier
                .and_then(|identifier| identifier.id)
                .filter(|value| !value.trim().is_empty());
            Ok(ArchivedVideo {
                id,
                broadcast_id,
                length_seconds,
            })
        })
        .collect()
}

pub(crate) fn recent_clips_from_typed(
    data: RecentClipsData,
) -> Result<Vec<RecentClip>, TwitchClientError> {
    let Some(user) = data.user else {
        return Ok(Vec::new());
    };
    let Some(clips) = user.clips else {
        return Ok(Vec::new());
    };
    clips
        .edges
        .into_iter()
        .map(|edge| {
            let node = edge.node.ok_or(TwitchClientError::MissingField(
                "data.user.clips.edges.node",
            ))?;
            let duration_seconds = node
                .duration_seconds
                .ok_or(TwitchClientError::MissingField(
                    "data.user.clips.edges.node.durationSeconds",
                ))?;
            if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
                return Err(TwitchClientError::InvalidField(
                    "data.user.clips.edges.node.durationSeconds",
                ));
            }
            Ok(RecentClip {
                id: required_text(node.id, "data.user.clips.edges.node.id")?,
                slug: required_text(node.slug, "data.user.clips.edges.node.slug")?,
                url: required_text(node.url, "data.user.clips.edges.node.url")?,
                duration_seconds,
                broadcast_id: node
                    .broadcast_identifier
                    .and_then(|identifier| identifier.id)
                    .filter(|value| !value.trim().is_empty()),
            })
        })
        .collect()
}

fn required_text(value: Option<String>, field: &'static str) -> Result<String, TwitchClientError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or(TwitchClientError::MissingField(field))
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

pub(crate) fn followers_page_from_typed(
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

pub(crate) fn inventory_snapshot_from_typed(
    data: InventoryData,
) -> Result<InventorySnapshot, TwitchClientError> {
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
    let mut snapshot = InventorySnapshot {
        drops: Vec::new(),
        completed_campaign_ids: Vec::new(),
    };
    for campaign in campaigns {
        let campaign_complete = !campaign.drops.is_empty()
            && campaign.drops.iter().all(|drop| {
                drop.self_data
                    .as_ref()
                    .and_then(|progress| progress.is_claimed)
                    == Some(true)
            });
        if campaign_complete {
            if let Some(id) = campaign
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                snapshot.completed_campaign_ids.push(id.to_owned());
            }
        }
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
            snapshot.drops.push(InventoryDrop {
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
    snapshot.completed_campaign_ids.sort_unstable();
    snapshot.completed_campaign_ids.dedup();
    Ok(snapshot)
}
