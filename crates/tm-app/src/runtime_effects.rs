use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tm_domain::{PredictionDecision, Streamer};
use tm_observability::{event_from_bet_result, Event as DiscordEvent};
use tm_twitch::{TwitchClient, TwitchClientError, TwitchFailureClass};

use crate::context::{contribute_streamer_community_goals, refresh_streamer_context};
use crate::observability::AppObservability;
use crate::prediction_journal::{
    PredictionPlacementJournal, PredictionPlacementRequest, PredictionPlacementReservation,
    PredictionPlacementStatus,
};
use crate::status::HealthTracker;
use crate::streak_recovery::milestone_resolves_current_stream;
use crate::utilities::time_now;

#[derive(Clone)]
pub(crate) struct RuntimeEffectContext {
    pub(crate) runtime: tm_runtime::RuntimeHandle,
    pub(crate) twitch: Arc<TwitchClient>,
    pub(crate) persistent_user_id: String,
    pub(crate) observability: AppObservability,
    pub(crate) health: HealthTracker,
    pub(crate) prediction_journal: PredictionPlacementJournal,
    prediction_scheduler: Option<PredictionEvaluationScheduler>,
}

#[derive(Clone)]
pub(crate) struct PredictionEvaluationScheduler {
    sender: tokio::sync::mpsc::Sender<PredictionEvaluationWork>,
    stop: tokio::sync::watch::Receiver<bool>,
}

struct PredictionEvaluationWork {
    context: RuntimeEffectContext,
    event_id: String,
    enqueued_at: Instant,
}

impl PredictionEvaluationScheduler {
    pub(crate) fn start(
        stop: tokio::sync::watch::Receiver<bool>,
        observability: &AppObservability,
    ) -> Self {
        const PREDICTION_QUEUE_CAPACITY: usize = 128;
        const MAX_PENDING_PREDICTION_EVALUATIONS: usize = 64;
        let (sender, receiver) = tokio::sync::mpsc::channel(PREDICTION_QUEUE_CAPACITY);
        let worker = tokio::spawn(run_prediction_evaluation_scheduler(
            stop.clone(),
            receiver,
            MAX_PENDING_PREDICTION_EVALUATIONS,
        ));
        observability.track_task(worker);
        Self { sender, stop }
    }

    pub(crate) async fn enqueue(
        &self,
        context: RuntimeEffectContext,
        event_id: String,
        enqueued_at: Instant,
    ) -> Result<()> {
        if *self.stop.borrow() {
            return Ok(());
        }
        let work = PredictionEvaluationWork {
            context,
            event_id,
            enqueued_at,
        };
        let mut stop = self.stop.clone();
        tokio::select! {
            _changed = stop.changed() => Ok(()),
            result = self.sender.send(work) => result.map_err(|_| anyhow::anyhow!("prediction evaluation scheduler closed")),
        }
    }
}

async fn run_prediction_evaluation_scheduler(
    mut stop: tokio::sync::watch::Receiver<bool>,
    mut receiver: tokio::sync::mpsc::Receiver<PredictionEvaluationWork>,
    max_pending: usize,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(max_pending));
    let mut channel_lanes = HashMap::<String, Arc<tokio::sync::Mutex<()>>>::new();
    loop {
        if *stop.borrow() {
            break;
        }
        let Some(work) = (tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    None
                } else {
                    continue;
                }
            }
            work = receiver.recv() => work,
        }) else {
            break;
        };
        let permit = tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
                continue;
            }
            permit = permits.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let Some(channel_key) = work
            .context
            .runtime
            .active_prediction_channel_id(work.event_id.clone())
            .await
            .ok()
            .flatten()
        else {
            continue;
        };
        let channel_lane = channel_lanes
            .entry(channel_key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let PredictionEvaluationWork {
            context,
            event_id,
            enqueued_at,
        } = work;
        let observability = context.observability.clone();
        let mut child_stop = stop.clone();
        let task = tokio::spawn(async move {
            let _permit = permit;
            if !wait_for_prediction_delay_or_stop(&context, &event_id, &mut child_stop).await {
                return;
            }
            let _channel_guard = tokio::select! {
                changed = child_stop.changed() => {
                    if changed.is_err() || *child_stop.borrow() {
                        return;
                    }
                    channel_lane.lock().await
                }
                guard = channel_lane.lock() => guard,
            };
            if *child_stop.borrow() {
                return;
            }
            context
                .runtime
                .metrics_handle()
                .record_effect_queue_latency(enqueued_at.elapsed());
            tokio::select! {
                _changed = child_stop.changed() => {}
                result = evaluate_prediction(&context, &event_id) => {
                    if let Err(error) = result {
                        tracing::warn!(event_id = %event_id, %error, "prediction evaluation failed");
                    }
                }
            }
        });
        observability.track_task(task);
    }
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
            prediction_journal: PredictionPlacementJournal::memory(),
            prediction_scheduler: None,
        }
    }

    pub(crate) fn new_with_journal(
        runtime: tm_runtime::RuntimeHandle,
        twitch: Arc<TwitchClient>,
        persistent_user_id: String,
        observability: AppObservability,
        health: HealthTracker,
        prediction_journal: PredictionPlacementJournal,
        prediction_scheduler: PredictionEvaluationScheduler,
    ) -> Self {
        Self {
            runtime,
            twitch,
            persistent_user_id,
            observability,
            health,
            prediction_journal,
            prediction_scheduler: Some(prediction_scheduler),
        }
    }
}

async fn runtime_streamer_by_channel_id(
    runtime: &tm_runtime::RuntimeHandle,
    channel_id: &str,
) -> Result<Option<Streamer>> {
    let snapshot = runtime.state_snapshot().await?;
    Ok(snapshot
        .streamers
        .into_iter()
        .find(|streamer| streamer.channel_id == channel_id))
}

#[must_use]
pub(crate) fn prediction_wait_duration(
    event: &tm_domain::PredictionEvent,
    now: tm_runtime::RuntimeTime,
) -> Duration {
    let target_seconds = event
        .streamer
        .prediction_window_seconds(event.window_seconds);
    let target_duration = if target_seconds.is_finite() && target_seconds > 0.0 {
        Duration::from_secs_f64(target_seconds.min(Duration::MAX.as_secs_f64() - 2.0))
    } else {
        Duration::ZERO
    };
    let target_millis = i128::try_from(target_duration.as_millis()).unwrap_or(i128::MAX);
    let elapsed_millis = (now - event.created_at).whole_milliseconds();
    let remaining_millis = (target_millis - elapsed_millis).max(0);
    Duration::from_millis(u64::try_from(remaining_millis).unwrap_or(u64::MAX))
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

pub(crate) async fn reconcile_prediction_journal(
    runtime: &tm_runtime::RuntimeHandle,
    journal: &PredictionPlacementJournal,
    account_id: &str,
    event: &tm_domain::MinerEvent,
) -> Result<()> {
    let tm_domain::MinerEvent::PredictionUser {
        event_id,
        kind,
        result,
    } = event
    else {
        return Ok(());
    };
    let authoritative = match kind {
        tm_domain::PredictionUserKind::PredictionMade => true,
        tm_domain::PredictionUserKind::PredictionResult => result
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(|value| value.as_str())
            .is_some_and(|value| matches!(value, "WIN" | "LOSE" | "REFUND")),
    };
    if !authoritative {
        return Ok(());
    }
    let Some(channel_id) = runtime.prediction_channel_id(event_id).await? else {
        return Ok(());
    };
    journal.confirm(account_id, &channel_id, event_id)
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
            context.enqueue_prediction_evaluation(event_id).await?;
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

impl RuntimeEffectContext {
    pub(crate) async fn enqueue_prediction_evaluation(&self, event_id: String) -> Result<()> {
        self.enqueue_prediction_evaluation_at(event_id, Instant::now())
            .await
    }

    pub(crate) async fn enqueue_prediction_evaluation_at(
        &self,
        event_id: String,
        enqueued_at: Instant,
    ) -> Result<()> {
        if let Some(scheduler) = self.prediction_scheduler.as_ref() {
            scheduler.enqueue(self.clone(), event_id, enqueued_at).await
        } else {
            spawn_prediction_evaluation(self, &event_id);
            Ok(())
        }
    }
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
    reconcile_claimed_bonus_streak(runtime, twitch, channel_id).await;
    if observability.show_claimed_bonus {
        let message = observability.bonus_claim_message(&streamer, false);
        tracing::info!(operation = "claim_bonus", "{message}");
        observability.spawn_event(DiscordEvent::BonusClaim, message);
    }
    Ok(())
}

async fn reconcile_claimed_bonus_streak(
    runtime: &tm_runtime::RuntimeHandle,
    twitch: &TwitchClient,
    channel_id: &str,
) {
    let Ok(Some(streamer)) = runtime_streamer_by_channel_id(runtime, channel_id).await else {
        return;
    };
    let Some(stream_started_at) = unresolved_online_streak_started_at(&streamer) else {
        return;
    };
    let Ok(Some(milestone)) = twitch
        .fetch_watch_streak_milestone(&streamer.channel_id)
        .await
    else {
        return;
    };
    if milestone_resolves_current_stream(&milestone, stream_started_at, time_now()) {
        let _ = runtime
            .mark_watch_streak_recovered(
                streamer.channel_id.clone(),
                milestone.value,
                milestone.achievement_timestamp,
                milestone.expires_at,
            )
            .await;
    }
}

fn unresolved_online_streak_started_at(streamer: &Streamer) -> Option<tm_runtime::RuntimeTime> {
    let stream = streamer.stream.as_ref()?;
    if !streamer.is_online
        || !streamer.settings.watch_streak
        || !stream.watch_streak_missing
        || stream.broadcast_id.trim().is_empty()
    {
        return None;
    }
    stream.stream_up_at
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
        let (effects, _) = refresh_streamer_context(runtime, twitch, &streamer).await?;
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
    if !wait_for_prediction_delay(context, event_id).await? {
        return Ok(());
    }
    evaluate_prediction(context, event_id).await
}

async fn wait_for_prediction_delay(context: &RuntimeEffectContext, event_id: &str) -> Result<bool> {
    let Some((wait, event)) = prediction_wait_for_event(&context.runtime, event_id).await? else {
        return Ok(false);
    };
    tracing::info!(
        operation = "on_message",
        "{}",
        context.observability.prediction_wait_message(&event, wait)
    );
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    Ok(true)
}

async fn wait_for_prediction_delay_or_stop(
    context: &RuntimeEffectContext,
    event_id: &str,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    if *stop.borrow() {
        return false;
    }
    let Ok(Some((wait, event))) = prediction_wait_for_event(&context.runtime, event_id).await
    else {
        return false;
    };
    tracing::info!(
        operation = "on_message",
        "{}",
        context.observability.prediction_wait_message(&event, wait)
    );
    if wait.is_zero() {
        return !*stop.borrow();
    }
    tokio::select! {
        changed = stop.changed() => changed.is_ok() && !*stop.borrow(),
        () = tokio::time::sleep(wait) => !*stop.borrow(),
    }
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

#[allow(clippy::too_many_lines)] // Keep the ordered policy gates and durable reservation together.
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

    // Rehydrate a durable reservation before applying current status, balance,
    // filter, or decision policy. A restart may observe a changed event or
    // balance, but those values must not turn an already-issued mutation into
    // a fresh bet or discard the exact decision that was persisted with it.
    if let Some(reservation) = context.prediction_journal.lookup(
        &context.persistent_user_id,
        &streamer.channel_id,
        event_id,
    )? {
        restore_journaled_prediction(context, event_id, &streamer, reservation).await?;
        return Ok(());
    }

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
    // Replaying an event after a process restart is unsafe when the prior
    // response was lost. The durable journal is checked before reserving local
    // state so a recovered pending/unknown request cannot reach Twitch twice.
    if let Some(reservation) = context.prediction_journal.lookup(
        &context.persistent_user_id,
        &streamer.channel_id,
        event_id,
    )? {
        restore_journaled_prediction(context, event_id, streamer, reservation).await?;
        return Ok(());
    }

    if !reserve_prediction_placement(context, event_id, decision, streamer).await? {
        // Another evaluation effect already reserved, confirmed, or resolved
        // this event while this one was waiting on the runtime lane.
        return Ok(());
    }

    match context
        .twitch
        .make_prediction(&event.event_id, &decision.outcome_id, decision.amount)
        .await
    {
        Ok(()) => complete_prediction_placement(context, event_id, event, decision, streamer).await,
        Err(error) => fail_prediction_placement(context, event_id, decision, streamer, error).await,
    }
}

async fn restore_journaled_prediction(
    context: &RuntimeEffectContext,
    event_id: &str,
    streamer: &Streamer,
    reservation: PredictionPlacementReservation,
) -> Result<()> {
    let stored_decision = PredictionDecision {
        choice: reservation.choice,
        outcome_id: reservation.outcome_id.into(),
        amount: reservation.amount,
    };
    match reservation.status {
        PredictionPlacementStatus::Pending | PredictionPlacementStatus::Unknown => {
            context
                .runtime
                .mark_prediction_placement_unknown(event_id, stored_decision)
                .await?;
        }
        PredictionPlacementStatus::Confirmed => {
            context
                .runtime
                .restore_prediction_placement(event_id, stored_decision)
                .await?;
        }
        PredictionPlacementStatus::Rejected => {
            context
                .runtime
                .stop_tracking_prediction(event_id, "REJECTED")
                .await?;
        }
    }
    tracing::warn!(
        event_id = %event_id,
        streamer = %context.observability.streamer_name(streamer),
        "prediction placement journal suppressed replay"
    );
    Ok(())
}

async fn reserve_prediction_placement(
    context: &RuntimeEffectContext,
    event_id: &str,
    decision: &PredictionDecision,
    streamer: &Streamer,
) -> Result<bool> {
    if !context
        .runtime
        .reserve_prediction_placement(event_id, decision.clone())
        .await?
    {
        return Ok(false);
    }
    let request = PredictionPlacementRequest {
        account_id: &context.persistent_user_id,
        channel_id: &streamer.channel_id,
        event_id,
        choice: decision.choice,
        outcome_id: &decision.outcome_id,
        amount: decision.amount,
        reserved_at_unix_seconds: time_now().unix_timestamp(),
    };
    match context.prediction_journal.reserve(&request) {
        Ok(true) => Ok(true),
        Ok(false) => {
            let _ = context
                .runtime
                .release_prediction_placement_reservation(event_id)
                .await;
            Ok(false)
        }
        Err(error) => {
            let _ = context
                .runtime
                .release_prediction_placement_reservation(event_id)
                .await;
            Err(error.context("persist prediction placement reservation"))
        }
    }
}

async fn complete_prediction_placement(
    context: &RuntimeEffectContext,
    event_id: &str,
    event: &tm_domain::PredictionEvent,
    decision: &PredictionDecision,
    streamer: &Streamer,
) -> Result<()> {
    context.health.record_bet();
    let deduct_stake = streamer.settings.bet.deduct_stake_on_place.unwrap_or(true);
    context
        .runtime
        .record_prediction_placed(&event.event_id, decision.clone(), deduct_stake)
        .await?;
    context.prediction_journal.confirm(
        &context.persistent_user_id,
        &streamer.channel_id,
        event_id,
    )?;
    let message = context
        .observability
        .prediction_placed_message(event, decision);
    tracing::info!(operation = "make_predictions", event_id = %event.event_id, "{message}");
    context
        .observability
        .spawn_event(DiscordEvent::BetGeneral, message);
    Ok(())
}

async fn fail_prediction_placement(
    context: &RuntimeEffectContext,
    event_id: &str,
    decision: &PredictionDecision,
    streamer: &Streamer,
    error: TwitchClientError,
) -> Result<()> {
    let failure_class = twitch_error_class(&error);
    if prediction_placement_is_ambiguous(&error) {
        context
            .runtime
            .mark_prediction_placement_unknown(event_id, decision.clone())
            .await?;
        // Keep the reservation on disk. If this process exits now, the next
        // run must wait for Twitch's prediction notification rather than
        // issue a second transaction.
        if let Err(journal_error) = context.prediction_journal.mark_unknown(
            &context.persistent_user_id,
            &streamer.channel_id,
            event_id,
        ) {
            // The reservation was durably written before the network call.
            // mark_unknown restores its prior in-memory status on failure, so
            // the on-disk Pending record remains fail-closed; preserve the
            // original mutation error while surfacing the persistence fault.
            tracing::error!(
                event_id = %event_id,
                %journal_error,
                "failed to persist ambiguous prediction placement"
            );
        }
    } else {
        if let Some(reservation) = context.prediction_journal.lookup(
            &context.persistent_user_id,
            &streamer.channel_id,
            event_id,
        )? {
            if reservation.status == PredictionPlacementStatus::Confirmed {
                // A viewer confirmation can race the HTTP response. Preserve
                // that authoritative success in both persistence and runtime
                // state even when the mutation response later reports a typed
                // rejection.
                context
                    .runtime
                    .restore_prediction_placement(
                        event_id,
                        PredictionDecision {
                            choice: reservation.choice,
                            outcome_id: reservation.outcome_id.into(),
                            amount: reservation.amount,
                        },
                    )
                    .await?;
                tracing::warn!(
                    event_id = %event_id,
                    "ignoring late mutation rejection after authoritative prediction confirmation"
                );
                return Ok(());
            }
        }
        context
            .runtime
            .stop_tracking_prediction(event_id, "REJECTED")
            .await?;
        if let Err(journal_error) = context.prediction_journal.reject(
            &context.persistent_user_id,
            &streamer.channel_id,
            event_id,
        ) {
            // A failed terminal write leaves the original durable Pending
            // reservation in place, which suppresses replay after restart.
            tracing::error!(
                event_id = %event_id,
                %journal_error,
                "failed to clear rejected prediction placement"
            );
        }
    }
    context.observability.spawn_event(
        DiscordEvent::BetFailed,
        format!(
            "Prediction failed for {} ({failure_class})",
            context.observability.streamer_name(streamer),
        ),
    );
    Err(error.into())
}

fn prediction_placement_is_ambiguous(error: &TwitchClientError) -> bool {
    // Only Twitch's typed rejection is authoritative. A malformed/truncated
    // response, auth/rate-limit response, or transport error may all follow a
    // mutation that Twitch accepted before the response was lost.
    !matches!(error, TwitchClientError::MutationRejected { .. })
}

fn twitch_error_class(error: &TwitchClientError) -> &'static str {
    match error.failure_class() {
        TwitchFailureClass::Unauthorized => "unauthorized",
        TwitchFailureClass::RateLimited => "rate-limited",
        TwitchFailureClass::ServerError => "server-error",
        TwitchFailureClass::Timeout => "timeout",
        TwitchFailureClass::ConnectionReset => "connection-reset",
        TwitchFailureClass::PersistedQueryNotFound => "persisted-query-not-found",
        TwitchFailureClass::Other => {
            if matches!(error, TwitchClientError::MutationRejected { .. }) {
                "mutation-rejected"
            } else {
                "unknown"
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{
        evaluate_prediction, fail_prediction_placement, PredictionEvaluationScheduler,
        RuntimeEffectContext,
    };
    use crate::observability::{AppObservability, AppObservabilitySettings};
    use crate::prediction_journal::{PredictionPlacementRequest, PredictionPlacementStatus};
    use crate::status::HealthTracker;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tm_config::ConfigFile;
    use tm_domain::{OffsetDateTime, PredictionDecision, PredictionEvent, PredictionOutcome};
    use tm_twitch::{TwitchClient, TwitchClientError, TwitchEndpoints};

    fn test_observability() -> AppObservability {
        AppObservability::new(
            None,
            tm_observability::DiscordClient::new(Duration::from_secs(1)).unwrap(),
            AppObservabilitySettings::default(),
        )
    }

    fn test_twitch() -> Arc<TwitchClient> {
        Arc::new(TwitchClient::with_client_and_endpoints(
            reqwest::Client::new(),
            "token",
            "user-agent",
            TwitchEndpoints::default(),
        ))
    }

    #[tokio::test]
    async fn journaled_prediction_is_restored_before_restart_policy_checks() {
        let config = ConfigFile {
            streamers: vec![String::from("tester")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_config(&config, OffsetDateTime::UNIX_EPOCH);
        state.streamers[0].channel_id = String::from("100");
        state.streamers[0].channel_points_enabled = Some(true);
        state.streamers[0].channel_points = 1;
        state.streamers[0].settings.make_predictions = true;
        state.streamers[0].settings.bet.minimum_points = Some(10_000);
        let event_id = String::from("restart-policy-guard");
        state.predictions.insert(
            event_id.clone(),
            PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: event_id.clone(),
                title: String::from("Restart policy guard"),
                status: String::from("LOCKED"),
                created_at: OffsetDateTime::UNIX_EPOCH,
                window_seconds: 30.0,
                outcomes: vec![PredictionOutcome {
                    id: "new-choice".into(),
                    title: String::from("New choice"),
                    ..PredictionOutcome::default()
                }],
                decision: PredictionDecision::default(),
                bet_placed: false,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        let runtime = tm_runtime::spawn_runtime_state(state);
        let observability = test_observability();
        let context = RuntimeEffectContext::new(
            runtime.clone(),
            test_twitch(),
            String::from("account"),
            observability,
            HealthTracker::default(),
        );
        let original = PredictionDecision {
            choice: Some(0),
            outcome_id: "original-choice".into(),
            amount: 75,
        };
        assert!(context
            .prediction_journal
            .reserve(&PredictionPlacementRequest {
                account_id: "account",
                channel_id: "100",
                event_id: &event_id,
                choice: original.choice,
                outcome_id: &original.outcome_id,
                amount: original.amount,
                reserved_at_unix_seconds: 1,
            })
            .unwrap());

        evaluate_prediction(&context, &event_id).await.unwrap();
        let restored = runtime.state_snapshot().await.unwrap().predictions[&event_id].clone();
        assert!(restored.bet_placed);
        assert!(!restored.bet_confirmed);
        assert_eq!(restored.decision, original);
    }

    #[tokio::test]
    async fn authoritative_confirmation_survives_late_typed_rejection() {
        let config = ConfigFile {
            streamers: vec![String::from("tester")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_config(&config, OffsetDateTime::UNIX_EPOCH);
        state.streamers[0].channel_id = String::from("100");
        state.streamers[0].channel_points_enabled = Some(true);
        state.streamers[0].settings.make_predictions = true;
        let event_id = String::from("late-rejection");
        state.predictions.insert(
            event_id.clone(),
            PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: event_id.clone(),
                title: String::from("Late rejection"),
                status: String::from("ACTIVE"),
                created_at: OffsetDateTime::UNIX_EPOCH,
                window_seconds: 30.0,
                outcomes: Vec::new(),
                decision: PredictionDecision::default(),
                bet_placed: true,
                bet_confirmed: true,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        let streamer = state.streamers[0].clone();
        let runtime = tm_runtime::spawn_runtime_state(state);
        let context = RuntimeEffectContext::new(
            runtime.clone(),
            test_twitch(),
            String::from("account"),
            test_observability(),
            HealthTracker::default(),
        );
        let decision = PredictionDecision {
            choice: Some(0),
            outcome_id: "confirmed-choice".into(),
            amount: 50,
        };
        assert!(context
            .prediction_journal
            .reserve(&PredictionPlacementRequest {
                account_id: "account",
                channel_id: "100",
                event_id: &event_id,
                choice: decision.choice,
                outcome_id: &decision.outcome_id,
                amount: decision.amount,
                reserved_at_unix_seconds: 1,
            })
            .unwrap());
        context
            .prediction_journal
            .confirm("account", "100", &event_id)
            .unwrap();

        fail_prediction_placement(
            &context,
            &event_id,
            &decision,
            &streamer,
            TwitchClientError::MutationRejected {
                context: String::from("prediction"),
                detail: String::from("late rejection"),
            },
        )
        .await
        .unwrap();
        let snapshot = runtime.state_snapshot().await.unwrap();
        assert!(snapshot.predictions[&event_id].bet_confirmed);
        assert!(snapshot.predictions[&event_id].bet_placed);
        assert_eq!(
            context
                .prediction_journal
                .lookup("account", "100", &event_id)
                .unwrap()
                .unwrap()
                .status,
            PredictionPlacementStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn canceled_long_prediction_does_not_block_new_same_channel_work() {
        let config = ConfigFile {
            streamers: vec![String::from("tester")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_config(&config, OffsetDateTime::UNIX_EPOCH);
        state.streamers[0].channel_id = String::from("100");
        state.streamers[0].channel_points_enabled = Some(true);
        let mut event_streamer = state.streamers[0].clone();
        event_streamer.settings.bet.delay = Some(0.0);
        let old_id = String::from("old-prediction");
        let new_id = String::from("new-prediction");
        state.predictions.insert(
            old_id.clone(),
            PredictionEvent {
                streamer: event_streamer.clone(),
                event_id: old_id.clone(),
                title: String::from("old"),
                status: String::from("ACTIVE"),
                created_at: OffsetDateTime::now_utc(),
                window_seconds: 0.5,
                outcomes: Vec::new(),
                decision: PredictionDecision::default(),
                bet_placed: false,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        state.predictions.insert(
            new_id.clone(),
            PredictionEvent {
                streamer: event_streamer,
                event_id: new_id.clone(),
                title: String::from("new"),
                status: String::from("CLOSED"),
                created_at: OffsetDateTime::UNIX_EPOCH,
                window_seconds: 0.0,
                outcomes: Vec::new(),
                decision: PredictionDecision::default(),
                bet_placed: false,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        let runtime = tm_runtime::spawn_runtime_state(state);
        let observability = AppObservability::new(
            None,
            tm_observability::DiscordClient::new(Duration::from_secs(1)).unwrap(),
            AppObservabilitySettings::default(),
        );
        let context = RuntimeEffectContext::new(
            runtime.clone(),
            Arc::new(TwitchClient::with_client_and_endpoints(
                reqwest::Client::new(),
                "token",
                "user-agent",
                TwitchEndpoints::default(),
            )),
            String::from("account"),
            observability.clone(),
            HealthTracker::default(),
        );
        let (stop_sender, stop_receiver) = tokio::sync::watch::channel(false);
        let scheduler = PredictionEvaluationScheduler::start(stop_receiver, &observability);
        scheduler
            .enqueue(context.clone(), old_id, Instant::now())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        runtime
            .stop_tracking_prediction("old-prediction", "SKIPPED")
            .await
            .unwrap();
        scheduler
            .enqueue(context, new_id.clone(), Instant::now())
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if !runtime
                    .state_snapshot()
                    .await
                    .unwrap()
                    .predictions
                    .contains_key(&new_id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("short new prediction should run while old delay is pending");
        stop_sender.send(true).unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    #[tokio::test]
    async fn prediction_scheduler_cancels_delayed_work_on_shutdown() {
        let config = ConfigFile {
            streamers: vec![String::from("tester")],
            ..ConfigFile::default()
        };
        let mut state = tm_runtime::RuntimeState::from_config(&config, OffsetDateTime::UNIX_EPOCH);
        state.streamers[0].channel_id = String::from("100");
        state.streamers[0].channel_points_enabled = Some(true);
        state.streamers[0].settings.make_predictions = true;
        state.streamers[0].settings.bet.delay = Some(0.05);
        let event_id = String::from("shutdown-prediction");
        state.predictions.insert(
            event_id.clone(),
            PredictionEvent {
                streamer: state.streamers[0].clone(),
                event_id: event_id.clone(),
                title: String::from("Shutdown prediction"),
                status: String::from("ACTIVE"),
                created_at: OffsetDateTime::now_utc(),
                window_seconds: 0.2,
                outcomes: Vec::new(),
                decision: PredictionDecision::default(),
                bet_placed: false,
                bet_confirmed: false,
                result_type: String::new(),
                result_string: String::new(),
            },
        );
        let runtime = tm_runtime::spawn_runtime_state(state);
        let observability = test_observability();
        let context = RuntimeEffectContext::new(
            runtime.clone(),
            test_twitch(),
            String::from("account"),
            observability.clone(),
            HealthTracker::default(),
        );
        let (stop_sender, stop_receiver) = tokio::sync::watch::channel(false);
        let scheduler = PredictionEvaluationScheduler::start(stop_receiver, &observability);
        scheduler
            .enqueue(context, event_id.clone(), Instant::now())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        stop_sender.send(true).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(runtime
            .state_snapshot()
            .await
            .unwrap()
            .predictions
            .contains_key(&event_id));
    }
}
