//! Bloom, member, composition, dispatch, and seal detail.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::{List, ListItem, ListState};

use crate::cursor::Cursor;
use crate::dto::{BloomView, CompositionFinding, CompositionView, DigestHex, MemberView, ViewDocument};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

use super::board::member_status_state;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "l", action: "journal" },
    KeyHint { keys: "t", action: "timeline" },
    KeyHint { keys: "d", action: "days" },
    KeyHint { keys: "c", action: "cost" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Stable identity of one selectable detail row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKey {
    Identity,
    Successor,
    Member(String),
    BlockedBy,
    Digest(DigestHex),
    Dispatch,
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
}

impl Detail {
    #[must_use]
    pub fn new(focus: Focus) -> Self {
        Self { focus, lines: Vec::new(), vanished: false, cursor: Cursor::new(), scroll: 0 }
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
        vec![ResourceKey::View]
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
            KeyCode::Char('d') => Outcome::Push(Nav::days()),
            KeyCode::Char('c') => Outcome::Push(Nav::cost()),
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
            self.rebuild(view);
            self.vanished = false;
            self.reseat_cursor();
            return;
        }
        if let Some(parent) = self.focus.parent()
            && focus_exists(&parent, view)
        {
            self.focus = parent;
            self.rebuild(view);
            self.vanished = false;
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
            self.rebuild(view);
            self.reseat_cursor();
        }
        let dimmed = self.vanished || store.view().is_stale();
        let muted = if dimmed {
            palette::body().add_modifier(Modifier::DIM)
        } else {
            palette::body()
        };
        let items: Vec<ListItem> =
            self.lines.iter().map(|line| ListItem::new(line.text.clone()).style(muted)).collect();
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
            | Focus::Workpiece { .. } => None,
        }
    }

    fn reseat_cursor(&mut self) {
        self.cursor.reseat(&self.lines, |line| line.key.clone(), |_, lines| lines.first().map(|line| line.key.clone()));
    }

    fn rebuild(&mut self, view: &ViewDocument) {
        self.lines = match &self.focus {
            Focus::Bloom { id } => bloom_lines(view, *id),
            Focus::Member { bloom, workpiece } | Focus::Dispatch { bloom, workpiece } => {
                member_lines(view, *bloom, workpiece)
            }
            Focus::Composition { bloom } => composition_lines(view, *bloom),
            Focus::Seal => seal_lines(view),
            Focus::Record { sequence } => vec![label(RowKey::Identity, format!("record {sequence}"))],
            Focus::Artifact { digest } => vec![label(RowKey::Identity, format!("artifact {}", digest.prefix()))],
            Focus::Transcript { nonce } => vec![label(RowKey::Identity, format!("transcript {nonce}"))],
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
        Focus::Record { .. } | Focus::Artifact { .. } | Focus::Transcript { .. } | Focus::Workpiece { .. } => false,
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

fn bloom_lines(view: &ViewDocument, id: DigestHex) -> Vec<Line> {
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
    if let Some(composition) = &bloom.composition {
        push_composition_section(&mut lines, composition);
    }
    lines.extend(lease_lines(bloom));
    for member in &bloom.members {
        let state = member_status_state(member);
        lines.push(Line {
            key: RowKey::Member(member.workpiece.clone()),
            text: format!("  {}  {state}", member.workpiece),
            enter: Some(Nav::focus(Focus::member(bloom.id, member.workpiece.clone()))),
            digest: None,
            openable: false,
        });
    }
    lines
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

fn member_lines(view: &ViewDocument, bloom: DigestHex, workpiece: &str) -> Vec<Line> {
    let Some((bloom, member)) = find_member(view, bloom, workpiece) else {
        return Vec::new();
    };
    let mut lines = vec![Line {
        key: RowKey::Identity,
        text: format!("member {workpiece}  bloom {}  {}", bloom.id.prefix(), bloom.id.as_hex()),
        enter: Some(Nav::focus(Focus::dispatch(bloom.id, member.workpiece.clone()))),
        digest: None,
        openable: false,
    }];
    lines.push(label(RowKey::Other(0), format!("state  {}", member_status_state(member))));
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
            key: RowKey::Dispatch,
            text: format!("cursor  {stage}  ×{}", cursor.attempts),
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
    use super::Detail;
    use crate::dto::{
        BloomView, CandidateRef, CompositionCursorView, DigestHex, MemberView, ReviewParkView, ViewDocument,
    };
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::Store;
    use crate::warroom::Focus;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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
    fn a_row_enter_refuses_paints_no_caret() {
        // The plausible bug: highlight_symbol tracks ListState, so a fact
        // line with no `enter` still paints `>` as if Enter would push.
        let (mut detail, store) = detail_over(Focus::Seal, ViewDocument::default());
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| detail.render(frame, frame.area(), &store)).expect("draw");
        assert_eq!(super::super::row_caret(&terminal, "seal"), "  ");
    }
}
