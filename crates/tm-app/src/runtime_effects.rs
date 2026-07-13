use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tm_domain::{PredictionDecision, Streamer};
use tm_observability::{event_from_bet_result, Event as DiscordEvent};
use tm_twitch::{TwitchClient, TwitchClientError, TwitchFailureClass};

use crate::context::{
    apply_runtime_context, contribute_streamer_community_goals, fetch_streamer_context,
    refresh_streamer_context_without_goal_effects,
};
use crate::effects::runtime_streamer_by_channel_id;
use crate::observability::AppObservability;
use crate::prediction::prediction_wait_duration;
use crate::status::HealthTracker;
use crate::utilities::time_now;

pub(crate) async fn execute_runtime_effects(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    effects: Vec<tm_runtime::RuntimeEffect>,
    observability: &AppObservability,
    health: HealthTracker,
) -> Result<()> {
    for effect in effects {
        execute_runtime_effect(
            runtime,
            twitch,
            persistent_user_id,
            effect,
            observability,
            &health,
        )
        .await?;
    }

    Ok(())
}

pub(crate) async fn execute_runtime_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    effect: tm_runtime::RuntimeEffect,
    observability: &AppObservability,
    health: &HealthTracker,
) -> Result<()> {
    match effect {
        tm_runtime::RuntimeEffect::ClaimBonus {
            channel_id,
            claim_id,
        } => {
            handle_claim_bonus_effect(
                runtime,
                twitch.as_ref(),
                persistent_user_id,
                &channel_id,
                &claim_id,
                observability,
                health,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::ClaimMoment {
            channel_id,
            moment_id,
        } => {
            handle_claim_moment_effect(
                runtime,
                twitch.as_ref(),
                &channel_id,
                &moment_id,
                observability,
                health,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::JoinRaid {
            channel_id,
            raid_id,
            target_login,
        } => {
            handle_join_raid_effect(
                runtime,
                twitch.as_ref(),
                &channel_id,
                &raid_id,
                &target_login,
                observability,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::ContributeCommunityGoals { channel_id } => {
            handle_community_goal_effect(
                runtime,
                twitch.as_ref(),
                persistent_user_id,
                &channel_id,
                observability,
                health,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::EvaluatePrediction { event_id } => {
            spawn_prediction_evaluation(runtime, twitch, &event_id, observability, health.clone());
        }
        tm_runtime::RuntimeEffect::PredictionSettled {
            event_id,
            streamer_username,
            title,
            decision_label,
            result_type,
            result_string,
        } => {
            handle_prediction_settled_effect(
                &event_id,
                &streamer_username,
                &title,
                &decision_label,
                &result_type,
                &result_string,
                observability,
            );
        }
    }

    Ok(())
}

pub(crate) async fn handle_claim_bonus_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    persistent_user_id: &str,
    channel_id: &str,
    claim_id: &str,
    observability: &AppObservability,
    health: &HealthTracker,
) -> Result<()> {
    twitch
        .claim_bonus(channel_id, claim_id, Some(persistent_user_id))
        .await?;
    health.record_claim();
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    if observability.show_claimed_bonus {
        let message = observability.bonus_claim_message(&streamer, false);
        tracing::info!(operation = "claim_bonus", "{message}");
        observability.spawn_event(DiscordEvent::BonusClaim, message);
    }
    let context = fetch_streamer_context(twitch, &streamer).await?;
    let _ = apply_runtime_context(runtime, &streamer, context).await?;
    Ok(())
}

pub(crate) async fn handle_claim_moment_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    channel_id: &str,
    moment_id: &str,
    observability: &AppObservability,
    health: &HealthTracker,
) -> Result<()> {
    twitch.claim_moment(moment_id).await?;
    health.record_claim();
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    let message = format!(
        "Claimed moment for {}",
        observability.streamer_label(&streamer)
    );
    tracing::info!(operation = "claim_moment", "{message}");
    observability.spawn_event(DiscordEvent::MomentClaim, message);
    Ok(())
}

pub(crate) async fn handle_join_raid_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    channel_id: &str,
    raid_id: &str,
    target_login: &str,
    observability: &AppObservability,
) -> Result<()> {
    twitch.join_raid(raid_id).await?;
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    let message =
        observability.join_raid_message(&observability.streamer_label(&streamer), target_login);
    tracing::info!(operation = "update_raid", "{message}");
    observability.spawn_event(DiscordEvent::JoinRaid, message);
    Ok(())
}

pub(crate) async fn handle_community_goal_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    persistent_user_id: &str,
    channel_id: &str,
    observability: &AppObservability,
    health: &HealthTracker,
) -> Result<()> {
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    if contribute_streamer_community_goals(twitch, &streamer).await? {
        refresh_streamer_context_without_goal_effects(
            runtime,
            twitch,
            &streamer,
            Some(persistent_user_id),
            observability,
            health,
        )
        .await?;
    }
    Ok(())
}

pub(crate) fn spawn_prediction_evaluation(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    event_id: &str,
    observability: &AppObservability,
    health: HealthTracker,
) {
    let runtime = runtime.clone();
    let twitch = Arc::clone(twitch);
    let task_observability = observability.clone();
    let event_id = event_id.to_string();
    let task = tokio::spawn(async move {
        if let Err(error) = evaluate_prediction_after_delay(
            &runtime,
            &twitch,
            &event_id,
            &task_observability,
            &health,
        )
        .await
        {
            tracing::warn!(event_id = %event_id, %error, "prediction evaluation failed");
        }
    });
    observability.track_task(task);
}

pub(crate) fn handle_prediction_settled_effect(
    event_id: &str,
    _streamer_username: &str,
    title: &str,
    decision_label: &str,
    result_type: &str,
    result_string: &str,
    observability: &AppObservability,
) {
    let message = observability.prediction_result_message(event_id, title, result_string);
    tracing::info!(
        operation = "on_message",
        decision = %decision_label,
        event_id = %event_id,
        result_type = %result_type,
        "{message}"
    );
    if let Some(event) = event_from_bet_result(result_type) {
        observability.spawn_event(event, message);
    }
}

pub(crate) async fn evaluate_prediction_after_delay(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    observability: &AppObservability,
    health: &HealthTracker,
) -> Result<()> {
    let Some((wait, event)) = prediction_wait_for_event(runtime, event_id).await? else {
        return Ok(());
    };
    tracing::info!(
        operation = "on_message",
        "{}",
        observability.prediction_wait_message(&event, wait)
    );
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    evaluate_prediction(runtime, twitch, event_id, observability, health).await
}

pub(crate) async fn prediction_wait_for_event(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
) -> Result<Option<(Duration, tm_domain::PredictionEvent)>> {
    let snapshot = runtime.state_snapshot().await?;
    Ok(snapshot
        .predictions
        .get(event_id)
        .cloned()
        .map(|event| (prediction_wait_duration(&event, time_now()), event)))
}

pub(crate) async fn evaluate_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    observability: &AppObservability,
    health: &HealthTracker,
) -> Result<()> {
    let snapshot = runtime.state_snapshot().await?;
    let Some(mut event) = snapshot.predictions.get(event_id).cloned() else {
        return Ok(());
    };
    if event.bet_placed || !event.result_type.is_empty() {
        return Ok(());
    }
    let Some(streamer) = snapshot
        .streamers
        .iter()
        .find(|streamer| streamer.channel_id == event.streamer.channel_id)
        .cloned()
    else {
        runtime.stop_tracking_prediction(event_id, "ERROR").await?;
        return Ok(());
    };

    if maybe_skip_prediction_for_status(runtime, event_id, &event, &streamer, observability).await?
    {
        return Ok(());
    }

    if maybe_skip_prediction_for_balance(runtime, event_id, &streamer, observability).await? {
        return Ok(());
    }

    event.streamer = streamer.clone();
    tracing::info!(
        operation = "make_predictions",
        "{}",
        observability.prediction_start_message(&event)
    );
    let decision = event.decide(streamer.channel_points);
    if decision.outcome_id.is_empty() {
        skip_prediction(
            runtime,
            event_id,
            format!(
                "skip prediction: no outcome selected for {}",
                observability.streamer_name(&streamer)
            ),
        )
        .await?;
        return Ok(());
    }

    let (skip, compared, reason) = event.should_skip_by_filter();
    if skip {
        let filter_reason = if reason.is_empty() {
            format!("filter_condition not satisfied (current {compared})")
        } else {
            reason
        };
        skip_prediction(
            runtime,
            event_id,
            format!(
                "skip prediction for {}: {}",
                observability.streamer_name(&streamer),
                filter_reason
            ),
        )
        .await?;
        return Ok(());
    }

    if decision.amount < 10 {
        skip_prediction(
            runtime,
            event_id,
            format!(
                "skip prediction: below Twitch minimum for {}",
                observability.streamer_name(&streamer)
            ),
        )
        .await?;
        return Ok(());
    }

    place_prediction(
        runtime,
        twitch,
        event_id,
        &event,
        &decision,
        &streamer,
        observability,
        health,
    )
    .await
}

pub(crate) async fn maybe_skip_prediction_for_status(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    streamer: &Streamer,
    observability: &AppObservability,
) -> Result<bool> {
    if event.status == "ACTIVE" {
        return Ok(false);
    }
    tracing::info!(
        event_id = %event_id,
        status = %event.status,
        "skip prediction: event status is not active for {}",
        observability.streamer_name(streamer)
    );
    runtime
        .stop_tracking_prediction(event_id, "SKIPPED")
        .await?;
    Ok(true)
}

pub(crate) async fn maybe_skip_prediction_for_balance(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
    streamer: &Streamer,
    observability: &AppObservability,
) -> Result<bool> {
    let Some(minimum_points) = streamer.settings.bet.minimum_points else {
        return Ok(false);
    };
    if streamer.channel_points > i64::from(minimum_points) {
        return Ok(false);
    }
    tracing::info!(
        event_id = %event_id,
        balance = streamer.channel_points,
        minimum_points,
        "skip prediction: balance below minimum_points for {}",
        observability.streamer_name(streamer)
    );
    runtime
        .stop_tracking_prediction(event_id, "SKIPPED")
        .await?;
    Ok(true)
}

pub(crate) async fn skip_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
    message: String,
) -> Result<()> {
    tracing::info!(event_id = %event_id, "{message}");
    runtime
        .stop_tracking_prediction(event_id, "SKIPPED")
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn place_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    decision: &PredictionDecision,
    streamer: &Streamer,
    observability: &AppObservability,
    health: &HealthTracker,
) -> Result<()> {
    match twitch
        .make_prediction(&event.event_id, &decision.outcome_id, decision.amount)
        .await
    {
        Ok(()) => {
            health.record_bet();
            let deduct_stake = streamer.settings.bet.deduct_stake_on_place.unwrap_or(true);
            runtime
                .record_prediction_placed(&event.event_id, decision.clone(), deduct_stake)
                .await?;
            let message = observability.prediction_placed_message(event, decision);
            tracing::info!(operation = "make_predictions", event_id = %event.event_id, "{message}");
            observability.spawn_event(DiscordEvent::BetGeneral, message);
            Ok(())
        }
        Err(error) => {
            runtime.stop_tracking_prediction(event_id, "ERROR").await?;
            let failure_class = twitch_error_class(&error);
            observability.spawn_event(
                DiscordEvent::BetFailed,
                format!(
                    "Prediction failed for {} ({failure_class})",
                    observability.streamer_name(streamer),
                ),
            );
            Err(error.into())
        }
    }
}

fn twitch_error_class(error: &TwitchClientError) -> &'static str {
    match error.failure_class() {
        TwitchFailureClass::Unauthorized => "unauthorized",
        TwitchFailureClass::RateLimited => "rate-limited",
        TwitchFailureClass::ServerError => "server-error",
        TwitchFailureClass::Timeout => "timeout",
        TwitchFailureClass::ConnectionReset => "connection-reset",
        TwitchFailureClass::Other => "mutation-rejected",
    }
}
