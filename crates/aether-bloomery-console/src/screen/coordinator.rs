//! Coordinator log: follow-tail over `GET /logs/coordinator` with a level floor and search.
//!
//! The route serves one bounded oldest-first page of journald output and
//! names the next cursor; follow re-polls with that cursor at view cadence,
//! the same way the transcript tail re-polls with its byte offset. The level
//! floor is a server parameter — one subscription per floor, so a floor
//! change restarts the stream — while the search needle is client-side over
//! the retained rows.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::dto::{CoordinatorLogEntry, CoordinatorLogsView};
use crate::keys::{KeyHint, Outcome};
use crate::palette;
use crate::store::{CoordinatorLogQuery, LogLevel, ResourceKey, Store};

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "f", action: "follow" },
    KeyHint { keys: "G", action: "tail" },
    KeyHint { keys: "l", action: "level" },
    KeyHint { keys: "/", action: "search" },
    KeyHint { keys: "n/N", action: "next" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Retained rows. The route pages at most a thousand per read; the screen
/// keeps the same bound so a long follow cannot grow without limit.
const RETENTION: usize = 1_000;

const MICROS_PER_SEC: u64 = 1_000_000;
const DAY_SECS: u64 = 86_400;

/// The coordinator-log tail.
pub struct CoordinatorLog {
    level: Option<LogLevel>,
    entries: Vec<CoordinatorLogEntry>,
    have: Option<String>,
    started: bool,
    ingested: Option<Ingested>,
    follow: bool,
    selected: Option<usize>,
    scroll: usize,
    search: Search,
    last_error: Option<String>,
    notice: Option<String>,
    truncated: bool,
    dropped: usize,
}

/// The exact cell sample already consumed. A follow re-poll rewrites the
/// same key with a fresh sample, so the query alone cannot tell "consumed"
/// from "new arrivals".
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ingested {
    query: CoordinatorLogQuery,
    fetched: Option<Instant>,
    error: Option<String>,
}

#[derive(Default)]
struct Search {
    editing: bool,
    needle: String,
    matches: Vec<usize>,
    at: Option<usize>,
}

impl CoordinatorLog {
    #[must_use]
    pub fn new() -> Self {
        Self {
            level: None,
            entries: Vec::new(),
            have: None,
            started: false,
            ingested: None,
            follow: true,
            selected: None,
            scroll: 0,
            search: Search::default(),
            last_error: None,
            notice: None,
            truncated: false,
            dropped: 0,
        }
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::CoordinatorLogs(self.query())]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    /// Enter never pushes; a log row has no drill-in.
    #[must_use]
    pub fn enter_pushes() -> bool {
        false
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        if self.search.editing {
            return self.handle_search_edit(key);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_sel(1);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.follow = false;
                self.move_sel(-1);
                Outcome::Handled
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow {
                    self.pin_tail();
                }
                Outcome::Handled
            }
            KeyCode::Char('G') => {
                self.follow = true;
                self.pin_tail();
                Outcome::Handled
            }
            KeyCode::Char('l') => {
                self.cycle_level();
                Outcome::Refresh
            }
            KeyCode::Char('/') => {
                self.search.editing = true;
                Outcome::Handled
            }
            KeyCode::Char('n') => {
                self.step_match(1);
                Outcome::Handled
            }
            KeyCode::Char('N') => {
                self.step_match(-1);
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        self.ingest(store);
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        self.ingest(store);
        if self.follow {
            self.pin_tail();
        }
        let banner = self.dropped_banner();
        let banner_h = u16::from(banner.is_some());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(banner_h), Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        if let Some(banner) = banner {
            frame.render_widget(Paragraph::new(banner).style(palette::body().add_modifier(Modifier::BOLD)), chunks[0]);
        }
        self.render_list(frame, chunks[1]);
        frame.render_widget(Paragraph::new(self.status_line()).style(palette::body()), chunks[2]);
    }

    fn query(&self) -> CoordinatorLogQuery {
        CoordinatorLogQuery { level: self.level, cursor: self.have.clone(), live: self.follow }
    }

    fn ingest(&mut self, store: &Store) {
        let query = self.query();
        let Some(cell) = store.coordinator_logs(&query) else {
            return;
        };
        if cell.inflight {
            return;
        }
        if cell.value.is_none() && cell.error.is_none() {
            return;
        }
        let stamp = Ingested { query, fetched: cell.fetched_at, error: cell.error.clone() };
        if self.ingested.as_ref() == Some(&stamp) {
            return;
        }
        self.ingested = Some(stamp);
        if let Some(error) = &cell.error {
            self.last_error = Some(error.clone());
            return;
        }
        let Some(page) = cell.value.as_ref() else {
            return;
        };
        self.apply_page(page);
    }

    fn apply_page(&mut self, page: &CoordinatorLogsView) {
        self.last_error = None;
        self.started = true;
        self.notice.clone_from(&page.notice);
        self.truncated = page.truncated;
        for entry in &page.entries {
            if self.entries.iter().any(|known| known.cursor == entry.cursor) {
                continue;
            }
            self.entries.push(entry.clone());
        }
        let excess = self.entries.len().saturating_sub(RETENTION);
        if excess > 0 {
            self.entries.drain(..excess);
            self.dropped = self.dropped.saturating_add(excess);
            self.selected = self.selected.map(|index| index.saturating_sub(excess));
            self.scroll = self.scroll.saturating_sub(excess);
        }
        if self.selected.is_some_and(|index| index >= self.entries.len()) {
            self.selected = self.entries.len().checked_sub(1);
        }
        self.have = page.next_cursor.clone().or_else(|| self.entries.last().map(|entry| entry.cursor.clone()));
        self.rescan();
        if self.selected.is_none() || self.follow {
            self.pin_tail();
        }
    }

    /// A new floor is a new stream: rows the old floor served must not
    /// survive beside rows the new one serves.
    fn cycle_level(&mut self) {
        self.level = LogLevel::cycle(self.level);
        self.entries.clear();
        self.have = None;
        self.started = false;
        self.ingested = None;
        self.selected = None;
        self.scroll = 0;
        self.search.matches.clear();
        self.search.at = None;
        self.last_error = None;
        self.notice = None;
        self.truncated = false;
        self.dropped = 0;
    }

    fn pin_tail(&mut self) {
        self.selected = self.entries.len().checked_sub(1);
    }

    fn move_sel(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let current = self.selected.unwrap_or(0).min(last);
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(last)
        };
        if next < current {
            self.follow = false;
        }
        self.selected = Some(next);
    }

    fn handle_search_edit(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                self.search.editing = false;
                self.search.needle.clear();
                self.rescan();
                Outcome::Handled
            }
            KeyCode::Enter => {
                self.search.editing = false;
                Outcome::Handled
            }
            KeyCode::Backspace => {
                self.search.needle.pop();
                self.rescan();
                Outcome::Handled
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                self.search.needle.push(ch);
                self.rescan();
                Outcome::Handled
            }
            _ => Outcome::Handled,
        }
    }

    fn rescan(&mut self) {
        self.search.matches.clear();
        if self.search.needle.is_empty() {
            self.search.at = None;
            return;
        }
        let needle = self.search.needle.clone();
        self.search.matches.extend(
            self.entries.iter().enumerate().filter(|(_, entry)| row_matches(entry, &needle)).map(|(index, _)| index),
        );
        if self.search.at.is_some_and(|at| at >= self.search.matches.len()) {
            self.search.at = None;
        }
    }

    fn step_match(&mut self, dir: i32) {
        if self.search.matches.is_empty() {
            return;
        }
        let len = self.search.matches.len();
        let next = match (self.search.at, dir < 0) {
            (None, false) => 0,
            (None, true) => len - 1,
            (Some(at), false) => (at + 1) % len,
            (Some(at), true) => (at + len - 1) % len,
        };
        self.search.at = Some(next);
        self.selected = Some(self.search.matches[next]);
        if self.selected != self.entries.len().checked_sub(1) {
            self.follow = false;
        }
    }

    fn render_list(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let height = usize::from(area.height.max(1));
        let width = usize::from(area.width.saturating_sub(2));
        if self.follow {
            self.scroll = self.entries.len().saturating_sub(height);
        } else if let Some(index) = self.selected {
            if index < self.scroll {
                self.scroll = index;
            } else if index >= self.scroll.saturating_add(height) {
                self.scroll = index.saturating_add(1).saturating_sub(height);
            }
        }
        let end = self.scroll.saturating_add(height).min(self.entries.len());
        let mut items = Vec::new();
        for entry in self.entries.get(self.scroll..end).unwrap_or_default() {
            items.push(ListItem::new(truncate(&row_text(entry), width)));
        }
        if items.is_empty() {
            items.push(ListItem::new(self.empty_label()));
        }
        let highlight =
            self.selected.filter(|index| *index >= self.scroll && *index < end).map(|index| index - self.scroll);
        let list = List::new(items)
            .style(palette::body())
            .highlight_style(palette::cursor())
            .highlight_symbol(super::caret(Self::enter_pushes()));
        let mut state = ListState::default().with_selected(highlight);
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn dropped_banner(&self) -> Option<String> {
        (self.dropped > 0).then(|| format!("{} earlier lines dropped", self.dropped))
    }

    fn status_line(&self) -> String {
        let mut parts = vec![
            "coordinator log".to_owned(),
            format!("level={}", self.level.map_or("all", LogLevel::label)),
            format!("{} lines", self.entries.len()),
        ];
        if self.truncated {
            parts.push("more".to_owned());
        }
        if self.follow {
            parts.push("FOLLOW".to_owned());
        }
        if !self.search.needle.is_empty() {
            let at = self.search.at.map_or(0, |index| index + 1);
            parts.push(format!("{}/{}  /{}", at, self.search.matches.len(), self.search.needle));
        }
        if self.search.editing {
            parts.push("_".to_owned());
        }
        if let Some(notice) = &self.notice {
            parts.push(notice.clone());
        }
        parts.join("  ")
    }

    fn empty_label(&self) -> String {
        if let Some(error) = &self.last_error {
            return format!("coordinator log  {error}");
        }
        if self.started {
            "coordinator log  (empty)".to_owned()
        } else {
            "coordinator log  loading".to_owned()
        }
    }
}

impl Default for CoordinatorLog {
    fn default() -> Self {
        Self::new()
    }
}

/// The painted row: clock, severity, then the message on one line. A
/// journald MESSAGE may carry newlines; they flatten to spaces so one entry
/// stays one selectable row.
fn row_text(entry: &CoordinatorLogEntry) -> String {
    let message: String = entry
        .message
        .chars()
        .map(|ch| {
            if ch.is_control() {
                ' '
            } else {
                ch
            }
        })
        .collect();
    format!("{}  {:5}  {}", clock(entry.timestamp_unix_micros), entry.level, message)
}

fn row_matches(entry: &CoordinatorLogEntry, needle: &str) -> bool {
    entry.level.contains(needle) || entry.message.contains(needle)
}

/// UTC clock for a journald realtime stamp. Journald numbers
/// microseconds; the date is the operator's `journalctl` concern, the
/// second is this row's.
fn clock(timestamp_unix_micros: u64) -> String {
    let day_secs = timestamp_unix_micros / MICROS_PER_SEC % DAY_SECS;
    format!("{:02}:{:02}:{:02}", day_secs / 3_600, day_secs % 3_600 / 60, day_secs % 60)
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width.max(1)).collect()
}

#[cfg(test)]
mod tests {
    use super::{CoordinatorLog, clock, row_text};
    use crate::dto::{CoordinatorLogEntry, CoordinatorLogsView};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::{CoordinatorLogQuery, LogLevel, ResourceKey, Store};
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    fn entry(cursor: &str, level: &str, message: &str) -> CoordinatorLogEntry {
        CoordinatorLogEntry {
            timestamp_unix_micros: 0,
            level: level.to_owned(),
            message: message.to_owned(),
            cursor: cursor.to_owned(),
        }
    }

    fn page(entries: Vec<CoordinatorLogEntry>, next_cursor: Option<&str>) -> CoordinatorLogsView {
        CoordinatorLogsView {
            entries,
            next_cursor: next_cursor.map(str::to_owned),
            truncated: next_cursor.is_some(),
            notice: None,
        }
    }

    fn query(cursor: Option<&str>) -> CoordinatorLogQuery {
        CoordinatorLogQuery { level: None, cursor: cursor.map(str::to_owned), live: true }
    }

    fn drawn(view: &mut CoordinatorLog, store: &Store) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test backend");
        terminal.draw(|frame| view.render(frame, frame.area(), store)).expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area().height {
            for x in 0..buffer.area().width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn log_footer_keys_are_handled() {
        assert_footer_honest(CoordinatorLog::key_hints(), |code| {
            Shell::probe(Nav::coordinator_log()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn follow_appends_the_next_page_without_duplicating() {
        // The plausible bug: re-ingesting the current key (a render between
        // the fetch and the cursor advance, or a refetch of an unchanged
        // tail) appends the same rows again, so the tail doubles every poll.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_coordinator_logs(
            query(None),
            Ok(page(vec![entry("c1", "info", "one"), entry("c2", "info", "two")], Some("c2"))),
        );
        let mut view = CoordinatorLog::new();
        view.reseat(&store);
        assert_eq!(view.entries.len(), 2);

        store.apply_coordinator_logs(query(Some("c2")), Ok(page(vec![entry("c3", "warn", "three")], Some("c3"))));
        view.reseat(&store);
        assert_eq!(view.entries.len(), 3);
        assert_eq!(view.entries[2].message, "three");

        store.apply_coordinator_logs(query(Some("c2")), Ok(page(vec![entry("c3", "warn", "three")], Some("c3"))));
        view.reseat(&store);
        assert_eq!(view.entries.len(), 3, "a refetched tail page must dedupe on cursor");
    }

    #[test]
    fn a_level_change_restarts_the_stream_on_a_new_subscription() {
        // The plausible bug: the floor cycles but the old rows stay, so an
        // `error` floor still shows the info rows it replaced — or the query
        // never names the floor and the route keeps serving every severity.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_coordinator_logs(
            query(None),
            Ok(page(vec![entry("c1", "info", "one"), entry("c2", "error", "two")], Some("c2"))),
        );
        let mut view = CoordinatorLog::new();
        view.reseat(&store);
        assert_eq!(view.entries.len(), 2);

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('l')), &store), Outcome::Refresh);
        assert_eq!(view.entries.len(), 0, "a new floor must not keep the old floor's rows");
        let ResourceKey::CoordinatorLogs(next) = view.subscriptions().pop().expect("one subscription") else {
            panic!("the log subscribes to the log route");
        };
        assert_eq!(next.level, Some(LogLevel::Error));
        assert_eq!(next.path(), "/logs/coordinator?level=error");

        for _ in 0..3 {
            view.handle_key(KeyEvent::from(KeyCode::Char('l')), &store);
        }
        let ResourceKey::CoordinatorLogs(next) = view.subscriptions().pop().expect("one subscription") else {
            panic!("the log subscribes to the log route");
        };
        assert_eq!(next.level, Some(LogLevel::Debug));
        view.handle_key(KeyEvent::from(KeyCode::Char('l')), &store);
        let ResourceKey::CoordinatorLogs(next) = view.subscriptions().pop().expect("one subscription") else {
            panic!("the log subscribes to the log route");
        };
        assert_eq!(next.level, None, "the cycle wraps back to unfiltered");
    }

    #[test]
    fn search_types_n_literally_and_steps_only_after_commit() {
        // The plausible bug: the edit mode steals `n`/`N` for stepping (the
        // transcript's wart), so a needle like "warning" is untypeable and
        // the operator can never search for the word the log actually uses.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_coordinator_logs(
            query(None),
            Ok(page(vec![entry("c1", "info", "all quiet"), entry("c2", "warn", "warning: disk hot")], None)),
        );
        let mut view = CoordinatorLog::new();
        view.reseat(&store);
        view.handle_key(KeyEvent::from(KeyCode::Char('/')), &store);
        for ch in ['w', 'a', 'r', 'n', 'i', 'n', 'g'] {
            view.handle_key(KeyEvent::from(KeyCode::Char(ch)), &store);
        }
        assert_eq!(view.search.needle, "warning");
        assert_eq!(view.search.matches, vec![1]);
        view.handle_key(KeyEvent::from(KeyCode::Enter), &store);
        view.handle_key(KeyEvent::from(KeyCode::Char('n')), &store);
        assert_eq!(view.selected, Some(1));
        assert_eq!(view.search.at, Some(0));
    }

    #[test]
    fn upward_motion_disarms_follow_and_tail_rearms() {
        // The plausible bug: follow stays armed through `k`, so new arrivals
        // yank the viewport back to the tail while the operator reads upward.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_coordinator_logs(
            query(None),
            Ok(page(vec![entry("c1", "info", "one"), entry("c2", "info", "two")], None)),
        );
        let mut view = CoordinatorLog::new();
        view.reseat(&store);
        assert!(view.follow, "a log opens following its tail");

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('k')), &store), Outcome::Handled);
        assert!(!view.follow, "reading upward must disarm follow");
        assert_eq!(view.selected, Some(0));

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('G')), &store), Outcome::Handled);
        assert!(view.follow, "G re-arms follow");
        assert_eq!(view.selected, Some(1));

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('f')), &store), Outcome::Handled);
        assert!(!view.follow, "f toggles follow off");
        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('f')), &store), Outcome::Handled);
        assert!(view.follow, "f toggles follow back on");
        assert_eq!(view.selected, Some(1));
    }

    #[test]
    fn a_failed_poll_keeps_the_tail_and_names_the_fault() {
        // The plausible bug: a 501 from a host without journald (or a refused
        // connection) clears the rows, so a coordinator restart blanks a tail
        // the operator was reading instead of holding it behind the error.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_coordinator_logs(query(None), Err("connection refused".to_owned()));
        let mut view = CoordinatorLog::new();
        view.reseat(&store);
        assert!(view.entries.is_empty());
        assert!(drawn(&mut view, &store).contains("connection refused"));

        store.apply_coordinator_logs(
            query(None),
            Ok(page(vec![entry("c1", "info", "one"), entry("c2", "info", "two")], Some("c2"))),
        );
        view.reseat(&store);
        assert_eq!(view.entries.len(), 2);
        store.apply_coordinator_logs(query(Some("c2")), Err("connection refused".to_owned()));
        view.reseat(&store);
        assert_eq!(view.entries.len(), 2, "the fault must not drop retained rows");
    }

    #[test]
    fn a_multiline_message_stays_one_row_with_a_clock() {
        // The plausible bug: a MESSAGE carrying newlines paints as several
        // rows, so selection and the line count drift from the entry count.
        assert_eq!(clock(3_661_000_000), "01:01:01");
        let row = row_text(&entry("c1", "warn", "first\nsecond"));
        assert!(!row.contains('\n'), "{row}");
        assert!(row.starts_with("00:00:00  warn   first second"), "{row}");
    }
}
