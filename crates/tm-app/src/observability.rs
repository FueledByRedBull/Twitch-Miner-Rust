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
    pending_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
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
            pending_tasks: Arc::new(Mutex::new(Vec::new())),
            emoji,
            show_claimed_bonus,
            show_game,
        }
    }

    pub(crate) fn streamer_name(&self, streamer: &Streamer) -> String {
        let mut anonymizer = self.lock_anonymizer();
        anonymizer.streamer_name(streamer)
    }

    fn anonymized(&self) -> bool {
        self.lock_anonymizer().enabled()
    }

    pub(crate) fn streamer_label(&self, streamer: &Streamer) -> String {
        let mut anonymizer = self.lock_anonymizer();
        let username = anonymizer.streamer_name(streamer);
        let channel_id = if anonymizer.enabled() {
            "[hidden]"
        } else {
            streamer.channel_id.as_str()
        };
        let channel_points = anonymizer.pseudo_channel_points(streamer);
        format!(
            "Streamer(username={username}, channel_id={channel_id}, channel_points={channel_points})"
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
        let from = one_line(from);
        let target_login = {
            let mut anonymizer = self.lock_anonymizer();
            anonymizer.name(&one_line(target_login))
        };
        self.decorate("🎭", format!("Joining raid from {from} to {target_login}"))
    }

    pub(crate) fn prediction_label(&self, event: &tm_domain::PredictionEvent) -> String {
        let anonymize = self.lock_anonymizer().enabled();
        let event_id = if anonymize {
            "[hidden]"
        } else {
            event.event_id.as_str()
        };
        let title = if anonymize {
            "[hidden]".to_string()
        } else {
            one_line(&event.title)
        };
        format!("EventPrediction(event_id={event_id}, title=\"{title}\")")
    }

    pub(crate) fn prediction_wait_message(
        &self,
        event: &tm_domain::PredictionEvent,
        wait: Duration,
    ) -> String {
        self.decorate(
            "⏰",
            format!(
                "Place the bet after: {:.2}s for: {}",
                wait.as_secs_f64(),
                self.prediction_label(event)
            ),
        )
    }

    pub(crate) fn prediction_start_message(&self, event: &tm_domain::PredictionEvent) -> String {
        self.decorate(
            "🍀",
            format!(
                "Going to complete bet for {} owned by {}",
                self.prediction_label(event),
                self.streamer_label(&event.streamer)
            ),
        )
    }

    pub(crate) fn prediction_placed_message(
        &self,
        event: &tm_domain::PredictionEvent,
        decision: &tm_domain::PredictionDecision,
    ) -> String {
        let outcome = event.decision_outcome();
        let outcome_label = outcome.map_or_else(
            || event.decision_label(),
            |outcome| {
                format!(
                    "{} ({}), Points: {}, Users: {} ({:.2}%), Odds: {:.2} ({:.2}%)",
                    one_line(&outcome.title),
                    outcome.color.to_uppercase(),
                    format_channel_points(outcome.total_points),
                    format_channel_points(outcome.total_users),
                    outcome.percentage_users,
                    outcome.odds,
                    outcome.odds_percentage
                )
            },
        );
        self.decorate(
            "🍀",
            format!(
                "Place {} channel points on: {outcome_label}",
                format_channel_points(decision.amount)
            ),
        )
    }

    pub(crate) fn prediction_result_message(
        &self,
        event_id: &str,
        title: &str,
        result: &str,
    ) -> String {
        let anonymize = self.lock_anonymizer().enabled();
        let event_id = if anonymize { "[hidden]" } else { event_id };
        let title = if anonymize {
            "[hidden]".to_string()
        } else {
            one_line(title)
        };
        let result = if anonymize {
            "[hidden]".to_string()
        } else {
            one_line(result)
        };
        self.decorate(
            "📊",
            format!("EventPrediction(event_id={event_id}, title=\"{title}\") - Result: {result}"),
        )
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
        self.decorate("💣", format!("Start session: '{session_id}'"))
    }

    pub(crate) fn loading_streamers_message(&self, count: usize) -> String {
        self.decorate(
            "🤓",
            format!("Loading data for {count} streamers. Please wait ..."),
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
        let task = tokio::spawn(async move {
            this.send_event(event, &message).await;
        });
        self.track_task(task);
    }

    pub(crate) fn track_task(&self, task: tokio::task::JoinHandle<()>) {
        match self.pending_tasks.lock() {
            Ok(mut pending) => {
                pending.retain(|task| !task.is_finished());
                pending.push(task);
            }
            Err(poisoned) => {
                tracing::warn!("observability task list lock poisoned");
                let mut pending = poisoned.into_inner();
                pending.retain(|task| !task.is_finished());
                pending.push(task);
            }
        }
    }

    pub(crate) async fn shutdown_pending_tasks(&self) {
        let tasks = match self.pending_tasks.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(poisoned) => {
                tracing::warn!("observability task list lock poisoned");
                let mut pending = poisoned.into_inner();
                std::mem::take(&mut *pending)
            }
        };
        for task in tasks {
            await_observability_task(task, Duration::from_secs(5)).await;
        }
    }
}

async fn await_observability_task(mut task: tokio::task::JoinHandle<()>, grace: Duration) {
    match tokio::time::timeout(grace, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "observability task failed while stopping");
        }
        Err(_) => {
            tracing::warn!("observability task exceeded grace period; aborting");
            task.abort();
            let _ = task.await;
        }
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
    observability.spawn_event(
        DiscordEvent::Startup,
        format!(
            "Start session: '{}' | configured_streamers={} follower_mode={}",
            session_id, summary.configured_streamers, summary.follower_mode
        ),
    );
    if active_paths.config_path != requested_paths.config_path {
        tracing::info!(
            requested_config_path = %requested_paths.config_path.display(),
            active_config_path = %active_paths.config_path.display(),
            "using fallback user config directory"
        );
    }
}

pub(crate) fn log_session_summary(
    summary: &tm_runtime::SessionSummary,
    session_id: &str,
    log_path: Option<&std::path::Path>,
    observability: &AppObservability,
) {
    tracing::info!(
        operation = "run",
        report_line = true,
        "{}",
        observability.decorate("🛑", format!("End session '{session_id}'"))
    );
    if let Some(log_path) = log_path {
        let log_path = privacy_safe_log_path(observability.anonymized(), log_path);
        tracing::info!(
            operation = "run",
            report_line = true,
            "{}",
            observability.decorate("📄", format!("Logs file: {log_path}"))
        );
    }
    tracing::info!(
        operation = "run",
        report_line = true,
        "{}",
        observability.decorate("⌛", format!("Duration {}", summary.duration))
    );

    for prediction in &summary.predictions {
        let mut lines = vec![
            observability.decorate("📊", prediction.bet_settings_line.clone()),
            observability.decorate("📊", prediction.event_line.clone()),
            format!("\t\t{}", prediction.streamer_line),
            format!("\t\t{}", prediction.bet_line),
        ];
        lines.extend(
            prediction
                .outcome_lines
                .iter()
                .map(|line| format!("\t\t{line}")),
        );
        lines.push(format!("\t\t{}", prediction.result_line));
        tracing::info!(
            operation = "prediction_report",
            report_line = true,
            "{}",
            lines.join("\n")
        );
    }

    tracing::info!(
        operation = "run",
        report_line = true,
        "{}",
        observability.decorate("📊", summary.total_points_line.clone())
    );
    for streamer in &summary.streamers {
        let streamer_line = format!(
            "Streamer(username={}, channel_id={}, channel_points={}), {}",
            streamer.username,
            streamer.channel_id,
            streamer.current_points,
            streamer.total_points_line
        );
        let history = if streamer.history_lines.is_empty() {
            String::from("No point history")
        } else {
            streamer.history_lines.join(", ")
        };
        tracing::info!(
            operation = "session_report",
            report_line = true,
            "{}\n{}",
            observability.decorate("🤖", streamer_line),
            observability.decorate("💰", history)
        );
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
    if client.send(&request).await.is_err() {
        tracing::warn!(
            error_class = "delivery-failed",
            "failed to send discord event"
        );
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

fn one_line(value: &str) -> String {
    value.replace(['\r', '\n', '\"'], " ").trim().to_string()
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
    pub(crate) health: crate::status::HealthTracker,
}

impl ChatLogger for TracingChatLogger {
    fn activity(&mut self) {
        self.health.success("chat");
    }

    fn printf(&mut self, message: &str) {
        let message = privacy_safe_chat_message(
            self.observability.anonymized(),
            "chat message [hidden]",
            message,
        );
        tracing::info!("{message}");
    }

    fn errorf(&mut self, message: &str) {
        let message = privacy_safe_chat_message(
            self.observability.anonymized(),
            "chat error [details hidden]",
            message,
        );
        tracing::error!("{message}");
    }

    fn emoji_eventf(&mut self, _emoji: &str, event: ChatEventKind, message: &str) {
        let message = privacy_safe_chat_message(
            self.observability.anonymized(),
            "chat mention [hidden]",
            message,
        );
        tracing::info!("{message}");
        if matches!(event, ChatEventKind::Mention) {
            self.observability
                .spawn_event(DiscordEvent::ChatMention, message);
        }
    }
}

fn privacy_safe_chat_message(anonymized: bool, redacted: &str, raw: &str) -> String {
    if anonymized {
        redacted.to_string()
    } else {
        raw.to_string()
    }
}

fn privacy_safe_log_path(anonymized: bool, path: &std::path::Path) -> String {
    if anonymized {
        String::from("[hidden]")
    } else {
        path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        await_observability_task, privacy_safe_chat_message, privacy_safe_log_path,
        AppObservability, TracingChatLogger,
    };
    use tm_irc::{ChatEventKind, ChatLogger};
    use tm_observability::DiscordClient;

    fn test_observability(anonymized: bool) -> anyhow::Result<AppObservability> {
        Ok(AppObservability::new(
            None,
            DiscordClient::new(Duration::from_secs(1))?,
            anonymized,
            false,
            false,
            false,
        ))
    }

    #[tokio::test]
    async fn timed_out_observability_tasks_are_aborted() {
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        await_observability_task(task, std::time::Duration::ZERO).await;
    }

    #[tokio::test]
    async fn anonymized_chat_logger_does_not_retain_raw_callback_text() -> anyhow::Result<()> {
        let observability = test_observability(true)?;
        let mut logger = TracingChatLogger {
            observability: observability.clone(),
            health: crate::status::HealthTracker::default(),
        };

        assert!(observability.anonymized());
        logger.printf("secret-user at #secret-channel wrote: secret-message");
        logger.errorf("chat #secret-channel authentication failed");
        logger.emoji_eventf(
            "",
            ChatEventKind::Mention,
            "secret-user at #secret-channel wrote: secret-message",
        );
        assert_eq!(
            privacy_safe_chat_message(true, "chat message [hidden]", "secret-message"),
            "chat message [hidden]"
        );
        assert_eq!(
            privacy_safe_chat_message(false, "chat message [hidden]", "public-message"),
            "public-message"
        );
        assert_eq!(
            privacy_safe_log_path(true, std::path::Path::new("C:/Users/secret/miner.log")),
            "[hidden]"
        );
        observability.shutdown_pending_tasks().await;
        Ok(())
    }
}
