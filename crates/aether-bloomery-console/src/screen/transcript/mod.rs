//! Transcript viewer: follow-tail, incremental search, one row per event.

mod buffer;
mod event;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::dto::DispatchFilePage;
use crate::keys::{KeyHint, Outcome};
use crate::palette;
use crate::store::{PromptQuery, ResourceKey, Store, TranscriptQuery};
use crate::warroom::Focus;

pub use buffer::{DEFAULT_CAP, LineBuffer};

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "expand" },
    KeyHint { keys: "f", action: "follow" },
    KeyHint { keys: "G", action: "tail" },
    KeyHint { keys: "/", action: "search" },
    KeyHint { keys: "n/N", action: "next" },
    KeyHint { keys: "</>", action: "pan" },
    KeyHint { keys: "p", action: "prompt" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

const SEARCH_BUDGET: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Transcript,
    Prompt,
}

impl Pane {
    fn toggle(self) -> Self {
        match self {
            Self::Transcript => Self::Prompt,
            Self::Prompt => Self::Transcript,
        }
    }
}

#[derive(Default)]
struct PromptPane {
    lines: Vec<String>,
    have: u64,
    started: bool,
    scroll: usize,
    error: Option<String>,
}

impl PromptPane {
    fn apply_page(&mut self, page: &DispatchFilePage) {
        self.error = None;
        if !self.started {
            self.have = page.cursor;
            self.started = true;
        }
        if page.cursor != self.have {
            return;
        }
        self.lines.extend_from_slice(&page.lines);
        self.have = page.next_cursor.unwrap_or(page.length);
    }

    fn empty_label(&self) -> String {
        if let Some(error) = &self.error {
            return format!("prompt  {error}");
        }
        if self.started {
            "prompt  (empty)".to_owned()
        } else {
            "prompt  loading".to_owned()
        }
    }
}

/// One dispatch's streamed transcript.
pub struct Transcript {
    nonce: String,
    buffer: LineBuffer,
    have: u64,
    started: bool,
    follow: bool,
    selected: Option<u64>,
    scroll: usize,
    pan: usize,
    expanded: Option<u64>,
    expand_scroll: usize,
    search: Search,
    last_error: Option<String>,
    pane: Pane,
    prompt: PromptPane,
}

#[derive(Default)]
struct Search {
    editing: bool,
    needle: String,
    matches: Vec<u64>,
    at: Option<usize>,
    scan_at: usize,
}

impl Transcript {
    #[must_use]
    pub fn new(nonce: impl Into<String>) -> Self {
        Self::with_cap(nonce, DEFAULT_CAP)
    }

    #[must_use]
    pub fn with_cap(nonce: impl Into<String>, cap: usize) -> Self {
        Self {
            nonce: nonce.into(),
            buffer: LineBuffer::with_cap(cap),
            have: 0,
            started: false,
            follow: false,
            selected: None,
            scroll: 0,
            pan: 0,
            expanded: None,
            expand_scroll: 0,
            search: Search::default(),
            last_error: None,
            pane: Pane::Transcript,
            prompt: PromptPane::default(),
        }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::transcript(&self.nonce)
    }

    #[must_use]
    pub fn follow(&self) -> bool {
        self.follow
    }

    #[must_use]
    pub fn parse_count(&self) -> usize {
        self.buffer.parse_count()
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        if self.pane == Pane::Prompt {
            vec![ResourceKey::Prompt(self.prompt_query())]
        } else {
            vec![ResourceKey::Transcript(self.query())]
        }
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    /// Enter expands the selected line in place; it does not push a frame.
    #[must_use]
    pub fn enter_pushes() -> bool {
        false
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        if self.search.editing {
            return self.handle_search_edit(key);
        }
        if let Some(id) = self.expanded {
            return self.handle_expanded(key, id);
        }
        if self.pane == Pane::Prompt {
            return self.handle_prompt(key);
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_sel(1);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.disarm();
                self.move_sel(-1);
                Outcome::Handled
            }
            KeyCode::Enter => {
                self.expanded = self.selected;
                self.expand_scroll = 0;
                Outcome::Handled
            }
            KeyCode::Char('f' | 'G') => {
                self.arm();
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
            KeyCode::Char('>') | KeyCode::Right => {
                self.pan = self.pan.saturating_add(4);
                Outcome::Handled
            }
            KeyCode::Char('<') | KeyCode::Left => {
                self.pan = self.pan.saturating_sub(4);
                Outcome::Handled
            }
            KeyCode::Char('p') => {
                self.pane = self.pane.toggle();
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    fn handle_prompt(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char('p') => {
                self.pane = self.pane.toggle();
                Outcome::Handled
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.prompt.scroll = self.prompt.scroll.saturating_add(1);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.prompt.scroll = self.prompt.scroll.saturating_sub(1);
                Outcome::Handled
            }
            KeyCode::Char('>') | KeyCode::Right => {
                self.pan = self.pan.saturating_add(4);
                Outcome::Handled
            }
            KeyCode::Char('<') | KeyCode::Left => {
                self.pan = self.pan.saturating_sub(4);
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            KeyCode::Enter | KeyCode::Char('f' | 'G' | '/' | 'n' | 'N') => Outcome::Handled,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        self.ingest(store);
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        self.ingest(store);
        if self.pane == Pane::Transcript {
            self.scan_matches(SEARCH_BUDGET);
            if self.follow {
                self.pin_tail();
            }
        }

        let banner = match self.pane {
            Pane::Transcript => self.buffer.banner(),
            Pane::Prompt => None,
        };
        let banner_h = u16::from(banner.is_some());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(banner_h), Constraint::Min(1), Constraint::Length(1)])
            .split(area);

        if let Some(banner) = banner {
            frame.render_widget(Paragraph::new(banner).style(palette::body().add_modifier(Modifier::BOLD)), chunks[0]);
        }

        match self.pane {
            Pane::Prompt => self.render_prompt(frame, chunks[1]),
            Pane::Transcript => {
                if let Some(id) = self.expanded {
                    self.render_expanded(frame, chunks[1], id);
                } else {
                    self.render_list(frame, chunks[1]);
                }
            }
        }
        frame.render_widget(Paragraph::new(self.status_line()).style(palette::body()), chunks[2]);
    }

    fn query(&self) -> TranscriptQuery {
        let cursor = self.started.then_some(self.have);
        TranscriptQuery { nonce: self.nonce.clone(), cursor, live: self.follow }
    }

    fn prompt_query(&self) -> PromptQuery {
        PromptQuery { nonce: self.nonce.clone(), cursor: self.prompt.started.then_some(self.prompt.have) }
    }

    fn ingest(&mut self, store: &Store) {
        if self.pane == Pane::Prompt {
            self.ingest_prompt(store);
            return;
        }
        let query = self.query();
        let Some(cell) = store.transcript(&query) else {
            return;
        };
        if let Some(error) = &cell.error {
            self.last_error = Some(error.clone());
        }
        let Some(page) = cell.value.as_ref() else {
            return;
        };
        self.apply_page(page);
    }

    fn ingest_prompt(&mut self, store: &Store) {
        let Some(cell) = store.prompt(&self.prompt_query()) else {
            return;
        };
        if let Some(error) = &cell.error {
            self.prompt.error = Some(error.clone());
        }
        let Some(page) = cell.value.as_ref() else {
            return;
        };
        self.prompt.apply_page(page);
    }

    fn apply_page(&mut self, page: &DispatchFilePage) {
        self.last_error = None;
        if !self.started {
            self.have = page.cursor;
            self.started = true;
        }
        if page.cursor != self.have {
            return;
        }
        let before = self.buffer.dropped();
        for line in &page.lines {
            self.buffer.push_line(line);
        }
        self.have = page.next_cursor.unwrap_or(page.length);
        self.forget_trimmed(self.buffer.dropped().saturating_sub(before));
        if self.selected.is_none() || self.follow {
            self.pin_tail();
        }
    }

    fn forget_trimmed(&mut self, extra: usize) {
        if extra == 0 {
            return;
        }
        let floor = self.buffer.abs_id(0);
        self.search.matches.retain(|&id| id >= floor);
        if self.search.scan_at > extra {
            self.search.scan_at -= extra;
        } else {
            self.search.scan_at = 0;
        }
        if let Some(id) = self.selected
            && id < floor
        {
            self.selected = Some(floor);
        }
        if let Some(id) = self.expanded
            && id < floor
        {
            self.expanded = None;
        }
    }

    fn arm(&mut self) {
        self.follow = true;
        self.pin_tail();
    }

    fn disarm(&mut self) {
        self.follow = false;
    }

    fn pin_tail(&mut self) {
        self.selected = self.buffer.last_id();
    }

    fn move_sel(&mut self, delta: i8) {
        let Some(index) = self.selected.and_then(|id| self.buffer.index_of(id)).or_else(|| {
            if self.buffer.is_empty() {
                None
            } else {
                Some(0)
            }
        }) else {
            return;
        };
        let next = if delta < 0 {
            index.saturating_sub(1)
        } else {
            index.saturating_add(1).min(self.buffer.len().saturating_sub(1))
        };
        if next < index {
            self.disarm();
        }
        self.selected = Some(self.buffer.abs_id(next));
    }

    fn handle_search_edit(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                self.search = Search::default();
                Outcome::Handled
            }
            KeyCode::Enter => {
                self.search.editing = false;
                Outcome::Handled
            }
            KeyCode::Backspace => {
                self.search.needle.pop();
                self.reset_matches();
                Outcome::Handled
            }
            KeyCode::Char('n') if !self.search.needle.is_empty() => {
                self.step_match(1);
                Outcome::Handled
            }
            KeyCode::Char('N') if !self.search.needle.is_empty() => {
                self.step_match(-1);
                Outcome::Handled
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                self.search.needle.push(ch);
                self.reset_matches();
                Outcome::Handled
            }
            _ => Outcome::Handled,
        }
    }

    fn handle_expanded(&mut self, key: KeyEvent, _id: u64) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.expanded = None;
                Outcome::Handled
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.expand_scroll = self.expand_scroll.saturating_add(1);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.expand_scroll = self.expand_scroll.saturating_sub(1);
                Outcome::Handled
            }
            KeyCode::Char('q') => Outcome::Quit,
            KeyCode::Char('r') => Outcome::Refresh,
            _ => Outcome::Handled,
        }
    }

    fn reset_matches(&mut self) {
        self.search.matches.clear();
        self.search.at = None;
        self.search.scan_at = 0;
    }

    fn step_match(&mut self, dir: i8) {
        self.scan_matches(SEARCH_BUDGET);
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
        if self.buffer.last_id() != self.selected {
            self.disarm();
        }
    }

    fn scan_matches(&mut self, budget: usize) {
        if self.search.needle.is_empty() {
            return;
        }
        let needle = self.search.needle.clone();
        let mut scanned = 0;
        while self.search.scan_at < self.buffer.len() && scanned < budget {
            let index = self.search.scan_at;
            if self.buffer.collapsed(index).is_some_and(|text| text.contains(&needle)) {
                self.search.matches.push(self.buffer.abs_id(index));
            }
            self.search.scan_at += 1;
            scanned += 1;
        }
    }

    fn render_list(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let height = usize::from(area.height.max(1));
        let width = usize::from(area.width.saturating_sub(2));
        let selected = self.selected.and_then(|id| self.buffer.index_of(id));
        if self.follow {
            self.scroll = self.buffer.len().saturating_sub(height);
        } else if let Some(index) = selected {
            if index < self.scroll {
                self.scroll = index;
            } else if index >= self.scroll.saturating_add(height) {
                self.scroll = index.saturating_add(1).saturating_sub(height);
            }
        }
        let end = self.scroll.saturating_add(height).min(self.buffer.len());
        let mut items = Vec::new();
        for index in self.scroll..end {
            let text = self.buffer.collapsed(index).unwrap_or("");
            items.push(ListItem::new(truncate(text, self.pan, width)));
        }
        if items.is_empty() {
            items.push(ListItem::new(self.empty_label()));
        }
        let highlight = selected.map(|index| index.saturating_sub(self.scroll));
        let list = List::new(items)
            .style(palette::body())
            .highlight_style(palette::cursor())
            .highlight_symbol(super::caret(Self::enter_pushes()));
        let mut state = ListState::default().with_selected(highlight);
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_expanded(&self, frame: &mut Frame<'_>, area: Rect, id: u64) {
        let raw = self.buffer.index_of(id).and_then(|index| self.buffer.raw(index));
        if let Some(raw) = raw
            && let Some(value) = event::expand_value(raw)
        {
            let lines = super::json::present(&value);
            let offset = u16::try_from(self.expand_scroll.min(lines.len().saturating_sub(1))).unwrap_or(u16::MAX);
            frame.render_widget(
                Paragraph::new(lines).style(palette::body()).wrap(Wrap { trim: false }).scroll((offset, 0)),
                area,
            );
            return;
        }
        let body = raw.map_or_else(|| "line is gone".to_owned(), event::expand);
        let lines = wrap(&body, usize::from(area.width.max(1)));
        let offset = self.expand_scroll.min(lines.len().saturating_sub(1));
        let items: Vec<ListItem> = lines.into_iter().skip(offset).map(ListItem::new).collect();
        frame.render_widget(List::new(items).style(palette::body()), area);
    }

    fn empty_label(&self) -> String {
        if let Some(error) = &self.last_error {
            return format!("transcript  {error}");
        }
        if self.started {
            "transcript  (empty)".to_owned()
        } else {
            "transcript  loading".to_owned()
        }
    }

    fn render_prompt(&self, frame: &mut Frame<'_>, area: Rect) {
        let height = usize::from(area.height.max(1));
        let width = usize::from(area.width.saturating_sub(2));
        let offset = self.prompt.scroll.min(self.prompt.lines.len().saturating_sub(1));
        let end = offset.saturating_add(height).min(self.prompt.lines.len());
        let mut items = Vec::new();
        for line in self.prompt.lines.iter().skip(offset).take(end.saturating_sub(offset)) {
            items.push(ListItem::new(truncate(line, self.pan, width)));
        }
        if items.is_empty() {
            items.push(ListItem::new(self.prompt.empty_label()));
        }
        frame.render_widget(List::new(items).style(palette::body()), area);
    }

    fn status_line(&self) -> String {
        let (file, count) = match self.pane {
            Pane::Prompt => ("prompt.md", self.prompt.lines.len()),
            Pane::Transcript => ("transcript.jsonl", self.buffer.len()),
        };
        let mut parts = vec![file.to_owned(), self.nonce.clone(), format!("{count} lines")];
        if self.pane == Pane::Transcript {
            if self.follow {
                parts.push("FOLLOW".to_owned());
            }
            if !self.search.needle.is_empty() {
                let at = self.search.at.map_or(0, |index| index + 1);
                parts.push(format!("{at}/{}  /{}", self.search.matches.len(), self.search.needle));
            }
            if self.search.editing {
                parts.push("_".to_owned());
            }
        }
        parts.join("  ")
    }
}

fn truncate(text: &str, pan: usize, width: usize) -> String {
    text.chars().skip(pan).take(width.max(1)).collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut rest = paragraph;
        while !rest.is_empty() {
            let end = rest
                .char_indices()
                .nth(width.saturating_sub(1))
                .map_or(rest.len(), |(index, ch)| index + ch.len_utf8());
            lines.push(rest[..end].to_owned());
            rest = &rest[end..];
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::Transcript;
    use crate::dto::DispatchFilePage;
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::{PromptQuery, ResourceKey, Store, TranscriptQuery};
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    fn store_with(page: DispatchFilePage) -> Store {
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_transcript(TranscriptQuery { nonce: "dispatch-1".to_owned(), cursor: None, live: false }, Ok(page));
        store
    }

    fn store_with_prompt(transcript: DispatchFilePage, prompt: DispatchFilePage) -> Store {
        let mut store = store_with(transcript);
        store.apply_prompt(PromptQuery { nonce: "dispatch-1".to_owned(), cursor: None }, Ok(prompt));
        store
    }

    fn drawn(view: &mut Transcript, store: &Store) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| view.render(frame, frame.area(), store)).expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area().height {
            for x in 0..buffer.area().width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        text
    }

    fn page(lines: &[&str]) -> DispatchFilePage {
        let joined: u64 = lines.iter().map(|line| u64::try_from(line.len()).unwrap_or(0).saturating_add(1)).sum();
        DispatchFilePage {
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            cursor: 0,
            next_cursor: None,
            length: joined,
            notice: None,
        }
    }

    #[test]
    fn follow_disarms_on_upward_motion_and_rearms_on_g() {
        // The plausible bug: follow stays armed through k, so new lines yank
        // the viewport back to the tail while the operator is reading up.
        let store = store_with(page(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"one"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"three"}]}}"#,
        ]));
        let mut view = Transcript::new("dispatch-1");
        view.reseat(&store);
        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('f')), &store), Outcome::Handled);
        assert!(view.follow(), "f arms follow");

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('k')), &store), Outcome::Handled);
        assert!(!view.follow(), "upward motion must disarm");

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('G')), &store), Outcome::Handled);
        assert!(view.follow(), "G re-arms follow");
        assert_eq!(view.selected, view.buffer.last_id());
    }

    #[test]
    fn search_walks_rendered_collapsed_lines() {
        // The plausible bug: search walks raw JSON, so a field name hits every
        // event and a collapsed preview the operator can see is unfindable.
        let store = store_with(page(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"alpha"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"beta"}]}}"#,
        ]));
        let mut view = Transcript::new("dispatch-1");
        view.reseat(&store);
        view.handle_key(KeyEvent::from(KeyCode::Char('/')), &store);
        for ch in ['b', 'e', 't', 'a'] {
            view.handle_key(KeyEvent::from(KeyCode::Char(ch)), &store);
        }
        view.scan_matches(64);
        assert_eq!(view.search.matches.len(), 1, "the needle must match the collapsed preview, not raw JSON keys");
        view.handle_key(KeyEvent::from(KeyCode::Char('n')), &store);
        assert_eq!(view.selected, Some(view.buffer.abs_id(1)));
    }

    #[test]
    fn a_draw_parses_only_the_visible_span() {
        // Same laziness pin as the buffer, through the paint path: a tall
        // fixture must not JSON-decode rows the viewport never shows.
        let lines: Vec<String> = (0..400)
            .map(|index| {
                format!(r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{index}"}}]}}}}"#)
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let store = store_with(page(&refs));
        let mut view = Transcript::new("dispatch-1");
        view.reseat(&store);
        assert_eq!(view.parse_count(), 0);

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| view.render(frame, frame.area(), &store)).expect("draw");
        assert!(view.parse_count() <= 10, "parsed {} rows for an 8-row viewport", view.parse_count());
    }

    #[test]
    fn the_dropped_head_banner_is_painted() {
        let store = store_with(page(&["one", "two", "three"]));
        let mut view = Transcript::with_cap("dispatch-1", 2);
        view.reseat(&store);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| view.render(frame, frame.area(), &store)).expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area().height {
            for x in 0..buffer.area().width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(text.contains("1 earlier lines dropped"), "{text}");
    }

    #[test]
    fn transcript_footer_keys_are_handled() {
        assert_footer_honest(Transcript::key_hints(), |code| {
            Shell::probe(Nav::transcript("dispatch-1")).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn transcript_footer_advertises_the_prompt_key() {
        assert!(
            Transcript::key_hints().iter().any(|hint| hint.keys == "p"),
            "the transcript footer must advertise p so the prompt pane is discoverable"
        );
    }

    #[test]
    fn p_swaps_the_pane_to_the_prompt() {
        // Tripwire: the subscription following the visible pane — if it does
        // not, the prompt never gets fetched, or the transcript keeps polling
        // while the prompt is on screen.
        let store = store_with_prompt(page(&["hello-from-transcript"]), page(&["hello-from-prompt"]));
        let mut view = Transcript::new("dispatch-1");
        view.reseat(&store);
        let text = drawn(&mut view, &store);
        assert!(text.contains("hello-from-transcript"), "{text}");
        assert!(!text.contains("hello-from-prompt"), "{text}");

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('p')), &store), Outcome::Handled);
        assert!(
            view.subscriptions().iter().any(|key| matches!(key, ResourceKey::Prompt(_))),
            "visible prompt pane must subscribe to the prompt resource"
        );
        let text = drawn(&mut view, &store);
        assert!(text.contains("hello-from-prompt"), "{text}");
        assert!(text.contains("prompt.md"), "{text}");
        assert!(!text.contains("hello-from-transcript"), "{text}");

        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('p')), &store), Outcome::Handled);
        let text = drawn(&mut view, &store);
        assert!(text.contains("hello-from-transcript"), "{text}");
        assert!(text.contains("transcript.jsonl"), "{text}");
        assert!(!text.contains("hello-from-prompt"), "{text}");
    }

    #[test]
    fn a_paged_prompt_accumulates_rather_than_replacing() {
        // Tripwire: the cursor guard — dropping it lets an out-of-order page
        // duplicate or truncate the prompt.
        let first = DispatchFilePage {
            lines: vec!["# Task".to_owned(), "do the thing".to_owned()],
            cursor: 0,
            next_cursor: Some(20),
            length: 40,
            notice: None,
        };
        let second = DispatchFilePage {
            lines: vec!["more prompt".to_owned()],
            cursor: 20,
            next_cursor: None,
            length: 40,
            notice: None,
        };
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_prompt(PromptQuery { nonce: "dispatch-1".to_owned(), cursor: None }, Ok(first));
        let mut view = Transcript::new("dispatch-1");
        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('p')), &store), Outcome::Handled);
        view.reseat(&store);
        store.apply_prompt(PromptQuery { nonce: "dispatch-1".to_owned(), cursor: Some(20) }, Ok(second));
        view.reseat(&store);
        assert_eq!(view.prompt.lines, ["# Task", "do the thing", "more prompt"]);
    }
}
