pub const TWITCH_URL: &str = "https://www.twitch.tv";
pub const GQL_URL: &str = "https://gql.twitch.tv/gql";
pub const CLIENT_ID: &str = "ue6666qo983tsx6so1t0vnawi233wa";
pub const DROP_ID: &str = "c2542d6d-cd10-4532-919b-3d19f30a768b";
pub const DEFAULT_CLIENT_VERSION: &str = "ef928475-9403-42f2-8a34-55784bd08e16";

mod client;
mod contracts;
mod cookies;
mod gql;
mod ids;
pub mod operations;
mod parsers;
mod types;

pub use client::TwitchClient;
pub use contracts::{extract_build_id, extract_settings_script_url, extract_spade_url};
pub use cookies::claim_bonus_cookie_header;
pub use gql::{gql_batch_request, gql_headers, gql_request};
pub use ids::{generate_client_session_id, generate_device_id, generate_transaction_id};
pub use operations::{PersistedOperationContract, PERSISTED_OPERATION_CONTRACTS};
pub use parsers::{
    community_goal_contribution_amount, minute_watched_request, operation_names,
    parse_available_drop_campaign_ids, parse_channel_points_context, parse_followers_page,
    parse_inventory_drops, parse_live_status, parse_stream_info, parse_user_points_contributions,
    validate_claim_bonus_response, validate_claim_drop_response, validate_community_goal_response,
    validate_gql_mutation_response,
};
pub use types::{
    ChannelPointsContext, ClaimBonusOutcome, ClaimDropOutcome, FollowersPage,
    GqlPersistedExtensions, GqlPersistedOperation, GqlPersistedQuery, GqlRequest, InventoryDrop,
    MinuteWatchedRequest, StreamInfo, TwitchClientError, TwitchContractError, TwitchEndpoints,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::{is_twitch_cookie_url, merge_cookie_headers};
    use tm_domain::{CommunityGoal, Stream};

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
    fn persisted_operation_inventory_matches_builders_and_has_unique_names() {
        let operations = [
            operations::get_id_from_login("abc"),
            operations::channel_follows(1, "DESC"),
            operations::channel_points_context("abc"),
            operations::is_stream_live("1"),
            operations::stream_info_overlay("abc"),
            operations::claim_community_points("1", "2"),
            operations::community_moment_claim("moment"),
            operations::join_raid("raid"),
            operations::make_prediction("event", "outcome", 1, "transaction"),
            operations::inventory(),
            operations::viewer_drops_dashboard(),
            operations::claim_drop_rewards("drop"),
            operations::drops_highlight_service_available("1"),
            operations::user_points_contribution("abc"),
            operations::contribute_community_goal(1, "1", "goal", "transaction"),
        ];
        assert_eq!(operations.len(), PERSISTED_OPERATION_CONTRACTS.len());
        let mut names = std::collections::HashSet::new();
        for operation in operations {
            let contract = PERSISTED_OPERATION_CONTRACTS
                .iter()
                .find(|contract| contract.operation_name == operation.operation_name)
                .expect("operation must be inventoried");
            assert_eq!(
                operation.extensions.persisted_query.sha256_hash,
                contract.sha256_hash
            );
            assert!(names.insert(contract.operation_name));
        }
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
