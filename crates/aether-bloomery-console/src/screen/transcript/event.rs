//! Defensive collapsed rendering of one transcript line.
//!
//! Unknown event types, non-JSON, and a truncated tail are rendered as a
//! raw one-line preview — never dropped. The two transcript dialects share
//! this: the Anthropic Messages stream carries `type`, the `muse exec
//! --json` envelope carries a `payload.kind` instead.

use serde_json::Value;

use super::super::metrics::{format_duration, format_micro_usd};
use super::session::{cost_micros_of, duration_millis_of, turns_label, turns_of};

/// One-line collapsed form of `raw`. JSON is inspected only here.
#[must_use]
pub fn collapse(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return one_line(raw);
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return payload_collapse(&value);
    };
    let preview = match kind {
        "assistant" => assistant_preview(&value),
        "user" => user_preview(&value),
        "system" => field(&value, "subtype").or_else(|| first_text(&value)).unwrap_or_default(),
        "result" => result_preview(&value),
        "item.completed" => item_preview(&value),
        "turn.completed" | "turn.started" | "turn.failed" | "thread.started" => {
            field(&value, "thread_id").unwrap_or_default()
        }
        other if other.contains("tool") => field(&value, "name").or_else(|| first_text(&value)).unwrap_or_default(),
        _ => first_text(&value).unwrap_or_default(),
    };
    if preview.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind}  {preview}")
    }
}

/// Parsed form for the expanded pane. Non-JSON returns `None`.
#[must_use]
pub fn expand_value(raw: &str) -> Option<Value> {
    serde_json::from_str(raw).ok()
}

/// Pretty form for the expanded pane. Non-JSON stays raw.
#[must_use]
pub fn expand(raw: &str) -> String {
    expand_value(raw).and_then(|value| serde_json::to_string_pretty(&value).ok()).unwrap_or_else(|| raw.to_owned())
}

/// An assistant turn's public text plus every tool it called, so a turn that
/// only calls tools still names them on its collapsed row.
fn assistant_preview(value: &Value) -> String {
    let mut parts: Vec<String> = content_blocks(value)
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
                .filter(|text| !text.is_empty())
                .map(one_line)
        })
        .collect();
    let tools: Vec<String> = content_blocks(value)
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    if !tools.is_empty() {
        parts.push(format!("tool:{}", tools.join("+")));
    }
    if parts.is_empty() {
        return first_text(value).unwrap_or_default();
    }
    parts.join("  ")
}

fn user_preview(value: &Value) -> String {
    if let Some(text) = first_text(value) {
        return text;
    }
    tool_results_preview(value)
}

/// Every tool result on a `user` event, each naming its error state, so a
/// failed call is visible without expanding the row.
fn tool_results_preview(value: &Value) -> String {
    content_blocks(value)
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|block| {
            let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
            match (failed, tool_result_text(block)) {
                (false, Some(text)) => format!("tool_result {text}"),
                (true, Some(text)) => format!("tool_result err {text}"),
                (false, None) => "tool_result ok".to_owned(),
                (true, None) => "tool_result err".to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

fn tool_result_text(block: &Value) -> Option<String> {
    if let Some(text) = block.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        return Some(one_line(text));
    }
    block.get("content").and_then(Value::as_array).and_then(|items| {
        items.iter().find_map(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(one_line)
                .or_else(|| item.as_str().filter(|text| !text.is_empty()).map(one_line))
        })
    })
}

/// The terminal `result` event as the session's own row: verdict, turns,
/// cost, and time, then the result text.
fn result_preview(value: &Value) -> String {
    let mut parts = Vec::new();
    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        parts.push("error".to_owned());
    } else if let Some(subtype) = field(value, "subtype") {
        parts.push(subtype);
    } else {
        parts.push("ok".to_owned());
    }
    if let Some(turns) = turns_of(value) {
        parts.push(turns_label(turns));
    }
    if let Some(cost) = cost_micros_of(value) {
        parts.push(format_micro_usd(cost));
    }
    if let Some(millis) = duration_millis_of(value) {
        parts.push(format_duration(millis));
    }
    if let Some(text) = first_text(value) {
        parts.push(text);
    }
    parts.join("  ")
}

/// A `muse exec --json` envelope: no `type`, a `payload.kind` instead. An
/// unknown kind renders its kind, never the raw line.
fn payload_collapse(value: &Value) -> String {
    let payload = value.get("payload");
    let kind = payload.and_then(|payload| payload.get("kind")).and_then(Value::as_str).unwrap_or("event");
    let preview = match kind {
        "run_terminal" => payload.map_or_else(String::new, terminal_preview),
        "run_started" => "started".to_owned(),
        _ => payload
            .and_then(|payload| field(payload, "text"))
            .or_else(|| payload.and_then(|payload| field(payload, "reason")))
            .unwrap_or_default(),
    };
    if preview.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind}  {preview}")
    }
}

fn terminal_preview(payload: &Value) -> String {
    let state = payload.get("terminal").and_then(Value::as_str).unwrap_or("unknown");
    let mut parts = vec![state.to_owned()];
    if let Some(text) = field(payload, "text") {
        parts.push(text);
    } else if let Some(reason) = field(payload, "reason") {
        parts.push(reason);
    }
    parts.join("  ")
}

fn item_preview(value: &Value) -> String {
    let Some(item) = value.get("item") else {
        return String::new();
    };
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("item");
    item.get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map_or_else(|| kind.to_owned(), |text| format!("{kind}  {}", one_line(text)))
}

fn content_blocks(value: &Value) -> impl Iterator<Item = &Value> {
    value
        .get("message")
        .and_then(|message| message.get("content"))
        .or_else(|| value.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn first_text(value: &Value) -> Option<String> {
    if let Some(text) = value.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        return Some(one_line(text));
    }
    if let Some(text) = value.get("result").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        return Some(one_line(text));
    }
    for block in content_blocks(value) {
        if let Some(text) = block.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            return Some(one_line(text));
        }
        if let Some(text) = block.as_str().filter(|text| !text.is_empty()) {
            return Some(one_line(text));
        }
    }
    None
}

fn field(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).filter(|text| !text.is_empty()).map(one_line)
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch == '\n' || ch == '\r' {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::collapse;

    #[test]
    fn an_assistant_turn_names_its_text_and_every_tool() {
        // The plausible bug: a turn that narrates and calls tools shows only
        // the prose, so the collapsed session hides what the agent did.
        let collapsed = collapse(
            r#"{"type":"assistant","message":{"content":[
                {"type":"text","text":"let me look"},
                {"type":"tool_use","id":"t1","name":"Read","input":{"path":"src/lib.rs"}},
                {"type":"tool_use","id":"t2","name":"Edit","input":{"path":"src/lib.rs"}}]}}"#,
        );
        assert!(collapsed.contains("let me look"), "{collapsed}");
        assert!(collapsed.contains("tool:Read+Edit"), "{collapsed}");
    }

    #[test]
    fn a_tool_only_turn_still_names_its_tool() {
        let collapsed = collapse(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        );
        assert_eq!(collapsed, "assistant  tool:Bash");
    }

    #[test]
    fn a_failed_tool_result_is_visible_without_expanding() {
        // The plausible bug: a `user` event carrying only tool results
        // collapses to a bare `user`, so a failed call is invisible until
        // the operator expands every row.
        let failed = collapse(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true}]}}"#,
        );
        assert!(failed.contains("tool_result err"), "{failed}");
        assert!(failed.contains("boom"), "{failed}");

        let ok = collapse(
            r#"{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"done"}]}]}}"#,
        );
        assert!(ok.contains("tool_result"), "{ok}");
        assert!(ok.contains("done"), "{ok}");
        assert!(!ok.contains("err"), "{ok}");
    }

    #[test]
    fn the_terminal_row_carries_verdict_turns_cost_and_time() {
        // The plausible bug: the terminal `result` row shows only its text,
        // so the session's turns / cost / duration are unreadable in place.
        let collapsed = collapse(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":3,"total_cost_usd":0.42,"duration_ms":12000,"result":"VERDICT: pass"}"#,
        );
        assert!(collapsed.starts_with("result  "), "{collapsed}");
        assert!(collapsed.contains("3 turns"), "{collapsed}");
        assert!(collapsed.contains("$0.42"), "{collapsed}");
        assert!(collapsed.contains("12s"), "{collapsed}");
        assert!(collapsed.contains("VERDICT: pass"), "{collapsed}");

        let failed = collapse(r#"{"type":"result","is_error":true,"result":"VERDICT: fail"}"#);
        assert!(failed.contains("error"), "{failed}");
        assert!(failed.contains("VERDICT: fail"), "{failed}");
        assert!(!failed.contains('$'), "{failed}");
    }

    #[test]
    fn the_muse_envelope_renders_instead_of_dumping_raw_json() {
        // The plausible bug: `muse exec --json` lines carry no `type`, so the
        // viewer paints the whole JSON envelope on one row.
        let completed = collapse(
            r#"{"payload_type":"run.terminal.completed","payload":{"kind":"run_terminal","terminal":"completed","text":"VERDICT: pass","reason":null}}"#,
        );
        assert!(completed.contains("run_terminal"), "{completed}");
        assert!(completed.contains("completed"), "{completed}");
        assert!(completed.contains("VERDICT: pass"), "{completed}");
        assert!(!completed.contains("payload_type"), "{completed}");

        let failed = collapse(
            r#"{"payload_type":"run.terminal.failed","payload":{"kind":"run_terminal","terminal":"failed","text":"","reason":"server_error"}}"#,
        );
        assert!(failed.contains("failed"), "{failed}");
        assert!(failed.contains("server_error"), "{failed}");

        let started = collapse(r#"{"payload_type":"run.lifecycle.started","payload":{"kind":"run_started"}}"#);
        assert_eq!(started, "run_started  started");
    }
}
