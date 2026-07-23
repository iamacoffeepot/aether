use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{ConsoleLine, LineStyle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownLine {
    pub style: LineStyle,
    pub code_block: bool,
    pub thematic_break: bool,
    pub runs: Vec<MarkdownRun>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownRun {
    pub text: String,
    pub tone: MarkdownTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownTone {
    Text,
    Heading,
    Emphasis,
    Strong,
    InlineCode,
    FencedCode,
    Link,
    Image,
    QuoteMarker,
    QuoteText,
    ListMarker,
    TaskMarker,
    TableBorder,
    TableHeader,
    TableText,
    ThematicBreak,
    MutedMarker,
    EscapedMarker,
}

#[derive(Debug, Clone, Default)]
struct MarkdownBlockState {
    in_fenced_code: bool,
}

pub fn format_visible_history(lines: &VecDeque<ConsoleLine>, start: usize, end: usize) -> Vec<MarkdownLine> {
    let mut state = MarkdownBlockState::default();
    let bounded_end = end.min(lines.len());
    let mut formatted = Vec::new();
    for index in 0..bounded_end {
        let Some(line) = lines.get(index) else {
            continue;
        };
        let next = lines.get(index + 1).map(|line| line.text.as_str());
        let markdown = state.format_line(line, next);
        if index >= start {
            formatted.push(markdown);
        }
    }
    formatted
}

impl MarkdownBlockState {
    fn format_line(&mut self, line: &ConsoleLine, next: Option<&str>) -> MarkdownLine {
        if line.style == LineStyle::Input {
            return literal_line(line);
        }

        let text = line.text.as_str();
        if fenced_code_marker(text).is_some() {
            let was_open = self.in_fenced_code;
            self.in_fenced_code = !self.in_fenced_code;
            return MarkdownLine {
                style: line.style,
                code_block: true,
                thematic_break: false,
                runs: vec![MarkdownRun {
                    text: if was_open {
                        String::from("```")
                    } else {
                        text.trim().to_string()
                    },
                    tone: MarkdownTone::MutedMarker,
                }],
            };
        }

        if self.in_fenced_code {
            return MarkdownLine {
                style: line.style,
                code_block: true,
                thematic_break: false,
                runs: vec![MarkdownRun { text: sanitize_text(text), tone: MarkdownTone::FencedCode }],
            };
        }

        if let Some(code) = text.strip_prefix("    ") {
            return MarkdownLine {
                style: line.style,
                code_block: true,
                thematic_break: false,
                runs: vec![MarkdownRun { text: sanitize_text(code), tone: MarkdownTone::FencedCode }],
            };
        }

        if thematic_break(text) {
            return MarkdownLine {
                style: line.style,
                code_block: false,
                thematic_break: true,
                runs: vec![MarkdownRun { text: String::new(), tone: MarkdownTone::ThematicBreak }],
            };
        }

        if table_separator(text) {
            return MarkdownLine {
                style: line.style,
                code_block: false,
                thematic_break: false,
                runs: vec![MarkdownRun { text: sanitize_text(text.trim()), tone: MarkdownTone::TableBorder }],
            };
        }

        if table_row(text) {
            return MarkdownLine {
                style: line.style,
                code_block: false,
                thematic_break: false,
                runs: table_runs(text, next.is_some_and(table_separator)),
            };
        }

        format_block_line(text, line.style)
    }
}

fn literal_line(line: &ConsoleLine) -> MarkdownLine {
    MarkdownLine {
        style: line.style,
        code_block: false,
        thematic_break: false,
        runs: vec![MarkdownRun { text: sanitize_text(&line.text), tone: MarkdownTone::Text }],
    }
}

fn format_block_line(text: &str, style: LineStyle) -> MarkdownLine {
    let trimmed = text.trim_start();
    if let Some(body) = heading_body(trimmed) {
        return MarkdownLine {
            style,
            code_block: false,
            thematic_break: false,
            runs: vec![MarkdownRun { text: sanitize_text(body), tone: MarkdownTone::Heading }],
        };
    }

    if let Some(body) = trimmed.strip_prefix("> ") {
        let mut runs = vec![MarkdownRun { text: String::from("> "), tone: MarkdownTone::QuoteMarker }];
        runs.extend(inline_runs(body, MarkdownTone::QuoteText));
        return MarkdownLine { style, code_block: false, thematic_break: false, runs };
    }

    if trimmed == ">" {
        return MarkdownLine {
            style,
            code_block: false,
            thematic_break: false,
            runs: vec![MarkdownRun { text: String::from(">"), tone: MarkdownTone::QuoteMarker }],
        };
    }

    if let Some((marker, task, body)) = unordered_list(trimmed) {
        let mut runs = vec![MarkdownRun { text: marker, tone: MarkdownTone::ListMarker }];
        if let Some(task) = task {
            runs.push(MarkdownRun { text: task, tone: MarkdownTone::TaskMarker });
        }
        runs.extend(inline_runs(body, MarkdownTone::Text));
        return MarkdownLine { style, code_block: false, thematic_break: false, runs };
    }

    if let Some((marker, body)) = ordered_list(trimmed) {
        let mut runs = vec![MarkdownRun { text: marker, tone: MarkdownTone::ListMarker }];
        runs.extend(inline_runs(body, MarkdownTone::Text));
        return MarkdownLine { style, code_block: false, thematic_break: false, runs };
    }

    MarkdownLine { style, code_block: false, thematic_break: false, runs: inline_runs(text, MarkdownTone::Text) }
}

fn fenced_code_marker(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn thematic_break(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.len() < 3 {
        return false;
    }
    let mut count = 0usize;
    let mut marker = None;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if ch != '-' && ch != '*' && ch != '_' {
            return false;
        }
        if marker.is_some_and(|marker| marker != ch) {
            return false;
        }
        marker = Some(ch);
        count += 1;
    }
    count >= 3
}

fn heading_body(text: &str) -> Option<&str> {
    let mut marker_len = 0usize;
    for ch in text.chars().take(6) {
        if ch == '#' {
            marker_len += 1;
        } else {
            break;
        }
    }
    if marker_len == 0 {
        return None;
    }
    let body = text.get(marker_len..)?;
    let first = body.chars().next()?;
    if !first.is_whitespace() {
        return None;
    }
    Some(body.trim().trim_end_matches('#').trim_end())
}

fn unordered_list(text: &str) -> Option<(String, Option<String>, &str)> {
    let mut chars = text.chars();
    let marker = chars.next()?;
    if marker != '-' && marker != '*' && marker != '+' {
        return None;
    }
    let rest = chars.as_str();
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let body = rest.trim_start();
    let (task, body) = task_marker(body).unwrap_or((None, body));
    Some((String::from("- "), task, body))
}

fn task_marker(text: &str) -> Option<(Option<String>, &str)> {
    text.strip_prefix("[ ] ")
        .map(|body| (Some(String::from("[ ] ")), body))
        .or_else(|| text.strip_prefix("[x] ").map(|body| (Some(String::from("[x] ")), body)))
        .or_else(|| text.strip_prefix("[X] ").map(|body| (Some(String::from("[x] ")), body)))
}

fn ordered_list(text: &str) -> Option<(String, &str)> {
    let mut digits = 0usize;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = text.get(digits..)?;
    if !(rest.starts_with(". ") || rest.starts_with(") ")) {
        return None;
    }
    Some((text.get(..digits + 2)?.to_string(), text.get(digits + 2..)?.trim_start()))
}

fn table_row(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.contains('|') && !trimmed.is_empty()
}

fn table_separator(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.contains('|')
        && trimmed.chars().all(|ch| ch == '|' || ch == '-' || ch == ':' || ch.is_whitespace())
        && trimmed.chars().filter(|ch| *ch == '-').count() >= 3
}

fn table_runs(text: &str, header: bool) -> Vec<MarkdownRun> {
    let cell_tone = if header {
        MarkdownTone::TableHeader
    } else {
        MarkdownTone::TableText
    };
    let mut runs = Vec::new();
    let mut cell = String::new();
    for ch in text.chars() {
        if ch == '|' {
            push_run(&mut runs, &cell, cell_tone);
            cell.clear();
            push_run(&mut runs, "|", MarkdownTone::TableBorder);
        } else {
            cell.push(ch);
        }
    }
    push_run(&mut runs, &cell, cell_tone);
    runs
}

fn inline_runs(text: &str, default_tone: MarkdownTone) -> Vec<MarkdownRun> {
    let mut runs = Vec::new();
    let mut plain = String::new();
    let mut index = 0usize;
    while index < text.len() {
        let rest = &text[index..];
        if let Some((consumed, escaped)) = escaped_at(rest) {
            push_owned_run(&mut runs, &mut plain, default_tone);
            push_run(&mut runs, escaped, MarkdownTone::EscapedMarker);
            index += consumed;
        } else if let Some((consumed, rendered, tone)) = image_at(rest) {
            push_owned_run(&mut runs, &mut plain, default_tone);
            push_run(&mut runs, &rendered, tone);
            index += consumed;
        } else if let Some((consumed, rendered, tone)) = link_at(rest) {
            push_owned_run(&mut runs, &mut plain, default_tone);
            push_run(&mut runs, &rendered, tone);
            index += consumed;
        } else if let Some((consumed, body, tone)) = delimited_at(rest) {
            push_owned_run(&mut runs, &mut plain, default_tone);
            push_run(&mut runs, body, tone);
            index += consumed;
        } else {
            let Some(ch) = rest.chars().next() else {
                break;
            };
            push_sanitized_char(&mut plain, ch);
            index += ch.len_utf8();
        }
    }
    push_owned_run(&mut runs, &mut plain, default_tone);
    if runs.is_empty() {
        runs.push(MarkdownRun { text: String::new(), tone: default_tone });
    }
    runs
}

fn escaped_at(text: &str) -> Option<(usize, &str)> {
    let rest = text.strip_prefix('\\')?;
    let ch = rest.chars().next()?;
    is_markdown_punctuation(ch).then_some((1 + ch.len_utf8(), &rest[..ch.len_utf8()]))
}

fn is_markdown_punctuation(ch: char) -> bool {
    matches!(ch, '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.' | '!' | '|' | '>')
}

fn image_at(text: &str) -> Option<(usize, String, MarkdownTone)> {
    let body = text.strip_prefix("![")?;
    let alt_end = body.find("](")?;
    let target_start = alt_end + 2;
    let target_end = body.get(target_start..)?.find(')')? + target_start;
    let alt = sanitize_text(&body[..alt_end]);
    let target = sanitize_text(&body[target_start..target_end]);
    let rendered = if target.is_empty() {
        format!("[image: {alt}]")
    } else {
        format!("[image: {alt}] ({target})")
    };
    Some((2 + target_end + 1, rendered, MarkdownTone::Image))
}

fn link_at(text: &str) -> Option<(usize, String, MarkdownTone)> {
    let body = text.strip_prefix('[')?;
    let label_end = body.find("](")?;
    let target_start = label_end + 2;
    let target_end = body.get(target_start..)?.find(')')? + target_start;
    let label = sanitize_text(&body[..label_end]);
    if label.is_empty() {
        return None;
    }
    let target = sanitize_text(&body[target_start..target_end]);
    let rendered = if target.is_empty() {
        label
    } else {
        format!("{label} ({target})")
    };
    Some((1 + target_end + 1, rendered, MarkdownTone::Link))
}

fn delimited_at(text: &str) -> Option<(usize, &str, MarkdownTone)> {
    for (open, close, tone) in [
        ("`", "`", MarkdownTone::InlineCode),
        ("**", "**", MarkdownTone::Strong),
        ("__", "__", MarkdownTone::Strong),
        ("*", "*", MarkdownTone::Emphasis),
        ("_", "_", MarkdownTone::Emphasis),
    ] {
        let Some(rest) = text.strip_prefix(open) else {
            continue;
        };
        let close_index = rest.find(close)?;
        if close_index == 0 {
            continue;
        }
        return Some((open.len() + close_index + close.len(), &rest[..close_index], tone));
    }
    None
}

fn push_owned_run(runs: &mut Vec<MarkdownRun>, text: &mut String, tone: MarkdownTone) {
    if text.is_empty() {
        return;
    }
    push_run(runs, text, tone);
    text.clear();
}

fn push_run(runs: &mut Vec<MarkdownRun>, text: &str, tone: MarkdownTone) {
    let sanitized = sanitize_text(text);
    if sanitized.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.tone == tone
    {
        last.text.push_str(&sanitized);
        return;
    }
    runs.push(MarkdownRun { text: sanitized, tone });
}

fn sanitize_text(text: &str) -> String {
    let mut sanitized = String::new();
    for ch in text.chars() {
        push_sanitized_char(&mut sanitized, ch);
    }
    sanitized
}

fn push_sanitized_char(text: &mut String, ch: char) {
    if ch.is_control() && ch != '\t' {
        text.push('?');
    } else {
        text.push(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::VecDeque;

    fn line(text: &str) -> ConsoleLine {
        ConsoleLine { text: text.to_string(), style: LineStyle::Output }
    }

    fn tones(markdown: &MarkdownLine) -> Vec<(&str, MarkdownTone)> {
        markdown.runs.iter().map(|run| (run.text.as_str(), run.tone)).collect()
    }

    #[test]
    fn inline_markdown_formats_common_spans() {
        let runs = inline_runs(
            "plain *em* **strong** `code` [site](https://example.test) ![alt](img.png)",
            MarkdownTone::Text,
        );

        assert_eq!(
            runs.iter().map(|run| (run.text.as_str(), run.tone)).collect::<Vec<_>>(),
            vec![
                ("plain ", MarkdownTone::Text),
                ("em", MarkdownTone::Emphasis),
                (" ", MarkdownTone::Text),
                ("strong", MarkdownTone::Strong),
                (" ", MarkdownTone::Text),
                ("code", MarkdownTone::InlineCode),
                (" ", MarkdownTone::Text),
                ("site (https://example.test)", MarkdownTone::Link),
                (" ", MarkdownTone::Text),
                ("[image: alt] (img.png)", MarkdownTone::Image),
            ]
        );
    }

    #[test]
    fn block_markdown_formats_heading_quote_lists_and_tasks() {
        let mut state = MarkdownBlockState::default();

        assert_eq!(tones(&state.format_line(&line("## Heading ##"), None)), vec![("Heading", MarkdownTone::Heading)]);
        assert_eq!(
            tones(&state.format_line(&line("> quoted"), None)),
            vec![("> ", MarkdownTone::QuoteMarker), ("quoted", MarkdownTone::QuoteText),]
        );
        assert_eq!(
            tones(&state.format_line(&line("- [x] done"), None)),
            vec![("- ", MarkdownTone::ListMarker), ("[x] ", MarkdownTone::TaskMarker), ("done", MarkdownTone::Text),]
        );
        assert_eq!(
            tones(&state.format_line(&line("1. first"), None)),
            vec![("1. ", MarkdownTone::ListMarker), ("first", MarkdownTone::Text),]
        );
    }

    #[test]
    fn fenced_code_state_survives_visible_window_slicing() {
        let lines =
            VecDeque::from([line("```rust"), line("let value = **literal**;"), line("```"), line("**strong**")]);

        let visible = format_visible_history(&lines, 1, 3);

        assert_eq!(visible.len(), 2);
        assert!(visible[0].code_block);
        assert_eq!(tones(&visible[0]), vec![("let value = **literal**;", MarkdownTone::FencedCode)]);
        assert!(visible[1].code_block);
    }

    #[test]
    fn table_rows_and_separator_use_table_tones() {
        let mut state = MarkdownBlockState::default();

        assert_eq!(
            tones(&state.format_line(&line("| Name | Value |"), Some("| --- | --- |"))),
            vec![
                ("|", MarkdownTone::TableBorder),
                (" Name ", MarkdownTone::TableHeader),
                ("|", MarkdownTone::TableBorder),
                (" Value ", MarkdownTone::TableHeader),
                ("|", MarkdownTone::TableBorder),
            ]
        );
        assert_eq!(
            tones(&state.format_line(&line("| --- | --- |"), None)),
            vec![("| --- | --- |", MarkdownTone::TableBorder)]
        );
    }

    #[test]
    fn escaped_markers_and_controls_are_visible() {
        let runs = inline_runs("\\*not emphasized\\* \\| bad\u{0007}", MarkdownTone::Text);

        assert_eq!(
            runs.iter().map(|run| (run.text.as_str(), run.tone)).collect::<Vec<_>>(),
            vec![
                ("*", MarkdownTone::EscapedMarker),
                ("not emphasized", MarkdownTone::Text),
                ("*", MarkdownTone::EscapedMarker),
                (" ", MarkdownTone::Text),
                ("|", MarkdownTone::EscapedMarker),
                (" bad?", MarkdownTone::Text),
            ]
        );
    }

    #[test]
    fn input_echoes_are_not_markdown_formatted() {
        let mut state = MarkdownBlockState::default();
        let input = ConsoleLine { text: String::from("> **literal**"), style: LineStyle::Input };

        assert_eq!(tones(&state.format_line(&input, None)), vec![("> **literal**", MarkdownTone::Text)]);
    }
}
