//! One bloom member's dispatch attempts. Enter opens the transcript viewer.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::{BloomDispatchView, DigestHex};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

use super::metrics::format_micro_usd;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// One member's attempts on a bloom, as `GET /blooms/{id}/dispatches` served them.
#[derive(Clone, Debug)]
pub struct DispatchList {
    bloom: DigestHex,
    workpiece: String,
    cursor: Cursor<String>,
    scroll: usize,
}

impl DispatchList {
    #[must_use]
    pub fn new(bloom: DigestHex, workpiece: impl Into<String>) -> Self {
        Self { bloom, workpiece: workpiece.into(), cursor: Cursor::new(), scroll: 0 }
    }

    #[must_use]
    pub fn focus(&self) -> Focus {
        Focus::dispatch(self.bloom, self.workpiece.clone())
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::BloomDispatches(self.bloom)]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<&String> {
        self.cursor.selected()
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let rows = self.rows(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, row_id);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, row_id);
                Outcome::Handled
            }
            KeyCode::Enter => self
                .cursor
                .selected()
                .filter(|nonce| !nonce.is_empty())
                .cloned()
                .map_or(Outcome::Handled, |nonce| Outcome::Push(Nav::transcript(nonce))),
            KeyCode::Esc => Outcome::Handled,
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = self.rows(store);
        self.cursor.reseat(&rows, row_id, |_, rows| rows.first().map(row_id));
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = self.rows(store);
        if self.cursor.selected().is_none() {
            self.reseat(store);
        }
        let dimmed = store.bloom_dispatches(self.bloom).is_some_and(super::super::store::Cell::is_stale);
        let muted = if dimmed {
            palette::body().add_modifier(Modifier::DIM)
        } else {
            palette::body()
        };
        let header = Row::new(["NONCE", "STAGE", "ATTEMPT", "VERDICT", "COST", "RETAINED"])
            .style(palette::body().add_modifier(Modifier::BOLD).patch(muted));
        let table_rows = if rows.is_empty() {
            vec![Row::new(["dispatches  (empty)", "", "", "", "", ""]).style(muted)]
        } else {
            rows.iter()
                .map(|row| {
                    Row::new([
                        Cell::from(row.nonce.clone()),
                        Cell::from(row.stage.to_string()),
                        Cell::from(row.attempt.to_string()),
                        Cell::from(row.verdict.clone().unwrap_or_else(|| "—".to_owned())),
                        Cell::from(row.cost.map_or_else(|| "—".to_owned(), format_micro_usd)),
                        Cell::from(if row.evidence_retained {
                            "kept"
                        } else {
                            "swept"
                        }),
                    ])
                    .style(muted)
                })
                .collect()
        };
        let table = Table::new(
            table_rows,
            [
                Constraint::Min(16),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Length(8),
            ],
        )
        .style(palette::body())
        .header(header)
        .row_highlight_style(palette::cursor())
        .highlight_symbol("> ");
        let mut state =
            TableState::default().with_selected(self.cursor.selected_index(&rows, row_id)).with_offset(self.scroll);
        frame.render_stateful_widget(table, area, &mut state);
        self.scroll = state.offset();
    }

    fn rows<'a>(&self, store: &'a Store) -> Vec<&'a BloomDispatchView> {
        let Some(page) = store.bloom_dispatches(self.bloom).and_then(|cell| cell.value.as_ref()) else {
            return Vec::new();
        };
        page.dispatches.iter().filter(|row| row.workpiece == self.workpiece).collect()
    }
}

fn row_id(row: &&BloomDispatchView) -> String {
    row.nonce.clone()
}

#[cfg(test)]
mod tests {
    use super::DispatchList;
    use crate::dto::{BloomDispatchView, BloomDispatchesView, DigestHex, StageId};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::store::Store;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn page() -> BloomDispatchesView {
        serde_json::from_str(
            r#"{
                "dispatches": [
                    {
                        "nonce": "dispatch-keep",
                        "workpiece": "wp-a",
                        "stage": "Construct",
                        "attempt": 2,
                        "verdict": "pass",
                        "cost": 1000000,
                        "evidence_retained": true
                    },
                    {
                        "nonce": "dispatch-other",
                        "workpiece": "wp-b",
                        "stage": "Verify",
                        "attempt": 1,
                        "verdict": "fail",
                        "evidence_retained": false
                    }
                ]
            }"#,
        )
        .expect("served dispatch-list JSON")
    }

    #[test]
    fn dispatch_list_renders_a_served_page() {
        // The plausible bug: the list fetches the document but paints only
        // workpiece, so nonce / stage / attempt / verdict / cost / retention
        // never reach the operator, or a sibling member's lap is mixed in.
        let bloom = digest(1);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_bloom_dispatches(bloom, Ok(page()));
        let mut list = DispatchList::new(bloom, "wp-a");
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).expect("test backend");
        terminal.draw(|frame| list.render(frame, frame.area(), &store)).expect("draw");
        let text: String = terminal.backend().buffer().content().iter().map(Cell::symbol).collect();
        assert!(text.contains("dispatch-keep"), "{text}");
        assert!(text.contains("Construct"), "{text}");
        assert!(text.contains('2'), "{text}");
        assert!(text.contains("pass"), "{text}");
        assert!(text.contains("$1"), "{text}");
        assert!(text.contains("kept"), "{text}");
        assert!(!text.contains("dispatch-other"), "{text}");
        assert!(!text.contains("Verify"), "{text}");
    }

    #[test]
    fn dispatch_list_footer_keys_are_handled() {
        // The plausible bug: the footer paints Enter while the match still
        // routes to the titled detail frame, so the advertised key is a no-op.
        let store = Store::new(Duration::from_secs(1));
        let mut list = DispatchList::new(digest(1), "wp-a");
        assert_footer_honest(DispatchList::key_hints(), |code| {
            list.handle_key(KeyEvent::from(code), &store) != Outcome::Ignored
        });
    }

    #[test]
    fn enter_on_a_row_produces_the_transcript_nav() {
        // The plausible bug: Enter still pushes Focus::Dispatch, so the
        // transcript viewer is never constructed from a list row.
        let bloom = digest(1);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_bloom_dispatches(
            bloom,
            Ok(BloomDispatchesView {
                dispatches: vec![BloomDispatchView {
                    nonce: "dispatch-1".to_owned(),
                    workpiece: "wp-a".to_owned(),
                    stage: StageId::Construct,
                    attempt: 1,
                    evidence_retained: true,
                    ..BloomDispatchView::default()
                }],
            }),
        );
        let mut list = DispatchList::new(bloom, "wp-a");
        list.reseat(&store);
        assert_eq!(
            list.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::transcript("dispatch-1"))
        );
    }
}
