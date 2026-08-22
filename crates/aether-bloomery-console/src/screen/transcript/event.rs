//! Defensive collapsed rendering of one transcript line.
//!
//! Unknown event types, non-JSON, and a truncated tail are rendered as a
//! raw one-line preview — never dropped.

use serde_json::Value;

/// One-line collapsed form of `raw`. JSON is inspected only here.
#[must_use]
pub fn collapse(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return one_line(raw);
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return one_line(raw);
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

fn assistant_preview(value: &Value) -> String {
    if let Some(text) = first_text(value) {
        return text;
    }
    content_blocks(value)
        .find_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("tool_use"))
                .then(|| block.get("name").and_then(Value::as_str).map(|name| format!("tool:{name}")))
                .flatten()
        })
        .unwrap_or_default()
}

fn user_preview(value: &Value) -> String {
    if let Some(text) = first_text(value) {
        return text;
    }
    content_blocks(value)
        .find_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("tool_result")).then(|| "tool_result".to_owned())
        })
        .unwrap_or_default()
}

fn result_preview(value: &Value) -> String {
    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        return field(value, "result").unwrap_or_else(|| "error".to_owned());
    }
    field(value, "subtype").or_else(|| first_text(value)).unwrap_or_else(|| "ok".to_owned())
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
