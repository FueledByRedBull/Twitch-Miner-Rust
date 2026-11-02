//! Classifies configuration changes for a miner that is already running.
//!
//! The rule table names every top-level key, so a setting added to the schema
//! cannot go unclassified: a unit test fails until it is labelled here.

use serde_json::{Map, Value};

use crate::{ConfigError, ConfigFile};

/// Whether a changed setting takes effect in a running miner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadClass {
    /// Applied by a reload without restarting the process.
    Live,
    /// Read once at startup; the new value is used after a restart.
    NeedsRestart,
}

/// One changed setting. `name` is the config key, or `section.field` for the
/// object sections, and never contains a channel name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingChange {
    pub name: String,
    pub class: ReloadClass,
    /// Previous and new value as compact JSON. Present only for settings that
    /// cannot hold a secret, an account name, or a channel name.
    pub values: Option<(String, String)>,
}

pub(crate) struct Rule {
    pub(crate) key: &'static str,
    class: ReloadClass,
    /// Whether the values may be shown in logs and notifications.
    show_values: bool,
}

const fn live(key: &'static str, show_values: bool) -> Rule {
    Rule {
        key,
        class: ReloadClass::Live,
        show_values,
    }
}

const fn restart(key: &'static str, show_values: bool) -> Rule {
    Rule {
        key,
        class: ReloadClass::NeedsRestart,
        show_values,
    }
}

// `claim_drops` and `watch_streak_vod_recovery` are live only while they keep the
// startup-time task layout: the Drop claim loop and the streak recovery loop are
// spawned once, so a change that would turn either on or off is held back by the
// application and reported as needing a restart.
pub(crate) const SETTING_RULES: &[Rule] = &[
    restart("config_schema_version", true),
    restart("username", false),
    restart("debug", true),
    restart("debug_deep", true),
    restart("show_seconds", true),
    restart("claim_drops_startup", true),
    live("farm_drops", true),
    live("claim_drops", true),
    live("watch_one_stream_when_drops_active", true),
    live("claim_moments", true),
    live("watch_streak_vod_recovery", true),
    live("betting(make_predictions)", true),
    live("follow_raid", true),
    live("community_goals", true),
    live("emojis", true),
    restart("save_logs", true),
    restart("show_username_in_console", true),
    live("show_claimed_bonus_msg", true),
    live("show_game", true),
    live("chat_presence", true),
    restart("disable_at_in_nickname", true),
    live("streamers", false),
    live("streamers_exclude", false),
    live("game_priority", true),
    live("game_exclude", true),
    live("watch_priority", true),
    restart("followers_order", true),
    restart("drop_account_link", true),
    restart("drop_discovery_games", true),
    restart("log_format", true),
    live("bet", true),
    restart("timezone", true),
    restart("privacy", true),
    live("discord", true),
    live("webhook", true),
    live("streamer_overrides", false),
];

/// Sections reported field by field. Only the fields of `bet`, `privacy`,
/// `discord` and `webhook` are compared this way; `bet.filter_condition` stays
/// one value.
const SECTIONS: &[&str] = &["bet", "privacy", "discord", "webhook"];

/// Fields that hold a secret even inside a section whose other fields are shown.
const SECRET_FIELDS: &[&str] = &["discord.webhook_api", "webhook.url"];

/// Lists every setting that differs between the running configuration and a
/// newly read one, in schema order.
pub fn diff_configs(old: &ConfigFile, new: &ConfigFile) -> Result<Vec<SettingChange>, ConfigError> {
    let (Value::Object(old), Value::Object(new)) =
        (serde_json::to_value(old)?, serde_json::to_value(new)?)
    else {
        return Ok(Vec::new());
    };
    let mut changes = Vec::new();
    for rule in SETTING_RULES {
        let (before, after) = (old.get(rule.key), new.get(rule.key));
        if before == after {
            continue;
        }
        match (
            SECTIONS.contains(&rule.key),
            before.and_then(Value::as_object),
            after.and_then(Value::as_object),
        ) {
            (true, Some(before), Some(after)) => {
                push_section_changes(&mut changes, rule, before, after);
            }
            _ => changes.push(change(rule.key, rule, before, after)),
        }
    }
    Ok(changes)
}

fn push_section_changes(
    changes: &mut Vec<SettingChange>,
    rule: &Rule,
    before: &Map<String, Value>,
    after: &Map<String, Value>,
) {
    for (field, previous) in before {
        let current = after.get(field);
        if current == Some(previous) {
            continue;
        }
        let name = format!("{}.{field}", rule.key);
        changes.push(change(&name, rule, Some(previous), current));
    }
}

fn change(name: &str, rule: &Rule, before: Option<&Value>, after: Option<&Value>) -> SettingChange {
    let visible = rule.show_values && !SECRET_FIELDS.contains(&name);
    SettingChange {
        name: name.to_string(),
        class: rule.class,
        values: visible.then(|| (render(before), render(after))),
    }
}

fn render(value: Option<&Value>) -> String {
    value.map_or_else(|| String::from("null"), Value::to_string)
}

/// Returns `running` with every live setting taken from `next`. Settings that
/// need a restart keep the value the process started with, so the next diff
/// still reports them until the process restarts.
pub fn merge_live_settings(
    running: &ConfigFile,
    next: &ConfigFile,
) -> Result<ConfigFile, ConfigError> {
    let (Value::Object(mut merged), Value::Object(next)) =
        (serde_json::to_value(running)?, serde_json::to_value(next)?)
    else {
        return Ok(running.clone());
    };
    for rule in SETTING_RULES
        .iter()
        .filter(|rule| rule.class == ReloadClass::Live)
    {
        if let Some(value) = next.get(rule.key) {
            merged.insert(rule.key.to_string(), value.clone());
        }
    }
    Ok(serde_json::from_value(Value::Object(merged))?)
}
