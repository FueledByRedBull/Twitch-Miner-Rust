//! Privacy-aware logs, reports, and optional Discord delivery.
//!
//! Privacy mode must hide account/channel/event identifiers and balances across
//! every sink. Observability failures must not mutate mining state, and log
//! rotation must retain private file permissions.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tm_domain::Streamer;
use tracing::field::{Field, Visit};
use tracing::{Event as TraceEvent, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

pub const DISCORD_USERNAME: &str = "Twitch Channel Points Miner";
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_LOG_ARCHIVES: usize = 5;
const MAX_LOG_ARCHIVE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
// Logger flags are independent user-facing switches and serialize as a stable flat policy.
#[allow(clippy::struct_excessive_bools)]
pub struct LoggerSettings {
    pub save: bool,
    pub emoji: bool,
    pub smart: bool,
    pub show_seconds: bool,
    pub console_username: bool,
    pub show_claimed_bonus: bool,
    pub debug: bool,
    pub debug_deep: bool,
    pub anonymize_logs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracingInitOptions {
    pub settings: LoggerSettings,
    pub base_dir: PathBuf,
    pub username: String,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Event {
    Startup,
    Shutdown,
    StreamerOnline,
    StreamerOffline,
    GainForRaid,
    GainForClaim,
    GainForWatch,
    GainForWatchStreak,
    BetWin,
    BetLose,
    BetRefund,
    BetFilters,
    BetGeneral,
    BetFailed,
    BetStart,
    BonusClaim,
    MomentClaim,
    JoinRaid,
    DropClaim,
    DropStatus,
    ChatMention,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscordSettings {
    pub webhook_api: String,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordWebhook {
    pub webhook_api: String,
    pub events: Vec<Event>,
}

#[derive(Debug, Error)]
pub enum ObservabilityError {
    #[error("logger init failed: {0}")]
    LoggerInit(#[from] tracing_subscriber::util::TryInitError),
    #[error("log file init failed: {0}")]
    LogFileIo(#[from] io::Error),
    #[error("discord http error: {0}")]
    DiscordHttp(#[from] reqwest::Error),
    #[error("discord delivery failed with status {0}")]
    DiscordStatus(reqwest::StatusCode),
}

#[derive(Debug, Clone)]
pub struct DiscordClient {
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct Anonymizer {
    enabled: bool,
    names: HashMap<String, String>,
    points: HashMap<String, PointsState>,
    next_streamer_index: usize,
    initial_points_min: i64,
    initial_points_max: i64,
}

#[derive(Debug, Clone, Copy)]
struct PointsState {
    initialized: bool,
    last_real: i64,
    pseudo: i64,
}

impl Anonymizer {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            names: HashMap::new(),
            points: HashMap::new(),
            next_streamer_index: 1,
            initial_points_min: 100,
            initial_points_max: 1_000,
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn name(&mut self, raw: &str) -> String {
        if !self.enabled {
            return raw.to_string();
        }
        let key = raw.trim().to_lowercase();
        if key.is_empty() {
            return String::new();
        }
        if let Some(existing) = self.names.get(&key) {
            return existing.clone();
        }
        let alias = format!("Streamer{}", self.next_streamer_index);
        self.next_streamer_index += 1;
        self.names.insert(key, alias.clone());
        alias
    }

    pub fn streamer_name(&mut self, streamer: &Streamer) -> String {
        if !streamer.username.trim().is_empty() {
            self.name(&streamer.username)
        } else if !streamer.channel_id.trim().is_empty() {
            self.name(&format!("id:{}", streamer.channel_id))
        } else {
            String::new()
        }
    }

    pub fn pseudo_channel_points(&mut self, streamer: &Streamer) -> i64 {
        if !self.enabled {
            return streamer.channel_points;
        }
        let key = if streamer.channel_id.trim().is_empty() {
            streamer.username.trim().to_lowercase()
        } else {
            format!("id:{}", streamer.channel_id.trim())
        };
        if key.is_empty() {
            return streamer.channel_points;
        }

        let state = self.points.entry(key).or_insert(PointsState {
            initialized: false,
            last_real: 0,
            pseudo: 0,
        });
        if !state.initialized {
            state.initialized = true;
            state.last_real = streamer.channel_points;
            state.pseudo = random_initial_points(self.initial_points_min, self.initial_points_max);
            return state.pseudo;
        }

        let delta = streamer.channel_points - state.last_real;
        state.pseudo += delta;
        state.last_real = streamer.channel_points;
        state.pseudo
    }
}

#[must_use]
pub fn normalize_event_name(raw: &str) -> Option<Event> {
    match raw.trim().to_uppercase().as_str() {
        "STARTUP" => Some(Event::Startup),
        "SHUTDOWN" => Some(Event::Shutdown),
        "STREAMER_ONLINE" => Some(Event::StreamerOnline),
        "STREAMER_OFFLINE" => Some(Event::StreamerOffline),
        "GAIN_FOR_RAID" => Some(Event::GainForRaid),
        "GAIN_FOR_CLAIM" => Some(Event::GainForClaim),
        "GAIN_FOR_WATCH" => Some(Event::GainForWatch),
        "GAIN_FOR_WATCH_STREAK" => Some(Event::GainForWatchStreak),
        "BET_WIN" => Some(Event::BetWin),
        "BET_LOSE" => Some(Event::BetLose),
        "BET_REFUND" => Some(Event::BetRefund),
        "BET_FILTERS" => Some(Event::BetFilters),
        "BET_GENERAL" => Some(Event::BetGeneral),
        "BET_FAILED" => Some(Event::BetFailed),
        "BET_START" => Some(Event::BetStart),
        "BONUS_CLAIM" => Some(Event::BonusClaim),
        "MOMENT_CLAIM" => Some(Event::MomentClaim),
        "JOIN_RAID" => Some(Event::JoinRaid),
        "DROP_CLAIM" => Some(Event::DropClaim),
        "DROP_STATUS" => Some(Event::DropStatus),
        "CHAT_MENTION" => Some(Event::ChatMention),
        _ => None,
    }
}

#[must_use]
pub fn event_from_gain_reason(reason: &str) -> Option<Event> {
    match reason.trim().to_uppercase().as_str() {
        "WATCH" => Some(Event::GainForWatch),
        "WATCH_STREAK" => Some(Event::GainForWatchStreak),
        "CLAIM" => Some(Event::GainForClaim),
        "RAID" => Some(Event::GainForRaid),
        _ => None,
    }
}

#[must_use]
pub fn event_from_bet_result(result: &str) -> Option<Event> {
    match result.trim().to_uppercase().as_str() {
        "WIN" => Some(Event::BetWin),
        "LOSE" => Some(Event::BetLose),
        "REFUND" => Some(Event::BetRefund),
        _ => None,
    }
}

#[must_use]
pub fn new_discord_webhook(settings: &DiscordSettings) -> Option<DiscordWebhook> {
    let webhook_api = settings.webhook_api.trim();
    if webhook_api.is_empty() {
        return None;
    }
    let events = settings
        .events
        .iter()
        .filter_map(|event| normalize_event_name(event))
        .collect();
    Some(DiscordWebhook {
        webhook_api: webhook_api.to_string(),
        events,
    })
}

#[must_use]
pub fn should_send_discord_event(webhook: &DiscordWebhook, event: Option<Event>) -> bool {
    let Some(event) = event else {
        return false;
    };
    webhook.events.is_empty() || webhook.events.contains(&event)
}

#[must_use]
pub fn discord_message(
    webhook: &DiscordWebhook,
    message: &str,
    event: Option<Event>,
) -> Option<String> {
    if !should_send_discord_event(webhook, event) {
        return None;
    }
    let clean_message = strip_ansi(message).trim().to_string();
    if clean_message.is_empty() {
        return None;
    }
    Some(clean_message)
}

impl DiscordClient {
    pub fn new(timeout: std::time::Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder().timeout(timeout).build()?,
        })
    }

    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub async fn send(&self, webhook_api: &str, message: &str) -> Result<(), ObservabilityError> {
        let response = self
            .client
            .post(webhook_api)
            .form(&[("content", message), ("username", DISCORD_USERNAME)])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ObservabilityError::DiscordStatus(response.status()));
        }
        Ok(())
    }
}

#[must_use]
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect()
}

#[must_use]
pub fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            let _ = chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        output.push(ch);
    }
    output
}

#[must_use]
pub fn deep_debug_enabled(settings: &LoggerSettings) -> bool {
    settings.debug && settings.debug_deep && !settings.anonymize_logs
}

pub fn log_file_path(base_dir: impl AsRef<Path>, username: &str, anonymize: bool) -> PathBuf {
    let file_name = if anonymize || username.trim().is_empty() {
        String::from("miner.log")
    } else {
        format!("{}.log", sanitize_filename(username.trim()))
    };
    base_dir.as_ref().join("log").join(file_name)
}

pub fn init_tracing(options: &TracingInitOptions) -> Result<(), ObservabilityError> {
    let filter = if deep_debug_enabled(&options.settings) {
        EnvFilter::new("trace")
    } else if options.settings.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };

    let event_format = GoStyleEventFormat {
        show_seconds: options.settings.show_seconds,
        timezone: options
            .timezone
            .as_deref()
            .and_then(|value| value.parse().ok()),
        console_username: (options.settings.console_username && !options.settings.anonymize_logs)
            .then(|| options.username.trim().to_string())
            .filter(|value| !value.is_empty()),
        include_fields: !options.settings.anonymize_logs,
    };
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .event_format(event_format.clone())
        .with_filter(filter.clone());

    let registry = tracing_subscriber::registry().with(console_layer);

    if options.settings.save {
        let path = log_file_path(
            &options.base_dir,
            &options.username,
            options.settings.anonymize_logs,
        );
        let writer = SharedFileWriter::new(path)?;
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .event_format(event_format)
            .with_writer(writer)
            .with_filter(filter);
        registry.with(file_layer).try_init()?;
    } else {
        registry.try_init()?;
    }

    Ok(())
}

pub fn open_log_file(
    base_dir: impl AsRef<Path>,
    username: &str,
    anonymize: bool,
) -> io::Result<File> {
    let path = log_file_path(base_dir, username, anonymize);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    open_private_log_file(&path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoStyleEventFormat {
    show_seconds: bool,
    timezone: Option<Tz>,
    console_username: Option<String>,
    include_fields: bool,
}

impl<S, N> FormatEvent<S, N> for GoStyleEventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &TraceEvent<'_>,
    ) -> fmt::Result {
        let mut visitor = MessageOnlyVisitor::new(self.include_fields);
        event.record(&mut visitor);
        let report_line = visitor.report_line();
        let operation = visitor.operation().map_or_else(
            || {
                event
                    .metadata()
                    .target()
                    .rsplit("::")
                    .next()
                    .unwrap_or("run")
                    .to_string()
            },
            str::to_string,
        );
        let body = visitor.render();
        let username_prefix = self
            .console_username
            .as_deref()
            .map_or(String::new(), |username| format!(" [{username}]"));
        let timestamp = current_log_timestamp(self.show_seconds, self.timezone);
        let line = if report_line {
            format_report_line(&timestamp, &body)
        } else {
            format_log_line(
                &timestamp,
                format_level(*event.metadata().level()),
                &operation,
                &username_prefix,
                &body,
            )
        };
        writeln!(writer, "{line}")
    }
}

struct MessageOnlyVisitor {
    message: Option<String>,
    operation: Option<String>,
    report_line: bool,
    fields: Vec<String>,
    include_fields: bool,
}

impl MessageOnlyVisitor {
    fn new(include_fields: bool) -> Self {
        Self {
            message: None,
            operation: None,
            report_line: false,
            fields: Vec::new(),
            include_fields,
        }
    }

    fn record_value(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(trim_matching_quotes(value));
            return;
        }
        if field.name() == "operation" {
            self.operation = Some(trim_matching_quotes(value));
            return;
        }
        if self.include_fields {
            self.fields.push(format!("{}={}", field.name(), value));
        }
    }

    fn operation(&self) -> Option<&str> {
        self.operation
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    }

    const fn report_line(&self) -> bool {
        self.report_line
    }

    fn render(self) -> String {
        match (self.message, self.fields.is_empty()) {
            (Some(message), true) => message,
            (Some(message), false) => format!("{message} | {}", self.fields.join(" ")),
            (None, _) => self.fields.join(" "),
        }
    }
}

impl Visit for MessageOnlyVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "report_line" {
            self.report_line = value;
        } else {
            self.record_value(field, &value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record_value(field, &format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, value);
    }
}

fn trim_matching_quotes(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|trimmed| trimmed.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

fn format_level(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

fn format_log_line(
    timestamp: &str,
    level: &str,
    operation: &str,
    username_prefix: &str,
    body: &str,
) -> String {
    format!("{timestamp} - {level} - [{operation}]{username_prefix}: {body}")
}

fn format_report_line(timestamp: &str, body: &str) -> String {
    format!("{timestamp} - {body}")
}

#[derive(Clone)]
struct SharedFileWriter {
    file: Arc<Mutex<RotatingFile>>,
}

impl SharedFileWriter {
    fn new(path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            file: Arc::new(Mutex::new(RotatingFile::new(
                path,
                MAX_LOG_BYTES,
                MAX_LOG_ARCHIVES,
            )?)),
        })
    }
}

impl<'a> MakeWriter<'a> for SharedFileWriter {
    type Writer = LockedFileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LockedFileWriter {
            file: Arc::clone(&self.file),
        }
    }
}

struct LockedFileWriter {
    file: Arc<Mutex<RotatingFile>>,
}

impl Write for LockedFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.flush()
    }
}

struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max_bytes: u64,
    max_archives: usize,
}

impl RotatingFile {
    fn new(path: PathBuf, max_bytes: u64, max_archives: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        prune_old_log_archives(&path, MAX_LOG_ARCHIVE_AGE)?;
        let file = open_private_log_file(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            size,
            max_bytes,
            max_archives,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.take();
        for index in (1..=self.max_archives).rev() {
            let source = archive_path(&self.path, index);
            if index == self.max_archives {
                if source.is_file() {
                    fs::remove_file(source)?;
                }
                continue;
            }
            if source.is_file() {
                fs::rename(source, archive_path(&self.path, index + 1))?;
            }
        }
        if self.path.is_file() {
            fs::rename(&self.path, archive_path(&self.path, 1))?;
        }
        self.file = Some(open_private_log_file(&self.path)?);
        self.size = 0;
        Ok(())
    }

    fn active_file(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("rotating log file is unavailable"))
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let incoming = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.size > 0 && self.size.saturating_add(incoming) > self.max_bytes {
            self.rotate()?;
        }
        let written = self.active_file()?.write(buf)?;
        self.size = self
            .size
            .saturating_add(u64::try_from(written).unwrap_or_default());
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.active_file()?.flush()
    }
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    path.with_extension(format!("log.{index}"))
}

fn open_private_log_file(path: &Path) -> io::Result<File> {
    let file = open_log_append(path)?;
    set_private_log_permissions(path)?;
    Ok(file)
}

#[cfg(unix)]
fn open_log_append(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_log_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(unix)]
fn set_private_log_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_log_permissions(path: &Path) -> io::Result<()> {
    fs::metadata(path).map(|_| ())
}

fn prune_old_log_archives(path: &Path, max_age: Duration) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return Ok(());
    };
    let prefix = format!("{file_name}.");
    let now = SystemTime::now();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let archive_index = name
            .strip_prefix(&prefix)
            .and_then(|suffix| suffix.parse::<usize>().ok());
        if archive_index.is_none() || !entry.file_type()?.is_file() {
            continue;
        }
        set_private_log_permissions(&entry.path())?;
        let old = entry
            .metadata()?
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if old {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn random_initial_points(minimum: i64, maximum: i64) -> i64 {
    if maximum <= minimum {
        return minimum;
    }
    let range = u128::try_from(maximum - minimum + 1).unwrap_or(1);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u128::from(std::process::id()), |duration| {
            duration.as_nanos()
        });
    minimum + i64::try_from(seed % range).unwrap_or_default()
}

fn current_log_timestamp(show_seconds: bool, timezone: Option<Tz>) -> String {
    let format = if show_seconds {
        "%H:%M:%S %d/%m/%y"
    } else {
        "%H:%M %d/%m/%y"
    };

    match timezone {
        Some(timezone) => Utc::now()
            .with_timezone(&timezone)
            .format(format)
            .to_string(),
        None => Local::now().format(format).to_string(),
    }
}

#[cfg(test)]
fn format_log_timestamp(iso_timestamp: &str, show_seconds: bool) -> Option<String> {
    let bytes = iso_timestamp.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }

    let year = iso_timestamp.get(2..4)?;
    let month = iso_timestamp.get(5..7)?;
    let day = iso_timestamp.get(8..10)?;
    let hour = iso_timestamp.get(11..13)?;
    let minute = iso_timestamp.get(14..16)?;

    if show_seconds {
        let second = iso_timestamp.get(17..19)?;
        return Some(format!("{hour}:{minute}:{second} {day}/{month}/{year}"));
    }

    Some(format!("{hour}:{minute} {day}/{month}/{year}"))
}

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
mod tests;
