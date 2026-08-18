//! The Board: bloom/member table. Alert and interrupt bands live on the shell.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::{BloomStatus, DigestHex, MemberView, ViewDocument};
use crate::keys::{KeyHint, Outcome};
use crate::store::{ResourceKey, Store};

/// Stable identity of one selectable row. Refreshes look this up so the
/// cursor does not walk out from under the operator when `/view` reorders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowId {
    Bloom { id: DigestHex },
    Member { bloom: DigestHex, workpiece: String },
}

/// One rendered row on the board.
#[derive(Clone, Debug)]
pub enum BoardRow {
    Bloom(BloomRow),
    Member(MemberRow),
}

/// A bloom header row.
#[derive(Clone, Debug)]
pub struct BloomRow {
    pub id: DigestHex,
    pub id_prefix: String,
    pub status: String,
    pub member_count: usize,
}

/// A member row under its bloom.
#[derive(Clone, Debug)]
pub struct MemberRow {
    pub bloom: DigestHex,
    pub workpiece: String,
    pub state: String,
    pub machinery: String,
    pub blocked_by: String,
    pub wedge_cause: String,
}

impl BoardRow {
    #[must_use]
    pub fn id(&self) -> RowId {
        match self {
            Self::Bloom(row) => RowId::Bloom { id: row.id },
            Self::Member(row) => RowId::Member { bloom: row.bloom, workpiece: row.workpiece.clone() },
        }
    }
}

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Board view state. Cursor and scroll live here so a later pop restores them.
#[derive(Clone, Debug, Default)]
pub struct Board {
    cursor: Cursor<RowId>,
    scroll: usize,
}

impl Board {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn cursor(&self) -> &Cursor<RowId> {
        &self.cursor
    }

    #[must_use]
    pub fn subscriptions(&self) -> &'static [ResourceKey] {
        &[ResourceKey::View]
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        HINTS
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let rows = rows_from(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, BoardRow::id);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, BoardRow::id);
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = rows_from(store);
        self.cursor.reseat(&rows, BoardRow::id, |id, rows| {
            if let RowId::Member { bloom, .. } = id {
                let bloom = *bloom;
                if rows.iter().any(|row| matches!(row, BoardRow::Bloom(row) if row.id == bloom)) {
                    return Some(RowId::Bloom { id: bloom });
                }
            }
            rows.first().map(BoardRow::id)
        });
    }

    /// True when k should leave the table for the chrome above it.
    #[must_use]
    pub fn selected_is_first(&self, store: &Store) -> bool {
        let rows = rows_from(store);
        matches!(self.cursor.selected_index(&rows, BoardRow::id), Some(0) | None)
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = rows_from(store);
        let dimmed = store.view().is_stale();
        self.render_table(frame, area, &rows, dimmed);
    }

    fn render_table(&mut self, frame: &mut Frame<'_>, area: Rect, rows: &[BoardRow], dimmed: bool) {
        let muted = if dimmed {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        };
        let header = Row::new(["BLOOM / MEMBER", "STATE", "MACH", "BLOCKED BY", "WEDGE"])
            .style(Style::default().add_modifier(Modifier::BOLD).patch(muted));
        let table_rows = rows.iter().map(|row| match row {
            BoardRow::Bloom(bloom) => Row::new([
                Cell::from(bloom.id_prefix.clone()),
                Cell::from(bloom.status.clone()),
                Cell::from(format!("{} mem", bloom.member_count)),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(Style::default().add_modifier(Modifier::BOLD).patch(muted)),
            BoardRow::Member(member) => Row::new([
                Cell::from(format!("  {}", member.workpiece)),
                Cell::from(member.state.clone()),
                Cell::from(member.machinery.clone()),
                Cell::from(member.blocked_by.clone()),
                Cell::from(member.wedge_cause.clone()),
            ])
            .style(muted),
        });
        let table = Table::new(
            table_rows,
            [
                Constraint::Length(28),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Length(20),
                Constraint::Min(8),
            ],
        )
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
        let mut table_state = TableState::default()
            .with_selected(self.cursor.selected_index(rows, BoardRow::id))
            .with_offset(self.scroll);
        frame.render_stateful_widget(table, area, &mut table_state);
        self.scroll = table_state.offset();
    }
}

/// The one-word state `scripts/bloomery-operator.py`'s `member_status_state`
/// prints. `has_order` is the outstanding-order bit that script reads from
/// the journal; the console is `/view`-only, so the live board always
/// passes `false`.
#[must_use]
pub fn member_status_state(member: &MemberView, has_order: bool) -> &'static str {
    if member.wedge.is_some() {
        return "WEDGED";
    }
    if member.pending_decision.is_some() {
        return "held";
    }
    if member.resolution.is_some() {
        return "integrated";
    }
    if has_order {
        return "running";
    }
    if member.blocked_by.as_deref().is_some_and(|name| !name.is_empty()) {
        return "blocked";
    }
    "idle"
}

fn rows_from(store: &Store) -> Vec<BoardRow> {
    store.view().value.as_ref().map(rows_of).unwrap_or_default()
}

fn rows_of(view: &ViewDocument) -> Vec<BoardRow> {
    let mut rows = Vec::new();
    for bloom in &view.blooms {
        rows.push(BoardRow::Bloom(BloomRow {
            id: bloom.id,
            id_prefix: bloom.id.prefix(),
            status: bloom_status_label(bloom.status),
            member_count: bloom.members.len(),
        }));
        for member in &bloom.members {
            rows.push(BoardRow::Member(MemberRow {
                bloom: bloom.id,
                workpiece: member.workpiece.clone(),
                state: member_status_state(member, false).to_owned(),
                machinery: format!("{}/{}", member.machinery_rolls, member.machinery_budget),
                blocked_by: member.blocked_by.clone().filter(|name| !name.is_empty()).unwrap_or_default(),
                wedge_cause: member.wedge_cause.map_or_else(String::new, |cause| cause.to_string()),
            }));
        }
    }
    rows
}

fn bloom_status_label(status: Option<BloomStatus>) -> String {
    status.map_or_else(|| "?".to_owned(), |status| status.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Board, BoardRow, member_status_state, rows_of};
    use crate::dto::{
        BloomStatus, BloomView, DigestHex, MemberView, PendingDecisionView, Present, ViewDocument, WedgeCause,
    };
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::store::Store;
    use crossterm::event::KeyEvent;
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn member(workpiece: &str) -> MemberView {
        MemberView { workpiece: workpiece.to_owned(), ..MemberView::default() }
    }

    #[test]
    fn member_status_state_matches_the_operator_script() {
        // The plausible bug: a dependent carrying blocked_by paints as idle
        // (the mysterious idleness the readiness scheduler exists to name),
        // or a wedge loses to blocked_by / resolution.
        assert_eq!(
            member_status_state(&MemberView { blocked_by: Some("wp-a".to_owned()), ..member("wp") }, false),
            "blocked"
        );
        assert_eq!(member_status_state(&member("wp"), true), "running");
        assert_eq!(
            member_status_state(
                &MemberView { resolution: Some(Present {}), blocked_by: Some("wp-a".to_owned()), ..member("wp") },
                false,
            ),
            "integrated"
        );
        assert_eq!(
            member_status_state(
                &MemberView { wedge: Some(Present {}), blocked_by: Some("wp-a".to_owned()), ..member("wp") },
                false,
            ),
            "WEDGED"
        );
        assert_eq!(
            member_status_state(
                &MemberView { pending_decision: Some(PendingDecisionView::default()), ..member("wp") },
                false,
            ),
            "held"
        );
        assert_eq!(member_status_state(&MemberView { blocked_by: Some(String::new()), ..member("wp") }, false), "idle");
        assert_eq!(member_status_state(&member("wp"), false), "idle");
    }

    #[test]
    fn rows_carry_machinery_blocker_and_wedge_cause() {
        // The plausible bug: the table prints bloom status and workpiece
        // only, so a machinery wedge and its blocked_by ancestor never reach
        // the operator.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                status: Some(BloomStatus::Sealed),
                members: vec![MemberView {
                    machinery_rolls: 2,
                    machinery_budget: 3,
                    blocked_by: Some("wp-a".to_owned()),
                    wedge: Some(Present {}),
                    wedge_cause: Some(WedgeCause::Machinery),
                    ..member("wp-b")
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let rows = rows_of(&view);
        assert_eq!(rows.len(), 2);
        let BoardRow::Member(member) = &rows[1] else {
            panic!("second row is the member");
        };
        assert_eq!(member.state, "WEDGED");
        assert_eq!(member.machinery, "2/3");
        assert_eq!(member.blocked_by, "wp-a");
        assert_eq!(member.wedge_cause, "Machinery");
    }

    #[test]
    fn board_footer_keys_are_handled() {
        // The plausible bug: the footer paints `r` / `q` / `j/k` while the
        // match dropped one of them, so an advertised key does nothing.
        let store = Store::new(Duration::from_secs(1));
        let mut board = Board::new();
        assert_footer_honest(board.key_hints(), |code| {
            board.handle_key(KeyEvent::from(code), &store) != Outcome::Ignored
        });
    }
}
