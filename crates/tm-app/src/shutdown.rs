use anyhow::Result;

use crate::{BackgroundTasks, SHUTDOWN_TASK_GRACE_PERIOD};

pub(crate) async fn shutdown_background_tasks(
    stop_tx: tokio::sync::watch::Sender<bool>,
    tasks: BackgroundTasks,
) {
    let _ = stop_tx.send(true);
    let mut waits = tokio::task::JoinSet::new();
    for (name, task) in [
        ("pubsub", tasks.pubsub),
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
