#![allow(clippy::cast_precision_loss, clippy::too_many_lines)]

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use time::OffsetDateTime;

use crate::types::{IrcMode, Streamer};

const STREAK_PRIORITY_MINUTES_BASE: f64 = 7.0;
const STREAK_PRIORITY_MINUTES_EXTENDED: f64 = 20.0;
const MAX_CONCURRENT_WATCHERS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchPriority {
    Order,
    Streak,
    Drops,
    Subscribed,
    PointsAscending,
    PointsDescending,
}

#[must_use]
pub fn default_watch_priorities() -> Vec<WatchPriority> {
    vec![
        WatchPriority::Streak,
        WatchPriority::Drops,
        WatchPriority::Order,
    ]
}

#[must_use]
pub fn parse_watch_priorities(priority_names: &[String]) -> Vec<WatchPriority> {
    if priority_names.is_empty() {
        return default_watch_priorities();
    }

    let mut parsed = Vec::new();
    let mut seen = HashSet::new();
    for raw_name in priority_names {
        let parsed_priority = match raw_name.trim().to_uppercase().as_str() {
            "ORDER" => Some(WatchPriority::Order),
            "STREAK" => Some(WatchPriority::Streak),
            "DROPS" => Some(WatchPriority::Drops),
            "SUBSCRIBED" | "SUBS" | "MULTIPLIER" => Some(WatchPriority::Subscribed),
            "POINTS_ASC" | "POINTS_ASCENDING" => Some(WatchPriority::PointsAscending),
            "POINTS_DESC" | "POINTS_DESCENDING" => Some(WatchPriority::PointsDescending),
            _ => None,
        };
        if let Some(priority) = parsed_priority {
            if seen.insert(priority) {
                parsed.push(priority);
            }
        }
    }
    if parsed.is_empty() {
        default_watch_priorities()
    } else {
        parsed
    }
}

#[must_use]
pub fn normalize_game_list(values: &[String]) -> Vec<String> {
    normalize_list(values)
}

#[must_use]
pub fn normalize_streamer_list(values: &[String]) -> Vec<String> {
    normalize_list(values)
}

fn normalize_list(values: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    for raw_value in values {
        let value = raw_value.trim().to_lowercase();
        if !value.is_empty() && seen.insert(value.clone()) {
            normalized.push(value);
        }
    }
    normalized
}

#[must_use]
pub fn should_join_chat(mode: IrcMode, online: bool) -> bool {
    match mode {
        IrcMode::Always => true,
        IrcMode::Never => false,
        IrcMode::Offline => !online,
        IrcMode::Online => online,
    }
}

#[must_use]
pub fn streak_priority_limit(started_at: Option<OffsetDateTime>, now: OffsetDateTime) -> f64 {
    match started_at {
        Some(started_at) if now - started_at > time::Duration::hours(10) => {
            STREAK_PRIORITY_MINUTES_EXTENDED
        }
        _ => STREAK_PRIORITY_MINUTES_BASE,
    }
}

#[must_use]
pub fn should_prioritize_streak(
    streamer: &Streamer,
    started_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> bool {
    let Some(stream) = streamer.stream.as_ref() else {
        return false;
    };
    if !streamer.settings.watch_streak || !stream.watch_streak_missing {
        return false;
    }
    if let Some(offline_at) = streamer.offline_at {
        if now - offline_at <= time::Duration::minutes(30) {
            return false;
        }
    }
    stream.minute_watched < streak_priority_limit(started_at, now)
}

#[must_use]
pub fn watch_interval(count: usize) -> Duration {
    if count == 0 {
        return Duration::from_secs(20);
    }
    Duration::from_secs_f64(20.0 / count as f64).max(Duration::from_secs(5))
}

#[must_use]
pub fn pick_streamers_to_watch(
    streamers: &[Streamer],
    watch_priorities: &[WatchPriority],
    game_priority: &[String],
    game_exclusions: &[String],
    started_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Vec<usize> {
    #[derive(Clone, Copy)]
    struct Candidate {
        idx: usize,
        rank: usize,
        position: usize,
        priority_game: bool,
        streak_ready: bool,
    }

    fn game_for(streamers: &[Streamer], idx: usize) -> Option<String> {
        streamers[idx]
            .stream
            .as_ref()
            .map(|stream| stream.game_name().to_lowercase())
            .filter(|value| !value.is_empty())
    }

    fn add_candidate(
        candidate: Candidate,
        streamers: &[Streamer],
        candidates: &[Candidate],
        selected: &mut Vec<usize>,
        seen: &mut HashSet<usize>,
        selected_games: &mut HashSet<String>,
    ) {
        if selected.len() >= MAX_CONCURRENT_WATCHERS || !seen.insert(candidate.idx) {
            return;
        }

        if let Some(candidate_game) = game_for(streamers, candidate.idx) {
            if selected_games.contains(&candidate_game) {
                let other_available = candidates.iter().any(|other| {
                    if seen.contains(&other.idx) {
                        return false;
                    }
                    game_for(streamers, other.idx)
                        .is_some_and(|other_game| other_game != candidate_game)
                });
                if other_available {
                    seen.remove(&candidate.idx);
                    return;
                }
            }
            selected_games.insert(candidate_game);
        }

        selected.push(candidate.idx);
    }

    let game_exclusions: HashSet<String> = game_exclusions
        .iter()
        .map(|item| item.to_lowercase())
        .collect();
    let game_priority_index: HashMap<String, usize> = game_priority
        .iter()
        .map(|item| item.to_lowercase())
        .enumerate()
        .map(|(idx, item)| (item, idx))
        .collect();

    let mut candidates = Vec::new();
    let mut candidate_by_idx = HashMap::new();
    let mut streak_candidates = Vec::new();
    let mut has_priority_game_streak = false;

    for (idx, streamer) in streamers.iter().enumerate() {
        if !streamer.is_online {
            continue;
        }
        if let Some(online_at) = streamer.online_at {
            if now - online_at < time::Duration::seconds(30) {
                continue;
            }
        }

        let game = streamer
            .stream
            .as_ref()
            .map(|stream| stream.game_name().to_lowercase())
            .filter(|value| !value.is_empty());
        if game
            .as_ref()
            .is_some_and(|game| game_exclusions.contains(game))
        {
            continue;
        }

        let rank = game
            .as_ref()
            .and_then(|game| game_priority_index.get(game).copied())
            .unwrap_or(game_priority.len() + 1);
        let priority_game = game
            .as_ref()
            .is_some_and(|game| game_priority_index.contains_key(game));
        let streak_ready = should_prioritize_streak(streamer, started_at, now);
        let candidate = Candidate {
            idx,
            rank,
            position: candidates.len(),
            priority_game,
            streak_ready,
        };
        candidates.push(candidate);
        candidate_by_idx.insert(idx, candidate);
        if streak_ready {
            streak_candidates.push(candidate);
            has_priority_game_streak |= priority_game;
        }
    }

    let priorities = if watch_priorities.is_empty() {
        default_watch_priorities()
    } else {
        watch_priorities.to_vec()
    };

    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut selected_games = HashSet::new();

    let skip_early_streak = !game_priority.is_empty() && !has_priority_game_streak;

    for priority in priorities {
        if selected.len() >= MAX_CONCURRENT_WATCHERS {
            break;
        }
        let mut ordered: Vec<Candidate> = match priority {
            WatchPriority::Streak => {
                if skip_early_streak {
                    Vec::new()
                } else {
                    streak_candidates.clone()
                }
            }
            WatchPriority::Drops => candidates
                .iter()
                .copied()
                .filter(|candidate| streamers[candidate.idx].settings.claim_drops)
                .collect(),
            WatchPriority::Subscribed => candidates
                .iter()
                .copied()
                .filter(|candidate| streamers[candidate.idx].has_active_multipliers())
                .collect(),
            WatchPriority::Order
            | WatchPriority::PointsAscending
            | WatchPriority::PointsDescending => candidates.clone(),
        };

        ordered.sort_by(|left, right| match priority {
            WatchPriority::Order => left.position.cmp(&right.position),
            WatchPriority::Subscribed => streamers[right.idx]
                .total_multiplier()
                .partial_cmp(&streamers[left.idx].total_multiplier())
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.rank.cmp(&right.rank))
                .then_with(|| left.position.cmp(&right.position)),
            WatchPriority::PointsAscending => streamers[left.idx]
                .channel_points
                .cmp(&streamers[right.idx].channel_points)
                .then_with(|| left.rank.cmp(&right.rank))
                .then_with(|| left.position.cmp(&right.position)),
            WatchPriority::PointsDescending => streamers[right.idx]
                .channel_points
                .cmp(&streamers[left.idx].channel_points)
                .then_with(|| left.rank.cmp(&right.rank))
                .then_with(|| left.position.cmp(&right.position)),
            _ => left
                .rank
                .cmp(&right.rank)
                .then_with(|| left.position.cmp(&right.position)),
        });

        for candidate in ordered {
            add_candidate(
                candidate,
                streamers,
                &candidates,
                &mut selected,
                &mut seen,
                &mut selected_games,
            );
            if selected.len() >= MAX_CONCURRENT_WATCHERS {
                break;
            }
        }
    }

    if !selected
        .iter()
        .any(|idx| candidate_by_idx[idx].streak_ready)
        && !streak_candidates.is_empty()
        && !selected.is_empty()
    {
        let mut streaks = streak_candidates.clone();
        streaks.sort_by(|left, right| {
            left.rank
                .cmp(&right.rank)
                .then_with(|| left.position.cmp(&right.position))
        });
        if let Some(streak_pick) = streaks
            .into_iter()
            .find(|candidate| !seen.contains(&candidate.idx))
        {
            if selected.len() < MAX_CONCURRENT_WATCHERS {
                add_candidate(
                    streak_pick,
                    streamers,
                    &candidates,
                    &mut selected,
                    &mut seen,
                    &mut selected_games,
                );
            } else {
                let keep = selected[0];
                selected = vec![keep, streak_pick.idx];
            }
        }
    }

    if skip_early_streak && selected.len() >= 2 {
        let first = candidate_by_idx[&selected[0]];
        let second = candidate_by_idx[&selected[1]];
        if first.streak_ready
            && !first.priority_game
            && (!second.streak_ready || second.priority_game)
        {
            selected.swap(0, 1);
        }
    }

    if selected.len() < MAX_CONCURRENT_WATCHERS {
        let mut fallback = candidates.clone();
        fallback.sort_by(|left, right| {
            left.rank
                .cmp(&right.rank)
                .then_with(|| left.position.cmp(&right.position))
        });
        for candidate in fallback {
            add_candidate(
                candidate,
                streamers,
                &candidates,
                &mut selected,
                &mut seen,
                &mut selected_games,
            );
            if selected.len() >= MAX_CONCURRENT_WATCHERS {
                break;
            }
        }
    }

    selected
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::types::{Stream, StreamerSettings};

    #[test]
    fn parse_watch_priorities_matches_go() {
        assert_eq!(parse_watch_priorities(&[]), default_watch_priorities());
        assert_eq!(
            parse_watch_priorities(&[
                String::from("drops"),
                String::from("ORDER"),
                String::from("drops"),
                String::from("points_desc"),
                String::from("ignored"),
            ]),
            vec![
                WatchPriority::Drops,
                WatchPriority::Order,
                WatchPriority::PointsDescending
            ]
        );
        assert_eq!(
            parse_watch_priorities(&[String::from("foo"), String::from("bar")]),
            default_watch_priorities()
        );
    }

    #[test]
    fn normalize_game_list_matches_go() {
        assert_eq!(
            normalize_game_list(&[
                String::from(" Valorant "),
                String::from("Tom Clancy's Rainbow Six Siege X"),
                String::from("five night's at freddys"),
                String::from("valorant"),
                String::from("   "),
            ]),
            vec![
                String::from("valorant"),
                String::from("tom clancy's rainbow six siege x"),
                String::from("five night's at freddys"),
            ]
        );
    }

    #[test]
    fn watch_interval_matches_go() {
        assert_eq!(watch_interval(0), Duration::from_secs(20));
        assert_eq!(watch_interval(2), Duration::from_secs(10));
        assert_eq!(watch_interval(10), Duration::from_secs(5));
    }

    #[test]
    fn streak_priority_limit_matches_go() {
        let now = datetime!(2026-03-27 06:00 UTC);
        assert_eq!(streak_priority_limit(None, now), 7.0);
        assert_eq!(
            streak_priority_limit(Some(now - time::Duration::hours(11)), now),
            20.0
        );
    }

    #[test]
    fn should_prioritize_streak_matches_go() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut streamer = Streamer {
            settings: StreamerSettings {
                watch_streak: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                watch_streak_missing: true,
                minute_watched: 3.0,
                ..Stream::default()
            }),
            ..Streamer::default()
        };
        assert!(should_prioritize_streak(&streamer, None, now));

        streamer.offline_at = Some(now - time::Duration::minutes(10));
        assert!(!should_prioritize_streak(&streamer, None, now));
    }

    #[test]
    fn should_join_chat_matches_go() {
        assert!(should_join_chat(IrcMode::Always, true));
        assert!(should_join_chat(IrcMode::Always, false));
        assert!(!should_join_chat(IrcMode::Never, true));
        assert!(!should_join_chat(IrcMode::Never, false));
        assert!(should_join_chat(IrcMode::Online, true));
        assert!(!should_join_chat(IrcMode::Online, false));
        assert!(!should_join_chat(IrcMode::Offline, true));
        assert!(should_join_chat(IrcMode::Offline, false));
    }

    #[test]
    fn subscribed_priority_falls_through_to_points_asc() {
        let online_at = datetime!(2026-03-27 05:58 UTC);
        let streamers = vec![
            Streamer {
                username: String::from("streamer1"),
                is_online: true,
                online_at: Some(online_at),
                channel_points: 1_000,
                ..Streamer::default()
            },
            Streamer {
                username: String::from("streamer8"),
                is_online: true,
                online_at: Some(online_at),
                channel_points: 900,
                ..Streamer::default()
            },
            Streamer {
                username: String::from("streamer9"),
                is_online: true,
                online_at: Some(online_at),
                channel_points: 10,
                ..Streamer::default()
            },
            Streamer {
                username: String::from("streamer10"),
                is_online: true,
                online_at: Some(online_at),
                channel_points: 20,
                ..Streamer::default()
            },
        ];
        let selected = pick_streamers_to_watch(
            &streamers,
            &parse_watch_priorities(&[
                String::from("STREAK"),
                String::from("SUBSCRIBED"),
                String::from("POINTS_ASC"),
            ]),
            &[],
            &[],
            None,
            datetime!(2026-03-27 06:00 UTC),
        );
        assert_eq!(selected, vec![2, 3]);
    }

    #[test]
    fn watch_picker_avoids_duplicate_games_when_alternatives_exist() {
        let online_at = datetime!(2026-03-27 05:58 UTC);
        let streamers = vec![
            Streamer {
                username: String::from("alpha"),
                is_online: true,
                online_at: Some(online_at),
                stream: Some(Stream {
                    game: Some(crate::types::Game::from_name("valorant")),
                    ..Stream::default()
                }),
                ..Streamer::default()
            },
            Streamer {
                username: String::from("bravo"),
                is_online: true,
                online_at: Some(online_at),
                stream: Some(Stream {
                    game: Some(crate::types::Game::from_name("valorant")),
                    ..Stream::default()
                }),
                ..Streamer::default()
            },
            Streamer {
                username: String::from("charlie"),
                is_online: true,
                online_at: Some(online_at),
                stream: Some(Stream {
                    game: Some(crate::types::Game::from_name("just chatting")),
                    ..Stream::default()
                }),
                ..Streamer::default()
            },
        ];

        let selected = pick_streamers_to_watch(
            &streamers,
            &[WatchPriority::Order],
            &[],
            &[],
            None,
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![0, 2]);
    }

    #[test]
    fn order_priority_uses_configured_position_even_with_game_priority() {
        let online_at = datetime!(2026-03-27 05:58 UTC);
        let streamers = vec![
            Streamer {
                username: String::from("alpha"),
                is_online: true,
                online_at: Some(online_at),
                stream: Some(Stream {
                    game: Some(crate::types::Game::from_name("other game")),
                    ..Stream::default()
                }),
                ..Streamer::default()
            },
            Streamer {
                username: String::from("bravo"),
                is_online: true,
                online_at: Some(online_at),
                stream: Some(Stream {
                    game: Some(crate::types::Game::from_name("priority game")),
                    ..Stream::default()
                }),
                ..Streamer::default()
            },
        ];

        let selected = pick_streamers_to_watch(
            &streamers,
            &[WatchPriority::Order],
            &[String::from("priority game")],
            &[],
            None,
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![0, 1]);
    }
}
