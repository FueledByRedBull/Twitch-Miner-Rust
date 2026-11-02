//! Streak discovery: live channels of `streak_discovery_games` that are tracked
//! only to collect a watch streak.
//!
//! The config-watch task refreshes them at most every six hours. A discovered
//! channel runs with the safe settings of [`tm_domain::StreamerSettings::for_discovered`],
//! is eligible for a watch slot only while its streak is open, and never replaces
//! or outranks a configured channel. This module holds the parts that need no
//! runtime: listing the candidates and planning the change.

use std::time::Duration;

use tm_domain::{normalize_streamer_list, Streamer};
use tm_twitch::{TwitchClient, TwitchClientError};

use crate::reload::{ChannelPlan, TrackedChannel};

/// Live channels read per game.
pub(crate) const CHANNELS_PER_GAME: u32 = 5;
/// Discovered channels tracked at once, across all games.
pub(crate) const MAX_DISCOVERED: usize = 20;
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// A tracked discovered channel and whether it should stay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredChannel {
    pub(crate) channel_id: String,
    pub(crate) login: String,
    /// Online, and not playing an excluded game.
    pub(crate) usable: bool,
}

/// Lists the live channels to track, most viewed first within each game, at
/// most [`CHANNELS_PER_GAME`] per game and [`MAX_DISCOVERED`] in total, so games
/// late in the list may get nothing. `excluded` logins are skipped. Any failed
/// read fails the call, so a refresh changes everything or nothing.
pub(crate) async fn list_channels(
    twitch: &TwitchClient,
    games: &[String],
    excluded: &[String],
) -> Result<Vec<String>, TwitchClientError> {
    let mut found = Vec::<String>::new();
    for game in games {
        if found.len() >= MAX_DISCOVERED {
            break;
        }
        let logins = twitch
            .fetch_live_channels_typed(game, CHANNELS_PER_GAME)
            .await?;
        let mut taken = 0;
        for login in normalize_streamer_list(&logins) {
            if taken == CHANNELS_PER_GAME || found.len() >= MAX_DISCOVERED {
                break;
            }
            if !excluded.contains(&login) && !found.contains(&login) {
                found.push(login);
                taken += 1;
            }
        }
    }
    Ok(found)
}

/// Works out which discovered channels to add and drop for a fresh listing.
///
/// A listed login that any tracked channel already answers to, configured or
/// discovered, is left alone. A discovered channel leaves when it is no longer
/// listed, is offline, or plays an excluded game. The number kept plus the
/// number added never exceeds [`MAX_DISCOVERED`].
pub(crate) fn plan_discovery(
    found: &[String],
    tracked: &[TrackedChannel],
    discovered: &[DiscoveredChannel],
) -> ChannelPlan {
    let remove = discovered
        .iter()
        .filter(|channel| !channel.usable || !found.contains(&channel.login))
        .map(|channel| channel.channel_id.clone())
        .collect::<Vec<_>>();
    let room = MAX_DISCOVERED.saturating_sub(discovered.len() - remove.len());
    ChannelPlan {
        resolve: found
            .iter()
            .filter(|login| !tracked.iter().any(|channel| channel.names.contains(login)))
            .take(room)
            .cloned()
            .collect(),
        remove,
    }
}

/// Whether the channel is playing a game the config excludes.
pub(crate) fn plays_excluded_game(streamer: &Streamer, game_exclusions: &[String]) -> bool {
    streamer.stream.as_ref().is_some_and(|stream| {
        let game = stream.game_name().to_lowercase();
        !game.is_empty() && game_exclusions.contains(&game)
    })
}

#[cfg(test)]
#[path = "../tests/unit/discovery_tests.rs"]
mod tests;
