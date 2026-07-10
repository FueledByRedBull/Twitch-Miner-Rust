use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tm_config::ConfigFile;
use tm_twitch::TwitchClient;

use crate::bootstrap::{load_and_validate_existing_session, DEFAULT_USER_AGENT};

pub(crate) async fn run_read_only_canary(
    config: &ConfigFile,
    work_dir: &Path,
    http_client: reqwest::Client,
) -> Result<()> {
    let session = load_and_validate_existing_session(config, work_dir, http_client.clone()).await?;
    let auth_token = session
        .auth_token()
        .ok_or_else(|| anyhow!("validated canary session has no auth token"))?;
    let twitch = TwitchClient::with_client_and_cookie_header(
        http_client,
        auth_token,
        DEFAULT_USER_AGENT,
        session.cookie_header_for_host("twitch.tv"),
    );

    let own_channel_id = twitch
        .fetch_channel_id(&config.username)
        .await
        .context("canary GetIDFromLogin")?;
    let target = config
        .streamers
        .first()
        .map_or(config.username.as_str(), String::as_str);
    let target_channel_id = twitch
        .fetch_channel_id(target)
        .await
        .context("canary target GetIDFromLogin")?;

    let _ = twitch
        .fetch_followers(1, "DESC")
        .await
        .context("canary ChannelFollows")?;
    let _ = twitch
        .fetch_channel_points_context(target)
        .await
        .context("canary ChannelPointsContext")?;
    let _ = twitch
        .is_stream_live(&target_channel_id)
        .await
        .context("canary WithIsStreamLiveQuery")?;
    let _ = twitch
        .fetch_stream_info(target)
        .await
        .context("canary VideoPlayerStreamInfoOverlayChannel")?;
    let _ = twitch.fetch_inventory().await.context("canary Inventory")?;
    let _ = twitch
        .fetch_viewer_drops_dashboard()
        .await
        .context("canary ViewerDropsDashboard")?;
    let _ = twitch
        .fetch_available_drop_campaign_ids(&target_channel_id)
        .await
        .context("canary DropsHighlightService_AvailableDrops")?;
    let _ = twitch
        .fetch_user_points_contribution(target)
        .await
        .context("canary UserPointsContribution")?;

    tracing::info!(
        read_operations = 9,
        own_channel_id_present = !own_channel_id.is_empty(),
        "credential-safe Twitch canary passed"
    );
    Ok(())
}
