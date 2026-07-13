use anyhow::{anyhow, Result};

use crate::status::StatusReporter;
use crate::{BackgroundTasks, SHUTDOWN_TASK_GRACE_PERIOD};

pub(crate) async fn shutdown_background_tasks(
    stop_tx: tokio::sync::watch::Sender<bool>,
    tasks: BackgroundTasks,
) {
    let _ = stop_tx.send(true);
    let mut waits = tokio::task::JoinSet::new();
    for (name, task) in [
        ("eventsub", tasks.eventsub),
        ("pubsub", tasks.pubsub),
        ("presence-poll", tasks.presence_poll),
        ("context", tasks.context),
        ("pending-claims", tasks.pending_claims),
        ("minute", tasks.minute),
        ("drop", tasks.drop),
        ("chat", tasks.chat),
    ] {
        if let Some(task) = task {
            waits.spawn(async move {
                await_shutdown_task(name, task).await;
            });
        }
    }
    while waits.join_next().await.is_some() {}
}

pub(crate) async fn await_shutdown_task(name: &str, mut task: tokio::task::JoinHandle<()>) {
    await_shutdown_task_with_grace(name, &mut task, SHUTDOWN_TASK_GRACE_PERIOD).await;
}

async fn await_shutdown_task_with_grace(
    name: &str,
    task: &mut tokio::task::JoinHandle<()>,
    grace: std::time::Duration,
) {
    match tokio::time::timeout(grace, &mut *task).await {
        Ok(Ok(())) => {
            tracing::info!(task = name, "shutdown task stopped");
        }
        Ok(Err(error)) => {
            tracing::warn!(task = name, %error, "shutdown task failed while stopping");
        }
        Err(_) => {
            tracing::warn!(
                task = name,
                timeout_seconds = SHUTDOWN_TASK_GRACE_PERIOD.as_secs(),
                "shutdown task exceeded grace period; aborting"
            );
            task.abort();
            let _ = task.await;
        }
    }
}

pub(crate) async fn wait_for_shutdown_or_task_failure(
    tasks: &BackgroundTasks,
    status: &StatusReporter,
) -> Result<()> {
    let signal = wait_for_shutdown_signal();
    tokio::pin!(signal);
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            result = &mut signal => return result,
            _ = heartbeat.tick() => {
                let finished = tasks.unexpectedly_finished();
                if !finished.is_empty() {
                    return Err(anyhow!("background task exited unexpectedly: {}", finished.join(", ")));
                }
                status.supervision_heartbeat()?;
            }
        }
    }
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use super::{await_shutdown_task, await_shutdown_task_with_grace};

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn shutdown_waiter_handles_completed_and_aborted_tasks() {
        await_shutdown_task("completed", tokio::spawn(async {})).await;

        let aborted = tokio::spawn(async { std::future::pending::<()>().await });
        aborted.abort();
        await_shutdown_task("aborted", aborted).await;
    }

    #[tokio::test]
    async fn shutdown_waiter_aborts_a_stuck_active_operation() {
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let task_started = Arc::clone(&started);
        let task_dropped = Arc::clone(&dropped);
        let mut task = tokio::spawn(async move {
            let _probe = DropProbe(task_dropped);
            task_started.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        });
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        await_shutdown_task_with_grace("active-operation", &mut task, std::time::Duration::ZERO)
            .await;
        assert!(dropped.load(Ordering::SeqCst));
    }
}
