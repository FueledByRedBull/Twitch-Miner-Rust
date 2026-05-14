#![allow(unused_imports)]
#![allow(clippy::wildcard_imports)]
use crate::*;

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

pub(crate) fn clear_console() {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "cls"]);
        command
    } else {
        Command::new("clear")
    };
    let _ = command.status();
}

pub(crate) fn new_session_id() -> String {
    format!("session-{}", generate_device_id())
}

pub(crate) fn time_now() -> tm_runtime::RuntimeTime {
    tm_runtime::RuntimeTime::now_utc()
}

pub(crate) fn set_console_title(title: &str) {
    if !cfg!(windows) || title.trim().is_empty() {
        return;
    }
    let _ = Command::new("cmd")
        .args(["/C", &format!("title {title}")])
        .status();
}
