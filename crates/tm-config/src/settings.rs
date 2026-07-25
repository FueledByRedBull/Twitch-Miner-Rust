use std::collections::HashMap;

use tm_domain::{
    BetSettings, Condition, DelayMode, FilterCondition, IrcMode, OutcomeKey, Strategy,
    StreamerSettings,
};

use super::{BetConfig, ConfigFile, StreamerSettingsOverride};

#[must_use]
pub fn parse_chat_presence(mode: &str, fallback: IrcMode) -> IrcMode {
    match mode.trim().to_uppercase().as_str() {
        "ALWAYS" => IrcMode::Always,
        "NEVER" => IrcMode::Never,
        "ONLINE" => IrcMode::Online,
        "OFFLINE" => IrcMode::Offline,
        _ => fallback,
    }
}

#[must_use]
pub fn build_base_streamer_settings(config: &ConfigFile) -> StreamerSettings {
    StreamerSettings {
        make_predictions: config.betting_make_predictions,
        follow_raid: config.follow_raid,
        farm_drops: config.farm_drops,
        claim_drops: config.claim_drops,
        single_watcher_during_drops: config.watch_one_stream_when_drops_active,
        claim_moments: config.claim_moments,
        watch_streak: true,
        watch_streak_vod_recovery: config.watch_streak_vod_recovery,
        community_goals: config.community_goals,
        bet: merge_bet_settings(&BetSettings::default(), &config.bet),
        irc_mode: parse_chat_presence(&config.chat_presence, IrcMode::Online),
    }
}

#[must_use]
pub fn build_override_settings<S: std::hash::BuildHasher>(
    base: &StreamerSettings,
    overrides: &HashMap<String, StreamerSettingsOverride, S>,
) -> HashMap<String, StreamerSettings> {
    overrides
        .iter()
        .filter_map(|(login, override_settings)| {
            let key = login.trim().to_lowercase();
            if key.is_empty() {
                return None;
            }
            Some((key, merge_streamer_settings(base, override_settings)))
        })
        .collect()
}

fn merge_streamer_settings(
    base: &StreamerSettings,
    override_settings: &StreamerSettingsOverride,
) -> StreamerSettings {
    let mut settings = base.clone();
    if let Some(value) = override_settings.make_predictions {
        settings.make_predictions = value;
    }
    if let Some(value) = override_settings.follow_raid {
        settings.follow_raid = value;
    }
    if let Some(value) = override_settings.farm_drops {
        settings.farm_drops = value;
    }
    if let Some(value) = override_settings.claim_drops {
        settings.claim_drops = value;
    }
    if let Some(value) = override_settings.watch_one_stream_when_drops_active {
        settings.single_watcher_during_drops = value;
    }
    if let Some(value) = override_settings.claim_moments {
        settings.claim_moments = value;
    }
    if let Some(value) = override_settings.watch_streak {
        settings.watch_streak = value;
    }
    if let Some(value) = override_settings.watch_streak_vod_recovery {
        settings.watch_streak_vod_recovery = value;
    }
    if let Some(value) = override_settings.community_goals {
        settings.community_goals = value;
    }
    settings.bet = merge_bet_settings(&settings.bet, &override_settings.bet);
    if let Some(chat_presence) = override_settings.chat_presence.as_deref() {
        settings.irc_mode = parse_chat_presence(chat_presence, settings.irc_mode);
    }
    settings
}

fn merge_bet_settings(base: &BetSettings, override_settings: &BetConfig) -> BetSettings {
    let mut bet = base.clone();
    if let Some(strategy) = override_settings.strategy.as_deref() {
        bet.strategy = parse_strategy(strategy).unwrap_or(bet.strategy);
    }
    if let Some(value) = override_settings.percentage {
        bet.percentage = Some(value);
    }
    if let Some(value) = override_settings.percentage_gap {
        bet.percentage_gap = Some(value);
    }
    if let Some(value) = override_settings.max_points {
        bet.max_points = Some(value);
    }
    if let Some(value) = override_settings.minimum_points {
        bet.minimum_points = Some(value);
    }
    if let Some(value) = override_settings.stealth_mode {
        bet.stealth_mode = Some(value);
    }
    if let Some(value) = override_settings.deduct_stake_on_place {
        bet.deduct_stake_on_place = Some(value);
    }
    if let Some(value) = override_settings.delay {
        bet.delay = Some(value);
    }
    if let Some(delay_mode) = override_settings.delay_mode.as_deref() {
        bet.delay_mode = parse_delay_mode(delay_mode).unwrap_or(bet.delay_mode);
    }
    if let Some(filter_condition) = override_settings.filter_condition.as_ref() {
        let mut current = bet.filter_condition.clone().unwrap_or(FilterCondition {
            by: OutcomeKey::TotalUsers,
            condition: Condition::Gte,
            value: None,
        });
        if let Some(by) = filter_condition.by.as_deref() {
            current.by = parse_outcome_key(by).unwrap_or(current.by);
        }
        if let Some(condition) = filter_condition.condition.as_deref() {
            current.condition = parse_condition(condition).unwrap_or(current.condition);
        }
        if filter_condition.value.is_some() {
            current.value = filter_condition.value;
        }
        if current.value.is_some() {
            bet.filter_condition = Some(current);
        }
    }
    bet
}

fn parse_strategy(raw: &str) -> Option<Strategy> {
    match raw.trim().to_uppercase().as_str() {
        "MOST_VOTED" => Some(Strategy::MostVoted),
        "HIGH_ODDS" => Some(Strategy::HighOdds),
        "PERCENTAGE" => Some(Strategy::Percentage),
        "SMART_MONEY" => Some(Strategy::SmartMoney),
        "SMART" => Some(Strategy::Smart),
        "NUMBER_1" => Some(Strategy::Number1),
        "NUMBER_2" => Some(Strategy::Number2),
        "NUMBER_3" => Some(Strategy::Number3),
        "NUMBER_4" => Some(Strategy::Number4),
        "NUMBER_5" => Some(Strategy::Number5),
        "NUMBER_6" => Some(Strategy::Number6),
        "NUMBER_7" => Some(Strategy::Number7),
        "NUMBER_8" => Some(Strategy::Number8),
        _ => None,
    }
}

fn parse_delay_mode(raw: &str) -> Option<DelayMode> {
    match raw.trim().to_uppercase().as_str() {
        "FROM_START" => Some(DelayMode::FromStart),
        "FROM_END" => Some(DelayMode::FromEnd),
        "PERCENTAGE" => Some(DelayMode::Percentage),
        _ => None,
    }
}

fn parse_outcome_key(raw: &str) -> Option<OutcomeKey> {
    match raw.trim().to_uppercase().as_str() {
        "PERCENTAGE_USERS" => Some(OutcomeKey::PercentageUsers),
        "ODDS" => Some(OutcomeKey::Odds),
        "ODDS_PERCENTAGE" => Some(OutcomeKey::OddsPercentage),
        "TOP_POINTS" => Some(OutcomeKey::TopPoints),
        "TOTAL_USERS" => Some(OutcomeKey::TotalUsers),
        "TOTAL_POINTS" => Some(OutcomeKey::TotalPoints),
        "DECISION_USERS" => Some(OutcomeKey::DecisionUsers),
        "DECISION_POINTS" => Some(OutcomeKey::DecisionPoints),
        _ => None,
    }
}

fn parse_condition(raw: &str) -> Option<Condition> {
    match raw.trim().to_uppercase().as_str() {
        "GT" => Some(Condition::Gt),
        "LT" => Some(Condition::Lt),
        "GTE" => Some(Condition::Gte),
        "LTE" => Some(Condition::Lte),
        _ => None,
    }
}
