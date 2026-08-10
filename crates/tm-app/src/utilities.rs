use tm_twitch::generate_device_id;

pub(crate) async fn sleep_or_stop(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    duration: std::time::Duration,
) -> bool {
    tokio::select! {
        changed = stop.changed() => {
            changed.is_err() || *stop.borrow()
        }
        () = tokio::time::sleep(duration) => false,
    }
}

pub(crate) fn new_session_id() -> String {
    format!("session-{}", generate_device_id())
}

pub(crate) fn time_now() -> tm_runtime::RuntimeTime {
    tm_runtime::RuntimeTime::now_utc()
}
