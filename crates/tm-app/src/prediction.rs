#[must_use]
pub(crate) fn prediction_wait_duration(
    event: &tm_domain::PredictionEvent,
    now: tm_runtime::RuntimeTime,
) -> std::time::Duration {
    let target_seconds = event
        .streamer
        .prediction_window_seconds(event.window_seconds);
    let target_millis =
        i128::try_from(std::time::Duration::from_secs_f64(target_seconds).as_millis())
            .unwrap_or(i128::MAX);
    let elapsed_millis = (now - event.created_at).whole_milliseconds();
    let remaining_millis = (target_millis - elapsed_millis).max(0);
    std::time::Duration::from_millis(u64::try_from(remaining_millis).unwrap_or(u64::MAX))
}
