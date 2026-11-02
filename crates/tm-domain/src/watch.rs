// Watch selection is one deterministic ranking pass whose ordering rules must remain visible
// together; bounded count-to-f64 conversions are required for platform watch cadence.
#![allow(clippy::too_many_lines)]

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use time::OffsetDateTime;

use crate::types::{IrcMode, Stream, Streamer};

// A sanitized current-production sample put the first WATCH_STREAK signal at
// or below seven minutes for 90% of observed broadcasts. Three more minutes
// bound detection/queue delay without tying eligibility to miner uptime.
const STREAK_PRIORITY_MINUTES: f64 = 10.0;
const STREAK_RESTART_COOLDOWN_MINUTES: i64 = 30;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchPriority {
    Order,
    Streak,
    Drops,
    Subscribed,
    PointsAscending,
    PointsDescending,
    LongestStreak,
    ExpiringStreak,
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
        let parsed_priority = parse_watch_priority(raw_name);
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
pub fn parse_watch_priority(raw_name: &str) -> Option<WatchPriority> {
    match raw_name.trim().to_uppercase().as_str() {
        "ORDER" => Some(WatchPriority::Order),
        "STREAK" => Some(WatchPriority::Streak),
        "DROPS" => Some(WatchPriority::Drops),
        "SUBSCRIBED" | "SUBS" | "MULTIPLIER" => Some(WatchPriority::Subscribed),
        "POINTS_ASC" | "POINTS_ASCENDING" => Some(WatchPriority::PointsAscending),
        "POINTS_DESC" | "POINTS_DESCENDING" => Some(WatchPriority::PointsDescending),
        "LONGEST_STREAK" | "STREAK_LONGEST" => Some(WatchPriority::LongestStreak),
        "EXPIRING_STREAK" | "STREAK_EXPIRING" => Some(WatchPriority::ExpiringStreak),
        _ => None,
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
pub const fn streak_priority_limit() -> f64 {
    STREAK_PRIORITY_MINUTES
}

#[must_use]
pub fn should_prioritize_streak(streamer: &Streamer, now: OffsetDateTime) -> bool {
    let Some(stream) = streamer.stream.as_ref() else {
        return false;
    };
    if !streamer.settings.watch_streak || !stream.watch_streak_missing {
        return false;
    }
    let restart_is_eligible = streamer.last_stream_ended_at.is_none_or(|ended_at| {
        now - ended_at >= time::Duration::minutes(STREAK_RESTART_COOLDOWN_MINUTES)
    });
    restart_is_eligible && stream.minute_watched < streak_priority_limit()
}

#[must_use]
pub fn watch_interval(count: usize) -> Duration {
    if count == 0 {
        return Duration::from_secs(20);
    }
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    Duration::from_secs_f64(20.0 / f64::from(count)).max(Duration::from_secs(5))
}

#[must_use]
pub fn pick_streamers_to_watch(
    streamers: &[Streamer],
    watch_priorities: &[WatchPriority],
    game_priority: &[String],
    game_exclusions: &[String],
    now: OffsetDateTime,
) -> Vec<usize> {
    #[derive(Clone, Copy)]
    struct Candidate {
        idx: usize,
        rank: usize,
        position: usize,
        priority_game: bool,
        streak_ready: bool,
        campaign_only: bool,
    }

    fn add_candidate(candidate: Candidate, selected: &mut Vec<usize>, seen: &mut HashSet<usize>) {
        if !seen.insert(candidate.idx) {
            return;
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
        // A channel at its points limit is treated like one with points
        // disabled: only a Drop campaign can still earn it a slot.
        let earns_points = streamer.can_earn_channel_points() && !streamer.points_limit_reached();
        if !streamer.is_online || (!earns_points && !streamer.can_watch_drop_campaign()) {
            continue;
        }
        // A discovered channel is only worth a slot while its streak is open.
        if streamer.discovered && !should_prioritize_streak(streamer, now) {
            continue;
        }
        if streamer
            .watch_suspended_until
            .is_some_and(|until| until > now)
        {
            continue;
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
        let campaign_only = !earns_points;
        let streak_ready = !campaign_only && should_prioritize_streak(streamer, now);
        let candidate = Candidate {
            idx,
            rank,
            position: candidates.len(),
            priority_game,
            streak_ready,
            campaign_only,
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

    let mut drop_candidates = candidates
        .iter()
        .copied()
        .filter(|candidate| streamers[candidate.idx].can_watch_drop_campaign())
        .collect::<Vec<_>>();
    drop_candidates.sort_by_key(|candidate| {
        let target = streamers[candidate.idx]
            .stream
            .as_ref()
            .and_then(|stream| stream.drop_watch_target)
            .filter(|target| target.feasible_at(now));
        (
            candidate.rank,
            target.is_none(),
            target.map(|target| target.ends_at),
            candidate.position,
        )
    });
    if priorities.contains(&WatchPriority::Drops) {
        if let Some(candidate) = drop_candidates.first() {
            if streamers[candidate.idx]
                .settings
                .single_watcher_during_drops
            {
                return vec![candidate.idx];
            }
        }
    }

    let mut selected = Vec::new();
    let mut seen = HashSet::new();

    let skip_early_streak = !game_priority.is_empty() && !has_priority_game_streak;

    for priority in priorities {
        let mut ordered: Vec<Candidate> = match priority {
            WatchPriority::Streak => {
                if skip_early_streak {
                    Vec::new()
                } else {
                    streak_candidates.clone()
                }
            }
            WatchPriority::Drops => drop_candidates.clone(),
            WatchPriority::Subscribed => candidates
                .iter()
                .copied()
                .filter(|candidate| {
                    !candidate.campaign_only && streamers[candidate.idx].has_active_multipliers()
                })
                .collect(),
            WatchPriority::LongestStreak | WatchPriority::ExpiringStreak => {
                streak_candidates.clone()
            }
            WatchPriority::Order
            | WatchPriority::PointsAscending
            | WatchPriority::PointsDescending => candidates
                .iter()
                .copied()
                .filter(|candidate| !candidate.campaign_only)
                .collect(),
        };

        ordered.sort_by(|left, right| match priority {
            // Drop candidates already have the complete reward deadline ordering.
            WatchPriority::Drops => Ordering::Equal,
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
            WatchPriority::LongestStreak => streamers[right.idx]
                .stream
                .as_ref()
                .and_then(|stream| active_streak_count(stream, now))
                .cmp(
                    &streamers[left.idx]
                        .stream
                        .as_ref()
                        .and_then(|stream| active_streak_count(stream, now)),
                )
                .then_with(|| left.rank.cmp(&right.rank))
                .then_with(|| left.position.cmp(&right.position)),
            WatchPriority::ExpiringStreak => compare_optional_deadlines(
                streamers[left.idx]
                    .stream
                    .as_ref()
                    .and_then(|stream| active_streak_deadline(stream, now)),
                streamers[right.idx]
                    .stream
                    .as_ref()
                    .and_then(|stream| active_streak_deadline(stream, now)),
            )
            .then_with(|| left.rank.cmp(&right.rank))
            .then_with(|| left.position.cmp(&right.position)),
            WatchPriority::Streak => left
                .rank
                .cmp(&right.rank)
                .then_with(|| left.position.cmp(&right.position)),
        });

        for candidate in ordered {
            add_candidate(candidate, &mut selected, &mut seen);
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
            add_candidate(streak_pick, &mut selected, &mut seen);
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

    let mut fallback = candidates
        .iter()
        .copied()
        .filter(|candidate| !candidate.campaign_only)
        .collect::<Vec<_>>();
    fallback.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.position.cmp(&right.position))
    });
    for candidate in fallback {
        add_candidate(candidate, &mut selected, &mut seen);
    }

    // Configured channels always rank ahead of discovered ones; the sort is stable.
    selected.sort_by_key(|idx| streamers[*idx].discovered);
    selected
}

/// How much a waiting channel is worth, compared lexicographically from the
/// first field to the last, so one uncollected streak outweighs any multiplier.
///
/// `watch_priority` is deliberately not part of the score: it gives every
/// channel its own position, so no two channels would ever tie and fair
/// rotation would stop. It keeps deciding the order channels enter the queue;
/// the score only breaks the tie between waiting channels at a rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ChannelScore {
    /// The channel has a watch streak to collect in its current broadcast.
    pub uncollected_streak: bool,
    /// The channel has an earnable Drop campaign.
    pub drops: bool,
    /// The active points multiplier in thousandths, the value `SUBSCRIBED` ranks by.
    pub multiplier_millis: u32,
    /// 1 for a channel in the top quartile of varying rewards in the ledger, else 0.
    pub yield_tier: u8,
}

impl ChannelScore {
    #[must_use]
    pub fn of(streamer: &Streamer, yield_tier: u8, now: OffsetDateTime) -> Self {
        Self {
            uncollected_streak: should_prioritize_streak(streamer, now),
            drops: streamer.can_watch_drop_campaign(),
            multiplier_millis: multiplier_millis(streamer.total_multiplier()),
            yield_tier,
        }
    }
}

// The clamp keeps the value inside `u32`; a NaN converts to zero.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn multiplier_millis(total: f64) -> u32 {
    (total * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32
}

fn compare_optional_deadlines(
    left: Option<OffsetDateTime>,
    right: Option<OffsetDateTime>,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn active_streak_deadline(stream: &Stream, now: OffsetDateTime) -> Option<OffsetDateTime> {
    stream
        .watch_streak_expires_at
        .filter(|deadline| *deadline > now)
}

fn active_streak_count(stream: &Stream, now: OffsetDateTime) -> Option<u32> {
    stream
        .watch_streak_expires_at
        .is_none_or(|deadline| deadline > now)
        .then_some(stream.watch_streak_count)
        .flatten()
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::types::{Stream, StreamerSettings};

    fn assert_f64_eq(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_watch_priorities_matches_go() {
        assert_eq!(parse_watch_priorities(&[]), default_watch_priorities());
        assert_eq!(
            parse_watch_priorities(&[
                String::from("drops"),
                String::from("ORDER"),
                String::from("drops"),
                String::from("points_desc"),
                String::from("longest_streak"),
                String::from("streak_expiring"),
                String::from("ignored"),
            ]),
            vec![
                WatchPriority::Drops,
                WatchPriority::Order,
                WatchPriority::PointsDescending,
                WatchPriority::LongestStreak,
                WatchPriority::ExpiringStreak,
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
        assert_eq!(watch_interval(1), Duration::from_secs(20));
        assert_eq!(watch_interval(2), Duration::from_secs(10));
        assert_eq!(watch_interval(2) * 2, Duration::from_secs(20));
        assert_eq!(watch_interval(10), Duration::from_secs(5));
        assert_eq!(watch_interval(17), Duration::from_secs(5));
    }

    #[test]
    fn streak_priority_limit_uses_observed_p90_plus_bounded_margin() {
        assert_f64_eq(streak_priority_limit(), 10.0);
    }

    #[test]
    fn streak_priority_contract_covers_new_broadcast_deadline_and_restart_cooldown() {
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
        assert!(should_prioritize_streak(&streamer, now));

        streamer.stream.as_mut().unwrap().minute_watched = 10.0;
        assert!(!should_prioritize_streak(&streamer, now));
        streamer.stream.as_mut().unwrap().minute_watched = 0.0;
        assert!(should_prioritize_streak(&streamer, now));

        streamer.last_stream_ended_at = Some(now - time::Duration::minutes(10));
        assert!(!should_prioritize_streak(&streamer, now));
        streamer.last_stream_ended_at = Some(now - time::Duration::minutes(30));
        assert!(should_prioritize_streak(&streamer, now));

        streamer.watch_suspended_until = Some(now + time::Duration::minutes(5));
        streamer.is_online = true;
        assert!(
            pick_streamers_to_watch(&[streamer], &[WatchPriority::Streak], &[], &[], now,)
                .is_empty()
        );
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
    fn watch_selection_excludes_only_confirmed_disabled_channels() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let make_streamer = |username: &str, enabled: Option<bool>, points_init: bool| Streamer {
            username: username.to_string(),
            is_online: true,
            points_init,
            channel_points_enabled: enabled,
            stream: Some(Stream::default()),
            ..Streamer::default()
        };
        let streamers = vec![
            make_streamer("enabled", Some(true), true),
            make_streamer("disabled", Some(false), true),
            make_streamer("unknown", None, true),
            make_streamer("pre-context", None, false),
        ];

        let selected = pick_streamers_to_watch(&streamers, &[WatchPriority::Order], &[], &[], now);

        assert_eq!(selected, vec![0, 2, 3]);
    }

    #[test]
    fn drops_priority_requires_verified_active_campaign_and_falls_through() {
        let online_at = datetime!(2026-03-27 05:58 UTC);
        let make_streamer = |username: &str, eligible: Option<bool>| Streamer {
            username: username.to_string(),
            is_online: true,
            online_at: Some(online_at),
            settings: StreamerSettings {
                farm_drops: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                drop_campaign_eligible: eligible,
                ..Stream::default()
            }),
            ..Streamer::default()
        };
        let streamers = vec![
            make_streamer("unknown", None),
            make_streamer("inactive", Some(false)),
            make_streamer("eligible", Some(true)),
        ];

        let selected = pick_streamers_to_watch(
            &streamers,
            &[WatchPriority::Drops, WatchPriority::Order],
            &[],
            &[],
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![2, 0, 1]);
    }

    #[test]
    fn equal_deadline_drop_targets_keep_channel_order_as_progress_refreshes() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let deadline = datetime!(2026-03-27 08:00 UTC);
        let mut streamers = [12, 11]
            .into_iter()
            .enumerate()
            .map(|(index, remaining_minutes)| Streamer {
                username: format!("channel-{index}"),
                is_online: true,
                settings: StreamerSettings {
                    farm_drops: true,
                    single_watcher_during_drops: false,
                    ..StreamerSettings::default()
                },
                stream: Some(Stream {
                    drop_campaign_eligible: Some(true),
                    drop_watch_target: Some(crate::types::DropWatchTarget {
                        ends_at: deadline,
                        remaining_minutes,
                        observed_at: now,
                    }),
                    ..Stream::default()
                }),
                ..Streamer::default()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::Drops], &[], &[], now),
            vec![0, 1]
        );
        streamers[0]
            .stream
            .as_mut()
            .expect("channel stream")
            .drop_watch_target
            .as_mut()
            .expect("watch target")
            .remaining_minutes = 10;
        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::Drops], &[], &[], now),
            vec![0, 1]
        );
        streamers[1]
            .stream
            .as_mut()
            .expect("channel stream")
            .drop_watch_target
            .as_mut()
            .expect("watch target")
            .ends_at = datetime!(2026-03-27 07:00 UTC);
        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::Drops], &[], &[], now),
            vec![1, 0]
        );
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
            datetime!(2026-03-27 06:00 UTC),
        );
        assert_eq!(selected, vec![2, 3, 1, 0]);
    }

    #[test]
    fn watch_picker_preserves_configured_order_across_duplicate_games() {
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
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![0, 1, 2]);
    }

    #[test]
    fn active_drop_campaign_can_reserve_the_single_watch_slot() {
        let online_at = datetime!(2026-03-27 05:58 UTC);
        let make_streamer = |username: &str, single_watcher: bool| Streamer {
            username: username.to_string(),
            is_online: true,
            online_at: Some(online_at),
            settings: StreamerSettings {
                farm_drops: true,
                single_watcher_during_drops: single_watcher,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                drop_campaign_eligible: Some(true),
                ..Stream::default()
            }),
            ..Streamer::default()
        };
        let streamers = vec![make_streamer("first", true), make_streamer("second", true)];

        let selected = pick_streamers_to_watch(
            &streamers,
            &[WatchPriority::Drops, WatchPriority::Order],
            &[],
            &[],
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![0]);
    }

    #[test]
    fn disabled_channel_can_watch_only_verified_drop_campaign_until_it_clears() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let disabled_campaign = Streamer {
            username: String::from("campaign"),
            is_online: true,
            channel_points_enabled: Some(false),
            settings: StreamerSettings {
                farm_drops: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                drop_campaign_eligible: Some(true),
                ..Stream::default()
            }),
            ..Streamer::default()
        };
        let ordinary = Streamer {
            username: String::from("ordinary"),
            is_online: true,
            channel_points_enabled: Some(true),
            ..Streamer::default()
        };
        let mut streamers = vec![disabled_campaign, ordinary];

        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::Drops], &[], &[], now),
            vec![0, 1]
        );

        streamers[0]
            .stream
            .as_mut()
            .expect("campaign stream")
            .drop_campaign_eligible = Some(false);
        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::Drops], &[], &[], now),
            vec![1]
        );
    }

    #[test]
    fn active_drop_campaign_watches_every_channel_when_single_watcher_is_disabled() {
        let online_at = datetime!(2026-03-27 05:58 UTC);
        let streamers = (0..6)
            .map(|index| Streamer {
                username: format!("streamer{index}"),
                is_online: true,
                online_at: Some(online_at),
                settings: StreamerSettings {
                    farm_drops: true,
                    single_watcher_during_drops: false,
                    ..StreamerSettings::default()
                },
                stream: Some(Stream {
                    drop_campaign_eligible: Some(true),
                    ..Stream::default()
                }),
                ..Streamer::default()
            })
            .collect::<Vec<_>>();

        let selected = pick_streamers_to_watch(
            &streamers,
            &[WatchPriority::Drops],
            &[],
            &[],
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn highest_ranked_drop_candidate_controls_single_watcher_override() {
        let online_at = datetime!(2026-03-27 05:58 UTC);
        let streamers = [false, true]
            .into_iter()
            .enumerate()
            .map(|(index, single_watcher)| Streamer {
                username: format!("streamer{index}"),
                is_online: true,
                online_at: Some(online_at),
                settings: StreamerSettings {
                    farm_drops: true,
                    single_watcher_during_drops: single_watcher,
                    ..StreamerSettings::default()
                },
                stream: Some(Stream {
                    drop_campaign_eligible: Some(true),
                    ..Stream::default()
                }),
                ..Streamer::default()
            })
            .collect::<Vec<_>>();

        let selected = pick_streamers_to_watch(
            &streamers,
            &[WatchPriority::Drops],
            &[],
            &[],
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![0, 1]);
    }

    #[test]
    fn streak_metadata_priorities_are_deterministic_for_every_channel() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let make_streamer =
            |username: &str, count: Option<u32>, expiry_hours: Option<i64>| Streamer {
                username: username.to_string(),
                is_online: true,
                online_at: Some(now - time::Duration::minutes(2)),
                settings: StreamerSettings {
                    watch_streak: true,
                    ..StreamerSettings::default()
                },
                stream: Some(Stream {
                    watch_streak_missing: true,
                    watch_streak_count: count,
                    watch_streak_expires_at: expiry_hours
                        .map(|hours| now + time::Duration::hours(hours)),
                    ..Stream::default()
                }),
                ..Streamer::default()
            };
        let streamers = vec![
            make_streamer("unknown", None, None),
            make_streamer("longest", Some(12), Some(10)),
            make_streamer("expiring", Some(4), Some(1)),
            make_streamer("expired", Some(99), Some(-1)),
        ];

        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::LongestStreak], &[], &[], now,),
            vec![1, 2, 0, 3]
        );
        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::ExpiringStreak], &[], &[], now,),
            vec![2, 1, 0, 3]
        );
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
            datetime!(2026-03-27 06:00 UTC),
        );

        assert_eq!(selected, vec![0, 1]);
    }

    fn limited(username: &str, balance: i64, limit: Option<i64>) -> Streamer {
        Streamer {
            username: username.to_string(),
            is_online: true,
            points_init: true,
            channel_points: balance,
            settings: StreamerSettings {
                points_limit: limit,
                ..StreamerSettings::default()
            },
            ..Streamer::default()
        }
    }

    #[test]
    fn a_channel_at_its_points_limit_gets_no_slot_for_points() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let streamers = vec![
            limited("below", 99, Some(100)),
            limited("at", 100, Some(100)),
            limited("above", 5_000, Some(100)),
            limited("unlimited", 5_000, None),
        ];

        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::Order], &[], &[], now),
            vec![0, 3]
        );
    }

    #[test]
    fn an_unknown_balance_never_reaches_a_points_limit() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut unknown = limited("unknown", 0, Some(1));
        unknown.points_init = false;
        unknown.channel_points = 10_000;

        assert!(!unknown.points_limit_reached());
        assert_eq!(
            pick_streamers_to_watch(&[unknown], &[WatchPriority::Order], &[], &[], now),
            vec![0]
        );
    }

    #[test]
    fn a_channel_at_its_points_limit_still_serves_a_drop_campaign() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut capped = limited("capped", 500, Some(100));
        capped.settings.farm_drops = true;
        capped.stream = Some(Stream {
            drop_campaign_eligible: Some(true),
            ..Stream::default()
        });
        let ordinary = limited("ordinary", 0, None);
        let streamers = vec![capped, ordinary];

        // Campaign first, and never again through the points-based lists.
        assert_eq!(
            pick_streamers_to_watch(
                &streamers,
                &[WatchPriority::Drops, WatchPriority::Order],
                &[],
                &[],
                now
            ),
            vec![0, 1]
        );
        assert_eq!(
            pick_streamers_to_watch(&streamers, &[WatchPriority::Order], &[], &[], now),
            vec![1]
        );
    }

    #[test]
    fn a_channel_returns_to_the_rotation_once_its_balance_drops_below_the_limit() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut channel = limited("channel", 100, Some(100));
        assert!(pick_streamers_to_watch(
            &[channel.clone()],
            &[WatchPriority::Order],
            &[],
            &[],
            now
        )
        .is_empty());
        channel.channel_points = 40;
        assert_eq!(
            pick_streamers_to_watch(&[channel], &[WatchPriority::Order], &[], &[], now),
            vec![0]
        );
    }

    fn open_streak(username: &str, discovered: bool) -> Streamer {
        Streamer {
            username: username.to_string(),
            is_online: true,
            discovered,
            settings: StreamerSettings {
                watch_streak: true,
                ..StreamerSettings::default()
            },
            stream: Some(Stream {
                watch_streak_missing: true,
                ..Stream::default()
            }),
            ..Streamer::default()
        }
    }

    #[test]
    fn a_discovered_channel_is_eligible_only_while_its_streak_is_open() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut discovered = open_streak("found", true);
        let orders = [WatchPriority::Streak, WatchPriority::Order];

        assert_eq!(
            pick_streamers_to_watch(&[discovered.clone()], &orders, &[], &[], now),
            vec![0]
        );
        discovered.stream.as_mut().unwrap().minute_watched = 10.0;
        assert!(pick_streamers_to_watch(&[discovered.clone()], &orders, &[], &[], now).is_empty());
        discovered.stream.as_mut().unwrap().minute_watched = 0.0;
        discovered.stream.as_mut().unwrap().watch_streak_missing = false;
        assert!(pick_streamers_to_watch(&[discovered.clone()], &orders, &[], &[], now).is_empty());

        // The same state on a configured channel is an ordinary rotation member.
        discovered.discovered = false;
        assert_eq!(
            pick_streamers_to_watch(&[discovered], &orders, &[], &[], now),
            vec![0]
        );
    }

    #[test]
    fn configured_channels_always_rank_before_discovered_ones() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut collected = open_streak("collected", false);
        collected.stream.as_mut().unwrap().watch_streak_missing = false;
        let streamers = vec![
            open_streak("found", true),
            collected,
            open_streak("open", false),
        ];

        for orders in [
            vec![WatchPriority::Streak, WatchPriority::Order],
            vec![WatchPriority::Order],
            vec![WatchPriority::LongestStreak],
        ] {
            let selected = pick_streamers_to_watch(&streamers, &orders, &[], &[], now);
            assert_eq!(selected.last(), Some(&0), "{orders:?}");
            assert_eq!(selected.len(), 3, "{orders:?}");
        }
    }

    #[test]
    fn discovered_settings_turn_off_everything_but_the_streak() {
        let base = StreamerSettings {
            make_predictions: true,
            follow_raid: true,
            farm_drops: true,
            claim_drops: true,
            single_watcher_during_drops: true,
            claim_moments: true,
            watch_streak: false,
            watch_streak_vod_recovery: true,
            community_goals: true,
            points_limit: Some(5),
            irc_mode: IrcMode::Always,
            ..StreamerSettings::default()
        };

        let settings = base.for_discovered();

        assert_eq!(
            settings,
            StreamerSettings {
                watch_streak: true,
                irc_mode: IrcMode::Never,
                ..StreamerSettings::default()
            }
        );
    }

    #[test]
    fn channel_scores_compare_streak_then_drops_then_multiplier_then_yield() {
        let score = |uncollected_streak, drops, multiplier_millis, yield_tier| ChannelScore {
            uncollected_streak,
            drops,
            multiplier_millis,
            yield_tier,
        };
        let ladder = [
            score(false, false, 0, 0),
            score(false, false, 0, 1),
            score(false, false, 1, 0),
            score(false, false, 2_500, 1),
            score(false, true, 0, 0),
            score(false, true, 0, 1),
            score(true, false, 0, 0),
            score(true, true, 9_000, 1),
        ];
        for pair in ladder.windows(2) {
            assert!(pair[0] < pair[1], "{pair:?}");
        }
        assert_eq!(ChannelScore::default(), score(false, false, 0, 0));
    }

    #[test]
    fn channel_scores_read_the_same_facts_the_priorities_rank_by() {
        let now = datetime!(2026-03-27 06:00 UTC);
        let mut subscriber = open_streak("subscriber", false);
        subscriber.active_multipliers = vec![
            crate::types::ActiveMultiplier { factor: 0.2 },
            crate::types::ActiveMultiplier { factor: 0.3 },
        ];
        subscriber.settings.farm_drops = true;
        subscriber.stream.as_mut().unwrap().drop_campaign_eligible = Some(true);

        assert_eq!(
            ChannelScore::of(&subscriber, 1, now),
            ChannelScore {
                uncollected_streak: true,
                drops: true,
                multiplier_millis: 500,
                yield_tier: 1,
            }
        );
        subscriber.stream.as_mut().unwrap().watch_streak_missing = false;
        subscriber.stream.as_mut().unwrap().drop_campaign_eligible = Some(false);
        assert_eq!(
            ChannelScore::of(&subscriber, 0, now),
            ChannelScore {
                multiplier_millis: 500,
                ..ChannelScore::default()
            }
        );
        assert_eq!(
            ChannelScore::of(&Streamer::default(), 0, now),
            ChannelScore::default()
        );
    }

    #[test]
    fn multiplier_millis_never_overflows_or_goes_negative() {
        assert_eq!(multiplier_millis(f64::NAN), 0);
        assert_eq!(multiplier_millis(-3.0), 0);
        assert_eq!(multiplier_millis(1.2344), 1_234);
        assert_eq!(multiplier_millis(f64::MAX), u32::MAX);
    }
}
