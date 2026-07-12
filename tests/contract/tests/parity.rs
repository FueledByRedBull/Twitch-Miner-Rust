use std::fs;
use std::path::Path;

use tm_config::{build_base_streamer_settings, BetConfig, ConfigFile};
use tm_domain::{
    parse_watch_priorities, pick_streamers_to_watch, BetSettings, Game, HistoryEntry,
    OffsetDateTime, PredictionEvent, PredictionOutcome, Strategy, Stream, Streamer,
    StreamerSettings,
};
use tm_pubsub::{parse_message, PubSubEvent};
use tm_runtime::apply_pubsub_gain;

fn vectors() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../parity/vectors.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn ts(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

#[test]
fn normalized_settings_contract_matches_expected() {
    let value = vectors();
    let input = &value["settings"];
    let bet = &input["bet"];
    let config = ConfigFile {
        betting_make_predictions: input["betting_make_predictions"].as_bool().unwrap(),
        follow_raid: input["follow_raid"].as_bool().unwrap(),
        claim_drops: input["claim_drops"].as_bool().unwrap(),
        community_goals: input["community_goals"].as_bool().unwrap(),
        chat_presence: input["chat_presence"].as_str().unwrap().to_string(),
        bet: BetConfig {
            strategy: Some(bet["strategy"].as_str().unwrap().to_string()),
            percentage: Some(bet["percentage"].as_u64().unwrap() as u32),
            percentage_gap: Some(bet["percentage_gap"].as_u64().unwrap() as u32),
            max_points: Some(bet["max_points"].as_u64().unwrap() as u32),
            minimum_points: Some(bet["minimum_points"].as_u64().unwrap() as u32),
            stealth_mode: Some(bet["stealth_mode"].as_bool().unwrap()),
            deduct_stake_on_place: Some(bet["deduct_stake_on_place"].as_bool().unwrap()),
            delay_mode: Some(bet["delay_mode"].as_str().unwrap().to_string()),
            delay: Some(bet["delay"].as_f64().unwrap()),
            filter_condition: None,
        },
        ..ConfigFile::default()
    };

    let settings = build_base_streamer_settings(&config);
    let expected = &input["expected"];
    assert_eq!(
        settings.make_predictions,
        expected["make_predictions"].as_bool().unwrap()
    );
    assert_eq!(
        settings.follow_raid,
        expected["follow_raid"].as_bool().unwrap()
    );
    assert_eq!(
        settings.claim_drops,
        expected["claim_drops"].as_bool().unwrap()
    );
    assert_eq!(
        settings.claim_moments,
        expected["claim_moments"].as_bool().unwrap()
    );
    assert_eq!(
        settings.watch_streak,
        expected["watch_streak"].as_bool().unwrap()
    );
    assert_eq!(
        settings.community_goals,
        expected["community_goals"].as_bool().unwrap()
    );
    assert_eq!(
        serde_json::to_value(settings.irc_mode).unwrap(),
        expected["chat_presence"]
    );
    assert_eq!(
        serde_json::to_value(settings.bet.strategy).unwrap(),
        expected["strategy"]
    );
    assert_eq!(
        settings.bet.percentage,
        Some(expected["percentage"].as_u64().unwrap() as u32)
    );
    assert_eq!(
        settings.bet.percentage_gap,
        Some(expected["percentage_gap"].as_u64().unwrap() as u32)
    );
    assert_eq!(
        settings.bet.max_points,
        Some(expected["max_points"].as_u64().unwrap() as u32)
    );
    assert_eq!(
        settings.bet.minimum_points,
        Some(expected["minimum_points"].as_u64().unwrap() as u32)
    );
    assert_eq!(
        settings.bet.stealth_mode,
        Some(expected["stealth_mode"].as_bool().unwrap())
    );
    assert_eq!(
        settings.bet.deduct_stake_on_place,
        Some(expected["deduct_stake_on_place"].as_bool().unwrap())
    );
    assert_eq!(
        serde_json::to_value(settings.bet.delay_mode).unwrap(),
        expected["delay_mode"]
    );
    assert_eq!(
        settings.bet.delay,
        Some(expected["delay"].as_f64().unwrap())
    );
}

#[test]
fn normalized_prediction_contract_matches_expected() {
    let value = vectors();
    let input = &value["prediction"];
    let settings = &input["settings"];
    let streamer = Streamer {
        channel_points: input["balance"].as_i64().unwrap(),
        settings: StreamerSettings {
            bet: BetSettings {
                strategy: match settings["strategy"].as_str().unwrap() {
                    "MOST_VOTED" => Strategy::MostVoted,
                    other => panic!("unsupported parity strategy: {other}"),
                },
                percentage: Some(settings["percentage"].as_u64().unwrap() as u32),
                max_points: Some(settings["max_points"].as_u64().unwrap() as u32),
                stealth_mode: Some(settings["stealth_mode"].as_bool().unwrap()),
                ..BetSettings::default()
            },
            ..StreamerSettings::default()
        },
        ..Streamer::default()
    };
    let outcomes = input["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|outcome| PredictionOutcome {
            id: outcome["id"].as_str().unwrap().to_string(),
            title: outcome["title"].as_str().unwrap().to_string(),
            color: outcome["color"].as_str().unwrap().to_string(),
            total_users: outcome["total_users"].as_i64().unwrap(),
            total_points: outcome["total_points"].as_i64().unwrap(),
            top_points: outcome["top_points"].as_i64().unwrap(),
            ..PredictionOutcome::default()
        })
        .collect();
    let mut event = PredictionEvent {
        streamer,
        event_id: String::from("parity-event"),
        title: String::from("Parity"),
        status: String::from("ACTIVE"),
        created_at: ts(100),
        window_seconds: 30.0,
        outcomes,
        decision: Default::default(),
        bet_placed: false,
        bet_confirmed: false,
        result_type: String::new(),
        result_string: String::new(),
    };
    event.update_outcomes();
    let decision = event.decide(input["balance"].as_i64().unwrap());
    let result = &input["result"];
    let settlement = event.parse_result(
        result["type"].as_str().unwrap(),
        result["points_won"].as_i64().unwrap(),
    );
    let expected = &input["expected"];
    assert_eq!(
        decision.choice,
        Some(expected["choice"].as_u64().unwrap() as usize)
    );
    assert_eq!(
        decision.outcome_id,
        expected["outcome_id"].as_str().unwrap()
    );
    assert_eq!(decision.amount, expected["amount"].as_i64().unwrap());
    assert_eq!(settlement.gained, expected["gained"].as_i64().unwrap());
    assert_eq!(settlement.placed, expected["placed"].as_i64().unwrap());
    assert_eq!(settlement.won, expected["won"].as_i64().unwrap());
    assert_eq!(
        settlement.result_type,
        expected["result_type"].as_str().unwrap()
    );
    assert_eq!(
        settlement.result_string,
        expected["result_string"].as_str().unwrap()
    );
}

#[test]
fn normalized_points_and_history_contract_matches_expected() {
    let value = vectors();
    let input = &value["points"];
    let mut streamer = Streamer {
        channel_points: input["initial_balance"].as_i64().unwrap(),
        ..Streamer::default()
    };
    let delta = apply_pubsub_gain(
        &mut streamer,
        input["earned"].as_i64().unwrap(),
        input["reason"].as_str().unwrap(),
        input["balance"].as_i64().unwrap(),
    );
    let history = streamer
        .history
        .get(input["reason"].as_str().unwrap())
        .cloned()
        .unwrap_or_else(HistoryEntry::default);
    let expected = &input["expected"];
    assert_eq!(
        streamer.channel_points,
        expected["balance"].as_i64().unwrap()
    );
    assert_eq!(delta, expected["delta"].as_i64().unwrap());
    assert_eq!(
        history.count,
        expected["history_count"].as_u64().unwrap() as u32
    );
    assert_eq!(history.amount, expected["history_amount"].as_i64().unwrap());
}

#[test]
fn normalized_watch_selection_contract_matches_expected() {
    let value = vectors();
    let input = &value["watch"];
    let streamers = input["streamers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| Streamer {
            username: item["username"].as_str().unwrap().to_string(),
            channel_id: item["channel_id"].as_str().unwrap().to_string(),
            channel_points: item["channel_points"].as_i64().unwrap(),
            is_online: item["online"].as_bool().unwrap(),
            settings: StreamerSettings {
                watch_streak: false,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                game: Some(Game::from_name(item["game"].as_str().unwrap())),
                ..Stream::default()
            }),
            ..Streamer::default()
        })
        .collect::<Vec<_>>();
    let priorities = input["priority_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let game_priority = input["game_priority"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let game_exclude = input["game_exclude"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let selected = pick_streamers_to_watch(
        &streamers,
        &parse_watch_priorities(&priorities),
        &game_priority,
        &game_exclude,
        None,
        ts(10_000),
    );
    let expected = input["expected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_u64().unwrap() as usize)
        .collect::<Vec<_>>();
    assert_eq!(selected, expected);
}

#[test]
fn normalized_pubsub_points_event_contract_matches_expected() {
    let value = vectors();
    let input = &value["pubsub"];
    let raw = serde_json::to_string(&input["raw"]).unwrap();
    let event = parse_message(&raw, &[]).unwrap().unwrap();
    let expected = &input["expected"];
    assert_eq!(
        event,
        PubSubEvent::PointsEarned {
            channel_id: expected["channel_id"].as_str().unwrap().to_string(),
            earned: expected["earned"].as_i64().unwrap(),
            reason: expected["reason"].as_str().unwrap().to_string(),
            balance: expected["balance"].as_i64().unwrap(),
        }
    );
}
