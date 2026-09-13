//! Ranged viewer for one retained evidence file.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::Value;

use crate::dto::DispatchFilePage;
use crate::keys::{KeyHint, Outcome};
use crate::palette;
use crate::store::{DispatchFileQuery, ResourceKey, Store};
use crate::warroom::Focus;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "scroll" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Retained complete lines. A verify log can dwarf the viewport; the cap
/// keeps the frame from growing without bound while still covering a
/// typical evidence.json.
const LINE_CAP: usize = 8_192;

/// One retained evidence file, paged from byte zero through the files route.
pub struct EvidenceFile {
    nonce: String,
    name: String,
    lines: Vec<String>,
    have: u64,
    started: bool,
    truncated: bool,
    offset: usize,
    error: Option<String>,
}

impl EvidenceFile {
    #[must_use]
    pub fn new(nonce: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            nonce: nonce.into(),
            name: name.into(),
            lines: Vec::new(),
            have: 0,
            started: false,
            truncated: false,
            offset: 0,
            error: None,
        }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::evidence_file(&self.nonce, &self.name)
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        if self.truncated {
            return Vec::new();
        }
        vec![ResourceKey::DispatchFile(self.query())]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
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
            KeyCode::Char('r') => {
                self.reset();
                Outcome::Refresh
            }
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        self.ingest(store);
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        self.ingest(store);
        let mut lines = vec![plain(format!("file  {}  {}", self.name, self.nonce))];
        if let Some(error) = &self.error {
            lines.push(plain(error.clone()));
        }
        if !self.started {
            lines.push(plain("loading"));
        } else if self.lines.is_empty() {
            lines.push(plain("file  (empty)"));
        } else {
            lines.extend(present_lines(&self.lines));
        }
        if self.truncated {
            lines.push(plain("truncated"));
        }
        self.offset = super::super::json::clamp_wrapped_scroll(self.offset, &lines, area.width);
        let offset = u16::try_from(self.offset).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(lines).style(palette::body()).wrap(Wrap { trim: false }).scroll((offset, 0)),
            area,
        );
    }

    fn query(&self) -> DispatchFileQuery {
        DispatchFileQuery { nonce: self.nonce.clone(), name: self.name.clone(), cursor: Some(self.have) }
    }

    fn ingest(&mut self, store: &Store) {
        let Some(cell) = store.dispatch_file(&self.query()) else {
            return;
        };
        if let Some(error) = &cell.error {
            self.error = Some(error.clone());
        }
        let Some(page) = cell.value.as_ref() else {
            return;
        };
        self.apply_page(page);
    }

    fn apply_page(&mut self, page: &DispatchFilePage) {
        self.error = None;
        if !self.started {
            self.have = page.cursor;
            self.started = true;
        }
        if page.cursor != self.have {
            return;
        }
        let room = LINE_CAP.saturating_sub(self.lines.len());
        if page.lines.len() > room {
            self.lines.extend_from_slice(&page.lines[..room]);
            self.truncated = true;
            return;
        }
        self.lines.extend_from_slice(&page.lines);
        match page.next_cursor {
            Some(next) => self.have = next,
            None => self.have = page.length,
        }
    }

    fn reset(&mut self) {
        self.lines.clear();
        self.have = 0;
        self.started = false;
        self.truncated = false;
        self.offset = 0;
        self.error = None;
    }
}

fn present_lines(lines: &[String]) -> Vec<Line<'static>> {
    if let Ok(value) = serde_json::from_str::<Value>(&lines.join("\n")) {
        return super::super::json::present(&value);
    }
    lines.iter().map(|line| plain(line.clone())).collect()
}

fn plain(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), palette::body()))
}

#[cfg(test)]
mod tests {
    use super::{EvidenceFile, HINTS};
    use crate::dto::DispatchFilePage;
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::{DispatchFileQuery, Store};
    use crate::warroom::Focus;
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use std::time::Duration;

    fn query(name: &str, cursor: u64) -> DispatchFileQuery {
        DispatchFileQuery { nonce: "dispatch-1".to_owned(), name: name.to_owned(), cursor: Some(cursor) }
    }

    fn page(lines: &[&str], cursor: u64, next: Option<u64>) -> DispatchFilePage {
        let length = next.unwrap_or_else(|| {
            cursor
                .saturating_add(lines.iter().map(|line| u64::try_from(line.len()).unwrap_or(0).saturating_add(1)).sum())
        });
        DispatchFilePage {
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            cursor,
            next_cursor: next,
            length,
            notice: None,
        }
    }

    fn store_with(name: &str, page: DispatchFilePage) -> Store {
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_dispatch_file(query(name, 0), Ok(page));
        store
    }

    fn drawn(view: &mut EvidenceFile, store: &Store) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test backend");
        terminal.draw(|frame| view.render(frame, frame.area(), store)).expect("draw");
        terminal.backend().buffer().content().iter().map(Cell::symbol).collect()
    }

    #[test]
    fn a_json_file_paints_as_highlighted_fields() {
        // The plausible bug: evidence.json is dumped as raw joined lines, so
        // a gate verdict is unreadable and a pretty-printed object loses its
        // keys.
        let store = store_with("evidence.json", page(&[r#"{"status":"fail","command":"verify.check"}"#], 0, None));
        let mut view = EvidenceFile::new("dispatch-1", "evidence.json");
        let text = drawn(&mut view, &store);
        assert!(text.contains("status"), "{text}");
        assert!(text.contains("fail"), "{text}");
        assert!(text.contains("verify.check"), "{text}");
    }

    #[test]
    fn a_text_file_paints_its_lines() {
        // The plausible bug: a log that is not JSON is forced through the
        // JSON presenter and paints as a single error blob.
        let store = store_with("verify.clippy.log", page(&["error: unused", "error: unwrap"], 0, None));
        let mut view = EvidenceFile::new("dispatch-1", "verify.clippy.log");
        let text = drawn(&mut view, &store);
        assert!(text.contains("error: unused"), "{text}");
        assert!(text.contains("error: unwrap"), "{text}");
    }

    #[test]
    fn a_second_page_appends_instead_of_replacing() {
        // The plausible bug: each files-route page replaces the buffer, so
        // the operator only ever sees the last slice of a long log.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_dispatch_file(query("verify.clippy.log", 0), Ok(page(&["one"], 0, Some(4))));
        store.apply_dispatch_file(query("verify.clippy.log", 4), Ok(page(&["two"], 4, None)));
        let mut view = EvidenceFile::new("dispatch-1", "verify.clippy.log");
        view.reseat(&store);
        view.reseat(&store);
        let text = drawn(&mut view, &store);
        assert!(text.contains("one"), "{text}");
        assert!(text.contains("two"), "{text}");
    }

    #[test]
    fn file_footer_keys_are_handled() {
        let nav = Nav::evidence_file("dispatch-1", "evidence.json");
        assert_footer_honest(HINTS, |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn focus_names_the_file() {
        assert_eq!(
            EvidenceFile::new("dispatch-1", "evidence.json").focus(),
            Focus::evidence_file("dispatch-1", "evidence.json")
        );
    }
}
