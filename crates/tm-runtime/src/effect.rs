#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEffect {
    ClaimBonus {
        channel_id: String,
        claim_id: String,
    },
    ClaimMoment {
        channel_id: String,
        moment_id: String,
    },
    JoinRaid {
        channel_id: String,
        raid_id: String,
        target_login: String,
    },
    ContributeCommunityGoals {
        channel_id: String,
    },
    EvaluatePrediction {
        event_id: String,
    },
    PredictionSettled {
        event_id: String,
        streamer_username: String,
        title: String,
        decision_label: String,
        result_type: String,
        result_string: String,
    },
}
