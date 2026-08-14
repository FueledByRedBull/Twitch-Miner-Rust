use super::*;
use crate::client::{
    endpoints_include_loopback_http, is_public_ipv4, normalize_channel_login,
    validate_remote_endpoint, validate_resolved_addresses,
};
use crate::cookies::{is_twitch_cookie_url, merge_cookie_headers};
use crate::hls::{lowest_bandwidth_variant_url, media_segment_url};
use crate::responses::{
    archived_videos_from_typed, available_drop_campaign_ids_from_typed,
    channel_points_context_from_typed, inventory_snapshot_from_typed, recent_clips_from_typed,
    user_contributions_from_typed, watch_streak_milestone_from_typed,
};
use reqwest::dns::Resolve;
use reqwest::StatusCode;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tm_domain::{CommunityGoal, Stream};

#[test]
fn extracts_build_id_from_homepage() {
    let html = concat!(
        r#"window.__twilightBuildID_old = "decoy";"#,
        r#"<script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#
    );
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

    let settings = concat!(
        r#"window.decoy={"spade_url":false};"#,
        r#"window.__settings={"spade_url":"https://spade.example/submit"}"#
    );
    assert_eq!(
        extract_spade_url(settings).unwrap(),
        "https://spade.example/submit"
    );
}

#[test]
fn remote_document_endpoints_require_public_https_or_loopback_http() {
    for accepted in [
        "https://spade.example/submit",
        "https://usher.ttvnw.net/playlist.m3u8",
        "https://8.8.8.8/playlist.m3u8",
    ] {
        let url = reqwest::Url::parse(accepted).unwrap();
        assert!(validate_remote_endpoint(&url, "test_url", false).is_ok());
    }

    for loopback in ["http://127.0.0.1:8080/test", "http://[::1]:8080/test"] {
        let url = reqwest::Url::parse(loopback).unwrap();
        assert!(validate_remote_endpoint(&url, "test_url", true).is_ok());
        assert!(validate_remote_endpoint(&url, "test_url", false).is_err());
    }

    for rejected in [
        "http://spade.example/submit",
        "http://localhost:8080/test",
        "http://miner.local/test",
        "https://user:password@spade.example/submit",
        "https://user@spade.example/submit",
        "https://:password@spade.example/submit",
        "https://localhost:8080/test",
        "https://miner.local/test",
        "https://192.168.1.10/test",
        "ftp://spade.example/test",
    ] {
        let url = reqwest::Url::parse(rejected).unwrap();
        assert!(matches!(
            validate_remote_endpoint(&url, "test_url", false),
            Err(TwitchClientError::InvalidField("test_url"))
        ));
    }
}

#[test]
fn loopback_remote_endpoint_detection_requires_http_and_a_loopback_literal() {
    let endpoints_for = |twitch_url: &str| TwitchEndpoints {
        twitch_url: twitch_url.to_string(),
        ..TwitchEndpoints::default()
    };

    assert!(endpoints_include_loopback_http(&endpoints_for(
        "http://127.0.0.1:8080/test"
    )));
    assert!(!endpoints_include_loopback_http(&endpoints_for(
        "http://8.8.8.8:8080/test"
    )));
    assert!(!endpoints_include_loopback_http(&endpoints_for(
        "https://127.0.0.1:8080/test"
    )));
}

#[test]
fn channel_login_normalization_enforces_nonempty_ascii_identifier() {
    assert_eq!(normalize_channel_login(""), None);
    assert_eq!(normalize_channel_login("   "), None);
    assert_eq!(
        normalize_channel_login("Alice9"),
        Some("alice9".to_string())
    );
    assert_eq!(
        normalize_channel_login(" Alice_9 "),
        Some("alice_9".to_string())
    );
    assert_eq!(normalize_channel_login("alice-9"), None);
}

#[test]
fn ipv4_public_policy_rejects_special_ranges_and_accepts_nearby_addresses() {
    for rejected in [
        (192, 0, 0, 1),
        (192, 0, 2, 1),
        (198, 51, 100, 1),
        (203, 0, 113, 1),
        (198, 18, 0, 1),
        (198, 19, 0, 1),
        (100, 64, 0, 1),
        (100, 127, 0, 1),
        (192, 88, 99, 1),
    ] {
        assert!(
            !is_public_ipv4(Ipv4Addr::new(
                rejected.0, rejected.1, rejected.2, rejected.3
            )),
            "{rejected:?} must remain non-public"
        );
    }

    for accepted in [
        (192, 0, 1, 1),
        (192, 0, 3, 1),
        (198, 50, 100, 1),
        (198, 51, 101, 1),
        (203, 1, 113, 1),
        (198, 17, 0, 1),
        (198, 20, 0, 1),
        (99, 64, 0, 1),
        (100, 63, 0, 1),
        (100, 128, 0, 1),
        (192, 87, 99, 1),
        (192, 88, 98, 1),
    ] {
        assert!(
            is_public_ipv4(Ipv4Addr::new(
                accepted.0, accepted.1, accepted.2, accepted.3
            )),
            "{accepted:?} must remain public"
        );
    }
}

#[test]
fn resolved_remote_endpoints_reject_private_mixed_and_reserved_addresses() {
    let public = SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 443);
    let public_v6 = SocketAddr::new(
        "2001:4860:4860::8888".parse::<Ipv6Addr>().unwrap().into(),
        443,
    );
    assert!(validate_resolved_addresses("cdn.example", &[public], false).is_ok());
    assert!(validate_resolved_addresses("cdn.example", &[public_v6], false).is_ok());
    assert!(validate_resolved_addresses(
        "cdn.example",
        &[SocketAddr::new(Ipv4Addr::new(192, 0, 42, 1).into(), 443)],
        false,
    )
    .is_ok());
    assert!(validate_resolved_addresses(
        "cdn.example",
        &[SocketAddr::new(
            "2001:200::1".parse::<Ipv6Addr>().unwrap().into(),
            443,
        )],
        false,
    )
    .is_ok());

    let private = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 443);
    for address in [
        private,
        SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 443),
        SocketAddr::new(Ipv4Addr::new(169, 254, 1, 1).into(), 443),
        SocketAddr::new(Ipv4Addr::new(192, 0, 2, 1).into(), 443),
        SocketAddr::new(Ipv4Addr::new(192, 88, 99, 1).into(), 443),
        SocketAddr::new("fd00::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("2001::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("2001:1ff::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("2001:db8::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("2001:10::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("2001:20::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("2002:0808:0808::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("3fff::1".parse::<Ipv6Addr>().unwrap().into(), 443),
        SocketAddr::new("ff02::1".parse::<Ipv6Addr>().unwrap().into(), 443),
    ] {
        assert!(validate_resolved_addresses("cdn.example", &[address], false).is_err());
    }

    assert!(validate_resolved_addresses("cdn.example", &[public, private], false).is_err());
    assert!(validate_resolved_addresses("cdn.example", &[], false).is_err());
}

#[tokio::test]
async fn validated_resolver_pins_and_validates_each_resolution_result() {
    let resolver = crate::client::ValidatedDnsResolver {
        allow_loopback_http: true,
    };
    let name = reqwest::dns::Name::from_str("127.0.0.1").unwrap();
    let addresses = (resolver.resolve(name).await.unwrap()).collect::<Vec<_>>();
    assert_eq!(
        addresses,
        vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0)]
    );

    // A later DNS answer is evaluated independently; it cannot inherit the
    // first answer's trust decision (the deterministic rebinding model).
    let public = SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 443);
    let private = SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 443);
    assert!(validate_resolved_addresses("cdn.example", &[public], false).is_ok());
    assert!(validate_resolved_addresses("cdn.example", &[private], false).is_err());
}

#[test]
fn loopback_resolution_is_allowed_only_for_loopback_literals() {
    let loopback = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8080);
    let loopback_v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 8080);
    let public = SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 8080);
    assert!(validate_resolved_addresses("127.0.0.1", &[loopback], true).is_ok());
    assert!(validate_resolved_addresses("[::1]", &[loopback_v6], true).is_ok());
    assert!(validate_resolved_addresses("127.0.0.1", &[loopback], false).is_err());
    assert!(validate_resolved_addresses("127.0.0.1", &[loopback, public], true).is_err());
    assert!(validate_resolved_addresses("public.example", &[loopback], true).is_err());
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

    let playback = operations::playback_access_token("TeStEr");
    assert_eq!(playback.operation_name, "PlaybackAccessToken");
    assert_eq!(playback.variables["login"], "tester");
    assert_eq!(playback.variables["isLive"], true);
    assert_eq!(playback.variables["playerType"], "site");
    assert_eq!(playback.variables["platform"], "web");
    assert!(playback.query.is_none());

    let serialized = serde_json::to_value(playback).unwrap();
    assert!(serialized.get("query").is_none());
    let fallback =
        serde_json::to_value(operations::playback_access_token_fallback("tester")).unwrap();
    assert!(fallback["query"]
        .as_str()
        .is_some_and(|query| query.contains("$platform")));
}

#[test]
fn master_playlist_selects_lowest_bandwidth_video_variant_by_meaning() {
    let base = reqwest::Url::parse("https://video.example/path/master.m3u8").unwrap();
    assert_eq!(
        lowest_bandwidth_variant_url(
            &base,
            concat!(
                r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio_only",URI="audio/index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=90,CODECS="mp4a.40.2",VIDEO="audio_only"
audio/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=800,CODECS="avc1.4d401f,mp4a.40.2",RESOLUTION=1280x720
https://cdn.example/high/index.m3u8
# harmless comment between the metadata and URI
#EXT-X-STREAM-INF:BANDWIDTH=200,CODECS="avc1.4d401e,mp4a.40.2",RESOLUTION=640x360
# another harmless comment
low/index.m3u8"#,
                "   \n"
            ),
        )
        .unwrap()
        .as_str(),
        "https://video.example/path/low/index.m3u8"
    );
}

#[test]
fn spade_contract_decodes_whitespace_and_json_string_escaping() {
    let settings = r#"window.__settings = {
        "spade_url"  :  "https:\/\/spade.example\/submit?separator=\u0026"
    };"#;
    assert_eq!(
        extract_spade_url(settings).unwrap(),
        "https://spade.example/submit?separator=&"
    );
}

#[test]
fn spade_contract_fails_clearly_without_the_primary_json_string_shape() {
    assert!(matches!(
        extract_spade_url(r#"window.__settings={"spadeUrl":"https://spade.example"}"#),
        Err(TwitchContractError::SpadeUrlNotFound)
    ));
    assert!(matches!(
        extract_spade_url(r#"window.__settings={"spade_url":"https:\x2f\x2fexample"}"#),
        Err(TwitchContractError::InvalidSpadeUrlString)
    ));
}

#[test]
fn hls_parsers_resolve_absolute_and_relative_segment_urls() {
    let master_base = reqwest::Url::parse("https://video.example/master.m3u8").unwrap();
    assert_eq!(
        lowest_bandwidth_variant_url(
            &master_base,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=200,CODECS=\"avc1.4d401e\",RESOLUTION=640x360\nhttps://media.example/live/index.m3u8\n",
        )
        .unwrap()
        .as_str(),
        "https://media.example/live/index.m3u8"
    );

    let media_base = reqwest::Url::parse("https://media.example/live/index.m3u8").unwrap();
    assert_eq!(
        media_segment_url(
            &media_base,
            "#EXTM3U\n\n# comment\n#EXTINF:2.0,\n  segments/first.ts  \n#EXTINF:2.0,\nhttps://cdn.example/second.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap()
        .as_str(),
        "https://cdn.example/second.ts"
    );
    assert_eq!(
        media_segment_url(
            &media_base,
            "#EXTM3U\n#EXTINF:0.0,\nhttps://cdn.example/absolute.ts\n",
        )
        .unwrap()
        .as_str(),
        "https://cdn.example/absolute.ts"
    );
}

#[test]
fn hls_parsers_reject_empty_audio_only_and_malformed_playlists() {
    let base = reqwest::Url::parse("https://video.example/path/master.m3u8").unwrap();
    assert!(matches!(
        lowest_bandwidth_variant_url(&base, ""),
        Err(TwitchClientError::InvalidField("master playlist"))
    ));
    assert!(matches!(
        lowest_bandwidth_variant_url(
            &base,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=90,CODECS=\"mp4a.40.2\"\naudio.m3u8\n",
        ),
        Err(TwitchClientError::MissingField("master playlist"))
    ));
    assert!(matches!(
        lowest_bandwidth_variant_url(
            &base,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=not-a-number,RESOLUTION=640x360\nvideo.m3u8\n",
        ),
        Err(TwitchClientError::InvalidField("master playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\nsegment-without-extinf.ts\n"),
        Err(TwitchClientError::MissingField("media playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\n#EXTINF:not-a-duration,\nsegment.ts\n"),
        Err(TwitchClientError::InvalidField("media playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\n#EXTINF:-0.1,\nsegment.ts\n"),
        Err(TwitchClientError::InvalidField("media playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\n#EXTINF:inf,\nsegment.ts\n"),
        Err(TwitchClientError::InvalidField("media playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\n#EXTINF:2.0,\nfirst.ts\n#EXTINF:2.0,\n",),
        Err(TwitchClientError::InvalidField("media playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\n#EXTINF:2.0,\nfirst.ts\nsecond.ts\n",),
        Err(TwitchClientError::InvalidField("media playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\n#EXTINF:2.0\nsegment.ts\n"),
        Err(TwitchClientError::InvalidField("media playlist"))
    ));
    assert!(matches!(
        media_segment_url(&base, "#EXTM3U\n#EXTINF:2.0,\n#EXTINF:2.0,\nsegment.ts\n",),
        Err(TwitchClientError::InvalidField("media playlist"))
    ));
}

#[test]
fn hls_media_parser_ignores_ll_hls_partial_and_trailing_tags() {
    let base = reqwest::Url::parse("https://video.example/live/index.m3u8").unwrap();
    assert_eq!(
        media_segment_url(
            &base,
            concat!(
                "#EXTM3U\n",
                "#EXT-X-TARGETDURATION:2\n",
                "#EXT-X-PART:DURATION=0.33333,URI=\"part-0.ts\"\n",
                "#EXTINF:2.0,\nsegment-0.ts\n",
                "#EXT-X-DISCONTINUITY\n",
                "#EXTINF:2.0,\nsegment-1.ts\n",
                "#EXT-X-PART:DURATION=0.33333,URI=\"part-2.ts\"\n",
                "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part-3.ts\"\n",
                "#EXT-X-ENDLIST\n",
            ),
        )
        .unwrap()
        .as_str(),
        "https://video.example/live/segment-1.ts"
    );
    assert!(matches!(
        media_segment_url(
            &base,
            "#EXTM3U\n#EXT-X-PART:DURATION=0.33333,URI=\"part-0.ts\"\n#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part-1.ts\"\n",
        ),
        Err(TwitchClientError::MissingField("media playlist"))
    ));
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
    assert!(!is_twitch_cookie_url("http://gql.twitch.tv/gql"));
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
        operations::playback_access_token("abc").operation_name,
        "PlaybackAccessToken"
    );
    assert_eq!(operations::reward_list("abc").operation_name, "RewardList");
    assert_eq!(
        operations::recent_archived_videos("abc").operation_name,
        "FilterableVideoTower_Videos"
    );
    assert_eq!(
        operations::recent_clips("abc").operation_name,
        "ClipsCards__User"
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
        operations::playback_access_token("abc"),
        operations::reward_list("abc"),
        operations::recent_archived_videos("abc"),
        operations::recent_clips("abc"),
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
    let single = serde_json::to_value(operations::get_id_from_login("tester")).expect("serialize");
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
            ..TwitchEndpoints::default()
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
fn classifies_sanitized_failure_categories() {
    assert_eq!(
        TwitchClientError::UnexpectedStatus {
            status: StatusCode::UNAUTHORIZED,
            context: "test",
        }
        .failure_class(),
        TwitchFailureClass::Unauthorized
    );
    assert_eq!(
        TwitchClientError::UnexpectedStatus {
            status: StatusCode::TOO_MANY_REQUESTS,
            context: "test",
        }
        .failure_class(),
        TwitchFailureClass::RateLimited
    );
    assert_eq!(
        TwitchClientError::UnexpectedStatus {
            status: StatusCode::BAD_GATEWAY,
            context: "test",
        }
        .failure_class(),
        TwitchFailureClass::ServerError
    );
}

#[test]
fn parses_numeric_and_http_date_retry_after_values() {
    assert_eq!(
        crate::client::retry_after_duration("0"),
        Some(Duration::from_secs(0))
    );
    assert!(crate::client::retry_after_duration("Wed, 31 Dec 2099 23:59:59 GMT").is_some());
    assert!(crate::client::retry_after_duration("not-a-date").is_none());
}

#[tokio::test]
async fn retries_read_only_requests_and_honors_retry_after() {
    let (base_url, requests, server) = spawn_http_server([
        (
            200,
            "<script>window.__twilightBuildID = \"ef928475-9403-42f2-8a34-55784bd08e16\"</script>",
        ),
        (429, ""),
        (200, r#"{"data":{"user":{"id":"100"}}}"#),
    ]);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: format!("{base_url}/gql"),
            ..TwitchEndpoints::default()
        },
    );

    assert_eq!(client.fetch_channel_id("tester").await.unwrap(), "100");
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    server.join().unwrap();
}

#[tokio::test]
async fn playback_network_errors_do_not_expose_tokenized_urls() {
    let (base_url, requests, server) = spawn_http_server([
        (
            200,
            "<script>window.__twilightBuildID = \"ef928475-9403-42f2-8a34-55784bd08e16\"</script>",
        ),
        (
            200,
            r#"{"data":{"streamPlaybackAccessToken":{"signature":"secret-signature","value":"secret-token"}}}"#,
        ),
    ]);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: format!("{base_url}/gql"),
            playback_url: String::from("http://127.0.0.1:9/hls/"),
        },
    );

    let error = client.prime_live_playback("tester").await.unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, TwitchClientError::PlaybackRequest { .. }));
    assert!(!message.contains("secret-signature"));
    assert!(!message.contains("secret-token"));
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    server.join().unwrap();
}

#[tokio::test]
async fn spade_network_errors_do_not_expose_tokenized_urls() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let base_url = format!("http://{address}");
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: format!("{base_url}/gql"),
            playback_url: format!("{base_url}/hls/"),
        },
    );
    let stream = Stream {
        payload: vec![serde_json::json!({
            "event": "minute-watched",
            "properties": { "broadcast_id": "123" }
        })],
        ..Stream::default()
    };

    let error = client
        .send_minute_watched(&format!("{base_url}/minute?sig=secret-token"), &stream)
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, TwitchClientError::PlaybackRequest { .. }));
    assert!(!message.contains("secret-token"));
    assert!(!message.contains(&base_url));
}

#[tokio::test]
async fn remote_document_network_errors_do_not_expose_tokenized_urls() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let page_url = format!("http://{address}/settings?sig=secret-token");
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: format!("http://{address}"),
            ..TwitchEndpoints::default()
        },
    );

    let error = client
        .fetch_settings_script_url(&page_url)
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(
        matches!(error, TwitchClientError::RemoteRequest { .. }),
        "unexpected sanitized error: {error:?}"
    );
    assert!(!message.contains("secret-token"));
    assert!(!message.contains(&address.to_string()));
}

#[tokio::test]
async fn remote_document_redirects_are_rejected_without_following_escape_target() {
    let (base_url, requests, server) = spawn_redirect_server();
    let page_url = base_url.clone();
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url,
            ..TwitchEndpoints::default()
        },
    );

    let error = client
        .fetch_settings_script_url(&page_url)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        TwitchClientError::UnexpectedStatus {
            status: StatusCode::FOUND,
            ..
        }
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    server.join().unwrap();
}

#[tokio::test]
async fn playback_priming_repeats_all_four_requests_on_every_tick() {
    let (endpoints, requests, server) = spawn_playback_server(2, false);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        "token",
        "ua",
        endpoints,
    );

    client.prime_live_playback("tester").await.unwrap();
    client.prime_live_playback("tester").await.unwrap();
    server.join().unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.contains(r#""operationName":"PlaybackAccessToken""#))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("GET /hls/tester.m3u8"))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("GET /variant.m3u8"))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("HEAD /segment.ts"))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("HEAD /older-segment.ts"))
            .count(),
        0
    );
    assert!(requests
        .iter()
        .filter(|request| request.contains(r#""operationName":"PlaybackAccessToken""#))
        .all(|request| !request.contains(r#""query":"#)));
}

#[tokio::test]
async fn playback_query_is_sent_only_after_persisted_hash_miss() {
    let (endpoints, requests, server) = spawn_playback_server(1, true);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        "token",
        "ua",
        endpoints,
    );

    client.prime_live_playback("tester").await.unwrap();
    server.join().unwrap();

    let requests = requests.lock().unwrap();
    let gql = requests
        .iter()
        .filter(|request| request.contains(r#""operationName":"PlaybackAccessToken""#))
        .collect::<Vec<_>>();
    assert_eq!(gql.len(), 2);
    assert!(!gql[0].contains(r#""query":"#));
    assert!(gql[1].contains(r#""query":"#));
}

#[tokio::test]
async fn retries_only_fixed_read_only_gql_service_errors() {
    let (base_url, requests, server) = spawn_http_server([
        (
            200,
            "<script>window.__twilightBuildID = \"ef928475-9403-42f2-8a34-55784bd08e16\"</script>",
        ),
        (
            200,
            r#"{"errors":[{"message":"service error","path":["user","videos"]}]}"#,
        ),
        (200, r#"{"data":{"user":{"id":"100"}}}"#),
    ]);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: base_url,
            ..TwitchEndpoints::default()
        },
    );

    assert_eq!(client.fetch_channel_id("tester").await.unwrap(), "100");
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    server.join().unwrap();

    let (base_url, requests, server) = spawn_http_server([
        (
            200,
            "<script>window.__twilightBuildID = \"ef928475-9403-42f2-8a34-55784bd08e16\"</script>",
        ),
        (
            200,
            r#"{"errors":[{"message":"service error"},{"message":"contract changed"}]}"#,
        ),
    ]);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: base_url,
            ..TwitchEndpoints::default()
        },
    );

    assert!(matches!(
        client.fetch_channel_id("tester").await,
        Err(TwitchClientError::GqlErrors { .. })
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    server.join().unwrap();

    let (base_url, requests, server) = spawn_http_server([
        (
            200,
            "<script>window.__twilightBuildID = \"ef928475-9403-42f2-8a34-55784bd08e16\"</script>",
        ),
        (200, r#"{"errors":[{"message":"service error"}]}"#),
        (200, r#"{"errors":[{"message":"service error"}]}"#),
        (200, r#"{"errors":[{"message":"service error"}]}"#),
    ]);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: base_url,
            ..TwitchEndpoints::default()
        },
    );

    assert!(matches!(
        client.fetch_channel_id("tester").await,
        Err(TwitchClientError::GqlErrors { .. })
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    server.join().unwrap();
}

#[tokio::test]
async fn resolves_current_login_from_stable_channel_id() {
    let (base_url, requests, server) = spawn_http_server([
        (
            200,
            "<script>window.__twilightBuildID = \"ef928475-9403-42f2-8a34-55784bd08e16\"</script>",
        ),
        (200, r#"{"data":{"user":{"id":"100","login":"new-login"}}}"#),
    ]);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: format!("{base_url}/gql"),
            ..TwitchEndpoints::default()
        },
    );

    assert_eq!(
        client.fetch_channel_login_by_id("100").await.unwrap(),
        "new-login"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    server.join().unwrap();
}

#[tokio::test]
async fn mutation_failure_is_not_replayed_after_an_uncertain_response() {
    let (base_url, requests, server) = spawn_http_server([
        (
            200,
            "<script>window.__twilightBuildID = \"ef928475-9403-42f2-8a34-55784bd08e16\"</script>",
        ),
        (503, ""),
    ]);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: format!("{base_url}/gql"),
            ..TwitchEndpoints::default()
        },
    );

    let error = client.claim_moment("moment-1").await.unwrap_err();
    assert!(matches!(
        error,
        TwitchClientError::UnexpectedStatus {
            status: StatusCode::SERVICE_UNAVAILABLE,
            ..
        }
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    server.join().unwrap();
}

#[tokio::test]
async fn mutation_inputs_fail_closed_before_network_io() {
    let client = TwitchClient::new("token", "ua").unwrap();
    for error in [
        client.claim_bonus("", "claim-1", None).await.unwrap_err(),
        client.claim_moment("").await.unwrap_err(),
        client.join_raid("").await.unwrap_err(),
        client
            .make_prediction("event", "outcome", 9)
            .await
            .unwrap_err(),
        client
            .make_prediction(
                "event",
                "outcome",
                i64::from(tm_domain::MAX_PREDICTION_POINTS) + 1,
            )
            .await
            .unwrap_err(),
        client.claim_drop("").await.unwrap_err(),
        client
            .contribute_community_goal(0, "channel", "goal")
            .await
            .unwrap_err(),
    ] {
        assert!(matches!(error, TwitchClientError::MutationRejected { .. }));
    }

    let unauthenticated = TwitchClient::new("", "ua").unwrap();
    assert!(matches!(
        unauthenticated.claim_moment("moment-1").await.unwrap_err(),
        TwitchClientError::MutationRejected { context, .. } if context == "mutation"
    ));
}

#[tokio::test]
async fn read_timeout_is_bounded_and_classified() {
    let (base_url, server) = spawn_fault_server(true);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: format!("{base_url}/gql"),
            ..TwitchEndpoints::default()
        },
    );

    let error = client.fetch_channel_id("tester").await.unwrap_err();
    assert_eq!(error.failure_class(), TwitchFailureClass::Timeout);
    server.join().unwrap();
}

#[tokio::test]
async fn connection_reset_is_bounded_and_classified() {
    let (base_url, server) = spawn_fault_server(false);
    let client = TwitchClient::with_client_and_endpoints(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap(),
        "token",
        "ua",
        TwitchEndpoints {
            twitch_url: base_url.clone(),
            gql_url: format!("{base_url}/gql"),
            ..TwitchEndpoints::default()
        },
    );

    let error = client.fetch_channel_id("tester").await.unwrap_err();
    assert_eq!(error.failure_class(), TwitchFailureClass::ConnectionReset);
    server.join().unwrap();
}

fn spawn_http_server<const N: usize>(
    responses: [(u16, &'static str); N],
) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = Arc::clone(&requests);
    let server = thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            request_count.fetch_add(1, Ordering::SeqCst);
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                503 => "Service Unavailable",
                _ => "Response",
            };
            let retry_after = if status == 429 {
                "Retry-After: 0\r\n"
            } else {
                ""
            };
            let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n{retry_after}Connection: close\r\n\r\n{body}",
                    body.len()
                );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (format!("http://{address}"), requests, server)
}

fn spawn_redirect_server() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = Arc::clone(&requests);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_http_request(&mut stream);
        request_count.fetch_add(1, Ordering::SeqCst);
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            address.port()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    (format!("http://{address}"), requests, server)
}

fn spawn_playback_server(
    tick_count: usize,
    first_apq_miss: bool,
) -> (
    TwitchEndpoints,
    Arc<std::sync::Mutex<Vec<String>>>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let server = thread::spawn(move || {
        let mut gql_requests = 0_usize;
        for _ in 0..(1 + 4 * tick_count + usize::from(first_apq_miss)) {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            recorded.lock().unwrap().push(request.clone());
            let body = if request.starts_with("GET / ") {
                r#"<script>window.__twilightBuildID = "ef928475-9403-42f2-8a34-55784bd08e16"</script>"#
            } else if request.contains(r#""operationName":"PlaybackAccessToken""#) {
                gql_requests += 1;
                if first_apq_miss && gql_requests == 1 {
                    r#"{"errors":[{"message":"PersistedQueryNotFound"}]}"#
                } else {
                    r#"{"data":{"streamPlaybackAccessToken":{"signature":"sig","value":"token"}}}"#
                }
            } else if request.starts_with("GET /hls/tester.m3u8") {
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=200,CODECS=\"avc1.4d401e,mp4a.40.2\",RESOLUTION=640x360\n/variant.m3u8\n"
            } else if request.starts_with("GET /variant.m3u8") {
                "#EXTM3U\n#EXTINF:2.0,\n/older-segment.ts\n#EXTINF:2.0,\n/segment.ts\n"
            } else if request.starts_with("HEAD /segment.ts") {
                ""
            } else {
                panic!("unexpected playback request: {request}");
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (
        TwitchEndpoints {
            twitch_url: format!("http://{address}"),
            gql_url: format!("http://{address}/gql"),
            playback_url: format!("http://{address}/hls/"),
        },
        requests,
        server,
    )
}

fn read_http_request(stream: &mut TcpStream) {
    let mut buffer = [0_u8; 4096];
    let _ = stream.read(&mut buffer);
}

fn spawn_fault_server(timeout: bool) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            if timeout {
                thread::sleep(Duration::from_millis(100));
            } else {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    });
    (format!("http://{address}"), server)
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
                        "isEnabled": true,
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
    assert_eq!(context.channel_points_enabled, Some(true));
    assert_eq!(context.claim_id.as_deref(), Some("claim-1"));
    assert_eq!(context.active_multiplier_count, 2);
    assert_eq!(context.active_multipliers.len(), 2);
    assert!((context.active_multipliers[0].factor - 1.5).abs() < f64::EPSILON);
    assert_eq!(context.community_goals.len(), 1);
}

#[test]
fn parses_stream_info_shape() {
    let mut payload = serde_json::json!({
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
    payload["data"]["user"]["stream"]["createdAt"] = serde_json::json!("invalid");
    assert!(parse_stream_info(&payload).unwrap().created_at.is_none());
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

fn protocol_fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    serde_json::from_slice(&std::fs::read(path).expect("fixture must be readable"))
        .expect("fixture must be valid JSON")
}

fn typed_identity_and_stream_fixtures() {
    let user_id: types::GqlResponse<types::UserIdData> =
        serde_json::from_value(protocol_fixture("twitch.user_id.json")).unwrap();
    assert_eq!(
        user_id.data.unwrap().user.unwrap().id.as_deref(),
        Some("100")
    );

    let context: types::GqlResponse<types::ChannelPointsData> =
        serde_json::from_value(protocol_fixture("twitch.channel_points_context.json")).unwrap();
    let context = context.data.unwrap();
    assert_eq!(
        context
            .community
            .unwrap()
            .channel
            .unwrap()
            .self_data
            .unwrap()
            .points
            .unwrap()
            .balance,
        Some(1234)
    );

    let live: types::GqlResponse<types::LiveStatusData> =
        serde_json::from_value(protocol_fixture("twitch.stream_live.online.json")).unwrap();
    assert!(live.data.unwrap().user.unwrap().stream.is_some());

    let stream: types::GqlResponse<types::StreamInfoData> =
        serde_json::from_value(protocol_fixture("twitch.stream_info.json")).unwrap();
    let stream = stream.data.unwrap().user.unwrap().stream.unwrap();
    assert_eq!(stream.id.as_deref(), Some("stream-1"));
    assert_eq!(stream.tags.len(), 2);
    assert_eq!(stream.created_at.as_deref(), Some("2026-07-13T10:00:00Z"));

    let reward: types::GqlResponse<types::RewardListData> =
        serde_json::from_value(protocol_fixture("twitch.reward_list.json")).unwrap();
    let milestone = watch_streak_milestone_from_typed(reward.data.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(milestone.value, Some(5));
    assert_eq!(
        milestone.achievement_timestamp.unix_timestamp(),
        1_783_938_600
    );
    assert_eq!(
        milestone.expires_at.unwrap().unix_timestamp(),
        1_784_025_000
    );

    let videos: types::GqlResponse<types::ArchivedVideosData> =
        serde_json::from_value(protocol_fixture("twitch.archived_videos.json")).unwrap();
    let videos = archived_videos_from_typed(videos.data.unwrap()).unwrap();
    assert_eq!(videos[0].broadcast_id.as_deref(), Some("broadcast-1"));
    assert_eq!(videos[0].length_seconds, 1200);

    let sparse_videos: types::GqlResponse<types::ArchivedVideosData> =
        serde_json::from_value(serde_json::json!({
            "data": { "user": { "videos": { "edges": [
                { "node": null },
                { "node": {
                    "id": "usable",
                    "broadcastIdentifier": null,
                    "lengthSeconds": 600
                } }
            ] } } }
        }))
        .unwrap();
    let sparse_videos = archived_videos_from_typed(sparse_videos.data.unwrap()).unwrap();
    assert_eq!(sparse_videos.len(), 1);
    assert_eq!(sparse_videos[0].id, "usable");
    assert_eq!(sparse_videos[0].broadcast_id, None);

    let invalid_video: types::GqlResponse<types::ArchivedVideosData> =
        serde_json::from_value(serde_json::json!({
            "data": { "user": { "videos": { "edges": [
                { "node": { "id": "missing-length" } }
            ] } } }
        }))
        .unwrap();
    assert!(matches!(
        archived_videos_from_typed(invalid_video.data.unwrap()),
        Err(TwitchClientError::MissingField(
            "data.user.videos.edges.node.lengthSeconds"
        ))
    ));

    let clips: types::GqlResponse<types::RecentClipsData> =
        serde_json::from_value(protocol_fixture("twitch.recent_clips.json")).unwrap();
    let clips = recent_clips_from_typed(clips.data.unwrap()).unwrap();
    assert_eq!(clips[0].slug, "UsefulClip");
    assert!((clips[0].duration_seconds - 24.5).abs() < f64::EPSILON);

    let followers: types::GqlResponse<types::FollowersData> =
        serde_json::from_value(protocol_fixture("twitch.followers.json")).unwrap();
    let follows = followers.data.unwrap().user.unwrap().follows.unwrap();
    assert_eq!(follows.edges.as_ref().unwrap().len(), 2);
    assert!(follows.page_info.unwrap().has_next_page);
}

fn typed_inventory_fixtures() {
    let inventory: types::GqlResponse<types::InventoryData> =
        serde_json::from_value(protocol_fixture("twitch.inventory.json")).unwrap();
    let campaigns = inventory
        .data
        .unwrap()
        .current_user
        .unwrap()
        .inventory
        .unwrap()
        .campaigns
        .unwrap();
    assert_eq!(campaigns[0].drops.len(), 1);
    assert_eq!(
        campaigns[0].drops[0]
            .self_data
            .as_ref()
            .unwrap()
            .drop_instance_id
            .as_deref(),
        Some("drop-1")
    );

    let campaigns: types::GqlResponse<types::AvailableDropsData> =
        serde_json::from_value(protocol_fixture("twitch.available_drop_campaigns.json")).unwrap();
    assert_eq!(
        campaigns
            .data
            .unwrap()
            .channel
            .unwrap()
            .campaigns
            .unwrap()
            .len(),
        2
    );

    let contributions: types::GqlResponse<types::UserContributionData> =
        serde_json::from_value(protocol_fixture("twitch.user_points_contribution.json")).unwrap();
    assert_eq!(
        contributions
            .data
            .unwrap()
            .user
            .unwrap()
            .channel
            .unwrap()
            .self_data
            .unwrap()
            .community_points
            .unwrap()
            .contributions
            .len(),
        2
    );
}

fn typed_mutation_fixtures() {
    let dashboard: types::GqlResponse<types::ViewerDropsDashboard> =
        serde_json::from_value(protocol_fixture("twitch.viewer_drops_dashboard.json")).unwrap();
    assert!(dashboard.data.unwrap().fields.contains_key("serverField"));

    let bonus: types::GqlResponse<types::ClaimBonusData> =
        serde_json::from_value(protocol_fixture("twitch.claim_bonus_success.json")).unwrap();
    assert_eq!(
        bonus.data.unwrap().claim.unwrap().status.as_deref(),
        Some("SUCCESS")
    );

    let drop: types::GqlResponse<types::ClaimDropData> =
        serde_json::from_value(protocol_fixture("twitch.claim_drop_success.json")).unwrap();
    assert_eq!(
        drop.data.unwrap().claim.unwrap().status.as_deref(),
        Some("ELIGIBLE_FOR_ALL")
    );

    let goal: types::GqlResponse<types::CommunityGoalContributionData> = serde_json::from_value(
        protocol_fixture("twitch.community_goal_contribution_success.json"),
    )
    .unwrap();
    assert!(goal.data.unwrap().contribution.is_some());

    let empty: types::GqlResponse<types::EmptyMutationData> =
        serde_json::from_value(protocol_fixture("twitch.empty_mutation_success.json")).unwrap();
    assert!(empty.data.is_some());
}

#[test]
fn typed_protocol_fixtures_cover_all_runtime_response_families() {
    typed_identity_and_stream_fixtures();
    typed_inventory_fixtures();
    typed_mutation_fixtures();
}

#[test]
fn typed_context_fails_closed_on_missing_or_invalid_goal_financial_fields() {
    let context: types::GqlResponse<types::ChannelPointsData> =
        serde_json::from_value(protocol_fixture("twitch.channel_points_context.json")).unwrap();
    let context = channel_points_context_from_typed(context.data.unwrap()).unwrap();
    assert_eq!(context.channel_points_enabled, Some(true));
    assert_eq!(context.community_goals[0].points_contributed, 5);
    assert_eq!(context.community_goals[0].amount_needed, 10);

    let missing_points: types::GqlResponse<types::ChannelPointsData> =
        serde_json::from_value(serde_json::json!({
            "data": {
                "community": {
                    "channel": {
                        "self": { "communityPoints": { "balance": 0 } },
                        "communityPointsSettings": {
                            "goals": [{
                                "id": "goal-1",
                                "amountNeeded": 10,
                                "perStreamUserMaximumContribution": 5
                            }]
                        }
                    }
                }
            }
        }))
        .unwrap();
    assert!(matches!(
        channel_points_context_from_typed(missing_points.data.unwrap()),
        Err(TwitchClientError::MissingField(
            "data.community.channel.communityPointsSettings.goals.pointsContributed"
        ))
    ));

    let negative_points: types::GqlResponse<types::ChannelPointsData> =
        serde_json::from_value(serde_json::json!({
            "data": {
                "community": {
                    "channel": {
                        "self": { "communityPoints": { "balance": 0 } },
                        "communityPointsSettings": {
                            "goals": [{
                                "id": "goal-1",
                                "pointsContributed": -1,
                                "amountNeeded": 10,
                                "perStreamUserMaximumContribution": 5
                            }]
                        }
                    }
                }
            }
        }))
        .unwrap();
    assert!(matches!(
        channel_points_context_from_typed(negative_points.data.unwrap()),
        Err(TwitchClientError::InvalidField(
            "data.community.channel.communityPointsSettings.goals.pointsContributed"
        ))
    ));
}

#[test]
fn channel_points_disabled_allows_missing_balance_without_becoming_creditable() {
    let payload = serde_json::json!({
        "data": {
            "community": {
                "channel": {
                    "self": { "communityPoints": null },
                    "communityPointsSettings": { "isEnabled": false, "goals": [] }
                }
            }
        }
    });
    let typed: types::GqlResponse<types::ChannelPointsData> =
        serde_json::from_value(payload.clone()).unwrap();
    let context = channel_points_context_from_typed(typed.data.unwrap()).unwrap();
    assert_eq!(context.balance, 0);
    assert_eq!(context.channel_points_enabled, Some(false));
    assert!(context.claim_id.is_none());
    assert!(parse_channel_points_context(&payload)
        .unwrap()
        .channel_points_enabled
        .is_some_and(|enabled| !enabled));
}

#[test]
fn channel_points_enabled_unknown_is_preserved_and_malformed_is_rejected() {
    let payload = serde_json::json!({
        "data": {
            "community": {
                "channel": {
                    "self": { "communityPoints": { "balance": 10 } },
                    "communityPointsSettings": { "goals": [] }
                }
            }
        }
    });
    let typed: types::GqlResponse<types::ChannelPointsData> =
        serde_json::from_value(payload.clone()).unwrap();
    assert_eq!(
        channel_points_context_from_typed(typed.data.unwrap())
            .unwrap()
            .channel_points_enabled,
        None
    );

    let mut null_marker = payload.clone();
    null_marker["data"]["community"]["channel"]["communityPointsSettings"]["isEnabled"] =
        serde_json::Value::Null;
    assert_eq!(
        parse_channel_points_context(&null_marker)
            .unwrap()
            .channel_points_enabled,
        None
    );
    let null_typed: types::GqlResponse<types::ChannelPointsData> =
        serde_json::from_value(null_marker).unwrap();
    assert_eq!(
        channel_points_context_from_typed(null_typed.data.unwrap())
            .unwrap()
            .channel_points_enabled,
        None
    );

    let malformed = serde_json::json!({
        "data": {
            "community": {
                "channel": {
                    "self": { "communityPoints": { "balance": 10 } },
                    "communityPointsSettings": { "isEnabled": "yes", "goals": [] }
                }
            }
        }
    });
    assert!(matches!(
        parse_channel_points_context(&malformed),
        Err(TwitchClientError::InvalidField(
            "data.community.channel.communityPointsSettings.isEnabled"
        ))
    ));
    let malformed_typed: Result<types::GqlResponse<types::ChannelPointsData>, _> =
        serde_json::from_value(malformed);
    assert!(malformed_typed.is_err());
}

#[test]
fn typed_inventory_fixtures_fail_closed_on_claim_safety_fields() {
    let missing_required: types::GqlResponse<types::InventoryData> =
        serde_json::from_value(serde_json::json!({
            "data": {
                "currentUser": {
                    "inventory": {
                        "dropCampaignsInProgress": [{
                            "timeBasedDrops": [{
                                "self": {
                                    "dropInstanceID": "drop-1",
                                    "isClaimed": false
                                }
                            }]
                        }]
                    }
                }
            }
        }))
        .unwrap();
    assert!(matches!(
        inventory_snapshot_from_typed(missing_required.data.unwrap()),
        Err(TwitchClientError::MissingField(
            "data.currentUser.inventory.timeBasedDrops.requiredMinutesWatched"
        ))
    ));

    let missing_claimed: types::GqlResponse<types::InventoryData> =
        serde_json::from_value(serde_json::json!({
            "data": {
                "currentUser": {
                    "inventory": {
                        "dropCampaignsInProgress": [{
                            "timeBasedDrops": [{
                                "requiredMinutesWatched": 60,
                                "self": { "dropInstanceID": "drop-1" }
                            }]
                        }]
                    }
                }
            }
        }))
        .unwrap();
    assert!(matches!(
        inventory_snapshot_from_typed(missing_claimed.data.unwrap()),
        Err(TwitchClientError::MissingField(
            "data.currentUser.inventory.timeBasedDrops.self.isClaimed"
        ))
    ));
}

#[test]
fn typed_inventory_classifies_only_fully_claimed_campaigns_as_complete() {
    let inventory: types::GqlResponse<types::InventoryData> =
        serde_json::from_value(serde_json::json!({
            "data": {
                "currentUser": {
                    "inventory": {
                        "dropCampaignsInProgress": [
                            {
                                "id": "campaign-complete",
                                "timeBasedDrops": [{
                                    "requiredMinutesWatched": 60,
                                    "self": {
                                        "dropInstanceID": "drop-complete",
                                        "currentMinutesWatched": 60,
                                        "isClaimed": true
                                    }
                                }]
                            },
                            {
                                "id": "campaign-incomplete",
                                "timeBasedDrops": [
                                    {
                                        "requiredMinutesWatched": 60,
                                        "self": {
                                            "dropInstanceID": "drop-claimed",
                                            "currentMinutesWatched": 60,
                                            "isClaimed": true
                                        }
                                    },
                                    {
                                        "requiredMinutesWatched": 120,
                                        "self": null
                                    }
                                ]
                            },
                            {
                                "timeBasedDrops": [{
                                    "requiredMinutesWatched": 30,
                                    "self": {
                                        "dropInstanceID": "drop-without-campaign-id",
                                        "currentMinutesWatched": 30,
                                        "isClaimed": true
                                    }
                                }]
                            }
                        ]
                    }
                }
            }
        }))
        .unwrap();

    let snapshot = inventory_snapshot_from_typed(inventory.data.unwrap()).unwrap();
    assert_eq!(
        snapshot.completed_campaign_ids,
        vec![String::from("campaign-complete")]
    );
    assert_eq!(snapshot.drops.len(), 3);
}

#[test]
fn typed_campaign_and_contribution_lists_fail_closed_on_missing_ids() {
    let campaigns: types::GqlResponse<types::AvailableDropsData> =
        serde_json::from_value(protocol_fixture("twitch.available_drop_campaigns.json")).unwrap();
    assert_eq!(
        available_drop_campaign_ids_from_typed(campaigns.data.unwrap()).unwrap(),
        vec![String::from("campaign-1"), String::from("campaign-2")]
    );

    for no_campaigns in [
        serde_json::json!({ "data": { "channel": null } }),
        serde_json::json!({
            "data": { "channel": { "viewerDropCampaigns": null } }
        }),
        serde_json::json!({
            "data": { "channel": { "viewerDropCampaigns": [] } }
        }),
    ] {
        let response: types::GqlResponse<types::AvailableDropsData> =
            serde_json::from_value(no_campaigns).unwrap();
        assert!(
            available_drop_campaign_ids_from_typed(response.data.unwrap())
                .unwrap()
                .is_empty()
        );
    }

    let missing_campaign_id: types::GqlResponse<types::AvailableDropsData> =
        serde_json::from_value(serde_json::json!({
            "data": { "channel": { "viewerDropCampaigns": [{ "id": " " }] } }
        }))
        .unwrap();
    assert!(matches!(
        available_drop_campaign_ids_from_typed(missing_campaign_id.data.unwrap()),
        Err(TwitchClientError::MissingField(
            "data.channel.viewerDropCampaigns.id"
        ))
    ));

    let contributions: types::GqlResponse<types::UserContributionData> =
        serde_json::from_value(protocol_fixture("twitch.user_points_contribution.json")).unwrap();
    assert_eq!(
        user_contributions_from_typed(contributions.data.unwrap()).unwrap(),
        vec![(String::from("goal-1"), 25), (String::from("goal-2"), 10)]
    );

    let missing_goal_id: types::GqlResponse<types::UserContributionData> =
        serde_json::from_value(serde_json::json!({
            "data": {
                "user": {
                    "channel": {
                        "self": {
                            "communityPoints": {
                                "goalContributions": [{ "goal": {} }]
                            }
                        }
                    }
                }
            }
        }))
        .unwrap();
    assert!(matches!(
        user_contributions_from_typed(missing_goal_id.data.unwrap()),
        Err(TwitchClientError::MissingField(
            "data.user.channel.self.communityPoints.goalContributions.goal.id"
        ))
    ));

    let missing_points: types::GqlResponse<types::UserContributionData> =
        serde_json::from_value(serde_json::json!({
            "data": {
                "user": {
                    "channel": {
                        "self": {
                            "communityPoints": {
                                "goalContributions": [{
                                    "goal": { "id": "goal-1" }
                                }]
                            }
                        }
                    }
                }
            }
        }))
        .unwrap();
    assert!(matches!(
            user_contributions_from_typed(missing_points.data.unwrap()),
            Err(TwitchClientError::MissingField(
                "data.user.channel.self.communityPoints.goalContributions.userPointsContributedThisStream"
            ))
        ));
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
