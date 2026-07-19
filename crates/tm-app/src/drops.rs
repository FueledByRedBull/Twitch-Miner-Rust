use std::sync::Arc;

use anyhow::{Context, Result};
use tm_config::ConfigFile;
use tm_domain::Streamer;
use tm_observability::Event as DiscordEvent;
use tm_twitch::{InventoryDrop, TwitchClient};

use crate::observability::AppObservability;
use crate::status::HealthTracker;

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
                        )
                        .await
                    {
                        health.failure("drop", "inventory-or-claim");
                        tracing::warn!(task = "drop", error_class = "inventory-or-claim", %error, "periodic drop claim failed");
                    } else {
                        health.success("drop");
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
    claim_available_drops_with_health(twitch, mode, observability, None).await
}

async fn claim_available_drops_with_health(
    twitch: &TwitchClient,
    mode: &str,
    observability: &AppObservability,
    health: Option<&HealthTracker>,
) -> Result<()> {
    let drops = twitch
        .fetch_claimable_drops()
        .await
        .with_context(|| format!("load {mode} drops inventory"))?;
    if let Some(health) = health {
        health.clear_drop_progress();
        for drop in &drops {
            health.record_drop_progress(
                drop.current_minutes_watched,
                drop.required_minutes_watched,
                drop.is_claimed,
            );
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
    for drop in drops.into_iter().filter(drop_is_claimable) {
        twitch
            .claim_drop(&drop.drop_instance_id)
            .await
            .with_context(|| format!("claim drop {}", drop.drop_instance_id))?;
        if let Some(health) = health {
            health.record_claim();
        }
        let message = observability.drop_claim_message(mode, &drop);
        tracing::info!(operation = "claim_drop", "{message}");
        observability
            .send_event(DiscordEvent::DropClaim, &message)
            .await;
    }
    Ok(())
}
