//! Conversation rendering of one transcript event.
//!
//! A parsed event becomes a variable-height block: wrapped assistant prose,
//! compact tool calls, folded tool results, quoted user text, thinking omitted.
//! Unknown events and non-JSON fall back to a raw preview, never dropped.

use std::mem::take;

use ratatui::style::{Modifier, Style};
use serde_json::Value;

use super::event::{collapse, content_blocks, first_text, one_line};
use super::session;
use crate::palette::{self, Role};

/// How a conversation row is painted. Prose uses the base ink, tools recede,
/// errors use the alert colour — existing palette tokens, no new ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Prose,
    Tool,
    Error,
    Quote,
}

impl Kind {
    #[must_use]
    pub fn style(self) -> Style {
        match self {
            Self::Prose | Self::Quote => palette::body(),
            Self::Tool => palette::body().add_modifier(Modifier::DIM),
            Self::Error => palette::paint(Role::Loud),
        }
    }
}

/// One screen row of a conversation event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub kind: Kind,
}

/// Rows for one raw transcript line at `width`. `expanded` unfolds tool-result
/// bodies; folded they stay a single `↳` line.
#[must_use]
pub fn event_lines(raw: &str, width: usize, expanded: bool) -> Vec<Line> {
    let width = width.max(1);
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return vec![Line { text: one_line(raw), kind: Kind::Prose }];
    };
    let lines = lines(&value, width, expanded);
    if lines.is_empty() {
        vec![Line { text: collapse(raw), kind: Kind::Prose }]
    } else {
        lines
    }
}

/// Rows for one parsed event at `width`. Empty means the caller should keep
/// the collapsed preview — thinking-only turns, unknown kinds, and the like.
#[must_use]
pub fn lines(value: &Value, width: usize, expanded: bool) -> Vec<Line> {
    let width = width.max(1);
    if let Some(kind) = value.get("type").and_then(Value::as_str) {
        return match kind {
            "assistant" => assistant(value, width),
            "user" => user(value, width, expanded),
            "result" => result_lines(value, width),
            _ => Vec::new(),
        };
    }
    muse(value, width)
}

fn assistant(value: &Value, width: usize) -> Vec<Line> {
    let mut lines = Vec::new();
    for block in content_blocks(value) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                    lines.extend(prose(text, width, Kind::Prose));
                }
            }
            Some("tool_use") => lines.push(tool_call(block)),
            // thinking / redacted_thinking / unknown blocks stay off the row
            _ => {}
        }
    }
    lines
}

fn tool_call(block: &Value) -> Line {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let input = block.get("input").map_or_else(String::new, input_preview);
    Line { text: format!("⚙ {name}({input})"), kind: Kind::Tool }
}

fn input_preview(input: &Value) -> String {
    let text = match input {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    text.lines().next().unwrap_or("").to_owned()
}

fn user(value: &Value, width: usize, expanded: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    for block in content_blocks(value) {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => lines.extend(tool_result(block, width, expanded)),
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                    lines.extend(quoted(text, width));
                }
            }
            _ => {}
        }
    }
    if lines.is_empty()
        && let Some(text) = first_text(value)
    {
        return quoted(&text, width);
    }
    lines
}

fn quoted(text: &str, width: usize) -> Vec<Line> {
    let inner = width.saturating_sub(2).max(1);
    wrap_prose(text, inner).into_iter().map(|text| Line { text: format!("> {text}"), kind: Kind::Quote }).collect()
}

fn tool_result(block: &Value, width: usize, expanded: bool) -> Vec<Line> {
    let err = block.get("is_error").and_then(Value::as_bool) == Some(true);
    let kind = if err {
        Kind::Error
    } else {
        Kind::Tool
    };
    let tag = if err {
        "err"
    } else {
        "ok"
    };
    let body = tool_result_body(block);
    if !expanded {
        let header = body
            .lines()
            .next()
            .filter(|line| !line.is_empty())
            .map_or_else(|| format!("↳ {tag}"), |first| format!("↳ {tag} {first}"));
        return vec![Line { text: header, kind }];
    }
    let mut lines = vec![Line { text: format!("↳ {tag}"), kind }];
    lines.extend(prose(&body, width, kind));
    lines
}

fn tool_result_body(block: &Value) -> String {
    if let Some(text) = block.get("content").and_then(Value::as_str) {
        return text.to_owned();
    }
    block
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str).or_else(|| item.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn result_lines(value: &Value, width: usize) -> Vec<Line> {
    let Some(summary) = session::summary_of(value) else {
        return Vec::new();
    };
    let kind = if summary.is_error == Some(true) {
        Kind::Error
    } else {
        Kind::Prose
    };
    let mut lines = vec![Line { text: summary.label(), kind }];
    if let Some(text) = value.get("result").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        lines.extend(prose(text, width, Kind::Prose));
    }
    lines
}

fn muse(value: &Value, width: usize) -> Vec<Line> {
    let Some(payload) = value.get("payload") else {
        return Vec::new();
    };
    match payload.get("kind").and_then(Value::as_str) {
        Some("run_started") => vec![Line { text: "started".to_owned(), kind: Kind::Tool }],
        Some("run_terminal") => muse_terminal(value, payload, width),
        _ => Vec::new(),
    }
}

fn muse_terminal(value: &Value, payload: &Value, width: usize) -> Vec<Line> {
    let summary = session::summary_of(value).unwrap_or_default();
    let kind = if summary.is_error == Some(true) {
        Kind::Error
    } else {
        Kind::Prose
    };
    let mut lines = vec![Line { text: summary.label(), kind }];
    if let Some(text) = payload.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        lines.extend(prose(text, width, Kind::Prose));
    } else if let Some(reason) = payload.get("reason").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        lines.extend(prose(reason, width, kind));
    }
    lines
}

fn prose(text: &str, width: usize, kind: Kind) -> Vec<Line> {
    wrap_prose(text, width).into_iter().map(|text| Line { text, kind }).collect()
}

/// Wrap prose to `width`. Fenced code blocks stay verbatim (not reflowed).
fn wrap_prose(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut fence = false;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.trim_start().starts_with("```") {
            fence = !fence;
            out.push(line.to_owned());
            continue;
        }
        if fence {
            out.push(line.to_owned());
            continue;
        }
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        out.extend(wrap_words(line, width));
    }
    out
}

fn wrap_words(line: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    for word in line.split(' ').filter(|word| !word.is_empty()) {
        if current.is_empty() {
            push_word(&mut rows, &mut current, word, width);
            continue;
        }
        let extra = word.chars().count().saturating_add(1);
        if current.chars().count().saturating_add(extra) <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            rows.push(take(&mut current));
            push_word(&mut rows, &mut current, word, width);
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

fn push_word(rows: &mut Vec<String>, current: &mut String, word: &str, width: usize) {
    if word.chars().count() <= width {
        word.clone_into(current);
        return;
    }
    rows.extend(hard_break(word, width));
}

fn hard_break(word: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut rest = word;
    while !rest.is_empty() {
        let end =
            rest.char_indices().nth(width.saturating_sub(1)).map_or(rest.len(), |(index, ch)| index + ch.len_utf8());
        rows.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{Kind, event_lines};

    fn texts(raw: &str, width: usize, expanded: bool) -> Vec<String> {
        event_lines(raw, width, expanded).into_iter().map(|line| line.text).collect()
    }

    fn kinds(raw: &str, width: usize, expanded: bool) -> Vec<Kind> {
        event_lines(raw, width, expanded).into_iter().map(|line| line.kind).collect()
    }

    #[test]
    fn assistant_prose_wraps_tools_stay_one_liners_and_thinking_is_hidden() {
        // The plausible bug: conversation still paints the collapsed log row,
        // so thinking leaks and a tool call is buried in wrapped JSON.
        let raw = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"HIDDEN_REASONING"},
            {"type":"text","text":"let me look at the file now"},
            {"type":"tool_use","name":"Read","input":{"path":"src/lib.rs"}}]}}"#;
        let lines = event_lines(raw, 12, false);
        let body: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert!(body.iter().any(|text| text.contains("let me")), "{body:?}");
        assert!(body.iter().any(|text| text.contains("⚙ Read(") && text.contains("src/lib.rs")), "{body:?}");
        assert!(body.iter().all(|text| !text.contains("HIDDEN_REASONING")), "{body:?}");
        assert_eq!(lines.iter().filter(|line| line.text.contains('⚙')).count(), 1, "{body:?}");
        assert!(
            lines.iter().filter(|line| line.kind == Kind::Prose).count() >= 2,
            "prose must wrap at width 12: {body:?}"
        );
    }

    #[test]
    fn a_tool_result_stays_folded_until_expanded() {
        // The plausible bug: conversation always dumps the tool body, so a
        // 200-line cargo test result fills the pane before Enter.
        let raw = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"ok-head\nSECRET_TAIL","is_error":false}]}}"#;
        let folded = texts(raw, 40, false);
        assert_eq!(folded.len(), 1, "{folded:?}");
        assert!(folded[0].starts_with("↳ ok"), "{folded:?}");
        assert!(folded[0].contains("ok-head"), "{folded:?}");
        assert!(!folded[0].contains("SECRET_TAIL"), "{folded:?}");

        let open = texts(raw, 40, true);
        assert!(open.iter().any(|text| text.contains("SECRET_TAIL")), "{open:?}");
        assert!(open.iter().any(|text| text == "↳ ok"), "{open:?}");
    }

    #[test]
    fn a_failed_tool_result_uses_the_error_kind() {
        let raw = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","content":"boom","is_error":true}]}}"#;
        assert_eq!(kinds(raw, 40, false), [Kind::Error]);
        assert!(texts(raw, 40, false)[0].starts_with("↳ err"), "{:?}", texts(raw, 40, false));
    }

    #[test]
    fn user_text_is_quoted() {
        let raw = r#"{"type":"user","message":{"content":[{"type":"text","text":"please fix it"}]}}"#;
        let lines = texts(raw, 40, false);
        assert_eq!(lines, ["> please fix it"]);
    }

    #[test]
    fn the_terminal_row_uses_the_session_label() {
        // The plausible bug: conversation re-derives turns/cost instead of
        // using the session footer, so the two views disagree on the meters.
        let raw = r#"{"type":"result","is_error":false,"num_turns":3,"total_cost_usd":0.42,"duration_ms":12000,"result":"VERDICT: pass"}"#;
        let lines = texts(raw, 40, false);
        assert!(lines.iter().any(|text| text.contains("3 turns") && text.contains("$0.42")), "{lines:?}");
        assert!(lines.iter().any(|text| text.contains("VERDICT: pass")), "{lines:?}");
    }

    #[test]
    fn the_muse_envelope_renders_instead_of_dumping_raw_json() {
        let completed = texts(
            r#"{"payload_type":"run.terminal.completed","payload":{"kind":"run_terminal","terminal":"completed","text":"VERDICT: pass","reason":null}}"#,
            40,
            false,
        );
        assert!(completed.iter().any(|text| text.contains("ok")), "{completed:?}");
        assert!(completed.iter().any(|text| text.contains("VERDICT: pass")), "{completed:?}");
        assert!(completed.iter().all(|text| !text.contains("payload_type")), "{completed:?}");

        let started = texts(r#"{"payload_type":"run.lifecycle.started","payload":{"kind":"run_started"}}"#, 40, false);
        assert_eq!(started, ["started"]);
    }

    #[test]
    fn non_json_is_kept_as_a_preview() {
        // The plausible bug: a truncated tail is dropped, so a killed lane's
        // last line vanishes from the conversation view.
        assert_eq!(texts("not-json {", 40, false), ["not-json {"]);
        let mystery = texts(r#"{"type":"mystery","payload":1}"#, 40, false);
        assert_eq!(mystery, ["mystery"]);
    }

    #[test]
    fn fenced_code_is_not_reflowed() {
        // The plausible bug: word wrap joins a fenced block into one paragraph,
        // so a snippet the agent wrote is no longer copyable as code.
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"see:\n```\nfn too_long_to_fit_this_width()\n```"}]}}"#;
        let lines = texts(raw, 12, false);
        assert!(lines.iter().any(|text| text == "fn too_long_to_fit_this_width()"), "{lines:?}");
        assert_eq!(lines.iter().filter(|text| *text == "```").count(), 2, "{lines:?}");
    }
}
