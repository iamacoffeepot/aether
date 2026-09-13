//! One dispatch's retained evidence: header, gate verdicts, and the file list.

mod file;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use serde_json::Value;

use crate::cursor::Cursor;
use crate::dto::{DispatchEvidenceView, DispatchFilePage};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette::{self, Role};
use crate::store::{Cell, DispatchFileQuery, ResourceKey, Store};
use crate::warroom::Focus;

pub use file::EvidenceFile;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

const EMPTY_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

const EVIDENCE_JSON: &str = "evidence.json";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// One dispatch's evidence directory as `GET /dispatches/{nonce}` served it.
#[derive(Clone, Debug)]
pub struct Evidence {
    nonce: String,
    cursor: Cursor<String>,
    scroll: usize,
    wants_gates: bool,
    has_files: bool,
}

impl Evidence {
    #[must_use]
    pub fn new(nonce: impl Into<String>) -> Self {
        Self { nonce: nonce.into(), cursor: Cursor::new(), scroll: 0, wants_gates: false, has_files: false }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::evidence(&self.nonce)
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        let mut keys = vec![ResourceKey::Dispatch(self.nonce.clone())];
        if self.wants_gates {
            keys.push(ResourceKey::DispatchFile(gates_query(&self.nonce)));
        }
        keys
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        if self.has_files {
            HINTS
        } else {
            EMPTY_HINTS
        }
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<&String> {
        self.cursor.selected()
    }

    #[must_use]
    pub fn enter_pushes(&self) -> bool {
        self.cursor.selected().is_some_and(|name| !name.is_empty())
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let files = self.files(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&files, Clone::clone);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&files, Clone::clone);
                Outcome::Handled
            }
            KeyCode::Enter => {
                self.cursor.selected().cloned().map_or(Outcome::Handled, |name| open_file(&self.nonce, &name))
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let files = self.files(store);
        self.has_files = !files.is_empty();
        self.wants_gates = self
            .header(store)
            .is_some_and(|header| header.retained && header.files.iter().any(|name| name == EVIDENCE_JSON));
        self.cursor.reseat(&files, Clone::clone, |_, files| files.first().cloned());
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        if self.cursor.selected().is_none() {
            self.reseat(store);
        }
        let gates = self.gates(store);
        let header = header_lines(self.header(store), gates.as_ref(), &self.nonce);
        let header_h = u16::try_from(header.len()).unwrap_or(u16::MAX).max(1);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(header_h), Constraint::Min(1)])
            .split(area);
        frame.render_widget(Paragraph::new(header).style(palette::body()), chunks[0]);
        self.render_files(frame, chunks[1], store);
    }

    fn render_files(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let files = self.files(store);
        let muted = if store.dispatch(&self.nonce).is_some_and(Cell::is_stale) {
            palette::body().add_modifier(Modifier::DIM)
        } else {
            palette::body()
        };
        let items = if files.is_empty() {
            vec![ListItem::new("files  (empty)").style(muted)]
        } else {
            files.iter().map(|name| ListItem::new(name.clone()).style(muted)).collect()
        };
        let list = List::new(items)
            .style(palette::body())
            .highlight_style(palette::cursor())
            .highlight_symbol(super::caret(self.enter_pushes()));
        let mut state = ListState::default()
            .with_selected(self.cursor.selected_index(&files, Clone::clone))
            .with_offset(self.scroll);
        frame.render_stateful_widget(list, area, &mut state);
        self.scroll = state.offset();
    }

    fn header<'a>(&self, store: &'a Store) -> Option<&'a DispatchEvidenceView> {
        store.dispatch(&self.nonce).and_then(|cell| cell.value.as_ref())
    }

    fn files(&self, store: &Store) -> Vec<String> {
        self.header(store).map(|header| header.files.clone()).unwrap_or_default()
    }

    fn gates(&self, store: &Store) -> Option<Value> {
        let page = store.dispatch_file(&gates_query(&self.nonce)).and_then(|cell| cell.value.as_ref())?;
        parse_gates(page)
    }
}

fn gates_query(nonce: &str) -> DispatchFileQuery {
    DispatchFileQuery { nonce: nonce.to_owned(), name: EVIDENCE_JSON.to_owned(), cursor: Some(0) }
}

fn open_file(nonce: &str, name: &str) -> Outcome {
    if name.is_empty() {
        return Outcome::Handled;
    }
    if name == TRANSCRIPT_FILE {
        Outcome::Push(Nav::transcript(nonce))
    } else {
        Outcome::Push(Nav::evidence_file(nonce, name))
    }
}

fn header_lines(header: Option<&DispatchEvidenceView>, gates: Option<&Value>, nonce: &str) -> Vec<Line<'static>> {
    let Some(header) = header else {
        return vec![plain(format!("evidence  {nonce}  loading"))];
    };
    let mut lines = vec![identity_line(header)];
    if let Some(notice) = header.notice.as_deref().filter(|text| !text.is_empty()) {
        lines.push(plain(format!("notice  {notice}")));
    }
    if let Some(archived) = header.archived.as_deref().filter(|text| !text.is_empty()) {
        lines.push(plain(format!("archived  {archived}")));
    }
    if let Some(commit) = first_line(header.commit_message.as_deref()) {
        lines.push(plain(format!("commit  {commit}")));
    }
    lines.extend(gate_lines(gates));
    lines
}

fn identity_line(header: &DispatchEvidenceView) -> Line<'static> {
    let (word, role) = if header.retained {
        ("kept", Role::Settled)
    } else {
        ("swept", Role::Attention)
    };
    Line::from(vec![
        Span::styled(format!("evidence  {}  ", header.nonce), palette::body()),
        Span::styled(word, palette::paint(role)),
    ])
}

fn gate_lines(value: Option<&Value>) -> Vec<Line<'static>> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    if let Some(status) = status_line(value) {
        lines.push(status);
    }
    if let Some(failed) = failed_line(value) {
        lines.push(failed);
    }
    if let Some(gates) = receipts_line(value) {
        lines.push(gates);
    }
    lines
}

fn status_line(value: &Value) -> Option<Line<'static>> {
    let status = value.get("status").and_then(Value::as_str)?;
    let role = match status {
        "pass" => Role::Settled,
        "fail" => Role::Loud,
        _ => Role::Attention,
    };
    let command = value.get("command").and_then(Value::as_str);
    let mut spans =
        vec![Span::styled("status  ", palette::body()), Span::styled(status.to_owned(), palette::paint(role))];
    if let Some(command) = command {
        spans.push(Span::styled(format!(" · {command}"), palette::body()));
    }
    Some(Line::from(spans))
}

fn failed_line(value: &Value) -> Option<Line<'static>> {
    let names: Vec<&str> =
        value.get("failed_verifiers").and_then(Value::as_array)?.iter().filter_map(Value::as_str).collect();
    if names.is_empty() {
        return None;
    }
    Some(Line::from(Span::styled(format!("failed  {}", names.join("  ")), palette::paint(Role::Loud))))
}

fn receipts_line(value: &Value) -> Option<Line<'static>> {
    let parts: Vec<String> = value.get("gates").and_then(Value::as_array)?.iter().filter_map(gate_receipt).collect();
    if parts.is_empty() {
        return None;
    }
    Some(plain(format!("gates  {}", parts.join("  "))))
}

fn gate_receipt(value: &Value) -> Option<String> {
    let command = value.get("command")?.as_str()?;
    Some(
        value
            .get("duration_millis")
            .and_then(Value::as_u64)
            .map_or_else(|| command.to_owned(), |millis| format!("{command} {millis} millis")),
    )
}

fn parse_gates(page: &DispatchFilePage) -> Option<Value> {
    serde_json::from_str(&page.lines.join("\n")).ok()
}

fn first_line(text: Option<&str>) -> Option<&str> {
    let text = text?.trim();
    if text.is_empty() {
        None
    } else {
        text.lines().next()
    }
}

fn plain(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), palette::body()))
}

#[cfg(test)]
mod tests {
    use super::{EMPTY_HINTS, EVIDENCE_JSON, Evidence, HINTS, TRANSCRIPT_FILE};
    use crate::dto::{DispatchEvidenceView, DispatchFilePage};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::{DispatchFileQuery, ResourceKey, Store};
    use crate::warroom::Focus;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use std::time::Duration;

    fn store_with(header: DispatchEvidenceView, gates: Option<&str>) -> Store {
        let mut store = Store::new(Duration::from_secs(1));
        let nonce = header.nonce.clone();
        store.apply_dispatch(nonce.clone(), Ok(header));
        if let Some(body) = gates {
            store.apply_dispatch_file(
                DispatchFileQuery { nonce, name: EVIDENCE_JSON.to_owned(), cursor: Some(0) },
                Ok(DispatchFilePage {
                    lines: vec![body.to_owned()],
                    cursor: 0,
                    next_cursor: None,
                    length: u64::try_from(body.len()).unwrap_or(0),
                    notice: None,
                }),
            );
        }
        store
    }

    fn drawn(view: &mut Evidence, store: &Store) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).expect("test backend");
        terminal.draw(|frame| view.render(frame, frame.area(), store)).expect("draw");
        terminal.backend().buffer().content().iter().map(Cell::symbol).collect()
    }

    #[test]
    fn the_browser_paints_files_and_gate_verdicts() {
        // The plausible bug: Enter still opens only the transcript, so
        // evidence.json's failed verifiers and the other retained files never
        // reach the operator even though the header already lists them.
        let store = store_with(
            DispatchEvidenceView {
                nonce: "dispatch-1".to_owned(),
                retained: true,
                commit_message: Some("feat(console): browse evidence\n".to_owned()),
                files: vec![EVIDENCE_JSON.to_owned(), "verify.clippy.log".to_owned(), TRANSCRIPT_FILE.to_owned()],
                ..DispatchEvidenceView::default()
            },
            Some(
                r#"{"command":"verify.check","status":"fail","failed_verifiers":["verify.clippy"],"gates":[{"command":"verify.fmt","duration_millis":10},{"command":"verify.clippy","duration_millis":400}]}"#,
            ),
        );
        let mut view = Evidence::new("dispatch-1");
        view.reseat(&store);
        let text = drawn(&mut view, &store);
        assert!(text.contains("dispatch-1"), "{text}");
        assert!(text.contains("kept"), "{text}");
        assert!(text.contains("feat(console): browse evidence"), "{text}");
        assert!(text.contains("fail"), "{text}");
        assert!(text.contains("verify.check"), "{text}");
        assert!(text.contains("verify.clippy"), "{text}");
        assert!(text.contains("verify.fmt 10 millis"), "{text}");
        assert!(text.contains("verify.clippy.log"), "{text}");
        assert!(text.contains(TRANSCRIPT_FILE), "{text}");
        assert!(text.contains(EVIDENCE_JSON), "{text}");
    }

    #[test]
    fn enter_on_transcript_jsonl_still_opens_the_session_viewer() {
        // The plausible bug: every retained name, including the session
        // transcript, is forced through the generic files viewer.
        let store = store_with(
            DispatchEvidenceView {
                nonce: "dispatch-1".to_owned(),
                retained: true,
                files: vec![TRANSCRIPT_FILE.to_owned(), "prompt.md".to_owned()],
                ..DispatchEvidenceView::default()
            },
            None,
        );
        let mut view = Evidence::new("dispatch-1");
        view.reseat(&store);
        assert_eq!(
            view.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::transcript("dispatch-1"))
        );
        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Char('j')), &store), Outcome::Handled);
        assert_eq!(
            view.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::evidence_file("dispatch-1", "prompt.md"))
        );
    }

    #[test]
    fn a_swept_header_does_not_advertise_enter() {
        // Tripwire: the footer paints Enter while the file list is empty, so
        // the advertised key is a no-op on a reclaimed directory.
        let store = store_with(
            DispatchEvidenceView {
                nonce: "dispatch-1".to_owned(),
                retained: false,
                notice: Some("evidence directory was reclaimed".to_owned()),
                ..DispatchEvidenceView::default()
            },
            None,
        );
        let mut view = Evidence::new("dispatch-1");
        view.reseat(&store);
        assert!(!view.key_hints().iter().any(|hint| hint.keys == "Enter"));
        assert_eq!(view.handle_key(KeyEvent::from(KeyCode::Enter), &store), Outcome::Handled);
        let text = drawn(&mut view, &store);
        assert!(text.contains("swept"), "{text}");
        assert!(text.contains("evidence directory was reclaimed"), "{text}");
    }

    #[test]
    fn evidence_footer_keys_are_handled() {
        let nav = Nav::evidence("dispatch-1");
        assert_footer_honest(HINTS, |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
        assert_footer_honest(EMPTY_HINTS, |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn subscriptions_follow_a_listed_evidence_json() {
        // The plausible bug: the browser never asks for evidence.json, so
        // gate verdicts stay blank on a retained verify run.
        let store = store_with(
            DispatchEvidenceView {
                nonce: "dispatch-1".to_owned(),
                retained: true,
                files: vec![EVIDENCE_JSON.to_owned()],
                ..DispatchEvidenceView::default()
            },
            None,
        );
        let mut view = Evidence::new("dispatch-1");
        assert_eq!(view.subscriptions(), vec![ResourceKey::Dispatch("dispatch-1".to_owned())]);
        view.reseat(&store);
        assert_eq!(
            view.subscriptions(),
            vec![
                ResourceKey::Dispatch("dispatch-1".to_owned()),
                ResourceKey::DispatchFile(DispatchFileQuery {
                    nonce: "dispatch-1".to_owned(),
                    name: EVIDENCE_JSON.to_owned(),
                    cursor: Some(0),
                }),
            ]
        );
    }

    #[test]
    fn focus_names_the_nonce() {
        assert_eq!(Evidence::new("dispatch-1").focus(), Focus::evidence("dispatch-1"));
    }
}
