use serde_json::json;

use crate::types::GqlPersistedOperation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistedOperationContract {
    pub operation_name: &'static str,
    pub sha256_hash: &'static str,
    pub read_only: bool,
}

pub const PERSISTED_OPERATION_CONTRACTS: &[PersistedOperationContract] = &[
    PersistedOperationContract {
        operation_name: "GetIDFromLogin",
        sha256_hash: "94e82a7b1e3c21e186daa73ee2afc4b8f23bade1fbbff6fe8ac133f50a2f58ca",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "ChannelFollows",
        sha256_hash: "eecf815273d3d949e5cf0085cc5084cd8a1b5b7b6f7990cf43cb0beadf546907",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "ChannelPointsContext",
        sha256_hash: "374314de591e69925fce3ddc2bcf085796f56ebb8cad67a0daa3165c03adc345",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "WithIsStreamLiveQuery",
        sha256_hash: "04e46329a6786ff3a81c01c50bfa5d725902507a0deb83b0edbf7abe7a3716ea",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "VideoPlayerStreamInfoOverlayChannel",
        sha256_hash: "e785b65ff71ad7b363b34878335f27dd9372869ad0c5740a130b9268bcdbe7e7",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "RewardList",
        sha256_hash: "0b1471876d7647993731b9e3c6a13bf304c67fb31d07f06a945d42286ee377c4",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "FilterableVideoTower_Videos",
        sha256_hash: "67004f7881e65c297936f32c75246470629557a393788fb5a69d6d9a25a8fd5f",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "ClipsCards__User",
        sha256_hash: "1cd671bfa12cec480499c087319f26d21925e9695d1f80225aae6a4354f23088",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "ClaimCommunityPoints",
        sha256_hash: "46aaeebe02c99afdf4fc97c7c0cba964124bf6b0af229395f1f6d1feed05b3d0",
        read_only: false,
    },
    PersistedOperationContract {
        operation_name: "CommunityMomentCallout_Claim",
        sha256_hash: "e2d67415aead910f7f9ceb45a77b750a1e1d9622c936d832328a0689e054db62",
        read_only: false,
    },
    PersistedOperationContract {
        operation_name: "JoinRaid",
        sha256_hash: "c6a332a86d1087fbbb1a8623aa01bd1313d2386e7c63be60fdb2d1901f01a4ae",
        read_only: false,
    },
    PersistedOperationContract {
        operation_name: "MakePrediction",
        sha256_hash: "b44682ecc88358817009f20e69d75081b1e58825bb40aa53d5dbadcc17c881d8",
        read_only: false,
    },
    PersistedOperationContract {
        operation_name: "Inventory",
        sha256_hash: "d86775d0ef16a63a33ad52e80eaff963b2d5b72fada7c991504a57496e1d8e4b",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "ViewerDropsDashboard",
        sha256_hash: "5a4da2ab3d5b47c9f9ce864e727b2cb346af1e3ea8b897fe8f704a97ff017619",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "DropsPage_ClaimDropRewards",
        sha256_hash: "a455deea71bdc9015b78eb49f4acfbce8baa7ccbedd28e549bb025bd0f751930",
        read_only: false,
    },
    PersistedOperationContract {
        operation_name: "DropsHighlightService_AvailableDrops",
        sha256_hash: "782dad0f032942260171d2d80a654f88bdd0c5a9dddc392e9bc92218a0f42d20",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "UserPointsContribution",
        sha256_hash: "23ff2c2d60708379131178742327ead913b93b1bd6f665517a6d9085b73f661f",
        read_only: true,
    },
    PersistedOperationContract {
        operation_name: "ContributeCommunityPointsCommunityGoal",
        sha256_hash: "5774f0ea5d89587d73021a2e03c3c44777d903840c608754a1be519f51e37bb6",
        read_only: false,
    },
];

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
pub fn reward_list(channel_id: &str) -> GqlPersistedOperation {
    GqlPersistedOperation::new(
        "RewardList",
        "0b1471876d7647993731b9e3c6a13bf304c67fb31d07f06a945d42286ee377c4",
        json!({
            "channelID": channel_id,
            "shouldIncludeAllSuspendedStreaks": false
        }),
    )
}

#[must_use]
pub fn recent_archived_videos(channel_login: &str) -> GqlPersistedOperation {
    GqlPersistedOperation::new(
        "FilterableVideoTower_Videos",
        "67004f7881e65c297936f32c75246470629557a393788fb5a69d6d9a25a8fd5f",
        json!({
            "broadcastType": "ARCHIVE",
            "channelOwnerLogin": channel_login.to_lowercase(),
            "includePreviewBlur": false,
            "limit": 7,
            "videoSort": "TIME"
        }),
    )
}

#[must_use]
pub fn recent_clips(channel_login: &str) -> GqlPersistedOperation {
    GqlPersistedOperation::new(
        "ClipsCards__User",
        "1cd671bfa12cec480499c087319f26d21925e9695d1f80225aae6a4354f23088",
        json!({
            "criteria": { "filter": "ALL_TIME" },
            "limit": 20,
            "login": channel_login.to_lowercase()
        }),
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
