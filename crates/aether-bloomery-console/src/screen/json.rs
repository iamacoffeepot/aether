//! The one place JSON becomes screen lines.
//!
//! The walk is iterative with a depth cap because the value is served data.

use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::palette::{self, Role};

const MAX_DEPTH: usize = 64;
const INDENT: &str = "  ";

enum Step<'a> {
    Value { value: &'a Value, indent: usize, depth: usize },
    Field { key: &'a str, value: &'a Value, indent: usize, depth: usize },
    Close { punct: &'static str, indent: usize },
}

/// Highlighted, newline-split lines for one JSON value.
#[must_use]
pub fn present(value: &Value) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut stack = vec![Step::Value { value, indent: 0, depth: 0 }];
    while let Some(step) = stack.pop() {
        match step {
            Step::Close { punct, indent } => lines.push(punct_line(indent, punct)),
            Step::Value { value, indent, depth } => push_value(&mut lines, &mut stack, value, indent, depth),
            Step::Field { key, value, indent, depth } => {
                push_field(&mut lines, &mut stack, key, value, indent, depth);
            }
        }
    }
    lines
}

fn push_value<'a>(
    lines: &mut Vec<Line<'static>>,
    stack: &mut Vec<Step<'a>>,
    value: &'a Value,
    indent: usize,
    depth: usize,
) {
    if depth >= MAX_DEPTH {
        lines.push(ellipsis(indent));
        return;
    }
    match value {
        Value::Object(map) => {
            lines.push(punct_line(indent, "{"));
            stack.push(Step::Close { punct: "}", indent });
            for (key, child) in map.iter().rev() {
                stack.push(Step::Field { key, value: child, indent: indent + 1, depth: depth + 1 });
            }
        }
        Value::Array(items) => {
            lines.push(punct_line(indent, "["));
            stack.push(Step::Close { punct: "]", indent });
            for child in items.iter().rev() {
                stack.push(Step::Value { value: child, indent: indent + 1, depth: depth + 1 });
            }
        }
        Value::String(text) if text.contains('\n') => {
            for segment in text.split('\n') {
                lines.push(string_segment(indent, segment));
            }
        }
        other => lines.push(Line::from(with_indent(indent, scalar_spans(other)))),
    }
}

fn push_field<'a>(
    lines: &mut Vec<Line<'static>>,
    stack: &mut Vec<Step<'a>>,
    key: &'a str,
    value: &'a Value,
    indent: usize,
    depth: usize,
) {
    if depth >= MAX_DEPTH {
        lines.push(Line::from(vec![key_span(indent, key), styled("…", Role::Text)]));
        return;
    }
    match value {
        Value::Object(map) => {
            lines.push(key_open(indent, key, "{"));
            stack.push(Step::Close { punct: "}", indent });
            for (child_key, child) in map.iter().rev() {
                stack.push(Step::Field { key: child_key, value: child, indent: indent + 1, depth: depth + 1 });
            }
        }
        Value::Array(items) => {
            lines.push(key_open(indent, key, "["));
            stack.push(Step::Close { punct: "]", indent });
            for child in items.iter().rev() {
                stack.push(Step::Value { value: child, indent: indent + 1, depth: depth + 1 });
            }
        }
        Value::String(text) if text.contains('\n') => {
            lines.push(Line::from(vec![key_span(indent, key)]));
            for segment in text.split('\n') {
                lines.push(string_segment(indent + 1, segment));
            }
        }
        other => {
            let mut spans = vec![key_span(indent, key)];
            spans.extend(scalar_spans(other));
            lines.push(Line::from(spans));
        }
    }
}

fn scalar_spans(value: &Value) -> Vec<Span<'static>> {
    match value {
        Value::Null => vec![styled("null", Role::Attention)],
        Value::Bool(true) => vec![styled("true", Role::Attention)],
        Value::Bool(false) => vec![styled("false", Role::Attention)],
        Value::Number(number) => vec![styled(number.to_string(), Role::Working)],
        Value::String(text) => {
            vec![styled("\"", Role::Text), styled(text.clone(), Role::Settled), styled("\"", Role::Text)]
        }
        Value::Array(_) | Value::Object(_) => vec![styled("…", Role::Text)],
    }
}

fn key_span(indent: usize, key: &str) -> Span<'static> {
    let pad = INDENT.repeat(indent);
    styled(format!("{pad}{key}: "), Role::Focus)
}

fn key_open(indent: usize, key: &str, punct: &'static str) -> Line<'static> {
    Line::from(vec![key_span(indent, key), styled(punct, Role::Text)])
}

fn punct_line(indent: usize, punct: &'static str) -> Line<'static> {
    Line::from(with_indent(indent, vec![styled(punct, Role::Text)]))
}

fn ellipsis(indent: usize) -> Line<'static> {
    Line::from(with_indent(indent, vec![styled("…", Role::Text)]))
}

fn string_segment(indent: usize, segment: &str) -> Line<'static> {
    let pad = INDENT.repeat(indent);
    Line::from(styled(format!("{pad}{segment}"), Role::Settled))
}

fn with_indent(indent: usize, mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    if indent > 0 {
        spans.insert(0, styled(INDENT.repeat(indent), Role::Text));
    }
    spans
}

fn styled(text: impl Into<String>, role: Role) -> Span<'static> {
    Span::styled(text.into(), palette::paint(role))
}

#[cfg(test)]
mod tests {
    use super::{MAX_DEPTH, present};
    use ratatui::text::Line;
    use serde_json::json;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn an_embedded_newline_becomes_its_own_line() {
        // The plausible bug: to_string_pretty keeps the newline escaped, so a
        // prompt body or diff stored as one string paints as a single cut line.
        let lines = present(&json!({"log": "one\ntwo"}));
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let one = texts.iter().position(|text| text.ends_with("one")).expect("line ending in one");
        assert!(texts[one + 1].ends_with("two"), "{texts:?}");
        assert!(texts.iter().all(|text| !text.contains("\\n")), "{texts:?}");
    }

    #[test]
    fn keys_strings_and_numbers_take_different_roles() {
        // Tripwire: a single-colour dump is the state this order replaces; a
        // regression to palette::body() everywhere collapses all four.
        let lines = present(&json!({"k": "s", "n": 1, "b": true}));
        let mut key = None;
        let mut string = None;
        let mut number = None;
        let mut literal = None;
        for line in &lines {
            for span in &line.spans {
                let text = span.content.as_ref();
                if text.contains("k:") {
                    key = span.style.fg;
                } else if text == "s" {
                    string = span.style.fg;
                } else if text == "1" {
                    number = span.style.fg;
                } else if text == "true" {
                    literal = span.style.fg;
                }
            }
        }
        let key = key.expect("key span");
        let string = string.expect("string span");
        let number = number.expect("number span");
        let literal = literal.expect("literal span");
        assert_ne!(key, string);
        assert_ne!(key, number);
        assert_ne!(key, literal);
        assert_ne!(string, number);
        assert_ne!(string, literal);
        assert_ne!(number, literal);
    }

    #[test]
    fn a_deeply_nested_value_stops_at_the_cap() {
        // Tripwire: a recursive walk over served data — this test overflows
        // the stack if the presenter ever recurses.
        let mut value = json!(1);
        for _ in 0..(MAX_DEPTH + 10) {
            value = json!([value]);
        }
        let lines = present(&value);
        assert!(lines.iter().any(|line| line_text(line).contains('…')), "{lines:?}");
    }
}
