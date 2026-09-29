use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tm_config::ConfigFile;
use tm_domain::Streamer;
use tm_observability::Event as DiscordEvent;
use tm_twitch::{InventoryDrop, TwitchClient};

use crate::observability::AppObservability;
use crate::status::HealthTracker;

const UNKNOWN_CLAIM_RETRY_DELAY: Duration = Duration::from_secs(30 * 60);
const MAX_TRACKED_CLAIMS: usize = 256;

#[derive(Clone, Default)]
pub(crate) struct DropClaimCoordinator {
    state: Arc<Mutex<DropClaimState>>,
}

#[derive(Default)]
struct DropClaimState {
    in_flight: HashSet<String>,
    unknown: HashMap<String, Instant>,
    completed: HashMap<String, Instant>,
}

struct DropClaimLease {
    state: Arc<Mutex<DropClaimState>>,
    drop_instance_id: String,
    resolved: bool,
}

impl DropClaimCoordinator {
    fn begin(&self, drop_instance_id: &str) -> Option<DropClaimLease> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .unknown
            .retain(|_, started| started.elapsed() < UNKNOWN_CLAIM_RETRY_DELAY);
        state
            .completed
            .retain(|_, completed| completed.elapsed() < UNKNOWN_CLAIM_RETRY_DELAY);
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
        // prior successful/unknown mutation. Keep a short completed tombstone
        // so a stale snapshot arriving immediately afterwards cannot replay it.
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

async fn claim_available_drops_with_health(
    twitch: &TwitchClient,
    mode: &str,
    observability: &AppObservability,
    health: Option<&HealthTracker>,
    coordinator: &DropClaimCoordinator,
) -> Result<()> {
    let drops = match twitch
        .fetch_inventory_typed()
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
    claim_inventory_drops_with_coordinator(twitch, mode, &drops, observability, health, coordinator)
        .await
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
    claim_inventory_drops_with_coordinator(twitch, mode, drops, observability, health, &coordinator)
        .await
}

pub(crate) async fn claim_inventory_drops_with_coordinator(
    twitch: &TwitchClient,
    mode: &str,
    drops: &[InventoryDrop],
    observability: &AppObservability,
    health: Option<&HealthTracker>,
    coordinator: &DropClaimCoordinator,
) -> Result<()> {
    if let Some(health) = health {
        health.record_drop_inventory(drops);
    }
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
        if let Err(error) = result {
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
        lease.complete();
        if let Some(health) = health {
            let mut claimed = drop.clone();
            claimed.is_claimed = true;
            health.record_drop_progress(&claimed);
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
