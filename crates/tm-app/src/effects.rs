use anyhow::Result;
use tm_domain::Streamer;

pub(crate) async fn runtime_streamer_by_channel_id(
    runtime: &tm_runtime::RuntimeHandle,
    channel_id: &str,
) -> Result<Option<Streamer>> {
    let snapshot = runtime.state_snapshot().await?;
    Ok(snapshot
        .streamers
        .into_iter()
        .find(|streamer| streamer.channel_id == channel_id))
}
