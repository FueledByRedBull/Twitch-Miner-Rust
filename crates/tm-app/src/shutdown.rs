#![allow(unused_imports)]
#![allow(clippy::wildcard_imports)]
use crate::*;

pub(crate) async fn shutdown_background_tasks(
    stop_tx: tokio::sync::watch::Sender<bool>,
    tasks: BackgroundTasks,
) {
    let _ = stop_tx.send(true);
    if let Some(task) = tasks.pubsub {
        await_shutdown_task("pubsub", task).await;
    }
    if let Some(task) = tasks.context {
        await_shutdown_task("context", task).await;
    }
    if let Some(task) = tasks.pending_claims {
        await_shutdown_task("pending-claims", task).await;
    }
    if let Some(task) = tasks.minute {
        await_shutdown_task("minute", task).await;
    }
    if let Some(task) = tasks.drop {
        await_shutdown_task("drop", task).await;
    }
    if let Some(task) = tasks.chat {
        await_shutdown_task("chat", task).await;
    }
}

pub(crate) async fn await_shutdown_task(name: &str, mut task: tokio::task::JoinHandle<()>) {
    match tokio::time::timeout(SHUTDOWN_TASK_GRACE_PERIOD, &mut task).await {
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

pub(crate) async fn wait_for_shutdown_signal() -> Result<()> {
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
