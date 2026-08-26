//! Paged journal stream and one-record detail.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use serde_json::Value;

use crate::cursor::Cursor;
use crate::dto::{DigestHex, JournalRecordView};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{JournalQuery, ResourceKey, Store};
use crate::warroom::Focus;

const LIST_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "n", action: "next" },
    KeyHint { keys: "f", action: "filter" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

const RECORD_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "scroll" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Newest-first journal page, optionally bloom-filtered.
#[derive(Clone, Debug)]
pub struct Journal {
    query: JournalQuery,
    filter: String,
    editing: bool,
    cursor: Cursor<u64>,
    scroll: usize,
}

impl Journal {
    #[must_use]
    pub fn new(bloom: Option<DigestHex>) -> Self {
        Self {
            query: JournalQuery { bloom, from_sequence: None },
            filter: String::new(),
            editing: false,
            cursor: Cursor::new(),
            scroll: 0,
        }
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::Journal(self.query)]
    }

    #[must_use]
    pub fn bloom(&self) -> Option<DigestHex> {
        self.query.bloom
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        LIST_HINTS
    }

    #[must_use]
    pub fn enter_pushes(&self) -> bool {
        self.cursor.selected().is_some()
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        if self.editing {
            return self.handle_filter(key);
        }
        let rows = self.rows(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, |row| row.sequence);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, |row| row.sequence);
                Outcome::Handled
            }
            KeyCode::Enter => self
                .cursor
                .selected()
                .copied()
                .map_or(Outcome::Handled, |sequence| Outcome::Push(Nav::focus(Focus::record(sequence)))),
            KeyCode::Char('n') => self.advance(store),
            KeyCode::Char('f') => {
                self.editing = true;
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = self.rows(store);
        self.cursor.reseat(&rows, |row| row.sequence, |_, rows| rows.first().map(|row| row.sequence));
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = self.rows(store);
        if self.cursor.selected().is_none() {
            self.reseat(store);
        }
        let mut items = Vec::new();
        if !self.filter.is_empty() || self.editing {
            let mark = if self.editing {
                "_"
            } else {
                ""
            };
            items.push(ListItem::new(format!("filter  {}{mark}", self.filter)));
        }
        if let Some(notice) =
            store.journal(self.query).and_then(|cell| cell.value.as_ref()).and_then(|page| page.notice.as_deref())
        {
            items.push(ListItem::new(notice.to_owned()));
        }
        items.extend(rows.iter().map(|row| ListItem::new(row.summary.clone())));
        if items.is_empty() {
            items.push(ListItem::new("journal  (empty)"));
        }
        let list = List::new(items)
            .style(palette::body())
            .highlight_style(palette::cursor())
            .highlight_symbol(super::caret(self.enter_pushes()));
        let selected = self
            .cursor
            .selected_index(&rows, |row| row.sequence)
            .map(|index| index + usize::from(!self.filter.is_empty() || self.editing));
        let mut state = ListState::default().with_selected(selected).with_offset(self.scroll);
        frame.render_stateful_widget(list, area, &mut state);
        self.scroll = state.offset();
    }

    fn handle_filter(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.editing = false;
                if let Some(digest) = parse_bloom_filter(&self.filter) {
                    self.query.bloom = Some(digest);
                    self.query.from_sequence = None;
                }
                Outcome::Handled
            }
            KeyCode::Backspace => {
                self.filter.pop();
                Outcome::Handled
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                self.filter.push(ch);
                Outcome::Handled
            }
            _ => Outcome::Handled,
        }
    }

    fn advance(&mut self, store: &Store) -> Outcome {
        let Some(next) =
            store.journal(self.query).and_then(|cell| cell.value.as_ref()).and_then(|page| page.next_from_sequence)
        else {
            return Outcome::Handled;
        };
        self.query.from_sequence = Some(next);
        self.cursor.select(None);
        Outcome::Refresh
    }

    fn rows(&self, store: &Store) -> Vec<JournalRow> {
        let Some(page) = store.journal(self.query).and_then(|cell| cell.value.as_ref()) else {
            return Vec::new();
        };
        page.records
            .iter()
            .filter(|record| {
                self.filter.is_empty()
                    || parse_bloom_filter(&self.filter).is_some()
                    || record_matches(record, &self.filter)
            })
            .map(|record| JournalRow { sequence: record.sequence, summary: record_summary(record) })
            .collect()
    }
}

struct JournalRow {
    sequence: u64,
    summary: String,
}

/// One decoded journal record.
#[derive(Clone, Debug)]
pub struct Record {
    sequence: u64,
    offset: usize,
}

impl Record {
    #[must_use]
    pub fn new(sequence: u64) -> Self {
        Self { sequence, offset: 0 }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::record(self.sequence)
    }

    #[must_use]
    pub fn subscriptions() -> Vec<ResourceKey> {
        Vec::new()
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        RECORD_HINTS
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.offset = self.offset.saturating_add(1);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.offset = self.offset.saturating_sub(1);
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let mut lines = vec![plain(format!("record  {}", self.sequence))];
        match store.record(self.sequence) {
            None => lines.push(plain("record not in the current page")),
            Some(record) => {
                lines.push(plain(format!("key  {}", record.idempotency_key)));
                lines.push(plain(format!("decider  {}", record.decider)));
                lines.push(plain("event"));
                lines.extend(super::json::present(&record.event));
                lines.push(plain("outcome"));
                lines.extend(super::json::present(&record.outcome));
            }
        }
        let line_count = lines.len();
        let offset = self.offset.min(line_count.saturating_sub(1));
        self.offset = offset;
        let offset = u16::try_from(offset).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(lines).style(palette::body()).wrap(Wrap { trim: false }).scroll((offset, 0)),
            area,
        );
    }
}

fn plain(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), palette::body()))
}

fn record_summary(record: &JournalRecordView) -> String {
    format!(
        "{}  {}  {}",
        record.sequence,
        variant_name(&record.event, "fact"),
        variant_name(&record.outcome, "outcome")
    )
}

fn record_matches(record: &JournalRecordView, needle: &str) -> bool {
    record_summary(record).contains(needle) || record.idempotency_key.contains(needle)
}

fn variant_name(value: &Value, field: &str) -> String {
    let Some(obj) = value.as_object() else {
        return value.to_string();
    };
    if let Some(inner) = obj.get(field) {
        if let Some(name) = inner.as_object().and_then(|map| map.keys().next()) {
            return name.clone();
        }
        if let Some(name) = inner.as_str() {
            return name.to_owned();
        }
    }
    obj.keys().next().cloned().unwrap_or_else(|| value.to_string())
}

fn parse_bloom_filter(filter: &str) -> Option<DigestHex> {
    let hex = filter.trim();
    if hex.len() != 64 {
        return None;
    }
    serde_json::from_value(Value::String(hex.to_owned())).ok()
}

#[cfg(test)]
mod tests {
    use super::{Journal, Record};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::warroom::Focus;
    use crossterm::event::KeyEvent;

    #[test]
    fn journal_footer_keys_are_handled() {
        assert_footer_honest(Journal::key_hints(), |code| {
            Shell::probe(Nav::journal(None)).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn record_footer_keys_are_handled() {
        let nav = Nav::focus(Focus::record(1));
        assert_footer_honest(Record::key_hints(), |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }
}
