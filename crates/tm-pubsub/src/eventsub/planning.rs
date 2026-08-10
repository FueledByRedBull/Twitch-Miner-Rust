use serde_json::{json, Value};
use tm_domain::Streamer;

use super::{
    EventSubSetupReport, EventSubStreamerCapability, SubscriptionRequest,
    EVENTSUB_ASSUMED_SUBSCRIPTION_COST, EVENTSUB_MAX_SUBSCRIPTIONS_PER_CONNECTION,
    EVENTSUB_MAX_TOTAL_COST,
};

#[cfg(test)]
pub(super) fn subscription_requests(
    _session_id: &str,
    tracked_streamers: &[Streamer],
) -> Vec<(String, Value)> {
    subscription_plan(tracked_streamers, None)
        .0
        .into_iter()
        .map(|request| (request.subscription_type, request.condition))
        .collect()
}

#[must_use]
pub fn plan_eventsub_capacity(
    tracked_streamers: &[Streamer],
    authorized_prediction_broadcaster_id: Option<&str>,
) -> EventSubSetupReport {
    subscription_plan(tracked_streamers, authorized_prediction_broadcaster_id).1
}

pub(super) fn subscription_plan(
    tracked_streamers: &[Streamer],
    authorized_prediction_broadcaster_id: Option<&str>,
) -> (Vec<SubscriptionRequest>, EventSubSetupReport) {
    subscription_plan_with_capacity(
        tracked_streamers,
        authorized_prediction_broadcaster_id,
        EVENTSUB_MAX_TOTAL_COST,
        0,
        EVENTSUB_MAX_TOTAL_COST,
    )
}

pub(super) fn subscription_plan_with_capacity(
    tracked_streamers: &[Streamer],
    authorized_prediction_broadcaster_id: Option<&str>,
    available_cost: u32,
    current_total_cost: u32,
    max_total_cost: u32,
) -> (Vec<SubscriptionRequest>, EventSubSetupReport) {
    let mut requests = Vec::new();
    let mut capabilities =
        initial_eventsub_capabilities(tracked_streamers, authorized_prediction_broadcaster_id);

    let mut remaining_cost = available_cost;
    let mut overflow_streamers = 0_usize;

    for (streamer_index, streamer) in tracked_streamers.iter().enumerate() {
        if streamer.channel_id.trim().is_empty() {
            capabilities[streamer_index].failure_class = Some(String::from("missing-channel-id"));
            continue;
        }
        let presence_types = ["stream.online", "stream.offline"];
        let required_cost = subscription_cost(streamer, authorized_prediction_broadcaster_id)
            .saturating_mul(u32::try_from(presence_types.len()).unwrap_or(u32::MAX));
        if reserve_capacity(&mut remaining_cost, required_cost) {
            capabilities[streamer_index].presence_source = String::from("eventsub+gql-polling");
            plan_types(
                &mut requests,
                &mut capabilities[streamer_index],
                streamer_index,
                &presence_types,
                || json!({ "broadcaster_user_id": streamer.channel_id }),
            );
        } else {
            overflow_streamers += 1;
            capabilities[streamer_index]
                .skipped_subscription_types
                .extend(presence_types.into_iter().map(String::from));
            capabilities[streamer_index].failure_class = Some(String::from("capacity-overflow"));
        }
    }

    for (streamer_index, streamer) in tracked_streamers.iter().enumerate() {
        if streamer.channel_id.trim().is_empty() || !streamer.settings.follow_raid {
            continue;
        }
        let raid_types = ["channel.raid"];
        let required_cost = subscription_cost(streamer, authorized_prediction_broadcaster_id);
        if reserve_capacity(&mut remaining_cost, required_cost) {
            plan_types(
                &mut requests,
                &mut capabilities[streamer_index],
                streamer_index,
                &raid_types,
                || json!({ "from_broadcaster_user_id": streamer.channel_id }),
            );
        } else {
            capabilities[streamer_index]
                .skipped_subscription_types
                .push(String::from("channel.raid"));
            capabilities[streamer_index]
                .failure_class
                .get_or_insert_with(|| String::from("capacity-overflow"));
        }
    }

    for (streamer_index, streamer) in tracked_streamers.iter().enumerate() {
        if streamer.channel_id.trim().is_empty()
            || !uses_eventsub_predictions(streamer, authorized_prediction_broadcaster_id)
        {
            continue;
        }
        let prediction_types = [
            "channel.prediction.begin",
            "channel.prediction.progress",
            "channel.prediction.lock",
            "channel.prediction.end",
        ];
        let required_cost = subscription_cost(streamer, authorized_prediction_broadcaster_id)
            .saturating_mul(u32::try_from(prediction_types.len()).unwrap_or(u32::MAX));
        if reserve_capacity(&mut remaining_cost, required_cost) {
            plan_types(
                &mut requests,
                &mut capabilities[streamer_index],
                streamer_index,
                &prediction_types,
                || json!({ "broadcaster_user_id": streamer.channel_id }),
            );
        } else {
            capabilities[streamer_index]
                .skipped_subscription_types
                .extend(prediction_types.into_iter().map(String::from));
            capabilities[streamer_index]
                .failure_class
                .get_or_insert_with(|| String::from("capacity-overflow"));
        }
    }

    let report = EventSubSetupReport {
        planned_subscriptions: requests.len(),
        active_subscriptions: 0,
        failed_subscriptions: 0,
        overflow_streamers,
        total_cost: current_total_cost,
        max_total_cost,
        verified: false,
        capabilities,
    };
    (requests, report)
}

fn initial_eventsub_capabilities(
    tracked_streamers: &[Streamer],
    authorized_prediction_broadcaster_id: Option<&str>,
) -> Vec<EventSubStreamerCapability> {
    tracked_streamers
        .iter()
        .enumerate()
        .map(|(streamer_index, streamer)| EventSubStreamerCapability {
            streamer_index,
            presence_source: String::from("gql-polling"),
            prediction_source: if !streamer.settings.make_predictions {
                String::from("disabled")
            } else if uses_eventsub_predictions(streamer, authorized_prediction_broadcaster_id) {
                String::from("eventsub-broadcaster")
            } else {
                String::from("pubsub-compatibility")
            },
            raid_source: if streamer.settings.follow_raid {
                String::from("eventsub-observation+pubsub-compatibility")
            } else {
                String::from("disabled")
            },
            planned_subscription_types: Vec::new(),
            active_subscription_types: Vec::new(),
            skipped_subscription_types: Vec::new(),
            failure_class: None,
        })
        .collect()
}

fn uses_eventsub_predictions(
    streamer: &Streamer,
    authorized_prediction_broadcaster_id: Option<&str>,
) -> bool {
    authorized_prediction_broadcaster_id.is_some_and(|broadcaster_id| {
        !broadcaster_id.trim().is_empty() && streamer.channel_id.trim() == broadcaster_id.trim()
    })
}

fn subscription_cost(streamer: &Streamer, authorized_broadcaster_id: Option<&str>) -> u32 {
    if authorized_broadcaster_id.is_some_and(|authorized| {
        !authorized.trim().is_empty() && authorized.trim() == streamer.channel_id.trim()
    }) {
        0
    } else {
        EVENTSUB_ASSUMED_SUBSCRIPTION_COST
    }
}

fn reserve_capacity(remaining_cost: &mut u32, required_cost: u32) -> bool {
    if required_cost > *remaining_cost {
        return false;
    }
    *remaining_cost -= required_cost;
    true
}

fn plan_types(
    requests: &mut Vec<SubscriptionRequest>,
    capability: &mut EventSubStreamerCapability,
    streamer_index: usize,
    subscription_types: &[&str],
    condition: impl Fn() -> Value,
) {
    for subscription_type in subscription_types {
        if requests.len() == EVENTSUB_MAX_SUBSCRIPTIONS_PER_CONNECTION {
            capability
                .skipped_subscription_types
                .push((*subscription_type).to_string());
            capability.failure_class = Some(String::from("subscription-count-overflow"));
            continue;
        }
        capability
            .planned_subscription_types
            .push((*subscription_type).to_string());
        requests.push(SubscriptionRequest {
            streamer_index,
            subscription_type: (*subscription_type).to_string(),
            condition: condition(),
        });
    }
}
