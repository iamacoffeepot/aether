//! The Board: bloom/member table. The live table sits in the workspace board pane.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::{
    BloomDispatchesView, BloomStatus, CoordinationView, DigestHex, MemberView, MetricDispatch, OrderView, PrecheckView,
    StageId, ViewDocument,
};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

use super::metrics::format_duration;
use super::partition::{MemberState, history_blooms, live_blooms};

/// Stable identity of one selectable row. Refreshes look this up so the
/// cursor does not walk out from under the operator when `/view` reorders.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowId {
    Bloom { id: DigestHex },
    Member { bloom: DigestHex, workpiece: String },
    Order { nonce: String },
}

/// One rendered row on the board.
#[derive(Clone, Debug)]
pub enum BoardRow {
    Bloom(BloomRow),
    Member(MemberRow),
    Order(OrderRow),
}

/// A bloom header row.
#[derive(Clone, Debug)]
pub struct BloomRow {
    pub id: DigestHex,
    pub id_prefix: String,
    pub status: String,
    pub member_count: usize,
    pub precheck: String,
    pub age: String,
    /// Why this bloom's live shared runs are grouped the way they are — one
    /// entry per run, each naming how many members it carries and the construct
    /// base they agree on. A bloom running one member per run reads as such
    /// here instead of looking like a bloom with nothing to coalesce.
    pub grouping: String,
}

/// A member row under its bloom.
#[derive(Clone, Debug)]
pub struct MemberRow {
    pub bloom: DigestHex,
    pub workpiece: String,
    pub state: String,
    pub stage: String,
    pub age: String,
    /// The members this one semantically depends on: siblings whose change
    /// lands in a package this member's verification compiles.
    ///
    /// `None` when the bloom's dispatch rows have not been read, which is not
    /// the same claim as an empty list — one says nobody looked, the other says
    /// this member's verification compiles no sibling's write. The cell renders
    /// them differently for exactly that reason.
    pub depends: Option<Vec<String>>,
}

/// A bloom-less live order (the whole-workspace base verify).
#[derive(Clone, Debug)]
pub struct OrderRow {
    pub nonce: String,
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
            Self::Order(row) => RowId::Order { nonce: row.nonce.clone() },
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
    KeyHint { keys: "o", action: "logs" },
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
    KeyHint { keys: "o", action: "logs" },
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
    last: HashMap<RowId, String>,
    flashed: HashSet<RowId>,
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
            Some(RowId::Order { .. }) | None => None,
        }
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> Option<DigestHex> {
        match self.cursor.selected() {
            Some(RowId::Bloom { id }) => Some(*id),
            Some(RowId::Member { bloom, .. }) => Some(*bloom),
            Some(RowId::Order { .. }) | None => None,
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
            KeyCode::Char('o') => Outcome::Push(Nav::coordinator_log()),
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = rows_from(store, self.lane);
        self.note_flash(&rows);
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
        self.render_table(frame, area, &rows, dimmed);
    }

    fn note_flash(&mut self, rows: &[BoardRow]) {
        let mut lit = HashSet::new();
        let mut next = HashMap::new();
        for row in rows {
            let id = row.id();
            let fingerprint = row_fingerprint(row);
            if self.last.get(&id).is_some_and(|prev| prev != &fingerprint) {
                lit.insert(id.clone());
            }
            next.insert(id, fingerprint);
        }
        self.last = next;
        self.flashed = lit;
    }

    fn row_style(&self, row: &BoardRow, dimmed: bool, muted: Style) -> Style {
        if dimmed {
            muted
        } else if self.flashed.contains(&row.id()) {
            palette::flash()
        } else {
            muted
        }
    }

    fn render_table(&mut self, frame: &mut Frame<'_>, area: Rect, rows: &[BoardRow], dimmed: bool) {
        let muted = if dimmed {
            palette::body().add_modifier(Modifier::DIM)
        } else {
            palette::body()
        };
        let title = match self.lane {
            BoardLane::Live => "BLOOM / MEMBER",
            BoardLane::History => "HISTORY (landed · superseded)",
        };
        let header = Row::new([title, "STATE", "STAGE / HEAD / PRECHECK", "DEPENDS", "AGE / VERIFY"])
            .style(palette::body().add_modifier(Modifier::BOLD).patch(muted));
        let table_rows = rows.iter().map(|row| {
            let style = self.row_style(row, dimmed, muted);
            match row {
                BoardRow::Bloom(bloom) => Row::new([
                    Cell::from(bloom.id_prefix.clone()),
                    Cell::from(format!("{}  {} mem", bloom.status, bloom.member_count)),
                    Cell::from(bloom.precheck.clone()),
                    Cell::from(bloom.grouping.clone()),
                    Cell::from(bloom.age.clone()),
                ])
                .style(palette::body().add_modifier(Modifier::BOLD).patch(style)),
                BoardRow::Member(member) => Row::new([
                    Cell::from(format!("  {}", member.workpiece)),
                    Cell::from(member.state.clone()),
                    Cell::from(member.stage.clone()),
                    Cell::from(depends_cell(member.depends.as_ref())),
                    Cell::from(member.age.clone()),
                ])
                .style(style),
                BoardRow::Order(order) => Row::new([
                    Cell::from(order.workpiece.clone()),
                    Cell::from(order.state.clone()),
                    Cell::from(order.stage.clone()),
                    Cell::from(String::new()),
                    Cell::from(order.age.clone()),
                ])
                .style(palette::body().add_modifier(Modifier::BOLD).patch(style)),
            }
        });
        let table = Table::new(
            table_rows,
            [
                Constraint::Min(14),
                Constraint::Length(10),
                Constraint::Length(34),
                Constraint::Length(18),
                Constraint::Length(12),
            ],
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
/// prints. `has_lane` is a live outstanding order or an unfinished request on
/// a live shared verify run, so `running` means the host still holds a lane.
#[must_use]
pub fn member_status_state(member: &MemberView, has_lane: bool) -> &'static str {
    MemberState::of(member, has_lane).label()
}

fn rows_from(store: &Store, lane: BoardLane) -> Vec<BoardRow> {
    store
        .view()
        .value
        .as_ref()
        .map(|view| {
            rows_of(view, lane, store.dispatches().value.as_ref().map_or(&[][..], Vec::as_slice), &|bloom| {
                store.bloom_dispatches(bloom).and_then(|cell| cell.value.clone())
            })
        })
        .unwrap_or_default()
}

/// The per-bloom dispatch view a row build reads its closures from.
///
/// A lookup rather than a map, because the board renders every live bloom and
/// the console fetches `/blooms/{id}/dispatches` on demand: a bloom whose page
/// has not been opened yet answers `None`, and the rows say `?` rather than
/// claiming the member depends on nothing.
type BloomDispatchLookup<'a> = dyn Fn(DigestHex) -> Option<BloomDispatchesView> + 'a;

fn rows_of(
    view: &ViewDocument,
    lane: BoardLane,
    dispatches: &[MetricDispatch],
    bloom_dispatches: &BloomDispatchLookup<'_>,
) -> Vec<BoardRow> {
    let blooms = match lane {
        BoardLane::Live => live_blooms(view).collect::<Vec<_>>(),
        BoardLane::History => history_blooms(view).collect::<Vec<_>>(),
    };
    let mut rows = Vec::new();
    if lane == BoardLane::Live {
        for order in view.orders.iter().filter(|order| order.stage == StageId::BaseVerify) {
            rows.push(BoardRow::Order(workspace_order_row(order)));
        }
    }
    for bloom in blooms {
        let members: Vec<&MemberView> = match lane {
            BoardLane::Live => bloom
                .members
                .iter()
                .filter(|member| {
                    bloom.coordination.is_some() || MemberState::of(member, view.has_lane(bloom.id, member)).walks()
                })
                .collect(),
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
            precheck: bloom.coordination.as_ref().map_or_else(
                || bloom.precheck.as_ref().map_or_else(String::new, PrecheckView::summary),
                |state| {
                    format!(
                        "{} · {}",
                        state.summary(bloom.members.iter().filter(|member| member.withdrawn.is_none()).count()),
                        state.precheck_status(bloom.precheck.as_ref())
                    )
                },
            ),
            age: elapsed_of(dispatches, bloom.id, None),
            grouping: grouping_reason(bloom.coordination.as_ref()),
        }));
        let edges = bloom_dispatches(bloom.id).map(|served| served.semantic_edges);
        for member in members {
            let order = view.order_for(bloom.id, &member.workpiece);
            let mut row = member_row(bloom.id, member, dispatches, order, view.has_lane(bloom.id, member));
            row.depends = edges.as_ref().map(|edges| {
                edges
                    .iter()
                    .filter(|edge| edge.member == member.workpiece)
                    .map(|edge| edge.depends_on.clone())
                    .collect()
            });
            if let Some(state) = &bloom.coordination {
                row.stage = format!("{} · {}", row.stage, state.member_summary(member));
                row.age = shared_age(state, member, dispatches, bloom.id, &member.workpiece, &row.age);
            }
            rows.push(BoardRow::Member(row));
        }
    }
    rows
}

fn workspace_order_row(order: &OrderView) -> OrderRow {
    OrderRow {
        nonce: order.nonce.clone(),
        workpiece: "base-verify".to_owned(),
        state: "running".to_owned(),
        stage: order.stage.label().to_owned(),
        age: "—".to_owned(),
    }
}

fn member_row(
    bloom: DigestHex,
    member: &MemberView,
    dispatches: &[MetricDispatch],
    order: Option<&OrderView>,
    has_lane: bool,
) -> MemberRow {
    MemberRow {
        bloom,
        workpiece: member.workpiece.clone(),
        state: member_status_state(member, has_lane).to_owned(),
        stage: member_stage(member, bloom, dispatches, order),
        age: elapsed_of(dispatches, bloom, Some(&member.workpiece)),
        depends: None,
    }
}

/// The DEPENDS cell for one member.
///
/// Three distinct answers, because collapsing any two of them is the mistake
/// this column exists to stop: `?` for a bloom nobody has read the closures of,
/// `—` for a member whose verification compiles no sibling's write, and the
/// peer names otherwise.
fn depends_cell(depends: Option<&Vec<String>>) -> String {
    match depends {
        None => "?".to_owned(),
        Some(peers) if peers.is_empty() => "—".to_owned(),
        Some(peers) => peers.join(" "),
    }
}

/// Why a bloom's live shared runs are grouped as they are: one entry per
/// non-terminal run, each naming its member count and the construct base its
/// members agree on — the key the sealed selector actually groups by.
///
/// A bloom with no coordination state states nothing rather than an empty
/// reason: the selector has not run, so there is no grouping to explain.
fn grouping_reason(coordination: Option<&CoordinationView>) -> String {
    let Some(state) = coordination else {
        return String::new();
    };
    let reasons: Vec<String> = state
        .runs
        .iter()
        .filter(|run| !run.plan.requests.is_empty())
        .map(|run| {
            let members = run.plan.requests.len();
            run.plan
                .requests
                .first()
                .and_then(|request| request.id)
                .map_or_else(|| format!("{members}×?"), |base| format!("{members}×{}", base.prefix()))
        })
        .collect();

    reasons.join(" ")
}

fn member_stage(
    member: &MemberView,
    bloom: DigestHex,
    dispatches: &[MetricDispatch],
    order: Option<&OrderView>,
) -> String {
    if let Some(order) = order {
        return order.stage.label().to_owned();
    }
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

fn shared_age(
    state: &CoordinationView,
    member: &MemberView,
    dispatches: &[MetricDispatch],
    bloom: DigestHex,
    workpiece: &str,
    fallback: &str,
) -> String {
    if let Some((_, millis)) = state.member_latency(member) {
        return format!("{} verify", format_duration(millis));
    }
    if state.live_run_for(member).is_some()
        && let Some(age) = live_elapsed(dispatches, bloom, workpiece)
    {
        return age;
    }
    fallback.to_owned()
}

fn live_elapsed(dispatches: &[MetricDispatch], bloom: DigestHex, workpiece: &str) -> Option<String> {
    let start = dispatches
        .iter()
        .filter(|row| row.bloom == bloom && row.workpiece == workpiece)
        .filter_map(|row| row.recorded_unix_millis)
        .min()?;
    let now = unix_now_millis()?;
    (now > start).then(|| format_duration(now - start))
}

fn unix_now_millis() -> Option<u64> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

fn row_fingerprint(row: &BoardRow) -> String {
    match row {
        BoardRow::Bloom(bloom) => format!("{}|{}", bloom.status, bloom.precheck),
        BoardRow::Member(member) => format!("{}|{}", member.state, member.stage),
        BoardRow::Order(order) => format!("{}|{}", order.state, order.stage),
    }
}

#[cfg(test)]
mod tests {
    use super::{Board, BoardLane, BoardRow, member_status_state, rows_of};
    use crate::dto::{
        BloomDispatchesView, BloomStatus, BloomView, CandidateRef, CompositionCursorView, CoordinationView, DigestHex,
        MemberPinView, MemberRequestView, MemberView, MetricDispatch, OrderView, PendingDecisionView, Present,
        SharedRunPlanView, SharedRunView, StageId, ViewDocument,
    };
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::palette::{Depth, Role, with_depth};
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

    /// A board build for which no bloom's dispatch rows have been read.
    fn no_closures(_: DigestHex) -> Option<BloomDispatchesView> {
        None
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

    fn live_order(bloom: DigestHex, workpiece: &str, stage: StageId) -> OrderView {
        OrderView { nonce: format!("dispatch-{workpiece}"), bloom, workpiece: workpiece.to_owned(), stage }
    }

    fn pinned(workpiece: &str) -> MemberView {
        MemberView {
            workpiece: workpiece.to_owned(),
            scope_revision: DigestHex::from_bytes([4; 32]),
            cursor: Some(CompositionCursorView {
                stage: Some(StageId::Verify),
                attempts: 1,
                candidate: Some(CandidateRef {
                    tree: DigestHex::from_bytes([1; 32]),
                    checkout: DigestHex::from_bytes([2; 32]),
                }),
            }),
            ..MemberView::default()
        }
    }

    fn request_for(member: &MemberView, id: DigestHex) -> MemberRequestView {
        MemberRequestView {
            member: MemberPinView {
                workpiece: member.workpiece.clone(),
                scope_revision: member.scope_revision,
                candidate: member.cursor.as_ref().and_then(|cursor| cursor.candidate.clone()).unwrap_or_default(),
            },
            id: Some(id),
        }
    }

    fn dispatch(bloom: DigestHex, workpiece: &str, recorded_unix_millis: Option<u64>, sequence: u64) -> MetricDispatch {
        MetricDispatch {
            bloom,
            workpiece: workpiece.to_owned(),
            recorded_unix_millis,
            sequence,
            ..MetricDispatch::default()
        }
    }

    /// Live sealed bloom plus a landed history bloom with independent dispatch stamps.
    fn ages_document() -> (ViewDocument, [MetricDispatch; 8]) {
        let live = digest(1);
        let landed = digest(2);
        let view = ViewDocument {
            blooms: vec![
                BloomView {
                    id: live,
                    status: Some(BloomStatus::Sealed),
                    members: vec![
                        in_flight_construct("wp-run"),
                        in_flight_construct("wp-gap"),
                        MemberView { resolution: Some(Present {}), ..member("wp-done") },
                    ],
                    ..BloomView::default()
                },
                BloomView {
                    id: landed,
                    status: Some(BloomStatus::Landed),
                    members: vec![MemberView { resolution: Some(Present {}), ..member("wp-landed") }],
                    ..BloomView::default()
                },
            ],
            orders: vec![
                live_order(live, "wp-run", StageId::Construct),
                live_order(live, "wp-gap", StageId::Construct),
            ],
            ..ViewDocument::default()
        };
        let dispatches = [
            dispatch(live, "wp-run", Some(1_000), 1),
            dispatch(live, "wp-run", Some(3_000), 2),
            dispatch(live, "wp-done", Some(2_000), 3),
            dispatch(live, "wp-done", Some(9_000), 4),
            dispatch(live, "wp-gap", Some(5_000), 5),
            dispatch(live, "wp-gap", None, 6),
            dispatch(landed, "wp-landed", Some(10_000), 7),
            dispatch(landed, "wp-landed", Some(7_210_000), 8),
        ];
        (view, dispatches)
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
        assert_eq!(member_status_state(&in_flight_construct("wp"), false), "idle");
        assert_eq!(member_status_state(&in_flight_construct("wp"), true), "running");
        assert_eq!(
            member_status_state(
                &MemberView { resolution: Some(Present {}), blocked_by: Some("wp-a".to_owned()), ..member("wp") },
                false
            ),
            "integrated"
        );
        assert_eq!(
            member_status_state(
                &MemberView { wedge: Some(Present {}), blocked_by: Some("wp-a".to_owned()), ..member("wp") },
                false
            ),
            "WEDGED"
        );
        assert_eq!(
            member_status_state(
                &MemberView { pending_decision: Some(PendingDecisionView::default()), ..member("wp") },
                false
            ),
            "held"
        );
        assert_eq!(member_status_state(&MemberView { blocked_by: Some(String::new()), ..member("wp") }, false), "idle");
        assert_eq!(member_status_state(&member("wp"), false), "idle");
    }

    #[test]
    fn a_member_on_a_shared_verify_run_paints_running() {
        // The plausible bug: a contextual shared run seats the composition
        // workpiece, so looking only at a per-member outstanding order paints
        // the covered member idle while STAGE already names the shared run.
        let bloom = digest(1);
        let covered = pinned("wp-run");
        let waiting = pinned("wp-idle");
        let request_id = DigestHex::from_bytes([8; 32]);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: bloom,
                status: Some(BloomStatus::Sealed),
                members: vec![covered.clone(), waiting],
                coordination: Some(CoordinationView {
                    runs: vec![SharedRunView {
                        plan: SharedRunPlanView { requests: vec![request_for(&covered, request_id)] },
                        phase: "Running".to_owned(),
                        physical_run: Some(DigestHex::from_bytes([7; 32])),
                        unfinished: vec![request_id],
                        ..SharedRunView::default()
                    }],
                    ..CoordinationView::default()
                }),
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let rows = rows_of(&view, BoardLane::Live, &[], &no_closures);
        let members: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                BoardRow::Member(member) => {
                    Some((member.workpiece.as_str(), member.state.as_str(), member.stage.as_str()))
                }
                BoardRow::Bloom(_) | BoardRow::Order(_) => None,
            })
            .collect();
        assert_eq!(members.len(), 2, "{members:?}");
        assert_eq!(members[0].0, "wp-run");
        assert_eq!(members[0].1, "running");
        assert!(members[0].2.contains("shared Running 07070707"), "{}", members[0].2);
        assert_eq!(members[1].0, "wp-idle");
        assert_eq!(members[1].1, "idle");
    }

    #[test]
    fn an_in_flight_construct_renders_running() {
        // Tripwire: running used to be inferred from the cursor, so a member
        // whose host still holds a Construct lane painted idle when the
        // cursor was missing, and a cursor without a lane painted running.
        let bloom = digest(1);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: bloom,
                status: Some(BloomStatus::Sealed),
                members: vec![in_flight_construct("wp")],
                ..BloomView::default()
            }],
            orders: vec![live_order(bloom, "wp", StageId::Construct)],
            ..ViewDocument::default()
        };
        let rows = rows_of(&view, BoardLane::Live, &[], &no_closures);
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
            orders: vec![live_order(digest(2), "wp-run", StageId::Construct)],
            ..ViewDocument::default()
        };
        let live = rows_of(&view, BoardLane::Live, &[], &no_closures);
        let live_ids: Vec<_> = live
            .iter()
            .filter_map(|row| match row {
                BoardRow::Bloom(bloom) => Some(bloom.id),
                BoardRow::Member(_) | BoardRow::Order(_) => None,
            })
            .collect();
        assert_eq!(live_ids, vec![digest(2)]);
        let BoardRow::Member(member) = &live[1] else {
            panic!("second live row is the walking member");
        };
        assert_eq!(member.workpiece, "wp-run");
        assert!(live.iter().all(|row| match row {
            BoardRow::Member(member) => member.workpiece != "wp-done",
            BoardRow::Bloom(_) | BoardRow::Order(_) => true,
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
        let history = rows_of(&view, BoardLane::History, &[], &no_closures);
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
        let bloom = digest(1);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: bloom,
                status: Some(BloomStatus::Sealed),
                members: vec![in_flight_construct("wp")],
                ..BloomView::default()
            }],
            orders: vec![live_order(bloom, "wp", StageId::Construct)],
            ..ViewDocument::default()
        };
        let dispatches = [
            MetricDispatch {
                bloom,
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
        let rows = rows_of(&view, BoardLane::Live, &dispatches, &no_closures);
        let BoardRow::Member(member) = &rows[1] else {
            panic!("second row is the member");
        };
        assert_eq!(member.stage, "Construct");
        assert_eq!(member.age, "3s");
    }

    #[test]
    fn displayed_bloom_ages_ignore_undisplayed_history() {
        // The plausible bug: AGE still walks every retained member and
        // landed bloom, so a live header absorbs a history span, a walking
        // member loses its own stamps, or a missing timestamp paints a duration.
        let (view, dispatches) = ages_document();
        let live = digest(1);
        let landed = digest(2);
        let live_rows = rows_of(&view, BoardLane::Live, &dispatches, &no_closures);
        assert_eq!(live_rows.len(), 3);
        let BoardRow::Bloom(bloom) = &live_rows[0] else {
            panic!("first live row is the bloom");
        };
        assert_eq!(bloom.id, live);
        assert_eq!(bloom.age, "8s");
        let BoardRow::Member(running) = &live_rows[1] else {
            panic!("second live row is the walking member");
        };
        assert_eq!(running.workpiece, "wp-run");
        assert_eq!(running.age, "2s");
        let BoardRow::Member(gap) = &live_rows[2] else {
            panic!("third live row is the single-stamp member");
        };
        assert_eq!(gap.workpiece, "wp-gap");
        assert_eq!(gap.age, "—");
        assert!(live_rows.iter().all(|row| match row {
            BoardRow::Member(member) => member.workpiece != "wp-done" && member.workpiece != "wp-landed",
            BoardRow::Bloom(bloom) => bloom.id != landed,
            BoardRow::Order(_) => true,
        }));

        let history_rows = rows_of(&view, BoardLane::History, &dispatches, &no_closures);
        assert_eq!(history_rows.len(), 2);
        let BoardRow::Bloom(history) = &history_rows[0] else {
            panic!("first history row is the landed bloom");
        };
        assert_eq!(history.id, landed);
        assert_eq!(history.age, "2h");
        let BoardRow::Member(landed_member) = &history_rows[1] else {
            panic!("second history row is the landed member");
        };
        assert_eq!(landed_member.workpiece, "wp-landed");
        assert_eq!(landed_member.age, "2h");
    }

    #[test]
    fn live_board_paint_keeps_history_age_off_the_age_column() {
        // The plausible bug: live paint still rescans undisplayed history, so
        // a landed bloom's span appears in the AGE column of the live table.
        let (view, dispatches) = ages_document();
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_view(Ok(view));
        store.apply_dispatches(Ok(dispatches.to_vec()));
        let mut board = Board::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test backend");
        terminal.draw(|frame| board.render(frame, frame.area(), &store)).expect("draw");
        let text: String = terminal.backend().buffer().content().iter().map(Cell::symbol).collect();
        assert!(text.contains("8s"), "{text}");
        assert!(text.contains("2s"), "{text}");
        assert!(text.contains("—"), "{text}");
        assert!(!text.contains("2h"), "{text}");
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
            orders: vec![live_order(digest(1), "wp", StageId::Construct)],
            ..ViewDocument::default()
        };
        let live: Vec<_> = rows_of(&view, BoardLane::Live, &[], &no_closures)
            .into_iter()
            .filter_map(|row| match row {
                BoardRow::Bloom(bloom) => Some(bloom.id),
                BoardRow::Member(_) | BoardRow::Order(_) => None,
            })
            .collect();
        let history: Vec<_> = rows_of(&view, BoardLane::History, &[], &no_closures)
            .into_iter()
            .filter_map(|row| match row {
                BoardRow::Bloom(bloom) => Some(bloom.id),
                BoardRow::Member(_) | BoardRow::Order(_) => None,
            })
            .collect();
        assert_eq!(live, vec![digest(1)]);
        assert_eq!(history, vec![digest(2), digest(3)]);
    }

    #[test]
    fn o_opens_the_coordinator_log() {
        // The plausible bug: the footer paints `o logs` while the match
        // drops it, so the advertised door goes nowhere.
        let store = Store::new(Duration::from_secs(1));
        assert_eq!(
            Board::new().handle_key(KeyEvent::from(KeyCode::Char('o')), &store),
            Outcome::Push(Nav::coordinator_log())
        );
        assert_eq!(
            Board::history().handle_key(KeyEvent::from(KeyCode::Char('o')), &store),
            Outcome::Push(Nav::coordinator_log())
        );
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
            orders: vec![live_order(digest(1), "wp", StageId::Construct)],
            ..ViewDocument::default()
        }));
        let mut board = Board::new();
        board.reseat(&store);
        assert_eq!(board.handle_key(KeyEvent::from(KeyCode::Char('j')), &store), Outcome::Handled);
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test backend");
        terminal.draw(|frame| board.render(frame, frame.area(), &store)).expect("draw");
        assert_eq!(super::super::row_caret(&terminal, "wp"), "> ");
    }

    #[test]
    fn a_changed_member_row_flashes_for_one_view_sample() {
        // The plausible bug: `/view` already re-polls, but a state change is
        // silent, so the HUD still reads as a still frame and the operator
        // hits `r` looking for movement that already happened.
        with_depth(Depth::Truecolor, || {
            let bloom = digest(1);
            let mut store = Store::new(Duration::from_secs(1));
            store.apply_view(Ok(ViewDocument {
                blooms: vec![BloomView {
                    id: bloom,
                    status: Some(BloomStatus::Sealed),
                    members: vec![in_flight_construct("wp")],
                    ..BloomView::default()
                }],
                orders: vec![live_order(bloom, "wp", StageId::Construct)],
                ..ViewDocument::default()
            }));
            let mut board = Board::new();
            board.reseat(&store);
            store.apply_view(Ok(ViewDocument {
                blooms: vec![BloomView {
                    id: bloom,
                    status: Some(BloomStatus::Sealed),
                    members: vec![in_flight_construct("wp")],
                    ..BloomView::default()
                }],
                orders: vec![live_order(bloom, "wp", StageId::Verify)],
                ..ViewDocument::default()
            }));
            board.reseat(&store);
            let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("test backend");
            terminal.draw(|frame| board.render(frame, frame.area(), &store)).expect("draw");
            let buffer = terminal.backend().buffer();
            let area = buffer.area();
            let working = Role::Working.color(Depth::Truecolor);
            let mut saw = false;
            for y in 0..area.height {
                let row: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                if !row.contains("wp") {
                    continue;
                }
                saw = true;
                assert_eq!(buffer[(4, y)].fg, working, "changed member row did not flash: {row}");
            }
            assert!(saw, "member row missing from board");
        });
    }

    #[test]
    fn a_live_base_verify_order_renders_as_a_stage_row() {
        // The plausible bug: outstanding orders stay off /view, so a bloom-less
        // BaseVerify never appears and the board cannot name the stage that is
        // actually running.
        let view = ViewDocument {
            orders: vec![OrderView {
                nonce: "dispatch-base".to_owned(),
                bloom: DigestHex::default(),
                workpiece: String::new(),
                stage: StageId::BaseVerify,
            }],
            ..ViewDocument::default()
        };
        let live = rows_of(&view, BoardLane::Live, &[], &no_closures);
        assert_eq!(live.len(), 1);
        let BoardRow::Order(order) = &live[0] else {
            panic!("the live board names the workspace stage");
        };
        assert_eq!(order.workpiece, "base-verify");
        assert_eq!(order.state, "running");
        assert_eq!(order.stage, "BaseVerify");
        assert!(rows_of(&view, BoardLane::History, &[], &no_closures).is_empty());
    }
}

#[cfg(test)]
mod depends_column_tests {
    use crate::dto::{
        BloomDispatchesView, BloomStatus, BloomView, CoordinationView, DigestHex, MemberView, SemanticEdgeView,
        ViewDocument,
    };

    use super::{BoardLane, BoardRow, depends_cell, rows_of};

    fn served(edges: &[(&str, &str)]) -> BloomDispatchesView {
        BloomDispatchesView {
            dispatches: Vec::new(),
            semantic_edges: edges
                .iter()
                .map(|(member, depends_on)| SemanticEdgeView {
                    member: (*member).to_owned(),
                    depends_on: (*depends_on).to_owned(),
                    through: vec!["aether-bloomery".to_owned()],
                })
                .collect(),
        }
    }

    // The bug the owner named: a grouping that is missing because the members
    // share no file still has a semantic dependency, and a board that renders
    // nothing there tells the operator the members are independent. The column
    // must name the peer.
    #[test]
    fn a_member_names_the_peers_its_closure_reaches() {
        let bloom = DigestHex::from_bytes([1; 32]);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: bloom,
                status: Some(BloomStatus::Sealed),
                coordination: Some(CoordinationView::default()),
                members: vec![
                    MemberView { workpiece: "issue-1".to_owned(), ..MemberView::default() },
                    MemberView { workpiece: "issue-2".to_owned(), ..MemberView::default() },
                ],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let edges = served(&[("issue-1", "issue-2")]);
        let rows = rows_of(&view, BoardLane::Live, &[], &|_| Some(edges.clone()));

        let members: Vec<&super::MemberRow> = rows
            .iter()
            .filter_map(|row| match row {
                BoardRow::Member(member) => Some(member),
                BoardRow::Bloom(_) | BoardRow::Order(_) => None,
            })
            .collect();
        assert_eq!(depends_cell(members[0].depends.as_ref()), "issue-2", "the dependent member names its peer");
        assert_eq!(depends_cell(members[1].depends.as_ref()), "—", "the peer depends on nobody in this direction");
    }

    // Tripwire: "nobody read this bloom's closures" and "this member reaches no
    // sibling's write" must not render the same. A blank cell for the first is
    // an unbacked claim of independence.
    #[test]
    fn an_unread_bloom_renders_a_question_rather_than_independence() {
        assert_eq!(depends_cell(None), "?");
        assert_eq!(depends_cell(Some(&Vec::new())), "—");
    }
}
