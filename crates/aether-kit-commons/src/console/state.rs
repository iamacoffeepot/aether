use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use aether_data::MailboxId;
use aether_math::Rgba;

use super::markdown::{MarkdownLine, format_visible_history};
use super::{ConsoleCommandInvoked, ConsoleConfig, ConsoleTheme};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub name: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleAction {
    InvokeExternal { mailbox: MailboxId, payload: ConsoleCommandInvoked },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    Input,
    Output,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLine {
    pub text: String,
    pub style: LineStyle,
}

#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub description: String,
    pub target: CommandTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTarget {
    BuiltIn(BuiltInCommand),
    External(MailboxId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInCommand {
    Help,
    Clear,
    Echo,
    Version,
    Diagnostics,
    Quit,
}

pub struct ConsoleState {
    pub open: bool,
    pub input: String,
    pub caret: usize,
    pub scroll_offset: usize,
    pub cursor_visible: bool,
    blink_ticks: u32,
    scrollback_limit: usize,
    history_cursor: Option<usize>,
    submitted: Vec<String>,
    lines: VecDeque<ConsoleLine>,
    commands: BTreeMap<String, CommandEntry>,
}

impl ConsoleState {
    #[must_use]
    pub fn new(config: &ConsoleConfig) -> Self {
        let mut state = Self {
            open: false,
            input: String::new(),
            caret: 0,
            scroll_offset: 0,
            cursor_visible: true,
            blink_ticks: 0,
            scrollback_limit: usize::try_from(config.scrollback_limit.max(1)).unwrap_or(usize::MAX),
            history_cursor: None,
            submitted: Vec::new(),
            lines: VecDeque::new(),
            commands: BTreeMap::new(),
        };
        state.register_builtins();
        state
    }

    #[must_use]
    pub fn theme_color(theme: &ConsoleTheme, style: LineStyle) -> Rgba {
        match style {
            LineStyle::Input => theme.input_color,
            LineStyle::Output => theme.output_color,
            LineStyle::Error => theme.error_color,
        }
    }

    #[must_use]
    pub fn lines(&self) -> &VecDeque<ConsoleLine> {
        &self.lines
    }

    #[must_use]
    pub fn commands(&self) -> &BTreeMap<String, CommandEntry> {
        &self.commands
    }

    pub fn register_external(&mut self, name: String, description: String, mailbox: MailboxId) {
        if let Some(name) = normalize_command_name(&name) {
            self.commands.insert(name, CommandEntry { description, target: CommandTarget::External(mailbox) });
        }
    }

    pub fn unregister_external(&mut self, name: &str) -> bool {
        let Some(name) = normalize_command_name(name) else {
            return false;
        };
        if matches!(self.commands.get(&name).map(|entry| entry.target), Some(CommandTarget::BuiltIn(_))) {
            return false;
        }
        self.commands.remove(&name).is_some()
    }

    pub fn push_output(&mut self, text: impl Into<String>) {
        self.push_line(ConsoleLine { text: text.into(), style: LineStyle::Output });
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.push_line(ConsoleLine { text: text.into(), style: LineStyle::Error });
    }

    pub fn append_command_output(&mut self, lines: Vec<String>, error: bool) {
        let style = if error {
            LineStyle::Error
        } else {
            LineStyle::Output
        };
        for text in lines {
            self.push_line(ConsoleLine { text, style });
        }
    }

    pub fn scroll_by(&mut self, rows: isize, visible_rows: usize) {
        if rows < 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub(rows.unsigned_abs());
        } else {
            self.scroll_offset = self.scroll_offset.saturating_add(rows.unsigned_abs());
        }
        self.clamp_scroll(visible_rows);
    }

    pub fn clamp_scroll(&mut self, visible_rows: usize) {
        self.scroll_offset = self.max_scroll_offset(visible_rows).min(self.scroll_offset);
    }

    #[must_use]
    pub fn max_scroll_offset(&self, visible_rows: usize) -> usize {
        self.lines.len().saturating_sub(visible_rows)
    }

    #[must_use]
    pub fn visible_history(&self, visible_rows: usize) -> Vec<ConsoleLine> {
        if visible_rows == 0 {
            return Vec::new();
        }
        let max_offset = self.max_scroll_offset(visible_rows);
        let offset = self.scroll_offset.min(max_offset);
        let end = self.lines.len().saturating_sub(offset);
        let start = end.saturating_sub(visible_rows);
        self.lines.iter().skip(start).take(end.saturating_sub(start)).cloned().collect()
    }

    #[must_use]
    pub fn visible_markdown_history(&self, visible_rows: usize) -> Vec<MarkdownLine> {
        if visible_rows == 0 {
            return Vec::new();
        }
        let max_offset = self.max_scroll_offset(visible_rows);
        let offset = self.scroll_offset.min(max_offset);
        let end = self.lines.len().saturating_sub(offset);
        let start = end.saturating_sub(visible_rows);
        format_visible_history(&self.lines, start, end)
    }

    pub fn insert_text(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            let byte = byte_index_for_char(&self.input, self.caret);
            self.input.insert(byte, ch);
            self.caret += 1;
        }
        self.reset_blink();
    }

    pub fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let start = byte_index_for_char(&self.input, self.caret - 1);
        let end = byte_index_for_char(&self.input, self.caret);
        self.input.replace_range(start..end, "");
        self.caret -= 1;
        self.reset_blink();
    }

    pub fn move_left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
        self.reset_blink();
    }

    pub fn move_right(&mut self) {
        self.caret = (self.caret + 1).min(char_count(&self.input));
        self.reset_blink();
    }

    pub fn history_prev(&mut self) {
        if self.submitted.is_empty() {
            return;
        }
        let next = self.history_cursor.map_or(self.submitted.len() - 1, |cursor| cursor.saturating_sub(1));
        self.apply_history(next);
    }

    pub fn history_next(&mut self) {
        let Some(cursor) = self.history_cursor else {
            return;
        };
        if cursor + 1 >= self.submitted.len() {
            self.history_cursor = None;
            self.input.clear();
            self.caret = 0;
        } else {
            self.apply_history(cursor + 1);
        }
        self.reset_blink();
    }

    pub fn tick_cursor(&mut self) {
        self.blink_ticks = self.blink_ticks.wrapping_add(1);
        if self.blink_ticks >= 30 {
            self.blink_ticks = 0;
            self.cursor_visible = !self.cursor_visible;
        }
    }

    pub fn submit(&mut self, prompt: &str) -> Vec<ConsoleAction> {
        let input = self.input.trim().to_string();
        let echoed = format!("{prompt}{}", self.input);
        self.push_line(ConsoleLine { text: echoed, style: LineStyle::Input });
        self.input.clear();
        self.caret = 0;
        self.history_cursor = None;
        self.scroll_offset = 0;
        self.reset_blink();

        if input.is_empty() {
            return Vec::new();
        }
        self.submitted.push(input.clone());
        let Some(parsed) = parse_command(&input) else {
            return Vec::new();
        };
        self.execute(parsed, input)
    }

    fn execute(&mut self, parsed: ParsedCommand, input: String) -> Vec<ConsoleAction> {
        let Some(entry) = self.commands.get(&parsed.name).cloned() else {
            self.push_error(format!("unknown command: {}", parsed.name));
            return Vec::new();
        };

        match entry.target {
            CommandTarget::BuiltIn(command) => self.execute_builtin(command, parsed.args),
            CommandTarget::External(mailbox) => {
                vec![ConsoleAction::InvokeExternal {
                    mailbox,
                    payload: ConsoleCommandInvoked { name: parsed.name, args: parsed.args, input },
                }]
            }
        }
    }

    fn execute_builtin(&mut self, command: BuiltInCommand, args: Vec<String>) -> Vec<ConsoleAction> {
        match command {
            BuiltInCommand::Help => {
                let lines: Vec<_> =
                    self.commands.iter().map(|(name, entry)| format!("{name} - {}", entry.description)).collect();
                for line in lines {
                    self.push_output(line);
                }
                Vec::new()
            }
            BuiltInCommand::Clear => {
                self.lines.clear();
                self.scroll_offset = 0;
                Vec::new()
            }
            BuiltInCommand::Echo => {
                self.push_output(args.join(" "));
                Vec::new()
            }
            BuiltInCommand::Version => {
                self.push_output(format!("aether-kit-commons {}", env!("CARGO_PKG_VERSION")));
                Vec::new()
            }
            BuiltInCommand::Diagnostics => {
                self.push_output(format!("lines: {}", self.lines.len()));
                self.push_output(format!("commands: {}", self.commands.len()));
                self.push_output(format!("scroll offset: {}", self.scroll_offset));
                Vec::new()
            }
            BuiltInCommand::Quit => vec![ConsoleAction::Quit],
        }
    }

    fn register_builtins(&mut self) {
        self.register_builtin("help", "list commands", BuiltInCommand::Help);
        self.register_builtin("clear", "clear scrollback", BuiltInCommand::Clear);
        self.register_builtin("echo", "write arguments to the console", BuiltInCommand::Echo);
        self.register_builtin("version", "show aether-kit-commons version", BuiltInCommand::Version);
        self.register_builtin("diagnostics", "show console diagnostics", BuiltInCommand::Diagnostics);
        self.register_builtin("quit", "request engine shutdown", BuiltInCommand::Quit);
    }

    fn register_builtin(&mut self, name: &str, description: &str, command: BuiltInCommand) {
        self.commands.insert(
            String::from(name),
            CommandEntry { description: String::from(description), target: CommandTarget::BuiltIn(command) },
        );
    }

    fn apply_history(&mut self, cursor: usize) {
        self.history_cursor = Some(cursor);
        self.input = self.submitted[cursor].clone();
        self.caret = char_count(&self.input);
        self.reset_blink();
    }

    fn push_line(&mut self, line: ConsoleLine) {
        self.lines.push_back(line);
        while self.lines.len() > self.scrollback_limit {
            self.lines.pop_front();
        }
    }

    fn reset_blink(&mut self) {
        self.blink_ticks = 0;
        self.cursor_visible = true;
    }
}

#[must_use]
pub fn parse_command(input: &str) -> Option<ParsedCommand> {
    let mut parts = input.split_whitespace();
    let name = normalize_command_name(parts.next()?)?;
    let args = parts.map(ToString::to_string).collect();
    Some(ParsedCommand { name, args })
}

fn normalize_command_name(name: &str) -> Option<String> {
    let normalized = name.trim().to_ascii_lowercase();
    (!normalized.is_empty() && normalized.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .then_some(normalized)
}

fn byte_index_for_char(text: &str, char_index: usize) -> usize {
    text.char_indices().nth(char_index).map_or(text.len(), |(index, _)| index)
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::ConsoleConfig;

    fn state() -> ConsoleState {
        ConsoleState::new(&ConsoleConfig { scrollback_limit: 32, ..ConsoleConfig::default() })
    }

    #[test]
    fn parsing_splits_name_and_args() {
        let parsed = parse_command("  ECHO hello  world ").expect("parsed");

        assert_eq!(parsed.name, "echo");
        assert_eq!(parsed.args, ["hello", "world"]);
    }

    #[test]
    fn help_output_is_alphabetical() {
        let mut state = state();
        state.register_external(String::from("aaa"), String::from("first external"), MailboxId(0x4000_0000_0000_0002));

        state.insert_text("help");
        state.submit("> ");
        let lines: Vec<_> =
            state.lines().iter().map(|line| line.text.as_str()).filter(|line| line.contains(" - ")).collect();

        assert_eq!(lines.first(), Some(&"aaa - first external"));
        assert_eq!(lines.last(), Some(&"version - show aether-kit-commons version"));
    }

    #[test]
    fn scroll_clamps_to_available_lines() {
        let mut state = state();
        for line in ["one", "two", "three"] {
            state.push_output(line);
        }

        state.scroll_by(99, 2);

        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn enter_submit_echoes_and_clears_input() {
        let mut state = state();
        state.insert_text("echo hello");

        state.submit("> ");

        assert_eq!(state.input, "");
        assert_eq!(state.caret, 0);
        assert_eq!(state.lines()[0].text, "> echo hello");
        assert_eq!(state.lines()[1].text, "hello");
    }

    #[test]
    fn clear_removes_scrollback() {
        let mut state = state();
        state.push_output("old");

        state.insert_text("clear");
        state.submit("> ");

        assert!(state.lines().is_empty());
    }

    #[test]
    fn unknown_command_outputs_error() {
        let mut state = state();

        state.insert_text("wat");
        state.submit("> ");

        assert_eq!(
            state.lines().back(),
            Some(&ConsoleLine { text: String::from("unknown command: wat"), style: LineStyle::Error })
        );
    }

    #[test]
    fn external_registration_replaces_and_removes() {
        let mut state = state();
        let first = MailboxId(0x4000_0000_0000_0003);
        let second = MailboxId(0x4000_0000_0000_0004);

        state.register_external(String::from("profile"), String::from("old"), first);
        state.register_external(String::from("profile"), String::from("new"), second);

        assert!(matches!(
            state.commands()["profile"].target,
            CommandTarget::External(id) if id == second
        ));
        assert!(state.unregister_external("profile"));
        assert!(!state.commands().contains_key("profile"));
    }

    #[test]
    fn builtin_registration_cannot_be_removed_by_extension() {
        let mut state = state();

        assert!(!state.unregister_external("help"));
        assert!(state.commands().contains_key("help"));
    }

    #[test]
    fn markdown_history_replays_fenced_code_before_visible_window() {
        let mut state = state();
        state.push_output("```rust");
        state.push_output("let value = **literal**;");
        state.push_output("```");

        let visible = state.visible_markdown_history(2);

        assert!(visible[0].code_block);
        assert_eq!(visible[0].runs[0].text, "let value = **literal**;");
    }

    #[test]
    fn markdown_history_keeps_raw_lines_unchanged() {
        let mut state = state();
        state.push_output("**strong**");

        let _ = state.visible_markdown_history(1);

        assert_eq!(state.lines()[0].text, "**strong**");
    }
}
