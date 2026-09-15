//! Follow-tail journal: live `GET /journal` pages, `/` search, Enter → record JSON.
//!
//! The first sample is the newest-first page (the route default). Follow then
//! re-polls `order=asc` from the highest sequence so each new fact appends
//! rather than replacing the viewport. Search is client-side over the retained
//! rows; bloom identity, when set, is a server filter on the query.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::journal_meaning;
use crate::cursor::Cursor;
use crate::dto::{DigestHex, JournalPage, JournalRecordView};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{JournalQuery, ResourceKey, Store};
use crate::warroom::Focus;

const LIST_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "f", action: "follow" },
    KeyHint { keys: "G", action: "tail" },
    KeyHint { keys: "/", action: "search" },
    KeyHint { keys: "n/N", action: "next" },
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

/// Retained rows. The route pages at most a thousand per read; the screen
/// keeps the same bound so a long follow cannot grow without limit.
const RETENTION: usize = 1_000;

/// Newest-first opening page, then an ascending follow of new facts.
pub struct Journal {
    bloom: Option<DigestHex>,
    records: Vec<JournalRecordView>,
    have: Option<u64>,
    started: bool,
    ingested: Option<Ingested>,
    follow: bool,
    cursor: Cursor<u64>,
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
    query: JournalQuery,
    fetched: Option<Instant>,
    error: Option<String>,
}

#[derive(Default)]
struct Search {
    editing: bool,
    needle: String,
    matches: Vec<u64>,
    at: Option<usize>,
}

impl Journal {
    #[must_use]
    pub fn new(bloom: Option<DigestHex>) -> Self {
        Self {
            bloom,
            records: Vec::new(),
            have: None,
            started: false,
            ingested: None,
            follow: true,
            cursor: Cursor::new(),
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
        vec![ResourceKey::Journal(self.query())]
    }

    #[must_use]
    pub fn bloom(&self) -> Option<DigestHex> {
        self.bloom
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        LIST_HINTS
    }

    #[must_use]
    pub fn enter_pushes(&self) -> bool {
        self.cursor.selected().is_some()
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
            KeyCode::Enter => self
                .cursor
                .selected()
                .copied()
                .map_or(Outcome::Handled, |sequence| Outcome::Push(Nav::focus(Focus::record(sequence)))),
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

    fn query(&self) -> JournalQuery {
        JournalQuery { bloom: self.bloom, from_sequence: self.have, descending: self.have.is_none(), live: self.follow }
    }

    fn ingest(&mut self, store: &Store) {
        let query = self.query();
        let Some(cell) = store.journal(query) else {
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

    fn apply_page(&mut self, page: &JournalPage) {
        self.last_error = None;
        self.started = true;
        self.notice.clone_from(&page.notice);
        self.truncated = page.truncated;
        let mut incoming = page.records.clone();
        if self.have.is_none() {
            incoming.reverse();
        }
        for record in incoming {
            if self.records.iter().any(|known| known.sequence == record.sequence) {
                continue;
            }
            self.records.push(record);
        }
        self.records.sort_by_key(|record| record.sequence);
        let excess = self.records.len().saturating_sub(RETENTION);
        if excess > 0 {
            self.records.drain(..excess);
            self.dropped = self.dropped.saturating_add(excess);
            self.scroll = self.scroll.saturating_sub(excess);
        }
        if let Some(max) = self.records.last().map(|record| record.sequence) {
            self.have = Some(max);
        }
        self.rescan();
        if self.cursor.selected().is_none() || self.follow {
            self.pin_tail();
        }
    }

    fn pin_tail(&mut self) {
        self.cursor.select(self.records.last().map(|record| record.sequence));
    }

    fn move_sel(&mut self, delta: i32) {
        if self.records.is_empty() {
            return;
        }
        let last = self.records.len() - 1;
        let current = self
            .cursor
            .selected()
            .and_then(|id| self.records.iter().position(|row| row.sequence == *id))
            .unwrap_or(0)
            .min(last);
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(last)
        };
        if next < current {
            self.follow = false;
        }
        self.cursor.select(Some(self.records[next].sequence));
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
        self.search
            .matches
            .extend(self.records.iter().filter(|record| record_matches(record, &needle)).map(|record| record.sequence));
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
        self.cursor.select(Some(self.search.matches[next]));
        if self.cursor.selected() != self.records.last().map(|record| &record.sequence) {
            self.follow = false;
        }
    }

    fn render_list(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let height = usize::from(area.height.max(1));
        let width = usize::from(area.width.saturating_sub(2));
        if self.follow {
            self.scroll = self.records.len().saturating_sub(height);
        } else if let Some(index) = self.cursor.selected_index(&self.records, |record| record.sequence) {
            if index < self.scroll {
                self.scroll = index;
            } else if index >= self.scroll.saturating_add(height) {
                self.scroll = index.saturating_add(1).saturating_sub(height);
            }
        }
        let end = self.scroll.saturating_add(height).min(self.records.len());
        let highlight = self
            .cursor
            .selected_index(&self.records, |record| record.sequence)
            .filter(|index| *index >= self.scroll && *index < end)
            .map(|index| index - self.scroll);
        let mut items = Vec::new();
        for (offset, record) in self.records.get(self.scroll..end).unwrap_or_default().iter().enumerate() {
            let selected = highlight.is_some_and(|at| at == offset);
            items.push(ListItem::new(row_line(record, selected, width)));
        }
        if items.is_empty() {
            items.push(ListItem::new(self.empty_label()));
        }
        let list = List::new(items).style(palette::body()).highlight_symbol(super::caret(self.enter_pushes()));
        let mut state = ListState::default().with_selected(highlight);
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn dropped_banner(&self) -> Option<String> {
        (self.dropped > 0).then(|| format!("{} earlier lines dropped", self.dropped))
    }

    fn status_line(&self) -> String {
        let mut parts = vec!["journal".to_owned(), format!("{} facts", self.records.len())];
        if let Some(bloom) = self.bloom {
            parts.push(bloom.prefix());
        }
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
            return format!("journal  {error}");
        }
        if self.started {
            "journal  (empty)".to_owned()
        } else {
            "journal  loading".to_owned()
        }
    }
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
                let mean = journal_meaning::meaning(record);
                lines.push(plain(format!("meaning  {mean}")));
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

fn record_matches(record: &JournalRecordView, needle: &str) -> bool {
    journal_meaning::rendered_line(record).contains(needle) || record.idempotency_key.contains(needle)
}

/// One fixed-column row. The selected row paints whole in the cursor style so
/// the selection stays visible; other rows tint the fact by class and refused
/// or failed outcomes in the warning colour.
fn row_line(record: &JournalRecordView, selected: bool, width: usize) -> Line<'static> {
    if selected {
        let text = journal_meaning::truncate_ellipsis(&journal_meaning::rendered_line(record), width.max(1));
        return Line::from(Span::styled(text, palette::cursor()));
    }
    truncate_line(full_line(record), width.max(1))
}

fn full_line(record: &JournalRecordView) -> Line<'static> {
    use journal_meaning::{
        BLOOM_WIDTH, FACT_WIDTH, MEMBER_WIDTH, OUTCOME_WIDTH, SEQ_WIDTH, TIME_WIDTH, bloom_prefix, fact_name,
        fact_role, format_cell, format_cell_right, meaning, member_name, outcome_is_warning, outcome_name,
        recorded_time,
    };
    let fact = fact_name(record);
    let outcome = outcome_name(record);
    let sequence = format_cell_right(&record.sequence.to_string(), SEQ_WIDTH);
    let time = format_cell(&recorded_time(record), TIME_WIDTH);
    let fact_cell = format_cell(&fact, FACT_WIDTH);
    let bloom = format_cell(&bloom_prefix(record), BLOOM_WIDTH);
    let member = format_cell(&member_name(record), MEMBER_WIDTH);
    let outcome_cell = format_cell(&outcome, OUTCOME_WIDTH);
    let mean = meaning(record);
    let fact_style = Style::default().fg(palette::color(fact_role(&fact)));
    let outcome_style = if outcome_is_warning(&outcome) {
        Style::default().fg(palette::color(palette::Role::Loud))
    } else {
        palette::body()
    };
    Line::from(vec![
        Span::styled(format!("{sequence} "), palette::body()),
        Span::styled(format!("{time} "), palette::body()),
        Span::styled(format!("{fact_cell} "), fact_style),
        Span::styled(format!("{bloom} "), palette::body()),
        Span::styled(format!("{member} "), palette::body()),
        Span::styled(format!("{outcome_cell} "), outcome_style),
        Span::styled(mean, palette::body()),
    ])
}

/// Clip a multi-span line to `width` characters, keeping each span's style.
fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    let mut kept = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        if kept >= width {
            break;
        }
        let count = span.content.chars().count();
        let room = width.saturating_sub(kept);
        if count <= room {
            kept += count;
            spans.push(span);
        } else {
            let content: String = span.content.chars().take(room).collect();
            spans.push(Span::styled(content, span.style));
            kept += room;
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::super::journal_meaning;
    use super::{Journal, Record};
    use crate::dto::{DigestHex, JournalPage, JournalRecordView};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::{JournalQuery, ResourceKey, Store};
    use crate::warroom::Focus;
    use crossterm::event::{KeyCode, KeyEvent};
    use serde_json::json;
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn record(sequence: u64, fact: &str, bloom: Option<DigestHex>, workpiece: Option<&str>) -> JournalRecordView {
        let mut body = serde_json::Map::new();
        if let Some(bloom) = bloom {
            body.insert("bloom".to_owned(), json!(bloom.as_hex()));
        }
        if let Some(workpiece) = workpiece {
            body.insert("workpiece".to_owned(), json!(workpiece));
        }
        JournalRecordView {
            sequence,
            idempotency_key: format!("{fact}:{sequence}"),
            event: json!({ "idempotency_key": format!("{fact}:{sequence}"), "fact": { fact: body } }),
            outcome: json!({ "outcome": "Applied" }),
            decider: "test".to_owned(),
            recorded_unix_millis: None,
        }
    }

    fn land_record(sequence: u64, bloom: DigestHex, previous: DigestHex, new: DigestHex) -> JournalRecordView {
        JournalRecordView {
            sequence,
            idempotency_key: format!("Land:{sequence}"),
            event: json!({
                "idempotency_key": format!("Land:{sequence}"),
                "fact": { "Land": { "bloom": bloom.as_hex(), "new_head": new.as_hex() } },
            }),
            outcome: json!({
                "Landed": {
                    "bloom": bloom.as_hex(),
                    "previous_base": previous.as_hex(),
                    "new_head": new.as_hex(),
                },
            }),
            decider: "test".to_owned(),
            recorded_unix_millis: Some(3_723_000),
        }
    }

    fn page(records: Vec<JournalRecordView>, truncated: bool) -> JournalPage {
        JournalPage { shown: u64::try_from(records.len()).unwrap_or(0), truncated, records, ..JournalPage::default() }
    }

    fn live(from: Option<u64>) -> JournalQuery {
        JournalQuery { from_sequence: from, descending: from.is_none(), live: true, bloom: None }
    }

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

    #[test]
    fn follow_appends_the_next_page_without_duplicating() {
        // The plausible bug: re-ingesting the opening newest-first page, or
        // following `next_from_sequence` backward, doubles the tail or never
        // shows a fact that landed after the first sample.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_journal(
            live(None),
            Ok(page(vec![record(3, "Land", Some(digest(1)), None), record(2, "Resolve", Some(digest(1)), None)], true)),
        );
        let mut view = Journal::new(None);
        view.reseat(&store);
        assert_eq!(view.records.iter().map(|row| row.sequence).collect::<Vec<_>>(), vec![2, 3]);
        let ResourceKey::Journal(next) = view.subscriptions().pop().expect("one subscription") else {
            panic!("the journal subscribes to the journal route");
        };
        assert!(!next.descending);
        assert_eq!(next.from_sequence, Some(3));
        assert_eq!(next.path(), "/journal?from_sequence=3&order=asc");

        store.apply_journal(
            live(Some(3)),
            Ok(page(vec![record(4, "AttemptCompleted", Some(digest(1)), Some("wp-a"))], false)),
        );
        view.reseat(&store);
        assert_eq!(view.records.iter().map(|row| row.sequence).collect::<Vec<_>>(), vec![2, 3, 4]);

        store.apply_journal(
            live(Some(3)),
            Ok(page(vec![record(4, "AttemptCompleted", Some(digest(1)), Some("wp-a"))], false)),
        );
        view.reseat(&store);
        assert_eq!(
            view.records.iter().map(|row| row.sequence).collect::<Vec<_>>(),
            vec![2, 3, 4],
            "a refetched tail page must dedupe on sequence"
        );
    }

    #[test]
    fn enter_opens_the_selected_record() {
        // The plausible bug: Enter is painted and does nothing, so the JSON
        // screen the operator already has is unreachable from the live tail.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_journal(live(None), Ok(page(vec![record(2, "Land", Some(digest(1)), None)], false)));
        let mut view = Journal::new(None);
        view.reseat(&store);
        assert_eq!(
            view.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::focus(Focus::record(2)))
        );
    }

    #[test]
    fn fixed_columns_name_the_fact_bloom_and_member() {
        // The plausible bug: the row is sequence-only, or it prints the full
        // hex, so the tail cannot be scanned for the bloom and member the
        // operator is watching.
        let row = record(9, "AttemptCompleted", Some(digest(0xab)), Some("issue-5932"));
        let line = journal_meaning::rendered_line(&row);
        assert!(line.contains("AttemptCompleted"), "{line}");
        assert!(line.contains(&digest(0xab).prefix()), "{line}");
        assert!(line.contains("issue-5932"), "{line}");
        assert!(!line.contains(&digest(0xab).as_hex()), "full hex crowds the row: {line}");
    }

    #[test]
    fn land_renders_the_landed_template_with_both_heads() {
        // The plausible bug: the row says what happened only in the
        // coordinator's vocabulary (`Land Landed`) with no statement of what
        // it means for the bloom.
        let row = land_record(11, digest(1), digest(0xf5), digest(0x1e));
        let mean = journal_meaning::meaning(&row);
        assert!(mean.contains(&digest(0xf5).prefix()), "previous head missing: {mean}");
        assert!(mean.contains(&digest(0x1e).prefix()), "new head missing: {mean}");
        assert!(mean.contains("landed"), "{mean}");
        let line = journal_meaning::rendered_line(&row);
        assert!(line.contains(&mean), "search matches the rendered line including the meaning: {line}");
        assert!(line.contains("01:02:03"), "recorded time column reads HH:MM:SS: {line}");
    }

    #[test]
    fn unknown_fact_renders_the_outcome_name() {
        // The plausible bug: a new fact blanks the meaning column because no
        // template names it yet.
        let row = record(12, "Frobnicate", Some(digest(2)), None);
        assert_eq!(journal_meaning::meaning(&row), "Applied");
    }

    #[test]
    fn bloom_column_starts_at_the_same_column_for_different_length_members() {
        // Tripwire: fixed columns keep the bloom at one offset no matter how
        // long the member name runs; variable separators let it drift.
        let short = record(13, "AttemptCompleted", Some(digest(3)), Some("wp-a"));
        let long = record(14, "AttemptCompleted", Some(digest(3)), Some("a-much-longer-workpiece-name"));
        let short_line = journal_meaning::rendered_line(&short);
        let long_line = journal_meaning::rendered_line(&long);
        let prefix = digest(3).prefix();
        let short_at = short_line.find(&prefix).expect("bloom prefix renders");
        let long_at = long_line.find(&prefix).expect("bloom prefix renders");
        assert_eq!(short_at, long_at, "bloom drifted: {short_line} vs {long_line}");
        assert_eq!(short_at, 5 + 1 + 8 + 1 + 30 + 1, "bloom follows the fixed sequence, time, and fact columns");
    }
}
