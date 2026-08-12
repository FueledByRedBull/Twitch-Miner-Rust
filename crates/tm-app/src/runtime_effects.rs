use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tm_domain::{PredictionDecision, Streamer};
use tm_observability::{event_from_bet_result, Event as DiscordEvent};
use tm_twitch::{TwitchClient, TwitchClientError, TwitchFailureClass};

use crate::context::{contribute_streamer_community_goals, refresh_streamer_context};
use crate::effects::runtime_streamer_by_channel_id;
use crate::observability::AppObservability;
use crate::prediction::prediction_wait_duration;
use crate::status::HealthTracker;
use crate::utilities::time_now;

#[derive(Clone)]
pub(crate) struct RuntimeEffectContext {
    pub(crate) runtime: tm_runtime::RuntimeHandle,
    pub(crate) twitch: Arc<TwitchClient>,
    pub(crate) persistent_user_id: String,
    pub(crate) observability: AppObservability,
    pub(crate) health: HealthTracker,
}

impl RuntimeEffectContext {
    pub(crate) fn new(
        runtime: tm_runtime::RuntimeHandle,
        twitch: Arc<TwitchClient>,
        persistent_user_id: String,
        observability: AppObservability,
        health: HealthTracker,
    ) -> Self {
        Self {
            runtime,
            twitch,
            persistent_user_id,
            observability,
            health,
        }
    }
}

pub(crate) async fn execute_runtime_effects(
    context: &RuntimeEffectContext,
    effects: Vec<tm_runtime::RuntimeEffect>,
) -> Result<()> {
    for effect in effects {
        execute_runtime_effect(context, effect).await?;
    }

    Ok(())
}

pub(crate) async fn execute_runtime_effect(
    context: &RuntimeEffectContext,
    effect: tm_runtime::RuntimeEffect,
) -> Result<()> {
    match effect {
        tm_runtime::RuntimeEffect::ClaimBonus {
            channel_id,
            claim_id,
        } => {
            handle_claim_bonus_effect(
                &context.runtime,
                context.twitch.as_ref(),
                &context.persistent_user_id,
                &channel_id,
                &claim_id,
                &context.observability,
                &context.health,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::ClaimMoment {
            channel_id,
            moment_id,
        } => {
            handle_claim_moment_effect(
                &context.runtime,
                context.twitch.as_ref(),
                &channel_id,
                &moment_id,
                &context.observability,
                &context.health,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::JoinRaid {
            channel_id,
            raid_id,
            target_login,
        } => {
            handle_join_raid_effect(
                &context.runtime,
                context.twitch.as_ref(),
                &channel_id,
                &raid_id,
                &target_login,
                &context.observability,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::ContributeCommunityGoals { channel_id } => {
            handle_community_goal_effect(
                &context.runtime,
                context.twitch.as_ref(),
                &context.persistent_user_id,
                &channel_id,
                &context.observability,
                &context.health,
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::EvaluatePrediction { event_id } => {
            spawn_prediction_evaluation(context, &event_id);
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
                &context.observability,
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
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    if !streamer.can_earn_channel_points() {
        runtime.release_claim_bonus(channel_id, claim_id).await?;
        return Ok(());
    }
    twitch
        .claim_bonus(channel_id, claim_id, Some(persistent_user_id))
        .await?;
    health.record_claim();
    if observability.show_claimed_bonus {
        let message = observability.bonus_claim_message(&streamer, false);
        tracing::info!(operation = "claim_bonus", "{message}");
        observability.spawn_event(DiscordEvent::BonusClaim, message);
    }
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
        let effects = refresh_streamer_context(runtime, twitch, &streamer).await?;
        for effect in effects {
            if let tm_runtime::RuntimeEffect::ClaimBonus {
                channel_id,
                claim_id,
            } = effect
            {
                handle_claim_bonus_effect(
                    runtime,
                    twitch,
                    persistent_user_id,
                    &channel_id,
                    &claim_id,
                    observability,
                    health,
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub(crate) fn spawn_prediction_evaluation(context: &RuntimeEffectContext, event_id: &str) {
    let task_context = context.clone();
    let event_id = event_id.to_string();
    let task = tokio::spawn(async move {
        if let Err(error) = evaluate_prediction_after_delay(&task_context, &event_id).await {
            tracing::warn!(event_id = %event_id, %error, "prediction evaluation failed");
        }
    });
    context.observability.track_task(task);
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
    context: &RuntimeEffectContext,
    event_id: &str,
) -> Result<()> {
    let Some((wait, event)) = prediction_wait_for_event(&context.runtime, event_id).await? else {
        return Ok(());
    };
    tracing::info!(
        operation = "on_message",
        "{}",
        context.observability.prediction_wait_message(&event, wait)
    );
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    evaluate_prediction(context, event_id).await
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
    context: &RuntimeEffectContext,
    event_id: &str,
) -> Result<()> {
    let snapshot = context.runtime.state_snapshot().await?;
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
        context
            .runtime
            .stop_tracking_prediction(event_id, "ERROR")
            .await?;
        return Ok(());
    };

    if maybe_skip_prediction_for_status(
        &context.runtime,
        event_id,
        &event,
        &streamer,
        &context.observability,
    )
    .await?
    {
        return Ok(());
    }

    if maybe_skip_prediction_for_balance(
        &context.runtime,
        event_id,
        &streamer,
        &context.observability,
    )
    .await?
    {
        return Ok(());
    }

    event.streamer = streamer.clone();
    tracing::info!(
        operation = "make_predictions",
        "{}",
        context.observability.prediction_start_message(&event)
    );
    let stealth_offset = prediction_stealth_offset(event_id);
    let decision = event.decide_with_stealth_offset(streamer.channel_points, stealth_offset);
    if decision.outcome_id.is_empty() {
        skip_prediction(
            &context.runtime,
            event_id,
            format!(
                "skip prediction: no outcome selected for {}",
                context.observability.streamer_name(&streamer)
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
            &context.runtime,
            event_id,
            format!(
                "skip prediction for {}: {}",
                context.observability.streamer_name(&streamer),
                filter_reason
            ),
        )
        .await?;
        return Ok(());
    }

    if decision.amount < 10 {
        skip_prediction(
            &context.runtime,
            event_id,
            format!(
                "skip prediction: below Twitch minimum for {}",
                context.observability.streamer_name(&streamer)
            ),
        )
        .await?;
        return Ok(());
    }

    place_prediction(context, event_id, &event, &decision, &streamer).await
}

fn prediction_stealth_offset(event_id: &str) -> u8 {
    stealth_offset_from_entropy(RandomState::new().hash_one(event_id))
}

pub(crate) const fn stealth_offset_from_entropy(entropy: u64) -> u8 {
    match entropy % 5 {
        0 => 1,
        1 => 2,
        2 => 3,
        3 => 4,
        _ => 5,
    }
}

pub(crate) async fn maybe_skip_prediction_for_status(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    streamer: &Streamer,
    observability: &AppObservability,
) -> Result<bool> {
    if !streamer.can_earn_channel_points() {
        tracing::info!(
            event_id = %event_id,
            "skip prediction: channel points disabled for {}",
            observability.streamer_name(streamer)
        );
        runtime.release_prediction(event_id).await?;
        return Ok(true);
    }
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

pub(crate) async fn place_prediction(
    context: &RuntimeEffectContext,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    decision: &PredictionDecision,
    streamer: &Streamer,
) -> Result<()> {
    match context
        .twitch
        .make_prediction(&event.event_id, &decision.outcome_id, decision.amount)
        .await
    {
        Ok(()) => {
            context.health.record_bet();
            let deduct_stake = streamer.settings.bet.deduct_stake_on_place.unwrap_or(true);
            context
                .runtime
                .record_prediction_placed(&event.event_id, decision.clone(), deduct_stake)
                .await?;
            let message = context
                .observability
                .prediction_placed_message(event, decision);
            tracing::info!(operation = "make_predictions", event_id = %event.event_id, "{message}");
            context
                .observability
                .spawn_event(DiscordEvent::BetGeneral, message);
            Ok(())
        }
        Err(error) => {
            context
                .runtime
                .stop_tracking_prediction(event_id, "ERROR")
                .await?;
            let failure_class = twitch_error_class(&error);
            context.observability.spawn_event(
                DiscordEvent::BetFailed,
                format!(
                    "Prediction failed for {} ({failure_class})",
                    context.observability.streamer_name(streamer),
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
