//! The Board: bloom/member table. The live table sits in the workspace board pane.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::{BloomStatus, DigestHex, MemberView, MetricDispatch, ViewDocument};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

use super::metrics::format_duration;
use super::partition::{MemberState, history_blooms, live_blooms};

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
    pub stage: String,
    pub age: String,
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
    pub fn enter_pushes(&self) -> bool {
        self.selected_focus().is_some()
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
        let header =
            Row::new([title, "STATE", "STAGE", "AGE"]).style(palette::body().add_modifier(Modifier::BOLD).patch(muted));
        let extras = metrics_of(store);
        let table_rows = rows.iter().map(|row| match row {
            BoardRow::Bloom(bloom) => {
                let extra = extras.iter().find(|extra| extra.bloom == bloom.id && extra.workpiece.is_none());
                Row::new([
                    Cell::from(bloom.id_prefix.clone()),
                    Cell::from(format!("{}  {} mem", bloom.status, bloom.member_count)),
                    Cell::from(""),
                    Cell::from(extra.map_or("—", |extra| extra.elapsed.as_str())),
                ])
                .style(palette::body().add_modifier(Modifier::BOLD).patch(muted))
            }
            BoardRow::Member(member) => Row::new([
                Cell::from(format!("  {}", member.workpiece)),
                Cell::from(member.state.clone()),
                Cell::from(member.stage.clone()),
                Cell::from(member.age.clone()),
            ])
            .style(muted),
        });
        let table = Table::new(
            table_rows,
            [Constraint::Min(14), Constraint::Length(10), Constraint::Length(16), Constraint::Length(8)],
        )
        .style(palette::body())
        .header(header)
        .row_highlight_style(palette::cursor())
        .highlight_symbol(super::caret(self.enter_pushes()));
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
    MemberState::of(member).label()
}

fn rows_from(store: &Store, lane: BoardLane) -> Vec<BoardRow> {
    store
        .view()
        .value
        .as_ref()
        .map(|view| rows_of(view, lane, store.dispatches().value.as_ref().map_or(&[][..], Vec::as_slice)))
        .unwrap_or_default()
}

fn rows_of(view: &ViewDocument, lane: BoardLane, dispatches: &[MetricDispatch]) -> Vec<BoardRow> {
    let blooms = match lane {
        BoardLane::Live => live_blooms(view).collect::<Vec<_>>(),
        BoardLane::History => history_blooms(view).collect::<Vec<_>>(),
    };
    let mut rows = Vec::new();
    for bloom in blooms {
        let members: Vec<&MemberView> = match lane {
            BoardLane::Live => bloom.members.iter().filter(|member| MemberState::of(member).walks()).collect(),
            BoardLane::History => bloom.members.iter().collect(),
        };
        if lane == BoardLane::Live && members.is_empty() {
            continue;
        }
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
        for member in members {
            rows.push(BoardRow::Member(member_row(bloom.id, member, dispatches)));
        }
    }
    rows
}

fn member_row(bloom: DigestHex, member: &MemberView, dispatches: &[MetricDispatch]) -> MemberRow {
    MemberRow {
        bloom,
        workpiece: member.workpiece.clone(),
        state: member_status_state(member).to_owned(),
        stage: member_stage(member, bloom, dispatches),
        age: elapsed_of(dispatches, bloom, Some(&member.workpiece)),
    }
}

fn member_stage(member: &MemberView, bloom: DigestHex, dispatches: &[MetricDispatch]) -> String {
    if let Some(stage) = member.cursor.as_ref().and_then(|cursor| cursor.stage) {
        return stage.label().to_owned();
    }
    dispatches
        .iter()
        .filter(|row| row.bloom == bloom && row.workpiece == member.workpiece)
        .max_by_key(|row| (row.recorded_unix_millis.unwrap_or(0), row.sequence))
        .map(|row| row.stage.label().to_owned())
        .unwrap_or_default()
}

fn bloom_status_label(status: Option<BloomStatus>) -> String {
    status.map_or_else(|| "?".to_owned(), |status| status.to_string())
}

struct SubjectMetrics {
    bloom: DigestHex,
    workpiece: Option<String>,
    elapsed: String,
}

fn metrics_of(store: &Store) -> Vec<SubjectMetrics> {
    let dispatches = store.dispatches().value.as_ref().map_or(&[][..], Vec::as_slice);
    let mut keys: Vec<(DigestHex, Option<String>)> = Vec::new();
    for row in dispatches {
        push_subject(&mut keys, row.bloom, None);
        push_subject(&mut keys, row.bloom, Some(row.workpiece.clone()));
    }
    if let Some(view) = store.view().value.as_ref() {
        for bloom in &view.blooms {
            push_subject(&mut keys, bloom.id, None);
            for member in &bloom.members {
                push_subject(&mut keys, bloom.id, Some(member.workpiece.clone()));
            }
        }
    }
    keys.into_iter()
        .map(|(bloom, workpiece)| SubjectMetrics {
            elapsed: elapsed_of(dispatches, bloom, workpiece.as_deref()),
            bloom,
            workpiece,
        })
        .collect()
}

fn push_subject(keys: &mut Vec<(DigestHex, Option<String>)>, bloom: DigestHex, workpiece: Option<String>) {
    if !keys.iter().any(|(existing, existing_workpiece)| *existing == bloom && *existing_workpiece == workpiece) {
        keys.push((bloom, workpiece));
    }
}

fn elapsed_of(dispatches: &[MetricDispatch], bloom: DigestHex, workpiece: Option<&str>) -> String {
    let stamps: Vec<u64> = dispatches
        .iter()
        .filter(|row| row.bloom == bloom && workpiece.is_none_or(|name| row.workpiece == name))
        .filter_map(|row| row.recorded_unix_millis)
        .collect();
    match (stamps.iter().copied().min(), stamps.iter().copied().max()) {
        (Some(first), Some(last)) if last > first => format_duration(last - first),
        _ => "—".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Board, BoardLane, BoardRow, member_status_state, rows_of};
    use crate::dto::{
        BloomStatus, BloomView, CompositionCursorView, DigestHex, MemberView, MetricDispatch, PendingDecisionView,
        Present, StageId, ViewDocument,
    };
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::palette::{Depth, with_depth};
    use crate::shell::Shell;
    use crate::store::Store;
    use crossterm::event::{KeyCode, KeyEvent};
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
        let rows = rows_of(&view, BoardLane::Live, &[]);
        let BoardRow::Member(member) = &rows[1] else {
            panic!("second row is the member");
        };
        assert_eq!(member.state, "running");
    }

    #[test]
    fn live_rows_keep_walking_members_and_drop_the_rest() {
        // The plausible bug: a resolved member still occupies a live row at
        // the same weight as a Construct attempt, or a bloom whose members
        // all rest keeps its header.
        let view = ViewDocument {
            blooms: vec![
                BloomView {
                    id: digest(1),
                    status: Some(BloomStatus::Sealed),
                    members: vec![MemberView { resolution: Some(Present {}), ..member("wp-done") }],
                    ..BloomView::default()
                },
                BloomView {
                    id: digest(2),
                    status: Some(BloomStatus::Sealed),
                    members: vec![in_flight_construct("wp-run")],
                    ..BloomView::default()
                },
            ],
            ..ViewDocument::default()
        };
        let live = rows_of(&view, BoardLane::Live, &[]);
        let live_ids: Vec<_> = live
            .iter()
            .filter_map(|row| match row {
                BoardRow::Bloom(bloom) => Some(bloom.id),
                BoardRow::Member(_) => None,
            })
            .collect();
        assert_eq!(live_ids, vec![digest(2)]);
        let BoardRow::Member(member) = &live[1] else {
            panic!("second live row is the walking member");
        };
        assert_eq!(member.workpiece, "wp-run");
        assert!(live.iter().all(|row| match row {
            BoardRow::Member(member) => member.workpiece != "wp-done",
            BoardRow::Bloom(_) => true,
        }));
    }

    #[test]
    fn history_keeps_every_member_of_a_landed_bloom() {
        // The plausible bug: the walking filter also empties History, whose
        // members are all integrated.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(2),
                status: Some(BloomStatus::Landed),
                members: vec![MemberView { resolution: Some(Present {}), ..member("wp-landed") }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let history = rows_of(&view, BoardLane::History, &[]);
        assert_eq!(history.len(), 2);
        let BoardRow::Member(member) = &history[1] else {
            panic!("second history row is the member");
        };
        assert_eq!(member.workpiece, "wp-landed");
        assert_eq!(member.state, "integrated");
    }

    #[test]
    fn member_row_stage_and_age_match_the_column_headers() {
        // The plausible bug: STAGE/AGE cells still carry machinery rolls and
        // a blocked_by id, so a column header is only true of bloom rows.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                status: Some(BloomStatus::Sealed),
                members: vec![in_flight_construct("wp")],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let dispatches = [
            MetricDispatch {
                bloom: digest(1),
                workpiece: "wp".to_owned(),
                recorded_unix_millis: Some(1_000),
                sequence: 1,
                ..MetricDispatch::default()
            },
            MetricDispatch {
                bloom: digest(1),
                workpiece: "wp".to_owned(),
                recorded_unix_millis: Some(4_000),
                sequence: 2,
                ..MetricDispatch::default()
            },
            MetricDispatch {
                bloom: digest(1),
                workpiece: "wp-other".to_owned(),
                recorded_unix_millis: Some(1_000),
                sequence: 3,
                ..MetricDispatch::default()
            },
            MetricDispatch {
                bloom: digest(1),
                workpiece: "wp-other".to_owned(),
                recorded_unix_millis: Some(100_000),
                sequence: 4,
                ..MetricDispatch::default()
            },
        ];
        let rows = rows_of(&view, BoardLane::Live, &dispatches);
        let BoardRow::Member(member) = &rows[1] else {
            panic!("second row is the member");
        };
        assert_eq!(member.stage, "Construct");
        assert_eq!(member.age, "3s");
    }

    #[test]
    fn landed_and_superseded_blooms_move_to_history() {
        // The plausible bug: the live table still lists a Landed bloom, so
        // the history complement is not a partition of the document.
        let view = ViewDocument {
            blooms: vec![
                BloomView {
                    id: digest(1),
                    status: Some(BloomStatus::Sealed),
                    members: vec![in_flight_construct("wp")],
                    ..BloomView::default()
                },
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
        let live: Vec<_> = rows_of(&view, BoardLane::Live, &[])
            .into_iter()
            .filter_map(|row| match row {
                BoardRow::Bloom(bloom) => Some(bloom.id),
                BoardRow::Member(_) => None,
            })
            .collect();
        let history: Vec<_> = rows_of(&view, BoardLane::History, &[])
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

    #[test]
    fn a_row_enter_pushes_paints_the_caret() {
        // The plausible bug: hiding the caret on rows Enter refuses also
        // hides it where Enter still pushes a frame.
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_view(Ok(ViewDocument {
            blooms: vec![BloomView { id: digest(1), members: vec![in_flight_construct("wp")], ..BloomView::default() }],
            ..ViewDocument::default()
        }));
        let mut board = Board::new();
        board.reseat(&store);
        assert_eq!(board.handle_key(KeyEvent::from(KeyCode::Char('j')), &store), Outcome::Handled);
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test backend");
        terminal.draw(|frame| board.render(frame, frame.area(), &store)).expect("draw");
        assert_eq!(super::super::row_caret(&terminal, "wp"), "> ");
    }
}
