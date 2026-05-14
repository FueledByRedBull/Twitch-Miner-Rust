#![allow(unused_imports)]
#![allow(clippy::wildcard_imports)]
use crate::*;

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
        && drop.current_minutes_watched >= drop.required_minutes_watched
}

pub(crate) fn spawn_drop_claim_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    twitch: Arc<TwitchClient>,
    observability: AppObservability,
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
                        claim_available_drops(twitch.as_ref(), "periodic", &observability).await
                    {
                        tracing::warn!(%error, "periodic drop claim failed");
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
    let drops = twitch
        .fetch_claimable_drops()
        .await
        .with_context(|| format!("load {mode} drops inventory"))?;
    for drop in drops.into_iter().filter(drop_is_claimable) {
        twitch
            .claim_drop(&drop.drop_instance_id)
            .await
            .with_context(|| format!("claim drop {}", drop.drop_instance_id))?;
        let message = observability.drop_claim_message(mode, &drop);
        tracing::info!("{message}");
        observability
            .send_event(DiscordEvent::DropClaim, &message)
            .await;
    }
    Ok(())
}
