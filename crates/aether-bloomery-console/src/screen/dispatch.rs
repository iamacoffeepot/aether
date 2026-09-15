//! One bloom member's dispatch attempts. Enter opens the evidence browser.

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

/// A swept dispatch has no `transcript.jsonl` on the host, so Enter would 404.
const SWEPT_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
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
    selected_retained: bool,
}

impl DispatchList {
    #[must_use]
    pub fn new(bloom: DigestHex, workpiece: impl Into<String>) -> Self {
        Self { bloom, workpiece: workpiece.into(), cursor: Cursor::new(), scroll: 0, selected_retained: true }
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
    pub fn key_hints(&self) -> &'static [KeyHint] {
        if self.selected_retained {
            HINTS
        } else {
            SWEPT_HINTS
        }
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<&String> {
        self.cursor.selected()
    }

    #[must_use]
    pub fn enter_pushes(&self, store: &Store) -> bool {
        let rows = self.rows(store);
        self.cursor
            .selected()
            .filter(|nonce| !nonce.is_empty())
            .is_some_and(|nonce| rows.iter().find(|row| &row.nonce == nonce).is_none_or(|row| row.evidence_retained))
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let rows = self.rows(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, |row| row.nonce.clone());
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, |row| row.nonce.clone());
                Outcome::Handled
            }
            KeyCode::Enter => {
                self.cursor.selected().filter(|nonce| !nonce.is_empty()).cloned().map_or(Outcome::Handled, |nonce| {
                    if rows.iter().find(|row| row.nonce == nonce).is_some_and(|row| !row.evidence_retained) {
                        Outcome::Handled
                    } else {
                        Outcome::Push(Nav::evidence(nonce))
                    }
                })
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = self.rows(store);
        self.cursor.reseat(&rows, |row| row.nonce.clone(), |_, rows| rows.first().map(|row| row.nonce.clone()));
        self.selected_retained = self.selected_row_retained(store);
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = self.rows(store);
        if self.cursor.selected().is_none() {
            self.reseat(store);
        }
        self.selected_retained = self.selected_row_retained(store);
        let dimmed = store.bloom_dispatches(self.bloom).is_some_and(super::super::store::Cell::is_stale);
        let muted = if dimmed {
            palette::body().add_modifier(Modifier::DIM)
        } else {
            palette::body()
        };
        let header = Row::new(["NONCE", "STAGE", "ATTEMPT", "VERDICT", "COST", "RETAINED", "PROVES"])
            .style(palette::body().add_modifier(Modifier::BOLD).patch(muted));
        let table_rows = if rows.is_empty() {
            vec![Row::new(["dispatches  (empty)", "", "", "", "", "", ""]).style(muted)]
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
                        Cell::from(row.coverage_label()),
                    ])
                    .style(muted)
                })
                .collect()
        };
        let proves_width = rows.iter().map(|row| row.coverage_label().chars().count()).max().unwrap_or(0).max(6);
        let table = Table::new(
            table_rows,
            [
                Constraint::Min(16),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(u16::try_from(proves_width).unwrap_or(u16::MAX)),
            ],
        )
        .style(palette::body())
        .header(header)
        .row_highlight_style(palette::cursor())
        .highlight_symbol(super::caret(self.enter_pushes(store)));
        let mut state = TableState::default()
            .with_selected(self.cursor.selected_index(&rows, |row| row.nonce.clone()))
            .with_offset(self.scroll);
        frame.render_stateful_widget(table, area, &mut state);
        self.scroll = state.offset();
    }

    fn rows(&self, store: &Store) -> Vec<BloomDispatchView> {
        let Some(page) = store.bloom_dispatches(self.bloom).and_then(|cell| cell.value.as_ref()) else {
            return Vec::new();
        };
        // "Proves this workpiece", not "is keyed on it": a grouped shared run
        // is one dispatch keyed on the composition that covers several members,
        // and filtering on equality is what left every covered member's list
        // empty while that run was the thing actually running.
        page.dispatches.iter().filter(|row| row.proves(&self.workpiece)).cloned().collect()
    }

    fn selected_row_retained(&self, store: &Store) -> bool {
        let Some(nonce) = self.cursor.selected() else {
            return true;
        };
        self.rows(store).iter().find(|row| &row.nonce == nonce).is_none_or(|row| row.evidence_retained)
    }
}

#[cfg(test)]
mod tests {
    use super::{DispatchList, HINTS, SWEPT_HINTS};
    use crate::dto::{BloomDispatchView, BloomDispatchesView, DigestHex, StageId};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::Store;
    use crate::warroom::Focus;
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

    fn shared_page(covers: Option<Vec<String>>) -> BloomDispatchesView {
        BloomDispatchesView {
            dispatches: vec![
                BloomDispatchView {
                    nonce: "dispatch-9".to_owned(),
                    workpiece: "wp-a".to_owned(),
                    stage: StageId::Construct,
                    attempt: 1,
                    evidence_retained: true,
                    covers: covers.clone().map(|_| Vec::new()),
                    ..BloomDispatchView::default()
                },
                BloomDispatchView {
                    nonce: "dispatch-9-step-0".to_owned(),
                    workpiece: "aether.bloomery.composition".to_owned(),
                    stage: StageId::AggregateVerify,
                    attempt: 1,
                    evidence_retained: true,
                    covers,
                    ..BloomDispatchView::default()
                },
            ],
        }
    }

    #[test]
    fn a_member_reaches_the_shared_run_proving_it() {
        // The plausible bug (issue 6071): the list filters on "workpiece equals
        // mine", so the step dispatch of the grouped verify that is actually
        // running — keyed on the composition — never appears, and the member's
        // lane log, gate logs and transcript stay unreachable from the board.
        let bloom = digest(1);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_bloom_dispatches(bloom, Ok(shared_page(Some(vec!["wp-a".to_owned(), "wp-b".to_owned()]))));
        let mut list = DispatchList::new(bloom, "wp-b");
        list.reseat(&store);
        assert_eq!(
            list.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::evidence("dispatch-9-step-0")),
            "the covered member opens the run proving it, not its own empty history"
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 8)).expect("test backend");
        terminal.draw(|frame| list.render(frame, frame.area(), &store)).expect("draw");
        let text: String = terminal.backend().buffer().content().iter().map(Cell::symbol).collect();
        assert!(text.contains("wp-a  wp-b"), "the row names what it proves: {text}");
        assert!(!text.contains("dispatch-9 "), "wp-b does not inherit wp-a's own attempt: {text}");
    }

    #[test]
    fn a_coordinator_without_coverage_falls_back_to_equality_and_says_so() {
        // The plausible bug: a predating coordinator serves no `covers`, and
        // reading absent coverage as empty coverage silently reports every row
        // as proving only itself — a claim the console cannot make.
        let bloom = digest(1);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_bloom_dispatches(bloom, Ok(shared_page(None)));
        let mut list = DispatchList::new(bloom, "wp-b");
        list.reseat(&store);
        assert_eq!(
            list.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Handled,
            "with coverage unknown the member falls back to its own rows, and it has none"
        );
        let mut own = DispatchList::new(bloom, "wp-a");
        own.reseat(&store);
        let mut terminal = Terminal::new(TestBackend::new(120, 8)).expect("test backend");
        terminal.draw(|frame| own.render(frame, frame.area(), &store)).expect("draw");
        let text: String = terminal.backend().buffer().content().iter().map(Cell::symbol).collect();
        assert!(text.contains("unknown"), "the row says coverage is unknown: {text}");
    }

    #[test]
    fn dispatch_list_footer_keys_are_handled() {
        // The plausible bug: the footer paints Enter while the match still
        // routes to the titled detail frame, so the advertised key is a no-op.
        let nav = Nav::focus(Focus::dispatch(digest(1), "wp-a"));
        assert_footer_honest(HINTS, |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
        assert_footer_honest(SWEPT_HINTS, |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn enter_on_a_swept_row_does_not_push_evidence() {
        // The plausible bug: Enter still pushes the evidence browser for a
        // nonce the coordinator has reclaimed, even though the row is labelled swept.
        let bloom = digest(1);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_bloom_dispatches(bloom, Ok(page()));
        let mut list = DispatchList::new(bloom, "wp-b");
        list.reseat(&store);
        assert_eq!(list.handle_key(KeyEvent::from(KeyCode::Enter), &store), Outcome::Handled);
    }

    #[test]
    fn a_swept_row_does_not_advertise_enter() {
        // Tripwire: the footer and the handler reading two different predicates —
        // an advertised Enter that the handler refuses is the same dead end in
        // the other direction.
        let bloom = digest(1);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_bloom_dispatches(bloom, Ok(page()));
        let mut swept = DispatchList::new(bloom, "wp-b");
        swept.reseat(&store);
        assert!(!swept.key_hints().iter().any(|hint| hint.keys == "Enter"));
        let mut kept = DispatchList::new(bloom, "wp-a");
        kept.reseat(&store);
        assert!(kept.key_hints().iter().any(|hint| hint.keys == "Enter"));
    }

    #[test]
    fn enter_on_a_row_produces_the_evidence_nav() {
        // The plausible bug: Enter still pushes the transcript viewer, so
        // retained files and gate verdicts stay unreachable from the list.
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
        assert_eq!(list.handle_key(KeyEvent::from(KeyCode::Enter), &store), Outcome::Push(Nav::evidence("dispatch-1")));
    }

    #[test]
    fn a_row_enter_refuses_paints_no_caret() {
        // The plausible bug: a swept row advertises `>` while Enter has
        // already decided it cannot open a transcript.
        let bloom = digest(1);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_bloom_dispatches(bloom, Ok(page()));
        let mut list = DispatchList::new(bloom, "wp-b");
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).expect("test backend");
        terminal.draw(|frame| list.render(frame, frame.area(), &store)).expect("draw");
        assert_eq!(super::super::row_caret(&terminal, "dispatch-other"), "  ");
    }
}
