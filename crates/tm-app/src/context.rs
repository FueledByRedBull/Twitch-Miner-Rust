use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tm_domain::Streamer;
use tm_observability::Event as DiscordEvent;
use tm_twitch::TwitchClient;

use crate::observability::AppObservability;
use crate::runtime_effects::execute_runtime_effects;
use crate::{CONTEXT_REFRESH_CONCURRENCY, PENDING_CLAIMS_INTERVAL};

pub(crate) fn apply_context_to_streamer(
    streamer: &mut Streamer,
    context: &tm_twitch::ChannelPointsContext,
) {
    streamer.apply_channel_points_context(
        context.balance,
        &context.active_multipliers,
        &context.community_goals,
    );
}

pub(crate) async fn contribute_streamer_community_goals(
    twitch: &TwitchClient,
    streamer: &Streamer,
) -> Result<bool> {
    if !streamer.settings.community_goals {
        return Ok(false);
    }
    let contributions = load_goal_contributions(twitch, &streamer.username).await?;
    let mut available_points = streamer.channel_points;
    let mut contributed = false;
    for goal in streamer.community_goals.values() {
        if !goal.is_active() {
            continue;
        }
        let user_points = contributions.get(&goal.id).copied().unwrap_or_default();
        let amount =
            tm_twitch::community_goal_contribution_amount(goal, user_points, available_points);
        if amount <= 0 {
            continue;
        }
        twitch
            .contribute_community_goal(amount, &streamer.channel_id, &goal.id)
            .await?;
        available_points -= amount;
        contributed = true;
        tracing::info!(
            streamer = %streamer.username,
            goal_id = %goal.id,
            title = %goal.title,
            amount,
            "contributed to community goal"
        );
    }
    Ok(contributed)
}

pub(crate) fn spawn_context_refresh_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    persistent_user_id: String,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(20 * 60),
            std::time::Duration::from_secs(20 * 60),
        );
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
                    if let Err(error) = refresh_snapshot_streamers(
                        &runtime,
                        &twitch,
                        &persistent_user_id,
                        &observability,
                    )
                    .await
                    {
                        tracing::warn!(%error, "context refresh snapshot failed");
                    }
                }
            }
        }
    })
}

pub(crate) fn spawn_pending_claim_loop(
    stop: tokio::sync::watch::Receiver<bool>,
    runtime: tm_runtime::RuntimeHandle,
    twitch: Arc<TwitchClient>,
    persistent_user_id: String,
    observability: AppObservability,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + PENDING_CLAIMS_INTERVAL,
            PENDING_CLAIMS_INTERVAL,
        );
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
                    if let Err(error) = refresh_snapshot_streamers(
                        &runtime,
                        &twitch,
                        &persistent_user_id,
                        &observability,
                    )
                    .await
                    {
                        tracing::warn!(%error, "pending bonus sweep failed");
                    }
                }
            }
        }
    })
}

pub(crate) async fn refresh_snapshot_streamers(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let snapshot = runtime.state_snapshot().await?;
    let mut refreshes = tokio::task::JoinSet::new();

    for streamer in snapshot.streamers {
        while refreshes.len() >= CONTEXT_REFRESH_CONCURRENCY {
            log_context_refresh_result(refreshes.join_next().await);
        }
        let runtime = runtime.clone();
        let twitch = Arc::clone(twitch);
        let persistent_user_id = persistent_user_id.to_string();
        let observability = observability.clone();
        refreshes.spawn(async move {
            let username = streamer.username.clone();
            let result = match refresh_streamer_context(
                &runtime,
                twitch.as_ref(),
                &streamer,
                Some(&persistent_user_id),
                &observability,
            )
            .await
            {
                Ok(effects) => {
                    execute_runtime_effects(
                        &runtime,
                        &twitch,
                        &persistent_user_id,
                        effects,
                        &observability,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            (username, result)
        });
    }

    while !refreshes.is_empty() {
        log_context_refresh_result(refreshes.join_next().await);
    }

    Ok(())
}

pub(crate) fn log_context_refresh_result(
    result: Option<std::result::Result<(String, Result<()>), tokio::task::JoinError>>,
) {
    match result {
        Some(Ok((username, Err(error)))) => {
            tracing::warn!(streamer = %username, %error, "context refresh failed");
        }
        Some(Err(error)) => {
            tracing::warn!(%error, "context refresh task failed");
        }
        _ => {}
    }
}

pub(crate) async fn refresh_streamer_context(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    persistent_user_id: Option<&str>,
    observability: &AppObservability,
) -> Result<Vec<tm_runtime::RuntimeEffect>> {
    let mut context = fetch_streamer_context(twitch, streamer).await?;
    if let Some(claim_id) = context.claim_id.as_deref() {
        twitch
            .claim_bonus(&streamer.channel_id, claim_id, persistent_user_id)
            .await
            .with_context(|| format!("claim refreshed bonus for {}", streamer.username))?;
        if observability.show_claimed_bonus {
            let message = observability.bonus_claim_message(streamer, false);
            tracing::info!("{message}");
            observability.spawn_event(DiscordEvent::BonusClaim, message);
        }
        context = fetch_streamer_context(twitch, streamer).await?;
    }
    apply_runtime_context(runtime, streamer, context).await
}

pub(crate) async fn refresh_streamer_context_without_goal_effects(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    streamer: &Streamer,
    persistent_user_id: Option<&str>,
    observability: &AppObservability,
) -> Result<()> {
    let _ = refresh_streamer_context(runtime, twitch, streamer, persistent_user_id, observability)
        .await?;
    Ok(())
}

pub(crate) async fn fetch_streamer_context(
    twitch: &TwitchClient,
    streamer: &Streamer,
) -> Result<tm_twitch::ChannelPointsContext> {
    twitch
        .fetch_channel_points_context(&streamer.username)
        .await
        .with_context(|| format!("fetch context for {}", streamer.username))
}

pub(crate) async fn apply_runtime_context(
    runtime: &tm_runtime::RuntimeHandle,
    streamer: &Streamer,
    context: tm_twitch::ChannelPointsContext,
) -> Result<Vec<tm_runtime::RuntimeEffect>> {
    Ok(runtime
        .apply_context_update(tm_runtime::ContextUpdate {
            channel_id: streamer.channel_id.clone(),
            balance: context.balance,
            active_multipliers: context.active_multipliers,
            community_goals: context.community_goals,
        })
        .await?)
}

pub(crate) async fn load_goal_contributions(
    twitch: &TwitchClient,
    username: &str,
) -> Result<HashMap<String, i64>> {
    let response = twitch.fetch_user_points_contribution(username).await?;
    let contributions = tm_twitch::parse_user_points_contributions(&response)
        .into_iter()
        .collect();
    Ok(contributions)
}
