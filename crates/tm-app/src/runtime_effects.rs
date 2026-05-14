#![allow(unused_imports)]
#![allow(clippy::wildcard_imports)]
use crate::*;

pub(crate) async fn execute_runtime_effects(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    effects: Vec<tm_runtime::RuntimeEffect>,
    observability: &AppObservability,
) -> Result<()> {
    for effect in effects {
        execute_runtime_effect(runtime, twitch, persistent_user_id, effect, observability).await?;
    }

    Ok(())
}

pub(crate) async fn execute_runtime_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &Arc<TwitchClient>,
    persistent_user_id: &str,
    effect: tm_runtime::RuntimeEffect,
    observability: &AppObservability,
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
            )
            .await?;
        }
        tm_runtime::RuntimeEffect::EvaluatePrediction { event_id } => {
            spawn_prediction_evaluation(runtime, twitch, &event_id, observability);
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
            )
            .await;
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
) -> Result<()> {
    twitch
        .claim_bonus(channel_id, claim_id, Some(persistent_user_id))
        .await?;
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    if observability.show_claimed_bonus {
        let message = observability.bonus_claim_message(&streamer, false);
        tracing::info!("{message}");
        observability
            .send_event(DiscordEvent::BonusClaim, &message)
            .await;
    }
    Ok(())
}

pub(crate) async fn handle_claim_moment_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    channel_id: &str,
    moment_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    twitch.claim_moment(moment_id).await?;
    let Some(streamer) = runtime_streamer_by_channel_id(runtime, channel_id).await? else {
        return Ok(());
    };
    let message = format!(
        "Claimed moment for {}",
        observability.streamer_label(&streamer)
    );
    tracing::info!("{message}");
    observability
        .send_event(DiscordEvent::MomentClaim, &message)
        .await;
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
        observability.join_raid_message(&observability.streamer_name(&streamer), target_login);
    tracing::info!("{message}");
    observability
        .send_event(DiscordEvent::JoinRaid, &message)
        .await;
    Ok(())
}

pub(crate) async fn handle_community_goal_effect(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    persistent_user_id: &str,
    channel_id: &str,
    observability: &AppObservability,
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
) {
    let runtime = runtime.clone();
    let twitch = Arc::clone(twitch);
    let observability = observability.clone();
    let event_id = event_id.to_string();
    tokio::spawn(async move {
        if let Err(error) =
            evaluate_prediction_after_delay(&runtime, &twitch, &event_id, &observability).await
        {
            tracing::warn!(event_id = %event_id, %error, "prediction evaluation failed");
        }
    });
}

pub(crate) async fn handle_prediction_settled_effect(
    event_id: &str,
    streamer_username: &str,
    title: &str,
    decision_label: &str,
    result_type: &str,
    result_string: &str,
    observability: &AppObservability,
) {
    let message = format!("Prediction settled for {streamer_username}: {title} - {result_string}");
    tracing::info!(
        decision = %decision_label,
        event_id = %event_id,
        result_type = %result_type,
        "{message}"
    );
    if let Some(event) = event_from_bet_result(result_type) {
        observability.send_event(event, &message).await;
    }
}

pub(crate) async fn evaluate_prediction_after_delay(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    observability: &AppObservability,
) -> Result<()> {
    let Some(wait) = prediction_wait_for_event(runtime, event_id).await? else {
        return Ok(());
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    evaluate_prediction(runtime, twitch, event_id, observability).await
}

pub(crate) async fn prediction_wait_for_event(
    runtime: &tm_runtime::RuntimeHandle,
    event_id: &str,
) -> Result<Option<Duration>> {
    let snapshot = runtime.state_snapshot().await?;
    Ok(snapshot
        .predictions
        .get(event_id)
        .cloned()
        .map(|event| prediction_wait_duration(&event, time_now())))
}

pub(crate) async fn evaluate_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    observability: &AppObservability,
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

pub(crate) async fn place_prediction(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    decision: &PredictionDecision,
    streamer: &Streamer,
    observability: &AppObservability,
) -> Result<()> {
    match twitch
        .make_prediction(&event.event_id, &decision.outcome_id, decision.amount)
        .await
    {
        Ok(()) => {
            let deduct_stake = streamer.settings.bet.deduct_stake_on_place.unwrap_or(true);
            runtime
                .record_prediction_placed(&event.event_id, decision.clone(), deduct_stake)
                .await?;
            let message = format!(
                "Placed prediction for {}: {} on {}",
                observability.streamer_name(streamer),
                decision.amount,
                event.decision_label()
            );
            tracing::info!(event_id = %event.event_id, "{message}");
            observability
                .send_event(DiscordEvent::BetGeneral, &message)
                .await;
            Ok(())
        }
        Err(error) => {
            runtime.stop_tracking_prediction(event_id, "ERROR").await?;
            observability
                .send_event(
                    DiscordEvent::BetFailed,
                    &format!(
                        "Prediction failed for {}: {error}",
                        observability.streamer_name(streamer)
                    ),
                )
                .await;
            Err(error.into())
        }
    }
}

pub(crate) async fn send_discord_event(
    webhook: Option<&tm_observability::DiscordWebhook>,
    client: &DiscordClient,
    event: DiscordEvent,
    message: &str,
) {
    let Some(webhook) = webhook else {
        return;
    };
    let Some(request) = build_discord_request(webhook, message, Some(event)) else {
        return;
    };
    if let Err(error) = client.send(&request).await {
        tracing::warn!(event = ?event, %error, "discord notification failed");
    }
}
