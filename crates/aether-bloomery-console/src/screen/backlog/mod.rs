//! Backlog pane over the coordinator's commission read routes.
//!
//! Data sources are those routes only. There is no GitHub client in this
//! crate to call — a GitHub number is a dim annotation parsed from a
//! canonical `issue-<N>` id, never a fetch path.

mod forest;
mod intent;
mod label;
mod standing;
mod workpiece;

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::{CommissionHeadView, DigestHex};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{CommissionCapability, ResourceKey, Store};
use crate::warroom::Focus;

use forest::{Forest, cycle_line, forest};
use intent::first_line;
use label::{annotation_text, workpiece_key};

pub use workpiece::Workpiece;

/// Coordinator paths this pane may request. A GitHub host cannot appear here.
#[cfg(test)]
pub const COORDINATOR_PATHS: &[&str] = &["/commissions", "/commissions/{id}", "/artifacts/{digest}/decoded"];

const PREDATING: &str = "this coordinator predates the commission store";

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// The open-workpiece list and its dependency forest.
#[derive(Clone, Debug, Default)]
pub struct Backlog {
    cursor: Cursor<String>,
    scroll: usize,
    listed: Vec<String>,
    intents: Vec<DigestHex>,
}

impl Backlog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        let mut keys = vec![ResourceKey::Commissions];
        for id in &self.listed {
            keys.push(ResourceKey::Commission(id.clone()));
        }
        for digest in &self.intents {
            if *digest != DigestHex::default() {
                keys.push(ResourceKey::Artifact(*digest));
            }
        }
        keys
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<&String> {
        self.cursor.selected()
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> Option<DigestHex> {
        let id = self.cursor.selected()?;
        let index = self.listed.iter().position(|listed| listed == id)?;
        self.intents.get(index).copied().filter(|digest| *digest != DigestHex::default())
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        if store.commission_capability() == Some(CommissionCapability::Absent) {
            return match key.code {
                KeyCode::Char('j' | 'k') | KeyCode::Down | KeyCode::Up | KeyCode::Enter => Outcome::Handled,
                KeyCode::Char('r') => Outcome::Refresh,
                KeyCode::Char('q') => Outcome::Quit,
                _ => Outcome::Ignored,
            };
        }
        let rows = rows_from(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, |row| row.id.clone());
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, |row| row.id.clone());
                Outcome::Handled
            }
            KeyCode::Enter => self
                .cursor
                .selected()
                .cloned()
                .map_or(Outcome::Handled, |id| Outcome::Push(Nav::focus(Focus::workpiece(id)))),
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        self.refresh_listed(store);
        let rows = rows_from(store);
        self.cursor.reseat(&rows, |row| row.id.clone(), |_, rows| rows.first().map(|row| row.id.clone()));
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        self.refresh_listed(store);
        if store.commission_capability() == Some(CommissionCapability::Absent) {
            frame.render_widget(Paragraph::new(PREDATING), area);
            return;
        }
        if store.commissions().value.is_none() {
            let message = store.commissions().error.clone().unwrap_or_else(|| "loading".to_owned());
            frame.render_widget(Paragraph::new(message), area);
            return;
        }

        let walked = forest_from(store);
        let band_height = u16::try_from(walked.cycles.len().min(4)).unwrap_or(0);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(band_height), Constraint::Min(3)])
            .split(area);
        if band_height > 0 {
            let bands: Vec<Line> = walked.cycles.iter().map(|cycle| Line::from(cycle_line(cycle))).collect();
            frame.render_widget(Paragraph::new(bands).style(palette::body().add_modifier(Modifier::BOLD)), chunks[0]);
        }

        let rows = rows_from(store);
        let table_rows = rows.iter().map(|row| {
            let indent = "  ".repeat(row.depth);
            let annotation = annotation_text(&row.id).unwrap_or_default();
            Row::new([
                Cell::from(Line::from(vec![
                    Span::raw(format!("{indent}{}", row.id)),
                    Span::styled(format!("  {annotation}"), palette::body().add_modifier(Modifier::DIM)),
                ])),
                Cell::from(row.status.clone()),
                Cell::from(row.intent.clone()),
            ])
        });
        let table = Table::new(table_rows, [Constraint::Length(28), Constraint::Length(10), Constraint::Min(16)])
            .style(palette::body())
            .header(Row::new(["WORKPIECE", "STATE", "INTENT"]).style(palette::body().add_modifier(Modifier::BOLD)))
            .row_highlight_style(palette::cursor())
            .highlight_symbol("> ");
        let mut state = TableState::default()
            .with_selected(self.cursor.selected_index(&rows, |row| row.id.clone()))
            .with_offset(self.scroll);
        frame.render_stateful_widget(table, chunks[1], &mut state);
        self.scroll = state.offset();
    }

    fn refresh_listed(&mut self, store: &Store) {
        if store.commission_capability() == Some(CommissionCapability::Absent) {
            self.listed.clear();
            self.intents.clear();
            return;
        }
        let Some(list) = store.commissions().value.as_ref() else {
            return;
        };
        self.listed = list.commissions.iter().map(|head| head.id.clone()).collect();
        self.intents = list.commissions.iter().map(|head| head.intent).collect();
    }
}

struct ListRow {
    id: String,
    depth: usize,
    status: String,
    intent: String,
}

fn rows_from(store: &Store) -> Vec<ListRow> {
    let Some(list) = store.commissions().value.as_ref() else {
        return Vec::new();
    };
    let walked = forest_from(store);
    let mut by_id: HashMap<&str, &CommissionHeadView> = HashMap::new();
    for head in &list.commissions {
        by_id.insert(head.id.as_str(), head);
    }
    walked
        .rows
        .iter()
        .filter_map(|row| {
            let head = by_id.get(row.id.as_str())?;
            Some(ListRow {
                id: row.id.clone(),
                depth: row.depth,
                status: head.status.clone(),
                intent: intent_line(store, head),
            })
        })
        .collect()
}

fn forest_from(store: &Store) -> Forest {
    let Some(list) = store.commissions().value.as_ref() else {
        return Forest::default();
    };
    let mut ids: Vec<String> = list.commissions.iter().map(|head| head.id.clone()).collect();
    ids.sort_by(|left, right| workpiece_key(left).cmp(workpiece_key(right)));
    let mut dependencies = HashMap::new();
    for id in &ids {
        if let Some(current) =
            store.commission(id).and_then(|cell| cell.value.as_ref()).and_then(|show| show.current.as_ref())
        {
            dependencies.insert(id.clone(), current.dependencies.clone());
        }
    }
    forest(&ids, &dependencies)
}

fn intent_line(store: &Store, head: &CommissionHeadView) -> String {
    store.artifact(head.intent).and_then(|cell| cell.value.as_ref()).and_then(first_line).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Backlog, COORDINATOR_PATHS, PREDATING, cycle_line, forest_from};
    use crate::dto::{CommissionHeadView, CommissionShowView, CommissionsView, DigestHex, ScopeRevisionView};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::{CommissionCapability, ResourceKey, Store};
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn coordinator_paths_are_the_only_data_sources() {
        // The plausible bug: the pane grows a GitHub client (or a github.com
        // URL) to resolve intent prose, so a coordinator-only workpiece is
        // unreadable and cutover reads the replica.
        for path in COORDINATOR_PATHS {
            assert!(path.starts_with('/'), "{path}");
            assert!(!path.contains("github"), "{path}");
        }
        let paths = [
            ResourceKey::Commissions,
            ResourceKey::Commission("wp-local".to_owned()),
            ResourceKey::Artifact(digest(1)),
        ]
        .map(|key| key.path());
        assert_eq!(paths[0], "/commissions");
        assert_eq!(paths[1], "/commissions/wp-local");
        assert!(paths[2].starts_with("/artifacts/"), "{}", paths[2]);
        assert!(paths.iter().all(|path| path.starts_with('/')));
        assert!(paths.iter().all(|path| !path.contains("github")));
    }

    #[test]
    fn a_cyclic_fixture_paints_the_cycle_band() {
        // The plausible bug: a back-edge is dropped so the pane paints an
        // acyclic tree and the operator never sees the deadlock.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_commissions(Ok(CommissionsView { commissions: vec![head("wp-a"), head("wp-b")] }));
        store.apply_commission("wp-a".to_owned(), Ok(show("wp-a", vec!["wp-b".to_owned()])));
        store.apply_commission("wp-b".to_owned(), Ok(show("wp-b", vec!["wp-a".to_owned()])));
        let bands: Vec<String> = forest_from(&store).cycles.into_iter().map(|cycle| cycle_line(&cycle)).collect();
        assert_eq!(bands.len(), 1, "{bands:?}");
        assert!(bands[0].contains("wp-a"), "{}", bands[0]);
        assert!(bands[0].contains("wp-b"), "{}", bands[0]);
        assert!(bands[0].starts_with("cycle  "), "{}", bands[0]);

        let mut backlog = Backlog::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("test backend");
        terminal.draw(|frame| backlog.render(frame, frame.area(), &store)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("cycle"), "{text}");
        assert!(text.contains("wp-a"), "{text}");
        assert!(text.contains("wp-b"), "{text}");
    }

    #[test]
    fn a_predating_coordinator_states_the_fact() {
        // The plausible bug: a 404 is painted as a generic fetch error, so
        // the operator cannot tell a missing store from a down coordinator.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_commissions_missing();
        assert_eq!(store.commission_capability(), Some(CommissionCapability::Absent));
        let mut backlog = Backlog::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| backlog.render(frame, frame.area(), &store)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains(PREDATING), "{text}");
    }

    #[test]
    fn backlog_footer_keys_are_handled() {
        assert_footer_honest(Backlog::key_hints(), |code| {
            Shell::probe(Nav::backlog()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    fn head(id: &str) -> CommissionHeadView {
        CommissionHeadView {
            id: id.to_owned(),
            intent: digest(1),
            status: "open".to_owned(),
            ..CommissionHeadView::default()
        }
    }

    fn show(id: &str, dependencies: Vec<String>) -> CommissionShowView {
        CommissionShowView {
            id: id.to_owned(),
            intent: digest(1),
            status: "open".to_owned(),
            current: Some(ScopeRevisionView { workpiece: id.to_owned(), dependencies, ..ScopeRevisionView::default() }),
            ..CommissionShowView::default()
        }
    }
}
