//! Bloom, member, composition, dispatch, and seal detail.

use std::collections::{HashMap, HashSet};

use aether_bloomery::WorkpieceId;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::{List, ListItem, ListState};

use crate::cursor::Cursor;
use crate::dto::{
    BloomStatus, BloomView, CompositionFinding, CompositionView, DigestHex, JournalRecordView, MemberView, StageId,
    ViewDocument,
};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{DispatchFileQuery, JournalQuery, JournalScope, ResourceKey, Store};
use crate::warroom::Focus;

use super::board::member_status_state;
use super::filed::{FiledRow, filings, read_receipt};
use super::journal_meaning;

/// The lane log a dispatch writes while it runs.
const LANE_LOG: &str = "lane.log";

/// How many of a member's journal records the detail frame carries. The whole
/// slice lives one key away on the journal screen; this is the recent history
/// the derived summary below it has to be read against.
const JOURNAL_TAIL: usize = 12;

/// How many trailing lane-log lines the detail frame carries.
const LANE_TAIL: usize = 12;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "l", action: "journal" },
    KeyHint { keys: "t", action: "timeline" },
    KeyHint { keys: "b", action: "time" },
    KeyHint { keys: "d", action: "days" },
    KeyHint { keys: "c", action: "cost" },
    KeyHint { keys: "o", action: "logs" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Stable identity of one selectable detail row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    Identity,
    Successor,
    Member(String),
    BlockedBy,
    Digest(DigestHex),
    Dispatch,
    Transcript(String),
    Filing(String),
    /// One journal record in the member's slice, keyed on its sequence.
    Record(u64),
    /// One line of the proving dispatch's lane log, keyed on its position.
    Lane(u16),
    Other(u16),
}

#[derive(Clone, Debug)]
struct Line {
    key: RowKey,
    text: String,
    enter: Option<Nav>,
    digest: Option<DigestHex>,
    openable: bool,
}

/// One pushed subject. Last-known lines stay when the subject vanishes.
#[derive(Clone, Debug)]
pub struct Detail {
    focus: Focus,
    lines: Vec<Line>,
    vanished: bool,
    cursor: Cursor<RowKey>,
    scroll: usize,
    last: HashMap<RowKey, String>,
    flashed: HashSet<RowKey>,
    /// Whether the subject is a landed bloom, learned from the last rebuild.
    ///
    /// The filed-findings tail costs a bloom-filtered journal page and the
    /// commission list, and only a landed bloom can have been read at all, so
    /// the subscription follows the status rather than the focus.
    landed: bool,
    /// The dispatch currently proving the member under this frame, learned from
    /// the last rebuild. The lane-log follow subscribes to it, so the tail can
    /// only be asked for once the dispatch page naming it has landed.
    proving: Option<String>,
}

impl Detail {
    #[must_use]
    pub fn new(focus: Focus) -> Self {
        Self {
            focus,
            lines: Vec::new(),
            vanished: false,
            cursor: Cursor::new(),
            scroll: 0,
            last: HashMap::new(),
            flashed: HashSet::new(),
            landed: false,
            proving: None,
        }
    }

    #[must_use]
    pub fn focus(&self) -> &Focus {
        &self.focus
    }

    #[must_use]
    pub fn vanished(&self) -> bool {
        self.vanished
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    #[must_use]
    pub fn selected_key(&self) -> Option<&RowKey> {
        self.cursor.selected()
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        let mut keys = vec![ResourceKey::View];
        if self.landed
            && let Focus::Bloom { id } = &self.focus
        {
            keys.push(ResourceKey::Journal(JournalQuery { bloom: Some(*id), ..JournalQuery::default() }));
            keys.push(ResourceKey::Commissions);
        }
        // A member frame reads like the journal screen does: the raw records
        // naming this member, and the log the dispatch proving it is writing
        // right now. Both follow, so the frame moves while the operator watches.
        if let Focus::Member { bloom, .. } | Focus::Dispatch { bloom, .. } = &self.focus {
            keys.push(ResourceKey::Journal(member_journal_query(*bloom)));
            keys.push(ResourceKey::BloomDispatches(*bloom));
            if let Some(nonce) = &self.proving {
                keys.push(ResourceKey::DispatchFile(lane_query(nonce)));
            }
        }
        keys
    }

    #[must_use]
    pub fn key_hints(&self) -> &'static [KeyHint] {
        HINTS
    }

    #[must_use]
    pub fn digest_under_cursor(&self) -> Option<DigestHex> {
        self.selected_line().and_then(|line| line.digest)
    }

    #[must_use]
    pub fn openable_digest(&self) -> Option<DigestHex> {
        self.selected_line().filter(|line| line.openable).and_then(|line| line.digest)
    }

    #[must_use]
    pub fn enter_pushes(&self) -> bool {
        self.selected_line().and_then(|line| line.enter.as_ref()).is_some()
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&self.lines, |line| line.key.clone());
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&self.lines, |line| line.key.clone());
                Outcome::Handled
            }
            KeyCode::Enter => {
                self.selected_line().and_then(|line| line.enter.clone()).map_or(Outcome::Handled, Outcome::Push)
            }
            KeyCode::Char('l') => self.bloom_id().map_or(Outcome::Handled, |id| Outcome::Push(Nav::journal(Some(id)))),
            KeyCode::Char('t') => self.bloom_id().map_or(Outcome::Handled, |id| Outcome::Push(Nav::timeline(id))),
            KeyCode::Char('b') => self.time_nav().map_or(Outcome::Handled, Outcome::Push),
            KeyCode::Char('d') => Outcome::Push(Nav::days()),
            KeyCode::Char('c') => Outcome::Push(Nav::cost()),
            KeyCode::Char('o') => Outcome::Push(Nav::coordinator_log()),
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let Some(view) = store.view().value.as_ref() else {
            return;
        };
        if focus_exists(&self.focus, view) {
            self.rebuild(view, store);
            self.vanished = false;
            self.note_flash();
            self.reseat_cursor();
            return;
        }
        if let Some(parent) = self.focus.parent()
            && focus_exists(&parent, view)
        {
            self.focus = parent;
            self.rebuild(view, store);
            self.vanished = false;
            self.note_flash();
            self.reseat_cursor();
            return;
        }
        if !self.lines.is_empty() {
            self.vanished = true;
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        if self.lines.is_empty()
            && let Some(view) = store.view().value.as_ref()
        {
            self.rebuild(view, store);
            self.reseat_cursor();
        }
        let dimmed = self.vanished || store.view().is_stale();
        let muted = if dimmed {
            palette::body().add_modifier(Modifier::DIM)
        } else {
            palette::body()
        };
        let items: Vec<ListItem> = self
            .lines
            .iter()
            .map(|line| {
                let style = if dimmed {
                    muted
                } else if self.flashed.contains(&line.key) {
                    palette::flash()
                } else {
                    muted
                };
                ListItem::new(line.text.clone()).style(style)
            })
            .collect();
        let list = List::new(items)
            .style(palette::body())
            .highlight_style(palette::cursor())
            .highlight_symbol(super::caret(self.enter_pushes()));
        let mut state = ListState::default()
            .with_selected(self.cursor.selected_index(&self.lines, |line| line.key.clone()))
            .with_offset(self.scroll);
        frame.render_stateful_widget(list, area, &mut state);
        self.scroll = state.offset();
    }

    fn selected_line(&self) -> Option<&Line> {
        let key = self.cursor.selected()?;
        self.lines.iter().find(|line| line.key == *key)
    }

    fn bloom_id(&self) -> Option<DigestHex> {
        match &self.focus {
            Focus::Bloom { id } | Focus::Composition { bloom: id } => Some(*id),
            Focus::Member { bloom, .. } | Focus::Dispatch { bloom, .. } => Some(*bloom),
            Focus::Seal
            | Focus::Record { .. }
            | Focus::Artifact { .. }
            | Focus::Transcript { .. }
            | Focus::Evidence { .. }
            | Focus::EvidenceFile { .. }
            | Focus::Workpiece { .. } => None,
        }
    }

    fn time_nav(&self) -> Option<Nav> {
        match &self.focus {
            Focus::Member { bloom, workpiece } => Some(Nav::time(*bloom, workpiece.clone())),
            Focus::Composition { bloom } => Some(Nav::time(*bloom, WorkpieceId::COMPOSITION)),
            Focus::Bloom { id } => match self.selected_line() {
                Some(Line { key: RowKey::Member(workpiece), .. }) => Some(Nav::time(*id, workpiece.clone())),
                _ => None,
            },
            Focus::Seal
            | Focus::Dispatch { .. }
            | Focus::Record { .. }
            | Focus::Artifact { .. }
            | Focus::Transcript { .. }
            | Focus::Evidence { .. }
            | Focus::EvidenceFile { .. }
            | Focus::Workpiece { .. } => None,
        }
    }

    fn reseat_cursor(&mut self) {
        self.cursor.reseat(&self.lines, |line| line.key.clone(), |_, lines| lines.first().map(|line| line.key.clone()));
    }

    fn note_flash(&mut self) {
        let mut lit = HashSet::new();
        let mut next = HashMap::new();
        for line in &self.lines {
            if self.last.get(&line.key).is_some_and(|prev| prev != &line.text) {
                lit.insert(line.key.clone());
            }
            next.insert(line.key.clone(), line.text.clone());
        }
        self.last = next;
        self.flashed = lit;
    }

    fn rebuild(&mut self, view: &ViewDocument, store: &Store) {
        self.landed = match &self.focus {
            Focus::Bloom { id } => find_bloom(view, *id).is_some_and(|found| found.status == Some(BloomStatus::Landed)),
            Focus::Member { .. }
            | Focus::Dispatch { .. }
            | Focus::Composition { .. }
            | Focus::Seal
            | Focus::Record { .. }
            | Focus::Artifact { .. }
            | Focus::Transcript { .. }
            | Focus::Evidence { .. }
            | Focus::EvidenceFile { .. }
            | Focus::Workpiece { .. } => false,
        };

        self.proving = match &self.focus {
            Focus::Member { bloom, workpiece } | Focus::Dispatch { bloom, workpiece } => {
                proving_dispatch(store, *bloom, workpiece)
            }
            Focus::Bloom { .. }
            | Focus::Composition { .. }
            | Focus::Seal
            | Focus::Record { .. }
            | Focus::Artifact { .. }
            | Focus::Transcript { .. }
            | Focus::Evidence { .. }
            | Focus::EvidenceFile { .. }
            | Focus::Workpiece { .. } => None,
        };

        self.lines = match &self.focus {
            Focus::Bloom { id } => bloom_lines(view, store, *id),
            Focus::Member { bloom, workpiece } | Focus::Dispatch { bloom, workpiece } => {
                member_lines(view, store, *bloom, workpiece, self.proving.as_deref())
            }
            Focus::Composition { bloom } => composition_lines(view, *bloom),
            Focus::Seal => seal_lines(view),
            Focus::Record { sequence } => vec![label(RowKey::Identity, format!("record {sequence}"))],
            Focus::Artifact { digest } => vec![label(RowKey::Identity, format!("artifact {}", digest.prefix()))],
            Focus::Transcript { nonce } => vec![label(RowKey::Identity, format!("transcript {nonce}"))],
            Focus::Evidence { nonce } => vec![label(RowKey::Identity, format!("evidence {nonce}"))],
            Focus::EvidenceFile { name, nonce } => vec![label(RowKey::Identity, format!("file {name} {nonce}"))],
            Focus::Workpiece { id } => vec![label(RowKey::Identity, format!("workpiece {id}"))],
        };
    }
}

fn focus_exists(focus: &Focus, view: &ViewDocument) -> bool {
    match focus {
        Focus::Bloom { id } => find_bloom(view, *id).is_some(),
        Focus::Member { bloom, workpiece } | Focus::Dispatch { bloom, workpiece } => {
            find_member(view, *bloom, workpiece).is_some()
        }
        Focus::Composition { bloom } => find_bloom(view, *bloom).is_some_and(|bloom| bloom.composition.is_some()),
        Focus::Seal => true,
        Focus::Record { .. }
        | Focus::Artifact { .. }
        | Focus::Transcript { .. }
        | Focus::Evidence { .. }
        | Focus::EvidenceFile { .. }
        | Focus::Workpiece { .. } => false,
    }
}

fn find_bloom(view: &ViewDocument, id: DigestHex) -> Option<&BloomView> {
    view.blooms.iter().find(|bloom| bloom.id == id)
}

fn find_member<'a>(
    view: &'a ViewDocument,
    bloom: DigestHex,
    workpiece: &str,
) -> Option<(&'a BloomView, &'a MemberView)> {
    let bloom = find_bloom(view, bloom)?;
    bloom.members.iter().find(|member| member.workpiece == workpiece).map(|member| (bloom, member))
}

fn bloom_lines(view: &ViewDocument, store: &Store, id: DigestHex) -> Vec<Line> {
    let Some(bloom) = find_bloom(view, id) else {
        return Vec::new();
    };
    let mut lines = vec![label(RowKey::Identity, format!("bloom {}  {}", bloom.id.prefix(), bloom.id.as_hex()))];
    lines.push(label(
        RowKey::Other(0),
        format!("status  {}", bloom.status.map_or_else(|| "?".to_owned(), |s| s.to_string())),
    ));
    if let Some(successor) = bloom.superseded_by {
        lines.push(Line {
            key: RowKey::Successor,
            text: format!("superseded by  {}  {}", successor.prefix(), successor.as_hex()),
            enter: Some(Nav::focus(Focus::bloom(successor))),
            digest: Some(successor),
            openable: false,
        });
    }
    push_alert_section(&mut lines, bloom);
    if let Some(coordination) = &bloom.coordination {
        for (index, text) in coordination
            .detail_lines(bloom.members.iter().filter(|member| member.withdrawn.is_none()).count())
            .into_iter()
            .enumerate()
        {
            lines.push(label(RowKey::Other(400 + u16::try_from(index).unwrap_or(99)), text));
        }
    }
    if let Some(composition) = &bloom.composition {
        // The composition frame is a tree-walk child of its bloom, not only a
        // needs-you jump: without this row the drill-down dead-ends at labels.
        lines.push(Line {
            key: RowKey::Other(12),
            text: format!("composition  {}  {}", bloom.id.prefix(), bloom.id.as_hex()),
            enter: Some(Nav::focus(Focus::composition(bloom.id))),
            digest: None,
            openable: false,
        });
        push_composition_section(&mut lines, composition);
    }
    lines.extend(lease_lines(bloom));
    for member in &bloom.members {
        let state = member_status_state(member, view.has_lane(bloom.id, member));
        lines.push(Line {
            key: RowKey::Member(member.workpiece.clone()),
            text: format!("  {}  {state}", member.workpiece),
            enter: Some(Nav::focus(Focus::member(bloom.id, member.workpiece.clone()))),
            digest: None,
            openable: false,
        });
    }
    push_filed_section(&mut lines, &filed_rows(store, bloom));
    lines
}

/// The findings this bloom's read filed (ADR-0216 §3).
///
/// Empty for every bloom the reader has not read — which today is every bloom,
/// since the lane is authored and not yet enabled — and the section disappears
/// entirely rather than rendering an empty heading.
fn filed_rows(store: &Store, bloom: &BloomView) -> Vec<FiledRow> {
    if let Some(journal) = store.journal(&JournalQuery { bloom: Some(bloom.id), ..JournalQuery::default() })
        && let Some(page) = journal.value.as_ref()
        && let Some(receipt) = read_receipt(page, bloom.id)
        && let Some(list) = store.commissions().value.as_ref()
    {
        return filings(list, receipt);
    }

    Vec::new()
}

/// The pile, with what it is not stated on its own row.
///
/// A filing is an unapproved proposal: it carries the read's derivation and no
/// approval, and no seal can name it, so nothing here is work until a person
/// scopes and approves it (ADR-0216 §3). The line says so where the pile is
/// read, because a list of work-order-shaped rows under a bloom otherwise reads
/// as a queue somebody already accepted.
fn push_filed_section(lines: &mut Vec<Line>, rows: &[FiledRow]) {
    if rows.is_empty() {
        return;
    }
    lines.push(label(RowKey::Other(300), format!("filed findings  {}", rows.len())));
    lines.push(label(
        RowKey::Other(301),
        "  unapproved reader proposals — work only once a person approves one".to_owned(),
    ));
    for row in rows {
        lines.push(Line {
            key: RowKey::Filing(row.id.clone()),
            text: format!("  {}  {}  {}", row.id, row.status, row.title),
            enter: Some(Nav::focus(Focus::workpiece(row.id.clone()))),
            digest: None,
            openable: false,
        });
        if !row.surface.is_empty() {
            lines.push(label(
                RowKey::Filing(format!("{}  surface", row.id)),
                format!("    surface  {}", row.surface.join("  ")),
            ));
        }
    }
}

fn push_alert_section(lines: &mut Vec<Line>, bloom: &BloomView) {
    if let Some(block) = &bloom.landing_blocked {
        lines.push(label(RowKey::Other(1), format!("land  blocked {}/{}", block.rolls, block.budget)));
    }
    if let Some(fault) = &bloom.executor_fault {
        let terminal = if fault.terminal {
            "  TERMINAL"
        } else {
            ""
        };
        lines.push(label(RowKey::Other(2), format!("fault  {}/{}{terminal}", fault.rolls, fault.budget)));
    }
    if let Some(park) = &bloom.review_park {
        if let Some(prompt) = &park.prompt {
            lines.push(label(RowKey::Other(3), format!("park  {prompt}")));
        } else {
            lines.push(label(RowKey::Other(3), "park".to_owned()));
        }
        lines.push(digest_line(RowKey::Digest(park.question), "question", park.question));
        if !park.options.is_empty() {
            lines.push(label(RowKey::Other(4), format!("  options  {}", park.options.join(", "))));
        }
        if let Some(blocked) = &park.blocked {
            lines.push(label(RowKey::Other(5), format!("  blocked  {blocked}")));
        }
    }
    if let Some(hold) = &bloom.operator_hold {
        lines.push(label(RowKey::Other(6), format!("hold  {}  by {}", hold.reason, hold.operator)));
    }
}

fn push_composition_section(lines: &mut Vec<Line>, composition: &CompositionView) {
    if let Some(cursor) = &composition.cursor {
        let stage = cursor.stage.map_or_else(|| "?".to_owned(), |stage| stage.to_string());
        lines.push(label(RowKey::Other(10), format!("composition cursor  {stage}  ×{}", cursor.attempts)));
        if let Some(candidate) = &cursor.candidate {
            lines.push(reference_line(RowKey::Digest(candidate.tree), "  tree", candidate.tree));
            lines.push(reference_line(RowKey::Digest(candidate.checkout), "  checkout", candidate.checkout));
        }
    }
    if let Some(wedge) = &composition.wedge {
        let stage = wedge.stage.map_or_else(|| "?".to_owned(), |stage| stage.to_string());
        lines.push(label(RowKey::Other(11), format!("composition wedge  {stage}")));
        lines.push(digest_line(RowKey::Digest(wedge.evidence), "  evidence", wedge.evidence));
    }
    for (index, finding) in composition.findings.iter().enumerate() {
        push_finding(lines, finding, index);
    }
}

fn push_finding(lines: &mut Vec<Line>, finding: &CompositionFinding, index: usize) {
    let implicated = if finding.implicated.is_empty() {
        String::new()
    } else {
        format!("  {}", finding.implicated.join(","))
    };
    lines.push(label(RowKey::Other(20 + u16::try_from(index).unwrap_or(u16::MAX)), format!("finding{implicated}")));
    lines.push(digest_line(RowKey::Digest(finding.subject), "  subject", finding.subject));
    lines.push(digest_line(RowKey::Digest(finding.detail), "  detail", finding.detail));
}

/// The query the member frame follows this bloom's journal with.
fn member_journal_query(bloom: DigestHex) -> JournalQuery {
    JournalQuery { bloom: Some(bloom), live: true, ..JournalQuery::default() }
}

/// The query the member frame follows one dispatch's lane log with.
fn lane_query(nonce: &str) -> DispatchFileQuery {
    DispatchFileQuery { nonce: nonce.to_owned(), name: LANE_LOG.to_owned(), cursor: None, live: true }
}

/// The dispatch proving this member right now — the newest row on the bloom's
/// dispatch page that covers it, a grouped shared run's step included.
fn proving_dispatch(store: &Store, bloom: DigestHex, workpiece: &str) -> Option<String> {
    store
        .bloom_dispatches(bloom)?
        .value
        .as_ref()?
        .dispatches
        .iter()
        .rev()
        .find(|row| row.proves(workpiece) && row.evidence_retained)
        .map(|row| row.nonce.clone())
}

/// The records in this bloom's loaded journal that name `workpiece`, oldest
/// first.
///
/// `member_name` reads the fact's own member field; a shared-run fact names
/// several members in its plan and none in that field, so a raw mention of the
/// workpiece counts too. Both are the record's own text, not a derivation.
fn member_records<'a>(store: &'a Store, bloom: DigestHex, workpiece: &str) -> Vec<&'a JournalRecordView> {
    let scope = JournalScope { bloom: Some(bloom), contains: None };
    let Some(archive) = store.journal_archive(&scope) else {
        return Vec::new();
    };
    archive
        .rows()
        .iter()
        .filter(|record| {
            journal_meaning::member_name(record) == workpiece
                || serde_json::to_string(&record.event).is_ok_and(|text| text.contains(workpiece))
        })
        .collect()
}

/// The member's own journal slice, sequence-stamped, above everything derived.
///
/// Each row opens its record, so a derived word below can be checked against
/// the fact it was reduced from without leaving for the journal screen. Empty
/// until a journal page for this bloom has landed, and the section disappears
/// entirely rather than rendering an empty heading.
fn push_journal_slice(lines: &mut Vec<Line>, bloom: DigestHex, records: &[&JournalRecordView]) {
    if records.is_empty() {
        return;
    }
    lines.push(Line {
        key: RowKey::Other(500),
        text: format!("journal  {} loaded records", records.len()),
        enter: Some(Nav::journal(Some(bloom))),
        digest: None,
        openable: false,
    });
    for record in records.iter().rev().take(JOURNAL_TAIL).rev() {
        lines.push(Line {
            key: RowKey::Record(record.sequence),
            text: format!("  {}", journal_meaning::rendered_line(record)),
            enter: Some(Nav::focus(Focus::record(record.sequence))),
            digest: None,
            openable: false,
        });
    }
}

/// The tail of the lane log the proving dispatch is writing.
fn push_lane_tail(lines: &mut Vec<Line>, store: &Store, nonce: Option<&str>) {
    let Some(nonce) = nonce else {
        return;
    };
    lines.push(Line {
        key: RowKey::Dispatch,
        text: format!("proving  {nonce}"),
        enter: Some(Nav::evidence(nonce)),
        digest: None,
        openable: false,
    });
    let Some(page) = store.dispatch_file(&lane_query(nonce)).and_then(|cell| cell.value.as_ref()) else {
        return;
    };
    let tail = page.lines.len().saturating_sub(LANE_TAIL);
    for (row, text) in (0u16..).zip(page.lines[tail..].iter()) {
        lines.push(label(RowKey::Lane(row), format!("  {text}")));
    }
}

/// How the record the derived word was reduced from is named beside it.
fn derived_from(records: &[&JournalRecordView]) -> String {
    records.last().map_or_else(String::new, |record| format!("   record {}", record.sequence))
}

fn member_lines(
    view: &ViewDocument,
    store: &Store,
    bloom: DigestHex,
    workpiece: &str,
    proving: Option<&str>,
) -> Vec<Line> {
    let Some((bloom, member)) = find_member(view, bloom, workpiece) else {
        return Vec::new();
    };
    let records = member_records(store, bloom.id, workpiece);
    let derived = derived_from(&records);

    let mut lines = Vec::new();
    push_journal_slice(&mut lines, bloom.id, &records);
    push_lane_tail(&mut lines, store, proving);
    push_verify_transcript(&mut lines, view, bloom.id, workpiece);
    lines.push(Line {
        key: RowKey::Identity,
        text: format!("member {workpiece}  bloom {}  {}", bloom.id.prefix(), bloom.id.as_hex()),
        enter: Some(Nav::focus(Focus::dispatch(bloom.id, member.workpiece.clone()))),
        digest: None,
        openable: false,
    });
    lines.push(label(
        RowKey::Other(0),
        format!("state  {}{derived}", member_status_state(member, view.has_lane(bloom.id, member))),
    ));
    push_member_coordination(&mut lines, bloom, member);
    if let Some(blocked) = member.blocked_by.as_deref().filter(|name| !name.is_empty()) {
        lines.push(Line {
            key: RowKey::BlockedBy,
            text: format!("blocked by  {blocked}"),
            enter: Some(Nav::focus(Focus::member(bloom.id, blocked))),
            digest: None,
            openable: false,
        });
    }
    if let Some(cursor) = &member.cursor {
        let stage = cursor.stage.map_or_else(|| "?".to_owned(), |stage| stage.to_string());
        lines.push(Line {
            key: RowKey::Other(110),
            text: format!("stage  {stage}  ×{}{derived}", cursor.attempts),
            enter: Some(Nav::focus(Focus::dispatch(bloom.id, member.workpiece.clone()))),
            digest: None,
            openable: false,
        });
        if let Some(candidate) = &cursor.candidate {
            lines.push(reference_line(RowKey::Digest(candidate.tree), "  tree", candidate.tree));
            lines.push(reference_line(RowKey::Digest(candidate.checkout), "  checkout", candidate.checkout));
        }
    }
    if member.wedge.is_some() {
        lines.push(label(RowKey::Other(1), "wedge  stopped".to_owned()));
    }
    if let Some(cause) = member.wedge_cause {
        lines.push(label(RowKey::Other(2), format!("cause  {cause}")));
    }
    if let Some(fault) = &member.host_fault {
        let findings = if fault.findings.is_empty() {
            "host fault".to_owned()
        } else {
            fault.findings.clone()
        };
        lines.push(label(RowKey::Other(3), format!("host fault  {findings}")));
    }
    if let Some(pending) = &member.pending_decision {
        lines.push(label(RowKey::Other(4), format!("pending  {}", pending.prompt)));
        lines.push(digest_line(RowKey::Digest(pending.question), "  question", pending.question));
        if !pending.options.is_empty() {
            lines.push(label(RowKey::Other(5), format!("  options  {}", pending.options.join(", "))));
        }
        if !pending.blocked.is_empty() {
            lines.push(label(RowKey::Other(6), format!("  blocked  {}", pending.blocked)));
        }
    }
    if let Some(awaiting) = &member.awaiting_surface {
        lines.push(label(RowKey::Other(8), format!("surface  {} ({} asked)", awaiting.summary, awaiting.requests)));
        for (row, request) in (9u16..99).zip(awaiting.paths.iter()) {
            lines.push(label(RowKey::Other(row), format!("  {}  {}", request.path, request.reason)));
        }
    }
    if let Some(withdrawn) = &member.withdrawn {
        let cause = match withdrawn.depends_on.as_deref() {
            Some(ancestor) if !ancestor.is_empty() => format!("{} ({ancestor})", withdrawn.cause),
            _ => withdrawn.cause.clone(),
        };
        lines.push(label(RowKey::Other(100), format!("withdrawn  {cause}  by {}", withdrawn.operator)));
        lines.push(label(RowKey::Other(101), format!("  reason  {}", withdrawn.reason)));
    }
    // ADR-0198: a lease is only useful if the operator can see who holds it and
    // what displaced whom. The eviction line names both parties on one row, so
    // a stopped member never reads as an unexplained stall.
    if let Some(eviction) = &member.evicted_by {
        lines.push(label(RowKey::Other(102), format!("evicted  {}  by {}", eviction.path, eviction.by)));
    }
    if !member.leases.is_empty() {
        lines.push(label(RowKey::Other(103), format!("leases  {}", member.leases.join("  "))));
    }
    if member.resolution.is_some() {
        lines.push(label(RowKey::Other(7), "resolution  integrated".to_owned()));
    }
    lines
}

fn push_verify_transcript(lines: &mut Vec<Line>, view: &ViewDocument, bloom: DigestHex, workpiece: &str) {
    let Some(order) = view.order_for(bloom, workpiece).filter(|order| order.stage == StageId::Verify) else {
        return;
    };
    lines.push(Line {
        key: RowKey::Transcript(order.nonce.clone()),
        text: format!("transcript  {}", order.nonce),
        enter: Some(Nav::transcript(&order.nonce)),
        digest: None,
        openable: false,
    });
}

fn push_member_coordination(lines: &mut Vec<Line>, bloom: &BloomView, member: &MemberView) {
    let Some(coordination) = &bloom.coordination else {
        return;
    };
    lines.push(label(RowKey::Other(400), coordination.member_summary(member)));
    if let Some((run, millis)) = coordination.member_latency(member) {
        lines.push(label(RowKey::Other(403), format!("verification latency  {millis} ms")));
        if let Some(run) = run {
            lines.push(label(RowKey::Other(404), format!("shared physical run  {}", run.prefix())));
        }
    }
    if let Some(context) = coordination.contexts.get(&member.workpiece) {
        lines.push(reference_line(RowKey::Other(401), "sealed base", context.bloom_base.checkout));
        lines.push(reference_line(RowKey::Other(402), "inherited head", context.starting_head.candidate.checkout));
    }
}

/// The bloom's whole lease table, path-first (ADR-0204 / ADR-0198).
///
/// Rendered on the bloom rather than only under each member because
/// contention is asked about path-first: an eviction names a path, and the
/// answer to "who else is on it" is one row here instead of a scan across
/// members. Empty while nothing has been observed writing, and the section
/// disappears entirely rather than rendering an empty heading.
fn lease_lines(bloom: &BloomView) -> Vec<Line> {
    if bloom.leases.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![label(RowKey::Other(200), format!("leases  {}", bloom.leases.len()))];
    for (row, lease) in (201u16..280).zip(bloom.leases.iter()) {
        let stage = lease.stage.map_or_else(|| "-".to_owned(), |stage| stage.to_string());
        lines.push(label(RowKey::Other(row), format!("  {}  {}  {stage}", lease.path, lease.holder)));
    }
    lines
}

fn composition_lines(view: &ViewDocument, id: DigestHex) -> Vec<Line> {
    let Some(bloom) = find_bloom(view, id) else {
        return Vec::new();
    };
    let mut lines = vec![label(RowKey::Identity, format!("composition {}  {}", bloom.id.prefix(), bloom.id.as_hex()))];
    if let Some(composition) = &bloom.composition {
        push_composition_section(&mut lines, composition);
    }
    lines
}

fn seal_lines(view: &ViewDocument) -> Vec<Line> {
    let mut lines = vec![label(RowKey::Identity, "seal door".to_owned())];
    match &view.spend_quiesce {
        Some(quiesce) => lines.push(label(RowKey::Other(0), quiesce.label())),
        None => lines.push(label(RowKey::Other(0), "open".to_owned())),
    }
    lines
}

fn label(key: RowKey, text: String) -> Line {
    Line { key, text, enter: None, digest: None, openable: false }
}

fn digest_line(key: RowKey, title: &str, digest: DigestHex) -> Line {
    Line {
        key,
        text: format!("{title}  {}  {}", digest.prefix(), digest.as_hex()),
        enter: Some(Nav::focus(Focus::artifact(digest))),
        digest: Some(digest),
        openable: true,
    }
}

/// A digest that is an identity (a bloom id, a git tree, a git commit) and is
/// not content in `aether.artifacts`.
fn reference_line(key: RowKey, title: &str, digest: DigestHex) -> Line {
    Line {
        key,
        text: format!("{title}  {}  {}", digest.prefix(), digest.as_hex()),
        enter: None,
        digest: Some(digest),
        openable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Detail, RowKey, lane_query, member_journal_query};
    use crate::dto::{
        BloomDispatchView, BloomDispatchesView, BloomView, CandidateRef, CompositionCursorView, CompositionView,
        DigestHex, DispatchFilePage, JournalPage, JournalRecordView, MemberView, OrderView, ReviewParkView, StageId,
        ViewDocument,
    };
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::{ResourceKey, Store};
    use crate::warroom::Focus;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn detail_over(focus: Focus, view: ViewDocument) -> (Detail, Store) {
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_view(Ok(view));
        let mut detail = Detail::new(focus);
        detail.reseat(&store);
        (detail, store)
    }

    fn walk_to_digest(detail: &mut Detail, store: &Store, target: DigestHex) {
        for _ in 0..32 {
            if detail.digest_under_cursor() == Some(target) {
                return;
            }
            assert_eq!(detail.handle_key(KeyEvent::from(KeyCode::Char('j')), store), Outcome::Handled);
        }
        panic!("never reached digest {}", target.as_hex());
    }

    #[test]
    fn a_member_frame_reads_its_journal_slice_and_lane_log_above_what_it_derives() {
        // The plausible bug (issue 6071): the member frame paints only derived
        // words, with no path back to the record they were reduced from and no
        // log text at all, so "idle Verify" cannot be checked without leaving
        // for the journal screen — and the grouped run writing the lane log
        // that would explain it is never named.
        let bloom = digest(1);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: bloom,
                members: vec![MemberView { workpiece: "wp-a".to_owned(), ..MemberView::default() }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_view(Ok(view));
        store.apply_journal(
            &member_journal_query(bloom),
            Ok(JournalPage {
                records: vec![JournalRecordView {
                    sequence: 4_182,
                    event: json!({ "AttemptCompleted": { "workpiece": "wp-a" } }),
                    ..JournalRecordView::default()
                }],
                ..JournalPage::default()
            }),
        );
        store.apply_bloom_dispatches(
            bloom,
            Ok(BloomDispatchesView {
                dispatches: vec![BloomDispatchView {
                    nonce: "dispatch-9-step-0".to_owned(),
                    workpiece: "aether.bloomery.composition".to_owned(),
                    stage: StageId::AggregateVerify,
                    attempt: 1,
                    evidence_retained: true,
                    covers: Some(vec!["wp-a".to_owned()]),
                    ..BloomDispatchView::default()
                }],
                semantic_edges: Vec::new(),
            }),
        );
        let mut detail = Detail::new(Focus::member(bloom, "wp-a"));
        detail.reseat(&store);
        assert!(
            detail.subscriptions().contains(&ResourceKey::DispatchFile(lane_query("dispatch-9-step-0"))),
            "the frame follows the lane log of the dispatch proving this member"
        );
        store.apply_dispatch_file(
            lane_query("dispatch-9-step-0"),
            Ok(DispatchFilePage {
                lines: vec!["Compiling aether-bloomery".to_owned(), "Finished dev profile".to_owned()],
                ..DispatchFilePage::default()
            }),
        );
        detail.reseat(&store);

        let texts: Vec<&str> = detail.lines.iter().map(|line| line.text.as_str()).collect();
        let journal_row = texts.iter().position(|text| text.contains("4182")).expect("the journal slice is painted");
        let lane_row =
            texts.iter().position(|text| text.contains("Finished dev profile")).expect("the lane tail is painted");
        let state_row =
            texts.iter().position(|text| text.starts_with("state  ")).expect("the derived state is still painted");
        assert!(journal_row < state_row, "the raw records sit above the derived summary: {texts:?}");
        assert!(lane_row < state_row, "so does the log text: {texts:?}");
        assert!(
            texts[state_row].contains("record 4182"),
            "the derived word names the record it can be checked against: {}",
            texts[state_row]
        );
        assert_eq!(
            detail.lines.iter().find(|line| line.key == RowKey::Record(4_182)).and_then(|line| line.enter.clone()),
            Some(Nav::focus(Focus::record(4_182))),
            "a record row opens that record"
        );
        assert_eq!(
            detail.lines.iter().find(|line| line.key == RowKey::Dispatch).and_then(|line| line.enter.clone()),
            Some(Nav::evidence("dispatch-9-step-0")),
            "the proving row is one key from the evidence, gate logs included"
        );
    }

    #[test]
    fn b_opens_the_member_time_breakdown() {
        // The plausible bug: the footer paints `b time` on a member frame
        // while the match drops it, so the advertised door goes nowhere.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView { workpiece: "wp-a".to_owned(), ..MemberView::default() }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let (mut detail, store) = detail_over(Focus::member(digest(1), "wp-a"), view);
        assert_eq!(
            detail.handle_key(KeyEvent::from(KeyCode::Char('b')), &store),
            Outcome::Push(Nav::time(digest(1), "wp-a"))
        );
    }

    #[test]
    fn o_opens_the_coordinator_log() {
        // The plausible bug: the footer paints `o logs` while the match
        // drops it, so the advertised door goes nowhere.
        let view = ViewDocument {
            blooms: vec![BloomView { id: digest(1), ..BloomView::default() }],
            ..ViewDocument::default()
        };
        let (mut detail, store) = detail_over(Focus::bloom(digest(1)), view);
        assert_eq!(
            detail.handle_key(KeyEvent::from(KeyCode::Char('o')), &store),
            Outcome::Push(Nav::coordinator_log())
        );
    }

    #[test]
    fn detail_footer_keys_are_handled() {
        // The plausible bug: Esc is painted and only the shell pops, so a
        // later caller that asks the screen itself sees Ignored.
        let nav = Nav::focus(Focus::bloom(DigestHex::from_bytes([1; 32])));
        assert_footer_honest(Detail::new(Focus::bloom(DigestHex::from_bytes([1; 32]))).key_hints(), |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn a_candidate_tree_is_shown_but_not_openable() {
        // The plausible bug: a candidate tree is a digest on the row, so `a`
        // (and Enter) treat it as artifact content and open a 404 frame.
        // Tripwire: a git tree hash is not artifact content — if this ever
        // returns Some, `a` on that row 404s again.
        let tree = digest(0x22);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    cursor: Some(CompositionCursorView {
                        candidate: Some(CandidateRef { tree, checkout: digest(0x33) }),
                        ..CompositionCursorView::default()
                    }),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let (mut detail, store) = detail_over(Focus::member(digest(1), "issue-1"), view);
        walk_to_digest(&mut detail, &store, tree);
        assert_eq!(detail.digest_under_cursor(), Some(tree));
        assert_eq!(detail.openable_digest(), None);
    }

    #[test]
    fn a_park_question_stays_openable() {
        // The plausible bug: closing the identity-digest doorway also hides
        // the artifact key on the rows that actually store bytes.
        // Tripwire: over-tightening the predicate would silently remove the
        // artifact key from the rows it is actually for.
        let question = digest(0x11);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                review_park: Some(ReviewParkView { question, ..ReviewParkView::default() }),
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let (mut detail, store) = detail_over(Focus::bloom(digest(1)), view);
        walk_to_digest(&mut detail, &store, question);
        assert_eq!(detail.openable_digest(), Some(question));
    }

    #[test]
    fn a_bloom_detail_walks_to_its_composition() {
        // The plausible bug: the composition frame is a needs-you-only jump,
        // so tree-walking from the bloom dead-ends at unselectable labels and
        // the drill-down is two trees depending on where the operator starts.
        let bloom = digest(1);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: bloom,
                composition: Some(CompositionView {
                    cursor: Some(CompositionCursorView {
                        stage: Some(StageId::Construct),
                        attempts: 2,
                        candidate: None,
                    }),
                    ..CompositionView::default()
                }),
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let (mut detail, store) = detail_over(Focus::bloom(bloom), view);
        for _ in 0..32 {
            if detail.selected_key() == Some(&RowKey::Other(12)) {
                break;
            }
            assert_eq!(detail.handle_key(KeyEvent::from(KeyCode::Char('j')), &store), Outcome::Handled);
        }
        assert_eq!(detail.selected_key(), Some(&RowKey::Other(12)));
        assert_eq!(
            detail.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::focus(Focus::composition(bloom)))
        );
    }

    #[test]
    fn a_live_verify_order_opens_the_transcript() {
        // The plausible bug: a live Verify order is only reachable through the
        // on-demand dispatch list, so the operator never sees the session while
        // it is still being written.
        let bloom = digest(1);
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: bloom,
                members: vec![MemberView { workpiece: "wp".to_owned(), ..MemberView::default() }],
                ..BloomView::default()
            }],
            orders: vec![OrderView {
                nonce: "dispatch-verify".to_owned(),
                bloom,
                workpiece: "wp".to_owned(),
                stage: StageId::Verify,
            }],
            ..ViewDocument::default()
        };
        let (mut detail, store) = detail_over(Focus::member(bloom, "wp"), view);
        assert_eq!(detail.selected_key(), Some(&RowKey::Transcript("dispatch-verify".to_owned())));
        assert_eq!(
            detail.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::transcript("dispatch-verify"))
        );
    }

    #[test]
    fn a_row_enter_refuses_paints_no_caret() {
        // The plausible bug: highlight_symbol tracks ListState, so a fact
        // line with no `enter` still paints `>` as if Enter would push.
        let (mut detail, store) = detail_over(Focus::Seal, ViewDocument::default());
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| detail.render(frame, frame.area(), &store)).expect("draw");
        assert_eq!(super::super::row_caret(&terminal, "seal"), "  ");
    }
}
