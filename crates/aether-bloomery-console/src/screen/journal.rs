//! Follow-tail journal over the whole ledger: live `GET /journal` pages, a
//! server-side text filter, paging in both directions, Enter → record JSON.
//!
//! The first sample is the newest-first page (the route default). Follow then
//! re-polls `order=asc` from the highest sequence so each new fact appends
//! rather than replacing the viewport. Rows accumulate in the store keyed by
//! filter scope, so paging back through history keeps what it has loaded and a
//! tail refresh does not drop it.
//!
//! The filter rides to the route as `contains` so a search reaches records no
//! page has loaded. A coordinator that predates that parameter ignores it and
//! answers unfiltered, so the same needle is applied again over the loaded
//! rows on paint: the search then narrows what is here instead of reaching the
//! whole journal, rather than silently showing unfiltered rows. A needle that
//! is a full 64-hex digest becomes the route's `bloom` filter instead.

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
use crate::store::{JournalArchive, JournalQuery, JournalScope, ResourceKey, Store};
use crate::warroom::Focus;

const LIST_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "n/p", action: "page" },
    KeyHint { keys: "g", action: "jump" },
    KeyHint { keys: "/", action: "filter" },
    KeyHint { keys: "f", action: "follow" },
    KeyHint { keys: "G", action: "tail" },
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

/// Newest-first opening page, an ascending follow of new facts, and pages in
/// either direction under one accumulated scope.
pub struct Journal {
    bloom: Option<DigestHex>,
    filter_bloom: Option<DigestHex>,
    needle: String,
    prompt: Option<Prompt>,
    started: bool,
    tail_sample: Option<Sample>,
    follow: bool,
    cursor: Cursor<u64>,
    scroll: usize,
    pending: Option<Page>,
    /// Where the cursor lands once the pending page settles.
    landing: Option<u64>,
    oldest_reached: bool,
    last_error: Option<String>,
    notice: Option<String>,
}

/// One page the screen has asked for and is waiting on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Page {
    from_sequence: Option<u64>,
    descending: bool,
}

/// The exact tail sample already consumed. A follow re-poll rewrites the same
/// key with a fresh sample, so the query alone cannot tell "consumed" from
/// "new arrivals".
#[derive(Clone, Debug, PartialEq, Eq)]
struct Sample {
    query: JournalQuery,
    fetched: Option<Instant>,
    error: Option<String>,
}

/// A line the operator is typing: the text filter, or a sequence to jump to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Prompt {
    kind: PromptKind,
    buffer: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptKind {
    Filter,
    Jump,
}

impl PromptKind {
    fn label(self) -> &'static str {
        match self {
            Self::Filter => "/",
            Self::Jump => "goto ",
        }
    }
}

impl Journal {
    #[must_use]
    pub fn new(bloom: Option<DigestHex>) -> Self {
        Self {
            bloom,
            filter_bloom: None,
            needle: String::new(),
            prompt: None,
            started: false,
            tail_sample: None,
            follow: true,
            cursor: Cursor::new(),
            scroll: 0,
            pending: None,
            landing: None,
            oldest_reached: false,
            last_error: None,
            notice: None,
        }
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        let mut keys = vec![ResourceKey::Journal(self.tail_key())];
        if let Some(page) = self.pending_key() {
            let key = ResourceKey::Journal(page);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// The bloom the crumb names: a typed full-hex id overrides the one the
    /// frame was opened with.
    #[must_use]
    pub fn bloom(&self) -> Option<DigestHex> {
        self.filter_bloom.or(self.bloom)
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
        if self.prompt.is_some() {
            return self.handle_prompt(key);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_sel(store, 1);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.follow = false;
                self.move_sel(store, -1);
                Outcome::Handled
            }
            KeyCode::Enter => self
                .cursor
                .selected()
                .copied()
                .map_or(Outcome::Handled, |sequence| Outcome::Push(Nav::focus(Focus::record(sequence)))),
            KeyCode::Char('n') => {
                self.page_older(store);
                Outcome::Handled
            }
            KeyCode::Char('p') => {
                self.page_newer(store);
                Outcome::Handled
            }
            KeyCode::Char('g') => {
                self.prompt = Some(Prompt { kind: PromptKind::Jump, buffer: String::new() });
                Outcome::Handled
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow {
                    self.pin_tail(store);
                }
                Outcome::Handled
            }
            KeyCode::Char('G') => {
                self.follow = true;
                self.pin_tail(store);
                Outcome::Handled
            }
            KeyCode::Char('/') => {
                self.prompt = Some(Prompt { kind: PromptKind::Filter, buffer: self.needle.clone() });
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
            self.pin_tail(store);
        }
        let banner = self.dropped_banner(store);
        let banner_h = u16::from(banner.is_some());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(banner_h), Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        if let Some(banner) = banner {
            frame.render_widget(Paragraph::new(banner).style(palette::body().add_modifier(Modifier::BOLD)), chunks[0]);
        }
        self.render_list(frame, chunks[1], store);
        frame.render_widget(Paragraph::new(self.status_line(store)).style(palette::body()), chunks[2]);
    }

    /// Which accumulated journal this screen reads: the bloom and the needle,
    /// the two things the route itself filters on.
    fn scope(&self) -> JournalScope {
        JournalScope { bloom: self.bloom(), contains: self.contains() }
    }

    fn contains(&self) -> Option<String> {
        (!self.needle.is_empty()).then(|| self.needle.clone())
    }

    /// What the shell polls for. A live tail subscribes a stable key and the
    /// follow cursor rides the request path; a paused screen keeps the
    /// newest-page key, which loads once and then sits still.
    fn tail_key(&self) -> JournalQuery {
        JournalQuery {
            bloom: self.bloom(),
            from_sequence: None,
            descending: true,
            live: self.follow,
            contains: self.contains(),
        }
    }

    fn pending_key(&self) -> Option<JournalQuery> {
        self.pending.map(|page| JournalQuery {
            bloom: self.bloom(),
            from_sequence: page.from_sequence,
            descending: page.descending,
            live: false,
            contains: self.contains(),
        })
    }

    fn rows<'a>(&self, store: &'a Store) -> Vec<&'a JournalRecordView> {
        let Some(archive) = store.journal_archive(&self.scope()) else {
            return Vec::new();
        };
        archive.rows().iter().filter(|record| self.needle.is_empty() || record_matches(record, &self.needle)).collect()
    }

    /// The loaded sequence span, filter or no filter: the paging cursors are
    /// about what was fetched, not about what the paint keeps.
    fn loaded_span(&self, store: &Store) -> Option<(u64, u64)> {
        let archive = store.journal_archive(&self.scope())?;
        Some((archive.rows().first()?.sequence, archive.rows().last()?.sequence))
    }

    fn ingest(&mut self, store: &Store) {
        self.ingest_tail(store);
        self.settle_pending(store);
    }

    fn ingest_tail(&mut self, store: &Store) {
        let query = self.tail_key();
        let Some(cell) = store.journal(&query) else {
            return;
        };
        if cell.inflight || (cell.value.is_none() && cell.error.is_none()) {
            return;
        }
        let sample = Sample { query, fetched: cell.fetched_at, error: cell.error.clone() };
        if self.tail_sample.as_ref() == Some(&sample) {
            return;
        }
        self.tail_sample = Some(sample);
        if let Some(error) = &cell.error {
            self.last_error = Some(error.clone());
            return;
        }
        let Some(page) = cell.value.as_ref() else {
            return;
        };
        self.absorb(store, page);
    }

    fn settle_pending(&mut self, store: &Store) {
        let (Some(page), Some(query)) = (self.pending, self.pending_key()) else {
            return;
        };
        let Some(cell) = store.journal(&query) else {
            return;
        };
        if cell.inflight {
            return;
        }
        if let Some(error) = &cell.error {
            self.last_error = Some(error.clone());
            self.pending = None;
            self.landing = None;
            return;
        }
        let Some(fetched) = cell.value.as_ref() else {
            return;
        };
        self.pending = None;
        if page.descending && !fetched.truncated {
            self.oldest_reached = true;
        }
        self.absorb(store, fetched);
        if let Some(target) = self.landing.take() {
            self.select_near(store, target);
        }
    }

    /// Adopt one settled page's envelope. The rows themselves were merged into
    /// the scope's archive by the store, so there is nothing to copy here.
    fn absorb(&mut self, store: &Store, page: &JournalPage) {
        self.last_error = None;
        self.started = true;
        self.notice.clone_from(&page.notice);
        if self.cursor.selected().is_none() || self.follow {
            self.pin_tail(store);
        }
    }

    fn pin_tail(&mut self, store: &Store) {
        self.cursor.select(self.rows(store).last().map(|record| record.sequence));
    }

    /// Select the row at `target`, else the nearest row below it, else the
    /// oldest loaded row — so a jump into a page whose exact sequence the
    /// filter hides still lands somewhere the operator can read.
    fn select_near(&mut self, store: &Store, target: u64) {
        let rows = self.rows(store);
        let at = rows
            .iter()
            .rev()
            .find(|record| record.sequence <= target)
            .or_else(|| rows.first())
            .map(|record| record.sequence);
        if at.is_some() {
            self.follow = false;
            self.cursor.select(at);
        }
    }

    fn move_sel(&mut self, store: &Store, delta: i32) {
        let rows = self.rows(store);
        if rows.is_empty() {
            return;
        }
        let last = rows.len() - 1;
        let current = self
            .cursor
            .selected()
            .and_then(|id| rows.iter().position(|row| row.sequence == *id))
            .unwrap_or(0)
            .min(last);
        // Rows run oldest first, so stepping off the top asks for older
        // history and stepping off the bottom asks for whatever landed since.
        if delta < 0 && current == 0 {
            self.page_older(store);
            return;
        }
        if delta > 0 && current == last {
            self.page_newer(store);
            return;
        }
        let next = if delta < 0 {
            current - 1
        } else {
            current + 1
        };
        if delta < 0 {
            self.follow = false;
        }
        self.cursor.select(Some(rows[next].sequence));
    }

    fn page_older(&mut self, store: &Store) {
        if self.oldest_reached || self.pending.is_some() {
            return;
        }
        let Some((oldest, _)) = self.loaded_span(store) else {
            return;
        };
        self.follow = false;
        self.pending = Some(Page { from_sequence: Some(oldest), descending: true });
        self.landing = Some(oldest.saturating_sub(1));
    }

    fn page_newer(&mut self, store: &Store) {
        if self.follow || self.pending.is_some() {
            return;
        }
        let Some((_, newest)) = self.loaded_span(store) else {
            return;
        };
        self.pending = Some(Page { from_sequence: Some(newest), descending: false });
        self.landing = Some(newest.saturating_add(1));
    }

    /// Fetch the page holding `sequence` and land the cursor on it. The route
    /// cursor is exclusive, so asking from one past the target puts the target
    /// at the head of a newest-first page.
    fn jump_to(&mut self, sequence: u64) {
        self.pending = Some(Page { from_sequence: Some(sequence.saturating_add(1)), descending: true });
        self.landing = Some(sequence);
        self.follow = false;
        self.oldest_reached = false;
    }

    fn handle_prompt(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Enter => {
                if let Some(Prompt { kind, buffer }) = self.prompt.take() {
                    self.apply_prompt(kind, &buffer);
                }
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.buffer.pop();
                }
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.buffer.push(ch);
                }
            }
            _ => {}
        }
        Outcome::Handled
    }

    fn apply_prompt(&mut self, kind: PromptKind, buffer: &str) {
        match kind {
            PromptKind::Jump => {
                if let Ok(sequence) = buffer.trim().parse::<u64>() {
                    self.jump_to(sequence);
                }
            }
            PromptKind::Filter => self.apply_filter(buffer),
        }
    }

    /// A full 64-hex needle is the route's bloom filter; anything else is the
    /// `contains` text filter. Either way the scope changes, so the pages
    /// accumulated under the old one are released and the search restarts from
    /// the newest matching page.
    fn apply_filter(&mut self, buffer: &str) {
        let typed = buffer.trim();
        if let Some(bloom) = DigestHex::from_hex(typed) {
            self.filter_bloom = Some(bloom);
            self.needle.clear();
        } else {
            if typed.is_empty() {
                self.filter_bloom = None;
            }
            typed.clone_into(&mut self.needle);
        }
        self.pending = None;
        self.landing = None;
        self.oldest_reached = false;
        self.tail_sample = None;
        self.started = false;
        self.scroll = 0;
        self.cursor.select(None);
    }

    fn render_list(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = self.rows(store);
        let height = usize::from(area.height.max(1));
        let width = usize::from(area.width.saturating_sub(2));
        let selected = self.cursor.selected().and_then(|id| rows.iter().position(|row| row.sequence == *id));
        if self.follow {
            self.scroll = rows.len().saturating_sub(height);
        } else if let Some(index) = selected {
            if index < self.scroll {
                self.scroll = index;
            } else if index >= self.scroll.saturating_add(height) {
                self.scroll = index.saturating_add(1).saturating_sub(height);
            }
        }
        let end = self.scroll.saturating_add(height).min(rows.len());
        let highlight = selected.filter(|index| *index >= self.scroll && *index < end).map(|index| index - self.scroll);
        let mut items = Vec::new();
        for (offset, record) in rows.get(self.scroll..end).unwrap_or_default().iter().enumerate() {
            items.push(ListItem::new(row_line(record, highlight.is_some_and(|at| at == offset), width)));
        }
        if items.is_empty() {
            items.push(ListItem::new(self.empty_label()));
        }
        let list = List::new(items).style(palette::body()).highlight_symbol(super::caret(self.enter_pushes()));
        let mut state = ListState::default().with_selected(highlight);
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn dropped_banner(&self, store: &Store) -> Option<String> {
        let dropped = store.journal_archive(&self.scope()).map_or(0, JournalArchive::dropped);
        (dropped > 0).then(|| format!("{dropped} earlier lines dropped"))
    }

    fn status_line(&self, store: &Store) -> String {
        let mut parts = vec!["journal".to_owned(), format!("{} facts", self.rows(store).len())];
        if let Some(bloom) = self.bloom() {
            parts.push(bloom.prefix());
        }
        if !self.oldest_reached {
            parts.push("more".to_owned());
        }
        if self.pending.is_some() {
            parts.push("paging".to_owned());
        }
        if self.follow {
            parts.push("FOLLOW".to_owned());
        }
        if !self.needle.is_empty() {
            parts.push(format!("/{}", self.needle));
        }
        if let Some(prompt) = &self.prompt {
            parts.push(format!("{}{}_", prompt.kind.label(), prompt.buffer));
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
    loaded: bool,
}

impl Record {
    #[must_use]
    pub fn new(sequence: u64) -> Self {
        Self { sequence, offset: 0, loaded: false }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::record(self.sequence)
    }

    /// While the record is not loaded, subscribe the page that holds it: a
    /// focus link from another screen can name a sequence the journal tail has
    /// long since paged past, and the route's cursor is exclusive, so asking
    /// from one past the sequence puts it at the head of a newest-first page.
    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        if self.loaded {
            return Vec::new();
        }
        vec![ResourceKey::Journal(JournalQuery {
            bloom: None,
            from_sequence: Some(self.sequence.saturating_add(1)),
            descending: true,
            live: false,
            contains: None,
        })]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        RECORD_HINTS
    }

    pub fn reseat(&mut self, store: &Store) {
        self.loaded = store.record(self.sequence).is_some();
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
        self.reseat(store);
        let mut lines = vec![plain(format!("record  {}", self.sequence))];
        match store.record(self.sequence) {
            None => lines.push(plain("fetching the page that holds this record")),
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

/// The console's own reading of the filter, a superset of the route's
/// `contains`: the route matches the sequence, the fact and outcome variants,
/// and the idempotency key, all of which the rendered line carries, so a
/// server-filtered page never loses a row here.
fn record_matches(record: &JournalRecordView, needle: &str) -> bool {
    journal_meaning::rendered_line(record).contains(needle)
        || record.idempotency_key.contains(needle)
        || record.sequence.to_string().contains(needle)
        || journal_meaning::fact_name(record).contains(needle)
        || journal_meaning::outcome_name(record).contains(needle)
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
        JournalQuery { from_sequence: from, descending: from.is_none(), live: true, ..JournalQuery::default() }
    }

    fn sequences(view: &Journal, store: &Store) -> Vec<u64> {
        view.rows(store).iter().map(|row| row.sequence).collect()
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
            &live(None),
            Ok(page(vec![record(3, "Land", Some(digest(1)), None), record(2, "Resolve", Some(digest(1)), None)], true)),
        );
        let mut view = Journal::new(None);
        view.reseat(&store);
        assert_eq!(sequences(&view, &store), vec![2, 3]);
        let ResourceKey::Journal(next) = view.subscriptions().remove(0) else {
            panic!("the journal subscribes to the journal route");
        };
        assert_eq!(next, JournalQuery { live: true, ..JournalQuery::default() });
        assert_eq!(
            store.request_path(&ResourceKey::Journal(next)),
            "/journal?from_sequence=3&order=asc",
            "the follow cursor rides the request, not the subscription key"
        );

        store.apply_journal(
            &live(Some(3)),
            Ok(page(vec![record(4, "AttemptCompleted", Some(digest(1)), Some("wp-a"))], false)),
        );
        view.reseat(&store);
        assert_eq!(sequences(&view, &store), vec![2, 3, 4]);

        store.apply_journal(
            &live(Some(3)),
            Ok(page(vec![record(4, "AttemptCompleted", Some(digest(1)), Some("wp-a"))], false)),
        );
        view.reseat(&store);
        assert_eq!(sequences(&view, &store), vec![2, 3, 4], "a refetched tail page must dedupe on sequence");
    }

    #[test]
    fn paging_older_asks_below_the_oldest_row_and_keeps_what_is_loaded() {
        // The plausible bug (issue 6064): the older page replaces the tail
        // instead of joining it, or it is asked for from the newest sequence,
        // so `n` either blanks the screen or re-reads the page already shown
        // and the operator can never reach yesterday.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_journal(&live(None), Ok(page(vec![record(9, "Land", Some(digest(1)), None)], true)));
        let mut view = Journal::new(None);
        view.reseat(&store);

        view.handle_key(KeyEvent::from(KeyCode::Char('n')), &store);
        let older = view.pending_key().expect("n asks for a page");
        assert_eq!(older.from_sequence, Some(9), "the older page starts below the oldest loaded row");
        assert!(older.descending);
        assert!(view.subscriptions().contains(&ResourceKey::Journal(older.clone())));

        store.apply_journal(&older, Ok(page(vec![record(8, "Seal", Some(digest(1)), None)], true)));
        view.reseat(&store);
        assert_eq!(sequences(&view, &store), vec![8, 9], "the older page joins the tail rather than replacing it");
        assert_eq!(view.cursor.selected().copied(), Some(8), "the cursor lands in the page just fetched");
        assert!(view.pending.is_none(), "a settled page stops being asked for");
    }

    #[test]
    fn the_last_older_page_stops_the_walk() {
        // The plausible bug: `n` at the bottom of the journal keeps asking for
        // sequences below 1, so the console spins on an empty page forever.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_journal(&live(None), Ok(page(vec![record(2, "Land", Some(digest(1)), None)], true)));
        let mut view = Journal::new(None);
        view.reseat(&store);
        view.handle_key(KeyEvent::from(KeyCode::Char('n')), &store);
        let older = view.pending_key().expect("n asks for a page");
        store.apply_journal(&older, Ok(page(vec![record(1, "Seal", Some(digest(1)), None)], false)));
        view.reseat(&store);
        assert!(view.oldest_reached);
        view.handle_key(KeyEvent::from(KeyCode::Char('n')), &store);
        assert!(view.pending.is_none(), "an exhausted walk asks for nothing");
    }

    #[test]
    fn a_filter_rides_to_the_route_and_releases_the_pages_it_replaces() {
        // The plausible bug: the needle only narrows the rows already fetched,
        // so a search cannot reach history; or it changes the query without
        // releasing the unfiltered pages, so the filtered view still paints
        // rows the coordinator never matched.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_journal(
            &live(None),
            Ok(page(vec![record(3, "Land", Some(digest(1)), None), record(2, "Seal", Some(digest(1)), None)], true)),
        );
        let mut view = Journal::new(None);
        view.reseat(&store);
        assert_eq!(sequences(&view, &store), vec![2, 3]);

        view.handle_key(KeyEvent::from(KeyCode::Char('/')), &store);
        for ch in "Land".chars() {
            view.handle_key(KeyEvent::from(KeyCode::Char(ch)), &store);
        }
        view.handle_key(KeyEvent::from(KeyCode::Enter), &store);
        let ResourceKey::Journal(filtered) = view.subscriptions().remove(0) else {
            panic!("the journal subscribes to the journal route");
        };
        assert_eq!(filtered.contains.as_deref(), Some("Land"));
        assert_eq!(
            store.request_path(&ResourceKey::Journal(filtered.clone())),
            "/journal?contains=Land",
            "the needle is a route parameter, not a local screen filter"
        );

        store.evict_unsubscribed_journals(&view.subscriptions());
        assert_eq!(sequences(&view, &store), Vec::<u64>::new(), "the unfiltered pages are released with their scope");

        store.apply_journal(&filtered, Ok(page(vec![record(1, "Land", Some(digest(1)), None)], false)));
        view.reseat(&store);
        assert_eq!(sequences(&view, &store), vec![1], "the filtered scope accumulates its own pages");
    }

    #[test]
    fn a_needle_the_coordinator_ignores_still_narrows_the_loaded_rows() {
        // The plausible bug: `contains` is sent and trusted, so against a
        // coordinator that predates the parameter the screen paints every
        // record while claiming to be filtered.
        let mut store = Store::new(Duration::from_secs(1));
        let mut view = Journal::new(None);
        view.apply_filter("Land");
        let unfiltered = vec![record(3, "Land", Some(digest(1)), None), record(2, "Seal", Some(digest(1)), None)];
        store.apply_journal(&view.tail_key(), Ok(page(unfiltered, true)));
        view.reseat(&store);
        assert_eq!(sequences(&view, &store), vec![3]);
    }

    #[test]
    fn a_full_hex_needle_becomes_the_route_bloom_filter() {
        // The plausible bug: a pasted bloom id is sent as search text, which
        // matches nothing — the row prints the eight-character prefix, never
        // the full hex.
        let mut view = Journal::new(None);
        view.apply_filter(&digest(0xab).as_hex());
        let key = view.tail_key();
        assert_eq!(key.bloom, Some(digest(0xab)));
        assert_eq!(key.contains, None);
        assert_eq!(view.bloom(), Some(digest(0xab)));
    }

    #[test]
    fn jumping_to_a_sequence_asks_for_the_page_that_holds_it() {
        // The plausible bug: the jump asks from the sequence itself, and the
        // route's cursor is exclusive, so `g 40` lands on 39 and the record
        // the operator asked for is the one page it never fetches.
        let mut store = Store::new(Duration::from_secs(1));
        let mut view = Journal::new(None);
        view.handle_key(KeyEvent::from(KeyCode::Char('g')), &store);
        for ch in "40".chars() {
            view.handle_key(KeyEvent::from(KeyCode::Char(ch)), &store);
        }
        view.handle_key(KeyEvent::from(KeyCode::Enter), &store);
        let jump = view.pending_key().expect("g asks for a page");
        assert_eq!(jump.from_sequence, Some(41));
        assert!(jump.descending);

        store.apply_journal(&jump, Ok(page(vec![record(40, "Land", Some(digest(1)), None)], true)));
        view.reseat(&store);
        assert_eq!(view.cursor.selected().copied(), Some(40));
    }

    #[test]
    fn an_unloaded_record_subscribes_the_page_that_holds_it() {
        // The plausible bug (issue 6064): a focus link to an older sequence
        // reports the record missing instead of fetching it, so every drill-in
        // from outside the current page is a dead end.
        let mut store = Store::new(Duration::from_secs(1));
        let mut view = Record::new(40);
        view.reseat(&store);
        let ResourceKey::Journal(query) = view.subscriptions().remove(0) else {
            panic!("an unloaded record subscribes the journal route");
        };
        assert_eq!(query.from_sequence, Some(41));

        store.apply_journal(&query, Ok(page(vec![record(40, "Land", Some(digest(1)), None)], true)));
        view.reseat(&store);
        assert!(store.record(40).is_some());
        assert!(view.subscriptions().is_empty(), "a loaded record stops asking for its page");
    }

    #[test]
    fn enter_opens_the_selected_record() {
        // The plausible bug: Enter is painted and does nothing, so the JSON
        // screen the operator already has is unreachable from the live tail.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_journal(&live(None), Ok(page(vec![record(2, "Land", Some(digest(1)), None)], false)));
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
