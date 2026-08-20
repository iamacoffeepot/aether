//! The Board: bloom/member table. The live table sits in the workspace board pane.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::{BloomStatus, DigestHex, MemberView, TimelineSpan, ViewDocument};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

use super::metrics::{
    Silence, axis_range, format_duration, format_micro_usd, paint_member_line, reconstructed_range, reconstructed_start,
};
use super::partition::{history_blooms, live_blooms};

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

const LIVE_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "h", action: "history" },
    KeyHint { keys: "l", action: "journal" },
    KeyHint { keys: "t", action: "timeline" },
    KeyHint { keys: "d", action: "days" },
    KeyHint { keys: "c", action: "cost" },
    KeyHint { keys: "b", action: "backlog" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

const HISTORY_HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "l", action: "journal" },
    KeyHint { keys: "t", action: "timeline" },
    KeyHint { keys: "d", action: "days" },
    KeyHint { keys: "c", action: "cost" },
    KeyHint { keys: "b", action: "backlog" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Live board or its landed/superseded complement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoardLane {
    #[default]
    Live,
    History,
}

/// Board view state. Cursor and scroll live here so a later pop restores them.
#[derive(Clone, Debug, Default)]
pub struct Board {
    cursor: Cursor<RowId>,
    scroll: usize,
    lane: BoardLane,
}

impl Board {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn history() -> Self {
        Self { lane: BoardLane::History, ..Self::default() }
    }

    #[must_use]
    pub fn lane(&self) -> BoardLane {
        self.lane
    }

    #[must_use]
    pub fn cursor(&self) -> &Cursor<RowId> {
        &self.cursor
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![
            ResourceKey::View,
            ResourceKey::MetricsSummary,
            ResourceKey::MetricsDays,
            ResourceKey::MetricsDispatches,
            ResourceKey::Spend,
        ]
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        match self.lane {
            BoardLane::Live => LIVE_HINTS,
            BoardLane::History => HISTORY_HINTS,
        }
    }

    #[must_use]
    pub fn selected_focus(&self) -> Option<Focus> {
        match self.cursor.selected() {
            Some(RowId::Bloom { id }) => Some(Focus::bloom(*id)),
            Some(RowId::Member { bloom, workpiece }) => Some(Focus::member(*bloom, workpiece.clone())),
            None => None,
        }
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> Option<DigestHex> {
        match self.cursor.selected() {
            Some(RowId::Bloom { id }) => Some(*id),
            Some(RowId::Member { bloom, .. }) => Some(*bloom),
            None => None,
        }
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let rows = rows_from(store, self.lane);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, BoardRow::id);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, BoardRow::id);
                Outcome::Handled
            }
            KeyCode::Enter => self.selected_focus().map_or(Outcome::Handled, |focus| Outcome::Push(Nav::focus(focus))),
            KeyCode::Char('h') if self.lane == BoardLane::Live => Outcome::Push(Nav::History),
            KeyCode::Char('l') => Outcome::Push(Nav::journal(None)),
            KeyCode::Char('t') => {
                self.digest_under_cursor().map_or(Outcome::Handled, |bloom| Outcome::Push(Nav::timeline(bloom)))
            }
            KeyCode::Char('d') => Outcome::Push(Nav::days()),
            KeyCode::Char('c') => Outcome::Push(Nav::cost()),
            KeyCode::Char('b') => Outcome::Push(Nav::backlog()),
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = rows_from(store, self.lane);
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

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = rows_from(store, self.lane);
        let dimmed = store.view().is_stale();
        self.render_table(frame, area, store, &rows, dimmed);
    }

    fn render_table(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store, rows: &[BoardRow], dimmed: bool) {
        let muted = if dimmed {
            palette::body().add_modifier(Modifier::DIM)
        } else {
            palette::body()
        };
        let title = match self.lane {
            BoardLane::Live => "BLOOM / MEMBER",
            BoardLane::History => "HISTORY (landed · superseded)",
        };
        let header = Row::new([title, "STATE", "ELAPSED", "COST", "LANE"])
            .style(palette::body().add_modifier(Modifier::BOLD).patch(muted));
        let extras = metrics_of(store);
        let table_rows = rows.iter().map(|row| match row {
            BoardRow::Bloom(bloom) => {
                let extra = extras.iter().find(|extra| extra.bloom == bloom.id);
                Row::new([
                    Cell::from(bloom.id_prefix.clone()),
                    Cell::from(format!("{}  {} mem", bloom.status, bloom.member_count)),
                    Cell::from(extra.map_or("", |extra| extra.elapsed.as_str())),
                    Cell::from(extra.map_or("", |extra| extra.cost.as_str())),
                    Cell::from(extra.map_or("", |extra| extra.lane.as_str())),
                ])
                .style(palette::body().add_modifier(Modifier::BOLD).patch(muted))
            }
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
                Constraint::Length(14),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Min(4),
            ],
        )
        .style(palette::body())
        .header(header)
        .row_highlight_style(palette::cursor())
        .highlight_symbol("> ");
        let mut table_state = TableState::default()
            .with_selected(self.cursor.selected_index(rows, BoardRow::id))
            .with_offset(self.scroll);
        frame.render_stateful_widget(table, area, &mut table_state);
        self.scroll = table_state.offset();
    }
}

/// The one-word state `scripts/bloomery-operator.py`'s `member_status_state`
/// prints. The script's `has_order` bit comes from the journal; `/view` does
/// not project outstanding orders, so `running` is the member cursor naming
/// a stage with an attempt underway on a member the host is not holding.
#[must_use]
pub fn member_status_state(member: &MemberView) -> &'static str {
    if member.wedge.is_some() {
        return "WEDGED";
    }
    if member.pending_decision.is_some() {
        return "held";
    }
    if member.resolution.is_some() {
        return "integrated";
    }
    if attempt_in_flight(member) {
        return "running";
    }
    if member.blocked_by.as_deref().is_some_and(|name| !name.is_empty()) {
        return "blocked";
    }
    "idle"
}

fn attempt_in_flight(member: &MemberView) -> bool {
    member.host_fault.is_none()
        && member.cursor.as_ref().is_some_and(|cursor| cursor.stage.is_some() && cursor.attempts > 0)
}

fn rows_from(store: &Store, lane: BoardLane) -> Vec<BoardRow> {
    store.view().value.as_ref().map(|view| rows_of(view, lane)).unwrap_or_default()
}

fn rows_of(view: &ViewDocument, lane: BoardLane) -> Vec<BoardRow> {
    let blooms = match lane {
        BoardLane::Live => live_blooms(view).collect::<Vec<_>>(),
        BoardLane::History => history_blooms(view).collect::<Vec<_>>(),
    };
    let mut rows = Vec::new();
    for bloom in blooms {
        let status = match (lane, bloom.superseded_by) {
            (BoardLane::History, Some(successor)) => {
                format!("{} → {}", bloom_status_label(bloom.status), successor.prefix())
            }
            _ => bloom_status_label(bloom.status),
        };
        rows.push(BoardRow::Bloom(BloomRow {
            id: bloom.id,
            id_prefix: bloom.id.prefix(),
            status,
            member_count: bloom.members.len(),
        }));
        for member in &bloom.members {
            rows.push(BoardRow::Member(MemberRow {
                bloom: bloom.id,
                workpiece: member.workpiece.clone(),
                state: member_status_state(member).to_owned(),
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

struct BloomMetrics {
    bloom: DigestHex,
    elapsed: String,
    cost: String,
    lane: String,
}

fn metrics_of(store: &Store) -> Vec<BloomMetrics> {
    let dispatches = store.dispatches().value.as_ref().map_or(&[][..], Vec::as_slice);
    let spend = store.spend().value.as_ref();
    let mut blooms: Vec<DigestHex> = Vec::new();
    for row in dispatches {
        if !blooms.contains(&row.bloom) {
            blooms.push(row.bloom);
        }
    }
    if let Some(view) = store.view().value.as_ref() {
        for bloom in &view.blooms {
            if !blooms.contains(&bloom.id) {
                blooms.push(bloom.id);
            }
        }
    }
    blooms
        .into_iter()
        .map(|bloom| {
            let rows: Vec<_> = dispatches.iter().filter(|row| row.bloom == bloom).collect();
            let stamps: Vec<u64> = rows.iter().filter_map(|row| row.recorded_unix_millis).collect();
            let elapsed = match (stamps.iter().copied().min(), stamps.iter().copied().max()) {
                (Some(first), Some(last)) if last > first => format_duration(last - first),
                _ => "—".to_owned(),
            };
            let cost = spend
                .and_then(|window| window.per_bloom.get(&bloom.as_hex()).copied())
                .filter(|micro| *micro > 0)
                .map_or_else(|| "—".to_owned(), format_micro_usd);
            let spans: Vec<TimelineSpan> = rows
                .iter()
                .map(|row| TimelineSpan {
                    workpiece: row.workpiece.clone(),
                    stage: row.stage,
                    sequence: row.sequence,
                    started_unix_millis: row.recorded_unix_millis,
                    reconstructed: row.reconstructed,
                })
                .collect();
            let reconstructed = spans.iter().any(|span| span.reconstructed || span.started_unix_millis.is_none());
            let (start, end) = if reconstructed {
                reconstructed_range(&spans)
            } else {
                axis_range(&spans, stamps.iter().copied().max().unwrap_or(1))
                    .map_or((0, 1), |(start, end, _)| (start, end))
            };
            let paint = if reconstructed {
                spans
                    .iter()
                    .map(|span| TimelineSpan { started_unix_millis: Some(reconstructed_start(span)), ..span.clone() })
                    .collect::<Vec<_>>()
            } else {
                spans
            };
            let lane = paint_member_line(&paint, start, end, 12, Silence::Queued, false, false);
            BloomMetrics { bloom, elapsed, cost, lane }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Board, BoardLane, BoardRow, member_status_state, rows_of};
    use crate::dto::{
        BloomStatus, BloomView, CompositionCursorView, DigestHex, MemberView, PendingDecisionView, Present, StageId,
        ViewDocument, WedgeCause,
    };
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::palette::{Depth, with_depth};
    use crate::shell::Shell;
    use crate::store::Store;
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn member(workpiece: &str) -> MemberView {
        MemberView { workpiece: workpiece.to_owned(), ..MemberView::default() }
    }

    fn in_flight_construct(workpiece: &str) -> MemberView {
        MemberView {
            cursor: Some(CompositionCursorView { stage: Some(StageId::Construct), attempts: 1, candidate: None }),
            ..member(workpiece)
        }
    }

    #[test]
    fn member_status_state_matches_the_operator_script() {
        // The plausible bug: a dependent carrying blocked_by paints as idle
        // (the mysterious idleness the readiness scheduler exists to name),
        // or a wedge loses to blocked_by / resolution.
        assert_eq!(member_status_state(&MemberView { blocked_by: Some("wp-a".to_owned()), ..member("wp") }), "blocked");
        assert_eq!(member_status_state(&in_flight_construct("wp")), "running");
        assert_eq!(
            member_status_state(&MemberView {
                resolution: Some(Present {}),
                blocked_by: Some("wp-a".to_owned()),
                ..member("wp")
            }),
            "integrated"
        );
        assert_eq!(
            member_status_state(&MemberView {
                wedge: Some(Present {}),
                blocked_by: Some("wp-a".to_owned()),
                ..member("wp")
            }),
            "WEDGED"
        );
        assert_eq!(
            member_status_state(&MemberView { pending_decision: Some(PendingDecisionView::default()), ..member("wp") }),
            "held"
        );
        assert_eq!(member_status_state(&MemberView { blocked_by: Some(String::new()), ..member("wp") }), "idle");
        assert_eq!(member_status_state(&member("wp")), "idle");
    }

    #[test]
    fn an_in_flight_construct_renders_running() {
        // Tripwire: rows_of used to pass has_order=false, so a member whose
        // cursor names a live Construct attempt painted idle — the mysterious
        // idleness the ladder's running rung exists to name.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                status: Some(BloomStatus::Sealed),
                members: vec![in_flight_construct("wp")],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let rows = rows_of(&view, BoardLane::Live);
        let BoardRow::Member(member) = &rows[1] else {
            panic!("second row is the member");
        };
        assert_eq!(member.state, "running");
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
        let rows = rows_of(&view, BoardLane::Live);
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
    fn landed_and_superseded_blooms_move_to_history() {
        // The plausible bug: the live table still lists a Landed bloom, so
        // the history complement is not a partition of the document.
        let view = ViewDocument {
            blooms: vec![
                BloomView { id: digest(1), status: Some(BloomStatus::Sealed), ..BloomView::default() },
                BloomView { id: digest(2), status: Some(BloomStatus::Landed), ..BloomView::default() },
                BloomView {
                    id: digest(3),
                    status: Some(BloomStatus::Superseded),
                    superseded_by: Some(digest(2)),
                    ..BloomView::default()
                },
            ],
            ..ViewDocument::default()
        };
        let live: Vec<_> = rows_of(&view, BoardLane::Live)
            .into_iter()
            .filter_map(|row| match row {
                BoardRow::Bloom(bloom) => Some(bloom.id),
                BoardRow::Member(_) => None,
            })
            .collect();
        let history: Vec<_> = rows_of(&view, BoardLane::History)
            .into_iter()
            .filter_map(|row| match row {
                BoardRow::Bloom(bloom) => Some(bloom.id),
                BoardRow::Member(_) => None,
            })
            .collect();
        assert_eq!(live, vec![digest(1)]);
        assert_eq!(history, vec![digest(2), digest(3)]);
    }

    #[test]
    fn board_footer_keys_are_handled() {
        // The plausible bug: the footer paints `r` / `q` / `j/k` while the
        // match dropped one of them, so an advertised key does nothing.
        let view = ViewDocument::default();
        assert_footer_honest(Board::new().key_hints(), |code| {
            Shell::showing(&view, None).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
        assert_footer_honest(Board::history().key_hints(), |code| {
            Shell::probe(Nav::History).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn fallback_board_row_keeps_the_severity_glyph() {
        // The plausible bug: 256-color fallback paints severity by color
        // alone and drops the WEDGED token the operator has to read.
        with_depth(Depth::Indexed, || {
            let mut store = Store::new(Duration::from_secs(1));
            store.apply_view(Ok(ViewDocument {
                blooms: vec![BloomView {
                    id: digest(1),
                    members: vec![MemberView { wedge: Some(Present {}), ..member("wp-wedge") }],
                    ..BloomView::default()
                }],
                ..ViewDocument::default()
            }));
            let mut board = Board::new();
            let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test backend");
            terminal.draw(|frame| board.render(frame, frame.area(), &store)).expect("draw");
            let text: String = terminal.backend().buffer().content().iter().map(Cell::symbol).collect();
            assert!(text.contains("WEDGED"), "{text}");
        });
    }
}
