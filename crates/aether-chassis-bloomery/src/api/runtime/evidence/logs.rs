//! `GET /logs/coordinator` — a bounded journald proxy.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

use super::super::reads::{clamp_limit, pairs, parse_u64};
use crate::api::dto::{CoordinatorLogEntry, CoordinatorLogsView};

/// Default page size.
pub const COORDINATOR_LOG_DEFAULT: u64 = 200;
/// Hard ceiling.
pub const COORDINATOR_LOG_MAX: u64 = 1_000;
const SYSTEMD_MARKER: &str = "/run/systemd/system";
const UNIT: &str = "bloomery";
const UNAVAILABLE: &str = "coordinator logs require systemd journald on this host";

/// Why the log route cannot answer.
#[derive(Debug)]
pub enum LogError {
    /// The host is not running under systemd.
    Unavailable { reason: String },
    /// A query parameter did not parse.
    BadQuery(String),
    /// journalctl ran and failed, or output did not decode.
    Io(String),
}

/// Parsed `GET /logs/coordinator` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    pub since: Option<String>,
    pub level: Option<String>,
    pub contains: Option<String>,
    pub cursor: Option<String>,
    pub limit: u64,
    pub notice: Option<String>,
}

impl LogQuery {
    pub fn parse(query: &str) -> Result<Self, String> {
        let mut since = None;
        let mut level = None;
        let mut contains = None;
        let mut cursor = None;
        let mut requested = None;
        for (key, value) in pairs(query) {
            match key.as_str() {
                "since" => since = Some(value),
                "level" => level = Some(value.to_ascii_lowercase()),
                "contains" => contains = Some(value),
                "cursor" => cursor = Some(value),
                "limit" => requested = Some(parse_u64("limit", &value)?),
                _ => {}
            }
        }
        if let Some(level) = &level {
            parse_level(level)?;
        }
        let (limit, notice) = clamp_limit(requested, COORDINATOR_LOG_DEFAULT, COORDINATOR_LOG_MAX);
        Ok(Self { since, level, contains, cursor, limit, notice })
    }
}

/// Read one page. `runner` is the journalctl seam tests inject.
pub fn read(
    query: &str,
    runner: impl FnOnce(&[String]) -> Result<Output, LogError>,
) -> Result<CoordinatorLogsView, LogError> {
    let parsed = LogQuery::parse(query).map_err(LogError::BadQuery)?;
    if !Path::new(SYSTEMD_MARKER).exists() {
        return Err(LogError::Unavailable { reason: UNAVAILABLE.to_owned() });
    }

    let argv = journalctl_argv(&parsed);
    let output = runner(&argv)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(LogError::Io(format!("journalctl exited {}: {stderr}", output.status)));
    }

    let min_priority = parsed.level.as_deref().and_then(|level| parse_level(level).ok()).unwrap_or(7);
    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(entry) = parse_entry(line) else {
            continue;
        };
        if entry_priority(line) > min_priority {
            continue;
        }
        if let Some(contains) = &parsed.contains
            && !entry.message.contains(contains)
        {
            continue;
        }
        entries.push(entry);
    }

    let limit = usize::try_from(parsed.limit).unwrap_or(usize::MAX);
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    let next_cursor = truncated.then(|| entries.last().map(|entry| entry.cursor.clone())).flatten();
    Ok(CoordinatorLogsView { entries, next_cursor, truncated, notice: parsed.notice })
}

/// Default journalctl runner.
pub fn journalctl(argv: &[String]) -> Result<Output, LogError> {
    Command::new("journalctl").args(argv).output().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            LogError::Unavailable { reason: UNAVAILABLE.to_owned() }
        } else {
            LogError::Io(error.to_string())
        }
    })
}

/// Filter and page an already-decoded journalctl JSONL body — the test seam
/// that does not spawn a process.
#[cfg(test)]
pub fn page_entries(query: &LogQuery, jsonl: &str) -> CoordinatorLogsView {
    let min_priority = query.level.as_deref().and_then(|level| parse_level(level).ok()).unwrap_or(7);
    let mut entries = Vec::new();
    for line in jsonl.lines() {
        let Some(entry) = parse_entry(line) else {
            continue;
        };
        if entry_priority(line) > min_priority {
            continue;
        }
        if let Some(contains) = &query.contains
            && !entry.message.contains(contains)
        {
            continue;
        }
        entries.push(entry);
    }
    let limit = usize::try_from(query.limit).unwrap_or(usize::MAX);
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    let next_cursor = truncated.then(|| entries.last().map(|entry| entry.cursor.clone())).flatten();
    CoordinatorLogsView { entries, next_cursor, truncated, notice: query.notice.clone() }
}

fn journalctl_argv(query: &LogQuery) -> Vec<String> {
    let mut argv = vec![
        "--user".to_owned(),
        "-u".to_owned(),
        UNIT.to_owned(),
        "--output=json".to_owned(),
        "--no-pager".to_owned(),
        "-n".to_owned(),
        COORDINATOR_LOG_MAX.to_string(),
    ];
    if let Some(since) = &query.since {
        argv.push("--since".to_owned());
        argv.push(since.clone());
    }
    if let Some(cursor) = &query.cursor {
        argv.push("--after-cursor".to_owned());
        argv.push(cursor.clone());
    }
    argv
}

fn parse_entry(line: &str) -> Option<CoordinatorLogEntry> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let message = value.get("MESSAGE")?.as_str()?.to_owned();
    let cursor = value.get("__CURSOR")?.as_str()?.to_owned();
    let timestamp_unix_micros = value
        .get("__REALTIME_TIMESTAMP")
        .and_then(|stamp| stamp.as_str())
        .and_then(|stamp| stamp.parse().ok())
        .unwrap_or(0);
    let level = priority_level(entry_priority(line));
    Some(CoordinatorLogEntry { timestamp_unix_micros, level: level.to_owned(), message, cursor })
}

fn entry_priority(line: &str) -> u8 {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| value.get("PRIORITY").and_then(serde_json::Value::as_str).and_then(|p| p.parse().ok()))
        .unwrap_or(6)
}

fn parse_level(level: &str) -> Result<u8, String> {
    match level {
        "error" => Ok(3),
        "warn" => Ok(4),
        "info" => Ok(6),
        "debug" | "trace" => Ok(7),
        other => Err(format!("level must be trace, debug, info, warn, or error, not {other}")),
    }
}

fn priority_level(priority: u8) -> &'static str {
    match priority {
        0..=3 => "error",
        4 => "warn",
        7 => "debug",
        _ => "info",
    }
}
