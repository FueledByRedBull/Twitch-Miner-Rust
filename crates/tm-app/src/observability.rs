use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::Result;
use tm_config::{AppPaths, ConfigFile};
use tm_domain::{format_channel_points, format_drop_progress, progress_percent, Streamer};
use tm_irc::{ChatEventKind, ChatLogger};
use tm_observability::{
    build_discord_request, new_discord_webhook, Anonymizer, DiscordClient, DiscordSettings,
    Event as DiscordEvent,
};
use tm_twitch::InventoryDrop;

#[derive(Clone)]
pub(crate) struct AppObservability {
    pub(crate) discord: Option<tm_observability::DiscordWebhook>,
    pub(crate) discord_client: DiscordClient,
    anonymizer: Arc<Mutex<Anonymizer>>,
    pub(crate) emoji: bool,
    pub(crate) show_claimed_bonus: bool,
    show_game: bool,
}

impl AppObservability {
    #[allow(clippy::fn_params_excessive_bools)]
    pub(crate) fn new(
        discord: Option<tm_observability::DiscordWebhook>,
        discord_client: DiscordClient,
        anonymize_logs: bool,
        emoji: bool,
        show_claimed_bonus: bool,
        show_game: bool,
    ) -> Self {
        Self {
            discord,
            discord_client,
            anonymizer: Arc::new(Mutex::new(Anonymizer::new(anonymize_logs))),
            emoji,
            show_claimed_bonus,
            show_game,
        }
    }

    pub(crate) fn streamer_name(&self, streamer: &Streamer) -> String {
        let mut anonymizer = self.lock_anonymizer();
        anonymizer.streamer_name(streamer)
    }

    pub(crate) fn channel_points(&self, streamer: &Streamer) -> String {
        let mut anonymizer = self.lock_anonymizer();
        format_channel_points(anonymizer.pseudo_channel_points(streamer))
    }

    pub(crate) fn streamer_label(&self, streamer: &Streamer) -> String {
        format!(
            "{} ({} points)",
            self.streamer_name(streamer),
            self.channel_points(streamer)
        )
    }

    fn decorate(&self, emoji: &str, message: String) -> String {
        if self.emoji {
            format!("{emoji} {message}")
        } else {
            message
        }
    }

    fn lock_anonymizer(&self) -> MutexGuard<'_, Anonymizer> {
        match self.anonymizer.lock() {
            Ok(anonymizer) => anonymizer,
            Err(poisoned) => {
                tracing::warn!("anonymizer lock poisoned");
                poisoned.into_inner()
            }
        }
    }

    pub(crate) fn online_message(&self, streamer: &Streamer) -> String {
        let mut message = format!("{} is Online!", self.streamer_label(streamer));
        if self.show_game {
            if let Some(game_name) = streamer_game_name(streamer) {
                message.push_str(" | Playing: ");
                message.push_str(&game_name);
            }
        }
        self.decorate("🥳", message)
    }

    pub(crate) fn offline_message(&self, streamer: &Streamer) -> String {
        self.decorate(
            "😴",
            format!("{} is Offline!", self.streamer_label(streamer)),
        )
    }

    pub(crate) fn game_change_message(
        &self,
        streamer: &Streamer,
        previous: &str,
        current: &str,
    ) -> Option<String> {
        if !self.show_game {
            return None;
        }
        let previous = previous.trim();
        let current = current.trim();
        if previous.is_empty() || current.is_empty() || previous.eq_ignore_ascii_case(current) {
            return None;
        }
        Some(self.decorate(
            "🎮",
            format!("{} now playing: {}!", self.streamer_name(streamer), current),
        ))
    }

    pub(crate) fn points_earned_message(
        &self,
        streamer: &Streamer,
        earned: i64,
        reason: &str,
    ) -> String {
        let reason = reason.trim().to_uppercase();
        let mut message = format!(
            "{} {} - Reason: {}",
            signed_points(earned),
            self.streamer_label(streamer),
            reason
        );
        if self.show_game && reason == "WATCH" {
            if let Some(game_name) = streamer_game_name(streamer) {
                message.push_str(" | Game: ");
                message.push_str(&game_name);
            }
        }
        self.decorate("🚀", message)
    }

    pub(crate) fn join_raid_message(&self, from: &str, target_login: &str) -> String {
        self.decorate("🎭", format!("Joining raid from {from} to {target_login}"))
    }

    pub(crate) fn chat_presence_message(&self, join: bool, streamer_name: &str) -> String {
        let action = if join { "Join" } else { "Leave" };
        self.decorate("💬", format!("{action} IRC Chat: {streamer_name}"))
    }

    pub(crate) fn bonus_claim_message(&self, streamer: &Streamer, startup: bool) -> String {
        let prefix = if startup {
            "Claimed startup bonus"
        } else {
            "Claimed bonus"
        };
        self.decorate(
            "🎁",
            format!("{prefix} → {}", self.streamer_label(streamer)),
        )
    }

    pub(crate) fn drop_claim_message(&self, mode: &str, drop: &InventoryDrop) -> String {
        self.decorate(
            "🎁",
            format!(
                "Claimed drop → {} | Campaign: {} | Progress: {} ({}%) | Mode: {}",
                drop.reward_name,
                drop.campaign_name,
                format_drop_progress(drop.current_minutes_watched, drop.required_minutes_watched),
                progress_percent(drop.current_minutes_watched, drop.required_minutes_watched),
                mode.to_uppercase()
            ),
        )
    }

    pub(crate) fn minute_watcher_resume_message(
        &self,
        gap: Duration,
        active_streamers: usize,
    ) -> String {
        self.decorate(
            "⏸",
            format!(
                "Minute watcher resumed after {} without activity; system sleep or OS suspension is likely ({} active streamer(s))",
                format_resume_gap(gap),
                active_streamers
            ),
        )
    }

    pub(crate) fn start_session_message(&self, session_id: &str) -> String {
        self.decorate("🟢", format!("Start session: '{session_id}'"))
    }

    pub(crate) fn loading_streamers_message(&self, count: usize) -> String {
        self.decorate(
            "⏳",
            format!("Loading data for {count} streamer(s). Please wait..."),
        )
    }

    pub(crate) fn loaded_streamers_message(&self, count: usize, elapsed: Duration) -> String {
        self.decorate(
            "✅",
            format!(
                "{count} Streamer loaded! ({:.1} seconds)",
                elapsed.as_secs_f64()
            ),
        )
    }

    pub(crate) async fn send_event(&self, event: DiscordEvent, message: &str) {
        send_discord_event(self.discord.as_ref(), &self.discord_client, event, message).await;
    }

    pub(crate) fn spawn_event(&self, event: DiscordEvent, message: String) {
        let this = self.clone();
        tokio::spawn(async move {
            this.send_event(event, &message).await;
        });
    }
}

pub(crate) fn build_observability(config: &ConfigFile) -> Result<AppObservability> {
    let discord = new_discord_webhook(&DiscordSettings {
        webhook_api: config.discord.webhook_api.clone(),
        events: config.discord.events.clone(),
    });
    let discord_client = DiscordClient::new(Duration::from_secs(15))?;
    Ok(AppObservability::new(
        discord,
        discord_client,
        config.privacy.anonymize_logs,
        config.emojis,
        config.show_claimed_bonus_msg,
        config.show_game,
    ))
}

pub(crate) async fn log_startup(
    requested_paths: &AppPaths,
    active_paths: &AppPaths,
    summary: &tm_runtime::RuntimeSummary,
    session_id: &str,
    observability: &AppObservability,
) {
    tracing::debug!(
        session_id = %session_id,
        work_dir = %active_paths.work_dir.display(),
        config_path = %active_paths.config_path.display(),
        configured_streamers = summary.configured_streamers,
        follower_mode = summary.follower_mode,
        "bootstrap complete"
    );
    send_discord_event(
        observability.discord.as_ref(),
        &observability.discord_client,
        DiscordEvent::Startup,
        &format!(
            "Start session: '{}' | configured_streamers={} follower_mode={}",
            session_id, summary.configured_streamers, summary.follower_mode
        ),
    )
    .await;
    if active_paths.config_path != requested_paths.config_path {
        tracing::info!(
            requested_config_path = %requested_paths.config_path.display(),
            active_config_path = %active_paths.config_path.display(),
            "using fallback user config directory"
        );
    }
}

pub(crate) fn log_session_summary(summary: &tm_runtime::SessionSummary) {
    tracing::info!("{}", summary.duration);
    tracing::info!("{}", summary.total_points_line);
    for streamer in &summary.streamers {
        tracing::info!(
            "{:<width$} {}",
            streamer.username,
            streamer.current_points,
            width = crate::SESSION_SUMMARY_INDENT
        );
        tracing::info!("{}", streamer.total_points_line);
        for line in &streamer.history_lines {
            tracing::info!("{line}");
        }
    }
}

pub(crate) async fn send_discord_event(
    discord: Option<&tm_observability::DiscordWebhook>,
    client: &DiscordClient,
    event: DiscordEvent,
    message: &str,
) {
    let Some(discord) = discord else {
        return;
    };
    let Some(request) = build_discord_request(discord, message, Some(event)) else {
        return;
    };
    if let Err(error) = client.send(&request).await {
        tracing::warn!(%error, "failed to send discord event");
    }
}

pub(crate) fn streamer_game_name(streamer: &Streamer) -> Option<String> {
    streamer
        .stream
        .as_ref()
        .and_then(|stream| stream.game.as_ref())
        .and_then(|game| {
            game.display_name
                .as_deref()
                .or(game.name.as_deref())
                .map(str::trim)
        })
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

pub(crate) fn signed_points(amount: i64) -> String {
    let sign = if amount >= 0 { "+" } else { "-" };
    format!("{sign}{} →", format_channel_points(amount.abs()))
}

pub(crate) fn format_resume_gap(gap: Duration) -> String {
    let seconds = gap.as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {secs}s")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

pub(crate) struct TracingChatLogger {
    pub(crate) observability: AppObservability,
}

impl ChatLogger for TracingChatLogger {
    fn printf(&mut self, message: &str) {
        tracing::info!("{message}");
    }

    fn errorf(&mut self, message: &str) {
        tracing::error!("{message}");
    }

    fn emoji_eventf(&mut self, _emoji: &str, event: ChatEventKind, message: &str) {
        tracing::info!("{message}");
        if matches!(event, ChatEventKind::Mention) {
            self.observability
                .spawn_event(DiscordEvent::ChatMention, message.to_string());
        }
    }
}
