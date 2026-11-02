use tm_domain::{Game, Stream};

use super::*;

fn logins(values: &[&str]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

fn channel(channel_id: &str, names: &[&str], discovered: bool) -> TrackedChannel {
    TrackedChannel {
        channel_id: channel_id.to_string(),
        names: logins(names),
        discovered,
    }
}

fn found_channel(channel_id: &str, login: &str, usable: bool) -> DiscoveredChannel {
    DiscoveredChannel {
        channel_id: channel_id.to_string(),
        login: login.to_string(),
        usable,
    }
}

#[test]
fn new_listed_channels_are_added_and_tracked_ones_are_left_alone() {
    let tracked = [
        channel("1", &["configured"], false),
        // Known to the config under another name, so the listing's name matches.
        channel("2", &["renamed", "old_name"], false),
        channel("3", &["found_a"], true),
    ];
    let discovered = [found_channel("3", "found_a", true)];

    let plan = plan_discovery(
        &logins(&["configured", "old_name", "found_a", "found_b", "found_c"]),
        &tracked,
        &discovered,
    );

    assert_eq!(plan.resolve, logins(&["found_b", "found_c"]));
    assert!(plan.remove.is_empty());
}

#[test]
fn a_discovered_channel_leaves_when_unlisted_offline_or_in_an_excluded_game() {
    let tracked = [
        channel("1", &["kept"], true),
        channel("2", &["gone"], true),
        channel("3", &["offline"], true),
        channel("4", &["slots"], true),
        channel("9", &["configured"], false),
    ];
    let discovered = [
        found_channel("1", "kept", true),
        found_channel("2", "gone", true),
        found_channel("3", "offline", false),
        found_channel("4", "slots", false),
    ];

    // "offline" and "slots" are still listed; being unusable removes them.
    let plan = plan_discovery(
        &logins(&["kept", "offline", "slots", "configured"]),
        &tracked,
        &discovered,
    );

    assert!(plan.resolve.is_empty());
    assert_eq!(plan.remove, logins(&["2", "3", "4"]));
}

#[test]
fn an_unchanged_listing_plans_nothing() {
    let tracked = [channel("1", &["a"], true), channel("2", &["b"], true)];
    let discovered = [found_channel("1", "a", true), found_channel("2", "b", true)];

    assert_eq!(
        plan_discovery(&logins(&["a", "b"]), &tracked, &discovered),
        ChannelPlan::default()
    );
    assert_eq!(
        plan_discovery(&[], &[], &[]),
        ChannelPlan::default(),
        "nothing listed and nothing tracked"
    );
}

#[test]
fn an_empty_listing_drops_every_discovered_channel() {
    let tracked = [channel("1", &["a"], true), channel("2", &["b"], false)];
    let discovered = [found_channel("1", "a", true)];

    let plan = plan_discovery(&[], &tracked, &discovered);

    assert_eq!(plan.remove, logins(&["1"]));
    assert!(plan.resolve.is_empty());
}

#[test]
fn kept_plus_added_never_exceeds_the_cap() {
    let kept = (0..MAX_DISCOVERED - 1)
        .map(|index| format!("kept{index}"))
        .collect::<Vec<_>>();
    let tracked = kept
        .iter()
        .enumerate()
        .map(|(index, login)| channel(&index.to_string(), &[login], true))
        .collect::<Vec<_>>();
    let discovered = kept
        .iter()
        .enumerate()
        .map(|(index, login)| found_channel(&index.to_string(), login, true))
        .collect::<Vec<_>>();
    let mut listing = kept.clone();
    listing.extend(logins(&["new1", "new2", "new3"]));

    let plan = plan_discovery(&listing, &tracked, &discovered);

    assert_eq!(plan.resolve, logins(&["new1"]));
    assert!(plan.remove.is_empty());

    // Channels that leave in the same change make room for new ones.
    let stale = discovered
        .iter()
        .map(|channel| found_channel(&channel.channel_id, &channel.login, false))
        .collect::<Vec<_>>();
    let plan = plan_discovery(&listing, &tracked, &stale);
    assert_eq!(plan.remove.len(), MAX_DISCOVERED - 1);
    assert_eq!(plan.resolve, logins(&["new1", "new2", "new3"]));
}

#[test]
fn only_a_named_excluded_game_counts() {
    let playing = |game: Option<&str>| Streamer {
        stream: Some(Stream {
            game: game.map(Game::from_name),
            ..Stream::default()
        }),
        ..Streamer::default()
    };
    let exclusions = logins(&["slots", "just chatting"]);

    assert!(plays_excluded_game(&playing(Some("Slots")), &exclusions));
    assert!(plays_excluded_game(
        &playing(Some(" Just Chatting ")),
        &exclusions
    ));
    assert!(!plays_excluded_game(&playing(Some("Chess")), &exclusions));
    assert!(!plays_excluded_game(&playing(None), &exclusions));
    assert!(!plays_excluded_game(&Streamer::default(), &exclusions));
    assert!(!plays_excluded_game(&playing(Some("Slots")), &[]));
}
