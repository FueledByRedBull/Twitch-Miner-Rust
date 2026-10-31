use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tm_config::{ConfigFile, DropAccountLink};
use tm_domain::Streamer;
use tm_observability::Event as DiscordEvent;
use tm_twitch::{InventoryDrop, InventorySnapshot, TwitchClient};

use crate::observability::AppObservability;
use crate::status::HealthTracker;

const UNKNOWN_CLAIM_RETRY_DELAY: Duration = Duration::from_secs(30 * 60);
const MAX_TRACKED_CLAIMS: usize = 256;
/// A channel that credits no Drop minutes for this long is benched.
const DROP_STALL_WINDOW: Duration = Duration::from_secs(20 * 60);
const DROP_STALL_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DROP_STALL_BENCH: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Default)]
pub(crate) struct DropClaimCoordinator {
    state: Arc<Mutex<DropClaimState>>,
    account_link: DropAccountLink,
}

#[derive(Default)]
struct DropClaimState {
    in_flight: HashSet<String>,
    unknown: HashMap<String, Instant>,
    /// Twitch can keep listing a claimed Drop as unclaimed and complete and
    /// accept the claim again, so an accepted claim is never sent again.
    completed: HashMap<String, Instant>,
    last_reported_progress: Option<Instant>,
    watches: HashMap<String, DropChannelWatch>,
    benched: HashMap<String, Instant>,
}

struct DropChannelWatch {
    progressed_at: Instant,
    checked_at: Instant,
    seen_at: Instant,
    minutes: Option<i64>,
}

struct DropClaimLease {
    state: Arc<Mutex<DropClaimState>>,
    drop_instance_id: String,
    resolved: bool,
}

impl DropClaimCoordinator {
    pub(crate) fn with_account_link(account_link: DropAccountLink) -> Self {
        Self {
            account_link,
            ..Self::default()
        }
    }

    pub(crate) fn account_link(&self) -> DropAccountLink {
        self.account_link
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, DropClaimState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Twitch pushed progress, so no Drop channel needs a stall check right now.
    pub(crate) fn record_reported_progress(&self, now: Instant) {
        self.locked().last_reported_progress = Some(now);
    }

    /// Whether a watched Drop channel should be asked about its session now.
    pub(crate) fn stall_check_due(&self, channel_id: &str, now: Instant) -> bool {
        let mut state = self.locked();
        let reported = state.last_reported_progress;
        let watch = state
            .watches
            .entry(channel_id.to_string())
            .or_insert(DropChannelWatch {
                progressed_at: now,
                checked_at: now,
                seen_at: now,
                minutes: None,
            });
        // A channel that left the watch set starts its window over on return.
        if now.duration_since(watch.seen_at) > DROP_STALL_CHECK_INTERVAL {
            watch.progressed_at = now;
            watch.checked_at = now;
            watch.minutes = None;
        }
        watch.seen_at = now;
        if reported.is_some_and(|at| now.duration_since(at) < DROP_STALL_CHECK_INTERVAL) {
            watch.progressed_at = now;
            watch.checked_at = now;
            return false;
        }
        if now.duration_since(watch.checked_at) < DROP_STALL_CHECK_INTERVAL {
            return false;
        }
        watch.checked_at = now;
        true
    }

    /// Records Twitch's answer and returns whether the channel was just benched.
    pub(crate) fn observe_drop_session(
        &self,
        channel_id: &str,
        session: Option<&tm_twitch::DropSession>,
        now: Instant,
    ) -> bool {
        let mut state = self.locked();
        let Some(watch) = state.watches.get_mut(channel_id) else {
            return false;
        };
        // Twitch credits one channel at a time; another channel's session says
        // nothing about this one.
        let credited_elsewhere = session
            .and_then(|session| session.channel_id.as_deref())
            .is_some_and(|credited| credited != channel_id);
        let minutes = session.map(|session| session.current_minutes);
        if credited_elsewhere || (minutes.is_some() && minutes != watch.minutes) {
            watch.progressed_at = now;
        }
        watch.minutes = minutes;
        if now.duration_since(watch.progressed_at) < DROP_STALL_WINDOW {
            return false;
        }
        state.watches.remove(channel_id);
        state
            .benched
            .insert(channel_id.to_string(), now + DROP_STALL_BENCH);
        true
    }

    pub(crate) fn is_benched(&self, channel_id: &str, now: Instant) -> bool {
        let mut state = self.locked();
        state.benched.retain(|_, until| *until > now);
        state.benched.contains_key(channel_id)
    }

    fn begin(&self, drop_instance_id: &str) -> Option<DropClaimLease> {
        let mut state = self.locked();
        state
            .unknown
            .retain(|_, started| started.elapsed() < UNKNOWN_CLAIM_RETRY_DELAY);
        if state.unknown.contains_key(drop_instance_id)
            || state.completed.contains_key(drop_instance_id)
            || state.in_flight.len() >= MAX_TRACKED_CLAIMS
            || !state.in_flight.insert(drop_instance_id.to_string())
        {
            return None;
        }
        Some(DropClaimLease {
            state: Arc::clone(&self.state),
            drop_instance_id: drop_instance_id.to_string(),
            resolved: false,
        })
    }

    fn mark_unknown(&self, drop_instance_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remember_claim(&mut state.unknown, drop_instance_id, Instant::now());
    }

    fn confirm(&self, drop_instance_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.unknown.remove(drop_instance_id);
        // A fresh claimed inventory is the authoritative reconciliation for a
        // prior successful/unknown mutation. Keep a completed tombstone so a
        // stale or wrong snapshot cannot replay it.
        remember_claim(&mut state.completed, drop_instance_id, Instant::now());
    }

    fn complete(&self, drop_instance_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.unknown.remove(drop_instance_id);
        remember_claim(&mut state.completed, drop_instance_id, Instant::now());
    }
}

fn remember_claim(entries: &mut HashMap<String, Instant>, drop_instance_id: &str, now: Instant) {
    if entries.len() >= MAX_TRACKED_CLAIMS && !entries.contains_key(drop_instance_id) {
        if let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, started)| **started)
            .map(|(drop_instance_id, _)| drop_instance_id.clone())
        {
            entries.remove(&oldest);
        }
    }
    entries.insert(drop_instance_id.to_string(), now);
}

impl Drop for DropClaimLease {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight.remove(&self.drop_instance_id);
        if !self.resolved {
            remember_claim(&mut state.unknown, &self.drop_instance_id, Instant::now());
        }
    }
}

impl DropClaimLease {
    fn complete(&mut self) {
        self.resolved = true;
        let coordinator = DropClaimCoordinator {
            state: Arc::clone(&self.state),
            account_link: DropAccountLink::default(),
        };
        coordinator.complete(&self.drop_instance_id);
    }
}

pub(crate) async fn claim_startup_drops_if_enabled(
    config: &ConfigFile,
    streamers: &[Streamer],
    twitch: &TwitchClient,
    observability: &AppObservability,
) -> Result<()> {
    if !config.claim_drops_startup
        || !streamers
            .iter()
            .any(|streamer| streamer.settings.claim_drops)
    {
        return Ok(());
    }

    claim_available_drops(twitch, "startup", observability).await?;

    Ok(())
}

pub(crate) fn drop_is_claimable(drop: &InventoryDrop) -> bool {
    !drop.is_claimed
        && !drop.drop_instance_id.trim().is_empty()
        && drop.required_minutes_watched > 0
        && drop.current_minutes_watched >= drop.required_minutes_watched
}

pub(crate) fn spawn_drop_claim_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    twitch: Arc<TwitchClient>,
    observability: AppObservability,
    health: HealthTracker,
    coordinator: DropClaimCoordinator,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop = stop;
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    if let Err(error) =
                        claim_available_drops_with_health(
                            twitch.as_ref(),
                            "periodic",
                            &observability,
                            Some(&health),
                            &coordinator,
                    )
                    .await
                    {
                        tracing::warn!(task = "drop", error_class = "inventory-or-claim", %error, "periodic drop claim failed");
                    }
                }
            }
        }
    })
}

pub(crate) async fn claim_available_drops(
    twitch: &TwitchClient,
    mode: &str,
    observability: &AppObservability,
) -> Result<()> {
    claim_available_drops_with_health(
        twitch,
        mode,
        observability,
        None,
        &DropClaimCoordinator::default(),
    )
    .await
}

pub(crate) async fn claim_available_drops_with_health(
    twitch: &TwitchClient,
    mode: &str,
    observability: &AppObservability,
    health: Option<&HealthTracker>,
    coordinator: &DropClaimCoordinator,
) -> Result<()> {
    let inventory = match twitch
        .fetch_inventory_snapshot_typed()
        .await
        .with_context(|| format!("load {mode} drops inventory"))
    {
        Ok(drops) => drops,
        Err(error) => {
            if let Some(health) = health {
                health.failure("drop", "inventory-or-claim");
            }
            return Err(error);
        }
    };
    claim_inventory_drops_with_coordinator(
        twitch,
        mode,
        &inventory,
        observability,
        health,
        coordinator,
    )
    .await
}

/// Campaigns the account-link setting rules out, from whichever source listed them.
pub(crate) fn link_excluded_campaign_ids(
    snapshot: &InventorySnapshot,
    catalog: &[tm_twitch::DropCatalogCampaign],
    account_link: DropAccountLink,
) -> HashSet<String> {
    let id = |campaign: &tm_twitch::DropCatalogCampaign| campaign.id.clone();
    match account_link {
        DropAccountLink::All => HashSet::new(),
        DropAccountLink::Linked => snapshot
            .unlinked_campaign_ids
            .iter()
            .cloned()
            .chain(catalog.iter().filter(|c| c.is_unlinked()).filter_map(id))
            .collect(),
        DropAccountLink::None => snapshot
            .link_required_campaign_ids
            .iter()
            .cloned()
            .chain(catalog.iter().filter(|c| c.requires_link()).filter_map(id))
            .collect(),
    }
}

/// Asks Twitch whether this channel still earns Drop minutes, and takes it out
/// of Drop priority when it has stopped.
pub(crate) async fn check_drop_stall(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    coordinator: &DropClaimCoordinator,
    observability: &AppObservability,
    streamer: &Streamer,
    broadcast_id: &str,
) {
    let now = Instant::now();
    if !coordinator.stall_check_due(&streamer.channel_id, now) {
        return;
    }
    let session = match twitch.fetch_drop_session_typed(&streamer.channel_id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(error_class = "drop-session", %error, "drop session read failed");
            return;
        }
    };
    if !coordinator.observe_drop_session(&streamer.channel_id, session.as_ref(), now) {
        return;
    }
    let game_id = streamer
        .stream
        .as_ref()
        .and_then(|stream| stream.game_id.as_deref());
    if let Err(error) = runtime
        .set_drop_watch_target_if_current(&streamer.channel_id, broadcast_id, game_id, false, None)
        .await
    {
        tracing::warn!(error_class = "drop-session", %error, "drop bench update failed");
    }
    let name = observability.streamer_name(streamer);
    tracing::warn!(
        task = "drop",
        error_class = "drop-progress-stalled",
        "no Drop progress on {name}; trying other channels for 30 minutes"
    );
    observability.spawn_event(
        DiscordEvent::DropStatus,
        format!("No Drop progress on {name}; trying other channels for 30 minutes"),
    );
}

#[cfg(test)]
pub(crate) async fn claim_inventory_drops(
    twitch: &TwitchClient,
    mode: &str,
    drops: &[InventoryDrop],
    observability: &AppObservability,
    health: Option<&HealthTracker>,
) -> Result<()> {
    let coordinator = DropClaimCoordinator::default();
    let inventory = InventorySnapshot {
        drops: drops.to_vec(),
        ..InventorySnapshot::default()
    };
    claim_inventory_drops_with_coordinator(
        twitch,
        mode,
        &inventory,
        observability,
        health,
        &coordinator,
    )
    .await
}

pub(crate) async fn claim_inventory_drops_with_coordinator(
    twitch: &TwitchClient,
    mode: &str,
    inventory: &InventorySnapshot,
    observability: &AppObservability,
    health: Option<&HealthTracker>,
    coordinator: &DropClaimCoordinator,
) -> Result<()> {
    if let Some(health) = health {
        health.record_drop_inventory(inventory);
    }
    let drops = &inventory.drops;
    for drop in drops {
        if drop.is_claimed {
            coordinator.confirm(&drop.drop_instance_id);
        }
    }
    if mode == "periodic" {
        for drop in drops
            .iter()
            .filter(|drop| !drop.is_claimed && drop.required_minutes_watched > 0)
        {
            let message = observability.drop_progress_message(drop);
            tracing::info!(operation = "drop_progress", "{message}");
        }
    }
    let mut failures = Vec::new();
    for drop in drops.iter().filter(|drop| drop_is_claimable(drop)) {
        let Some(mut lease) = coordinator.begin(&drop.drop_instance_id) else {
            continue;
        };
        let result = twitch
            .claim_drop(&drop.drop_instance_id)
            .await
            .with_context(|| format!("claim drop {}", drop.drop_instance_id));
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                coordinator.mark_unknown(&drop.drop_instance_id);
                if let Ok(reconciled) = twitch.fetch_inventory_typed().await {
                    if let Some(current) = reconciled.iter().find(|current| {
                        current.drop_instance_id == drop.drop_instance_id && current.is_claimed
                    }) {
                        lease.complete();
                        if let Some(health) = health {
                            health.record_drop_progress(current);
                            health.record_claim();
                        }
                        let message = observability.drop_claim_message(mode, drop);
                        tracing::info!(operation = "claim_drop_reconciled", "{message}");
                        observability
                            .send_event(DiscordEvent::DropClaim, &message)
                            .await;
                        continue;
                    }
                }
                failures.push(error);
                continue;
            }
        };
        lease.complete();
        if let Some(health) = health {
            let mut claimed = drop.clone();
            claimed.is_claimed = true;
            health.record_drop_progress(&claimed);
        }
        if outcome == tm_twitch::ClaimDropOutcome::AlreadyClaimed {
            // The inventory snapshot was stale: this request claimed nothing,
            // so it is neither counted nor announced.
            tracing::debug!(operation = "claim_drop", "drop was already claimed");
            continue;
        }
        if let Some(health) = health {
            health.record_claim();
        }
        let message = observability.drop_claim_message(mode, drop);
        tracing::info!(operation = "claim_drop", "{message}");
        observability
            .send_event(DiscordEvent::DropClaim, &message)
            .await;
    }
    if failures.is_empty() {
        if let Some(health) = health {
            health.success("drop");
        }
        Ok(())
    } else {
        if let Some(health) = health {
            health.failure("drop", "inventory-or-claim");
        }
        Err(anyhow!(
            "{} drop claim(s) failed: {}",
            failures.len(),
            failures
                .iter()
                .map(|error| format!("{error:#}"))
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

/// Channel availability is authoritative; inventory alone never makes a channel eligible.
pub(crate) fn channel_drop_target(
    drops: &[InventoryDrop],
    campaigns: &[String],
    excluded: &HashSet<String>,
    now: tm_domain::OffsetDateTime,
) -> (bool, Option<tm_domain::DropWatchTarget>) {
    let mut eligible = false;
    let mut best = None;
    for campaign in campaigns.iter().filter(|id| !excluded.contains(*id)) {
        let rewards: Vec<_> = drops
            .iter()
            .filter(|drop| &drop.campaign_id == campaign)
            .collect();
        // Missing inventory is not evidence of an unfinished watch reward.
        eligible |= rewards.iter().any(|drop| {
            !drop.is_claimed
                && !drop.subscription_required
                && drop.prerequisites_met != Some(false)
                && drop.starts_at.is_none_or(|start| start <= now)
                && drop.ends_at.is_none_or(|end| {
                    end > now
                        && (end - now).whole_seconds()
                            >= drop
                                .required_minutes_watched
                                .saturating_sub(drop.current_minutes_watched)
                                .saturating_mul(60)
                })
                && drop.current_minutes_watched < drop.required_minutes_watched
                && (drop.starts_at.is_none()
                    || drop.ends_at.is_none()
                    || drop.prerequisites_met.is_none())
        });
        for target in rewards
            .into_iter()
            .filter_map(|drop| watch_target(drop, now))
        {
            eligible = true;
            if best.is_none_or(|previous: tm_domain::DropWatchTarget| {
                (target.ends_at, target.remaining_minutes)
                    < (previous.ends_at, previous.remaining_minutes)
            }) {
                best = Some(target);
            }
        }
    }
    (eligible, best)
}

/// Unknown prerequisites or timing cannot establish a deadline priority.
pub(crate) fn watch_target(
    drop: &InventoryDrop,
    now: tm_domain::OffsetDateTime,
) -> Option<tm_domain::DropWatchTarget> {
    if drop.is_claimed
        || drop.subscription_required
        || drop.prerequisites_met != Some(true)
        || drop.current_minutes_watched < 0
        || drop.required_minutes_watched <= 0
        || drop.starts_at? > now
    {
        return None;
    }
    let target = tm_domain::DropWatchTarget {
        ends_at: drop.ends_at?,
        remaining_minutes: drop
            .required_minutes_watched
            .saturating_sub(drop.current_minutes_watched),
        observed_at: now,
    };
    target.feasible_at(now).then_some(target)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::time::{Duration, Instant};

    #[test]
    fn account_link_setting_selects_which_campaigns_are_excluded() {
        use std::collections::HashSet;
        use tm_config::DropAccountLink;
        let snapshot = tm_twitch::InventorySnapshot {
            unlinked_campaign_ids: vec![String::from("unlinked")],
            link_required_campaign_ids: vec![String::from("unlinked"), String::from("linked")],
            ..tm_twitch::InventorySnapshot::default()
        };
        let excluded = |mode| super::link_excluded_campaign_ids(&snapshot, &[], mode);
        assert!(excluded(DropAccountLink::All).is_empty());
        assert_eq!(
            excluded(DropAccountLink::Linked),
            HashSet::from([String::from("unlinked")])
        );
        assert_eq!(
            excluded(DropAccountLink::None),
            HashSet::from([String::from("unlinked"), String::from("linked")])
        );
    }

    #[test]
    fn drop_stall_benches_a_channel_only_without_minute_progress() {
        let coordinator = super::DropClaimCoordinator::default();
        let start = Instant::now();
        let at = |minutes: u64| start + Duration::from_secs(minutes * 60);
        let session = |minutes: i64, channel: &str| tm_twitch::DropSession {
            channel_id: Some(channel.to_string()),
            current_minutes: minutes,
        };

        assert!(!coordinator.stall_check_due("a", at(0)));
        assert!(!coordinator.stall_check_due("a", at(1)));
        for (minute, minutes) in [(5, 10), (10, 15), (15, 15)] {
            assert!(coordinator.stall_check_due("a", at(minute)));
            assert!(!coordinator.observe_drop_session(
                "a",
                Some(&session(minutes, "a")),
                at(minute)
            ));
        }
        assert!(coordinator.stall_check_due("a", at(20)));
        // Credit going to another channel is not this channel's stall.
        assert!(!coordinator.observe_drop_session("a", Some(&session(15, "b")), at(20)));
        assert!(coordinator.stall_check_due("a", at(25)));
        assert!(!coordinator.observe_drop_session("a", None, at(25)));
        for minute in [30, 35] {
            assert!(coordinator.stall_check_due("a", at(minute)));
            assert!(!coordinator.observe_drop_session("a", None, at(minute)));
        }
        assert!(coordinator.stall_check_due("a", at(40)));
        assert!(coordinator.observe_drop_session("a", None, at(40)));
        assert!(coordinator.is_benched("a", at(41)));
        assert!(!coordinator.is_benched("a", at(71)));
    }

    #[test]
    fn reported_drop_progress_skips_the_session_check() {
        let coordinator = super::DropClaimCoordinator::default();
        let start = Instant::now();
        assert!(!coordinator.stall_check_due("a", start));
        coordinator.record_reported_progress(start + Duration::from_secs(290));
        assert!(!coordinator.stall_check_due("a", start + Duration::from_secs(300)));
    }

    use super::DropClaimCoordinator;

    #[test]
    fn channel_target_obeys_availability_exclusions_and_deadlines() {
        use tm_domain::OffsetDateTime;
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let reward = tm_twitch::InventoryDrop {
            campaign_id: "restricted".into(),
            starts_at: Some(now),
            ends_at: Some(now + std::time::Duration::from_secs(3600)),
            required_minutes_watched: 20,
            prerequisites_met: Some(true),
            ..Default::default()
        };
        let excluded = std::collections::HashSet::new();
        assert_eq!(
            super::channel_drop_target(std::slice::from_ref(&reward), &[], &excluded, now),
            (false, None)
        );
        let campaigns = vec!["restricted".to_string()];
        assert_eq!(
            super::channel_drop_target(&[], &campaigns, &excluded, now),
            (false, None)
        );
        let mut completed = reward.clone();
        completed.is_claimed = true;
        assert_eq!(
            super::channel_drop_target(&[completed], &campaigns, &excluded, now),
            (false, None)
        );
        assert!(super::channel_drop_target(
            std::slice::from_ref(&reward),
            &campaigns,
            &excluded,
            now
        )
        .1
        .is_some());
        let blocked = std::collections::HashSet::from(["restricted".to_string()]);
        assert_eq!(
            super::channel_drop_target(std::slice::from_ref(&reward), &campaigns, &blocked, now),
            (false, None)
        );
        let mut expired = reward.clone();
        expired.ends_at = Some(now);
        assert_eq!(
            super::channel_drop_target(&[expired], &campaigns, &excluded, now),
            (false, None)
        );
        let mut locked = reward;
        locked.prerequisites_met = Some(false);
        assert_eq!(
            super::channel_drop_target(&[locked.clone()], &campaigns, &excluded, now),
            (false, None)
        );
        locked.ends_at = None;
        assert_eq!(
            super::channel_drop_target(std::slice::from_ref(&locked), &campaigns, &excluded, now),
            (false, None)
        );
        locked.prerequisites_met = None;
        assert_eq!(
            super::channel_drop_target(&[locked], &campaigns, &excluded, now),
            (true, None)
        );
    }

    #[test]
    fn deadline_target_requires_feasible_earned_prerequisites_and_no_subscription() {
        use tm_domain::OffsetDateTime;
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let mut drop = tm_twitch::InventoryDrop {
            starts_at: Some(now - std::time::Duration::from_secs(3600)),
            ends_at: Some(now + std::time::Duration::from_secs(1200)),
            required_minutes_watched: 30,
            current_minutes_watched: 10,
            prerequisites_met: Some(true),
            ..Default::default()
        };
        assert_eq!(
            super::watch_target(&drop, now).unwrap().remaining_minutes,
            20
        );
        assert!(!super::drop_is_claimable(&drop));
        drop.ends_at = Some(now + std::time::Duration::from_secs(1140));
        assert!(super::watch_target(&drop, now).is_none());
        drop.ends_at = Some(now + std::time::Duration::from_secs(3600));
        drop.prerequisites_met = Some(false);
        assert!(super::watch_target(&drop, now).is_none());
        drop.prerequisites_met = Some(true);
        drop.subscription_required = true;
        assert!(super::watch_target(&drop, now).is_none());
        drop.subscription_required = false;
        drop.is_claimed = true;
        assert!(super::watch_target(&drop, now).is_none());
        drop.is_claimed = false;
        drop.ends_at = None;
        assert!(super::watch_target(&drop, now).is_none());
    }

    #[test]
    fn coordinator_deduplicates_in_flight_claims_and_releases_after_completion() {
        let coordinator = DropClaimCoordinator::default();
        let mut lease = coordinator.begin("drop-1").expect("first claim owns lease");
        assert!(coordinator.begin("drop-1").is_none());
        lease.complete();
        drop(lease);
        assert!(coordinator.begin("drop-1").is_none());
        coordinator.confirm("drop-1");
        assert!(coordinator.begin("drop-1").is_none());
    }

    #[test]
    fn coordinator_does_not_resend_an_accepted_claim() {
        // A freshly booted host has no instant that far in the past.
        let Some(accepted) =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(2 * 24 * 60 * 60))
        else {
            return;
        };
        let coordinator = DropClaimCoordinator::default();
        coordinator
            .state
            .lock()
            .expect("claim state")
            .completed
            .insert(String::from("drop-1"), accepted);
        assert!(coordinator.begin("drop-1").is_none());
    }

    #[test]
    fn coordinator_holds_unknown_outcome_until_reconciled() {
        let coordinator = DropClaimCoordinator::default();
        let lease = coordinator.begin("drop-unknown").expect("claim owns lease");
        coordinator.mark_unknown("drop-unknown");
        drop(lease);
        assert!(coordinator.begin("drop-unknown").is_none());
        coordinator.confirm("drop-unknown");
        assert!(coordinator.begin("drop-unknown").is_none());
    }

    #[test]
    fn coordinator_marks_an_aborted_claim_unknown() {
        let coordinator = DropClaimCoordinator::default();
        drop(coordinator.begin("drop-aborted").expect("claim owns lease"));
        assert!(coordinator.begin("drop-aborted").is_none());
    }
}
