//! Shell: endpoint, store, fetch lanes, a three-pane workspace, and pushed frames.
//!
//! Screens receive the store read-only and cannot fetch or mutate it.
//! The header and the one-line footer are shell chrome. Only the middle
//! band swaps between the workspace and the top pushed frame.

pub mod chrome;
mod workspace;

use std::iter::once;
use std::thread;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::Block;

use crate::fetch::{FetchLanes, FetchReply, ResourceBody};
use crate::http::Endpoint;
use crate::keys::{INLINE_HINTS, KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::screen::{Screen, compose};
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;
use workspace::Workspace;

#[cfg(test)]
use crate::dto::{BloomDispatchesView, DigestHex, ViewDocument};
#[cfg(test)]
use crate::fetch::FetchProbe;
#[cfg(test)]
use crate::screen::Board;
#[cfg(test)]
use workspace::PaneId;

const ARTIFACT_HINT: KeyHint = KeyHint { keys: "a", action: "artifact" };

/// The running console: one workspace, one store, two fetch lanes.
pub struct Shell {
    endpoint_label: String,
    store: Store,
    fetch: Option<FetchLanes>,
    workspace: Workspace,
    stack: Vec<Screen>,
}

impl Shell {
    #[must_use]
    pub fn new<'scope>(scope: &'scope thread::Scope<'scope, '_>, endpoint: Endpoint, view_cadence: Duration) -> Self {
        let endpoint_label = endpoint.label();
        Self::assemble(endpoint_label, Store::new(view_cadence), Some(FetchLanes::spawn(scope, endpoint)))
    }

    fn assemble(endpoint_label: String, store: Store, fetch: Option<FetchLanes>) -> Self {
        Self { endpoint_label, store, fetch, workspace: Workspace::new(), stack: Vec::new() }
    }

    /// Drain finished fetches and issue any due subscriptions. No HTTP.
    pub fn pump(&mut self) {
        self.drain_replies();
        self.request_due();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind != KeyEventKind::Press {
            return Outcome::Ignored;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Quit;
        }
        if key.code == KeyCode::Tab && self.stack.is_empty() {
            self.workspace.cycle();
            return Outcome::Handled;
        }
        let outcome = self.delegate_key(key);
        if outcome != Outcome::Ignored {
            return outcome;
        }
        match key.code {
            KeyCode::Esc if !self.stack.is_empty() => {
                self.stack.pop();
                Outcome::Handled
            }
            KeyCode::Char('a') => self.handle_artifact(),
            _ => Outcome::Ignored,
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        frame.render_widget(Block::default().style(palette::body()), frame.area());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
            .split(frame.area());
        let dashboard = compose(&self.store);
        frame.render_widget(
            chrome::header(&self.endpoint_label, self.store.view(), Some(&dashboard), frame.area().width),
            chunks[0],
        );
        if self.stack.is_empty() {
            self.workspace.render(frame, chunks[1], &self.store);
        } else if let Some(screen) = self.stack.last_mut() {
            screen.render(frame, chunks[1], &self.store);
        }
        frame.render_widget(chrome::footer(&self.footer_trail(), INLINE_HINTS, chunks[2].width), chunks[2]);
    }

    fn drain_replies(&mut self) {
        let Some(fetch) = &self.fetch else {
            return;
        };
        let replies: Vec<FetchReply> = fetch.drain().collect();
        let mut view_changed = false;
        let mut other_changed = false;
        for reply in replies {
            if matches!(reply.key, ResourceKey::View) {
                view_changed = true;
            } else {
                other_changed = true;
            }
            self.apply_reply(reply);
        }
        if view_changed {
            self.reseat_top();
        } else if other_changed && let Some(top) = self.stack.last_mut() {
            top.reseat(&self.store);
        }
    }

    fn apply_reply(&mut self, reply: FetchReply) {
        match (reply.key, reply.outcome) {
            (ResourceKey::View, Ok(ResourceBody::View(view))) => self.store.apply_view(Ok(view)),
            (ResourceKey::View, Err(error)) => self.store.apply_view(Err(error)),
            (ResourceKey::View, Ok(_)) => self.store.apply_view(Err("view lane returned a non-view body".to_owned())),
            (ResourceKey::Journal(query), Ok(ResourceBody::Journal(page))) => self.store.apply_journal(query, Ok(page)),
            (ResourceKey::Journal(query), Err(error)) => self.store.apply_journal(query, Err(error)),
            (ResourceKey::Journal(query), Ok(_)) => {
                self.store.apply_journal(query, Err("journal lane returned a non-journal body".to_owned()));
            }
            (ResourceKey::Artifact(digest), Ok(ResourceBody::Artifact(body))) => {
                self.store.apply_artifact(digest, Ok(body));
            }
            (ResourceKey::Artifact(digest), Err(error)) => self.store.apply_artifact(digest, Err(error)),
            (ResourceKey::Artifact(digest), Ok(_)) => {
                self.store.apply_artifact(digest, Err("artifact lane returned a non-artifact body".to_owned()));
            }
            (ResourceKey::Transcript(query), Ok(ResourceBody::Transcript(page))) => {
                self.store.apply_transcript(query, Ok(page));
            }
            (ResourceKey::Transcript(query), Err(error)) => self.store.apply_transcript(query, Err(error)),
            (ResourceKey::Transcript(query), Ok(_)) => {
                self.store.apply_transcript(query, Err("transcript lane returned a non-transcript body".to_owned()));
            }
            (ResourceKey::Prompt(query), Ok(ResourceBody::Prompt(page))) => {
                self.store.apply_prompt(query, Ok(page));
            }
            (ResourceKey::Prompt(query), Err(error)) => self.store.apply_prompt(query, Err(error)),
            (ResourceKey::Prompt(query), Ok(_)) => {
                self.store.apply_prompt(query, Err("prompt lane returned a non-prompt body".to_owned()));
            }
            (ResourceKey::MetricsSummary, Ok(ResourceBody::Summary(value))) => self.store.apply_summary(Ok(value)),
            (ResourceKey::MetricsSummary, Err(error)) => self.store.apply_summary(Err(error)),
            (ResourceKey::MetricsSummary, Ok(_)) => {
                self.store.apply_summary(Err("summary lane returned a non-summary body".to_owned()));
            }
            (ResourceKey::MetricsDays, Ok(ResourceBody::Days(value))) => self.store.apply_days(Ok(value)),
            (ResourceKey::MetricsDays, Err(error)) => self.store.apply_days(Err(error)),
            (ResourceKey::MetricsDays, Ok(_)) => {
                self.store.apply_days(Err("days lane returned a non-days body".to_owned()));
            }
            (ResourceKey::MetricsTimeline(bloom), Ok(ResourceBody::Timeline(value))) => {
                self.store.apply_timeline(bloom, Ok(value));
            }
            (ResourceKey::MetricsTimeline(bloom), Err(error)) => self.store.apply_timeline(bloom, Err(error)),
            (ResourceKey::MetricsTimeline(bloom), Ok(_)) => {
                self.store.apply_timeline(bloom, Err("timeline lane returned a non-timeline body".to_owned()));
            }
            (ResourceKey::MetricsSeats, Ok(ResourceBody::Seats(value))) => self.store.apply_seats(Ok(value)),
            (ResourceKey::MetricsSeats, Err(error)) => self.store.apply_seats(Err(error)),
            (ResourceKey::MetricsSeats, Ok(_)) => {
                self.store.apply_seats(Err("seats lane returned a non-seats body".to_owned()));
            }
            (ResourceKey::MetricsDispatches, Ok(ResourceBody::Dispatches(value))) => {
                self.store.apply_dispatches(Ok(value));
            }
            (ResourceKey::MetricsDispatches, Err(error)) => self.store.apply_dispatches(Err(error)),
            (ResourceKey::MetricsDispatches, Ok(_)) => {
                self.store.apply_dispatches(Err("dispatches lane returned a non-dispatches body".to_owned()));
            }
            (ResourceKey::BloomDispatches(bloom), Ok(ResourceBody::BloomDispatches(value))) => {
                self.store.apply_bloom_dispatches(bloom, Ok(value));
            }
            (ResourceKey::BloomDispatches(bloom), Err(error)) => self.store.apply_bloom_dispatches(bloom, Err(error)),
            (ResourceKey::BloomDispatches(bloom), Ok(_)) => {
                self.store.apply_bloom_dispatches(
                    bloom,
                    Err("bloom-dispatches lane returned a non-dispatches body".to_owned()),
                );
            }
            (ResourceKey::Spend, Ok(ResourceBody::Spend(value))) => self.store.apply_spend(Ok(value)),
            (ResourceKey::Spend, Err(error)) => self.store.apply_spend(Err(error)),
            (ResourceKey::Spend, Ok(_)) => {
                self.store.apply_spend(Err("spend lane returned a non-spend body".to_owned()));
            }
            (ResourceKey::Commissions, Ok(ResourceBody::Commissions(value))) => self.store.apply_commissions(Ok(value)),
            (ResourceKey::Commissions, Ok(ResourceBody::CommissionsMissing)) => self.store.apply_commissions_missing(),
            (ResourceKey::Commissions, Err(error)) => self.store.apply_commissions(Err(error)),
            (ResourceKey::Commissions, Ok(_)) => {
                self.store.apply_commissions(Err("commissions lane returned a non-commissions body".to_owned()));
            }
            (ResourceKey::Commission(id), Ok(ResourceBody::Commission(value))) => {
                self.store.apply_commission(id, Ok(value));
            }
            (ResourceKey::Commission(id), Err(error)) => self.store.apply_commission(id, Err(error)),
            (ResourceKey::Commission(id), Ok(_)) => {
                self.store.apply_commission(id, Err("commission lane returned a non-commission body".to_owned()));
            }
        }
    }

    fn request_due(&mut self) {
        let keys = self.subscribed();
        for key in keys {
            if !self.store.due(&key) {
                continue;
            }
            self.send_request(&key);
        }
    }

    fn refresh(&mut self) {
        for key in self.subscribed() {
            if self.store.is_inflight(&key) {
                continue;
            }
            self.send_request(&key);
        }
    }

    fn send_request(&mut self, key: &ResourceKey) {
        let Some(fetch) = &self.fetch else {
            return;
        };
        self.store.mark_inflight(key);
        if let Err(error) = fetch.request(key.clone()) {
            self.store.apply_err(key, error);
        }
    }

    fn subscribed(&self) -> Vec<ResourceKey> {
        let mut keys = self.workspace.subscriptions();
        for screen in &self.stack {
            for key in screen.subscriptions() {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    fn delegate_key(&mut self, key: KeyEvent) -> Outcome {
        let outcome = match self.stack.last_mut() {
            Some(top) => top.handle_key(key, &self.store),
            None => self.workspace.handle_key(key, &self.store),
        };
        match outcome {
            Outcome::Refresh => {
                self.refresh();
                Outcome::Handled
            }
            Outcome::Push(nav) => {
                self.push_nav(nav);
                Outcome::Handled
            }
            other => other,
        }
    }

    fn handle_artifact(&mut self) -> Outcome {
        let Some(digest) = self.stack.last().and_then(Screen::openable_digest) else {
            return Outcome::Ignored;
        };
        self.push_nav(Nav::focus(Focus::artifact(digest)));
        Outcome::Handled
    }

    fn push_nav(&mut self, nav: Nav) {
        self.stack.push(Screen::from_nav(nav));
        if let Some(top) = self.stack.last_mut() {
            top.reseat(&self.store);
        }
    }

    fn reseat_top(&mut self) {
        self.workspace.reseat(&self.store);
        if let Some(top) = self.stack.last_mut() {
            top.reseat(&self.store);
        }
    }

    /// Path from the workspace through each pushed frame, painted in the footer.
    fn footer_trail(&self) -> String {
        if self.stack.is_empty() {
            String::new()
        } else {
            once("board".to_owned()).chain(self.stack.iter().map(Screen::label)).collect::<Vec<_>>().join(" › ")
        }
    }

    fn footer_hints(&self) -> Vec<KeyHint> {
        let mut hints = Vec::new();
        if let Some(screen) = self.stack.last() {
            if screen.openable_digest().is_some() {
                hints.push(ARTIFACT_HINT);
            }
            hints.extend_from_slice(screen.key_hints());
            return hints;
        }
        hints.extend(self.workspace.key_hints(&self.store));
        hints
    }
}

#[cfg(test)]
impl Shell {
    fn harness(view_cadence: Duration) -> (Self, FetchProbe) {
        let (fetch, probe) = FetchLanes::pair();
        (Self::assemble("127.0.0.1:8910".to_owned(), Store::new(view_cadence), Some(fetch)), probe)
    }

    pub(crate) fn showing(view: &ViewDocument, error: Option<&str>) -> Self {
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_view(Ok(view.clone()));
        if let Some(error) = error {
            store.apply_view(Err(error.to_owned()));
        }
        let mut shell = Self::assemble("127.0.0.1:8910".to_owned(), store, None);
        shell.reseat_top();
        shell
    }

    fn top_focus(&self) -> Option<Focus> {
        self.stack.last().and_then(Screen::focus)
    }

    fn apply_view(&mut self, view: ViewDocument) {
        self.store.apply_view(Ok(view));
        self.reseat_top();
    }

    fn apply_bloom_dispatches(&mut self, bloom: DigestHex, page: BloomDispatchesView) {
        self.store.apply_bloom_dispatches(bloom, Ok(page));
        self.reseat_top();
    }

    fn top_scroll(&self) -> usize {
        self.stack.last().map_or(0, Screen::scroll)
    }

    fn top_selected(&self) -> Option<String> {
        self.stack.last().and_then(Screen::selected_key)
    }

    fn stack_depth(&self) -> usize {
        self.stack.len()
    }

    fn top_digest(&self) -> Option<DigestHex> {
        self.stack.last().and_then(Screen::digest_under_cursor)
    }

    pub(crate) fn probe(nav: Nav) -> Self {
        let mut shell = Self::assemble("127.0.0.1:8910".to_owned(), Store::new(Duration::from_secs(1)), None);
        shell.push_nav(nav);
        shell
    }

    fn board(&self) -> &Board {
        self.workspace.board()
    }

    fn chrome_selected(&self) -> Option<&Focus> {
        self.workspace.chrome_selected()
    }

    fn focused_pane(&self) -> PaneId {
        self.workspace.focus()
    }
}

#[cfg(test)]
mod tests {
    use super::PaneId;
    use super::Shell;
    use super::chrome;
    use crate::dto::{
        BloomDispatchView, BloomDispatchesView, BloomStatus, BloomView, CompositionCursorView, CompositionFinding,
        CompositionView, DigestHex, ExecutorFaultView, HostFaultView, LandingBlock, MemberView, OperatorHoldView,
        PendingDecisionView, Present, ReviewParkView, SpendQuiesce, StageId, ViewDocument, WedgeCause,
    };
    use crate::fetch::{FetchReply, ResourceBody};
    use crate::http::Endpoint;
    use crate::keys::{INLINE_HINTS, Outcome, assert_footer_honest, footer_line};
    use crate::nav::Nav;
    use crate::palette::{Role, depth};
    use crate::screen::{Dashboard, RowId};
    use crate::store::{Cell, ResourceKey};
    use crate::warroom::Focus;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};

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
    fn board_prints_alert_tokens_as_text() {
        // The plausible bug: the band uses color alone, so a park/wedge is
        // invisible on a monochrome or inverted terminal.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(0xab),
                review_park: Some(ReviewParkView::default()),
                landing_blocked: Some(LandingBlock { rolls: 1, budget: 2 }),
                members: vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    wedge: Some(Present {}),
                    host_fault: Some(HostFaultView::default()),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let mut shell = Shell::showing(&view, None);
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let text = buffer_text(&terminal);
        let prefix = digest(0xab).prefix();
        assert!(
            text.lines()
                .any(|line| line.contains(&prefix) && line.contains("park") && line.contains("accept or defer")),
            "park row grammar missing from:\n{text}"
        );
        assert!(
            text.lines().any(|line| {
                line.contains("issue-1") && line.contains("wedge") && line.contains("widen the surface or eject")
            }),
            "wedge row grammar missing from:\n{text}"
        );
    }

    #[test]
    fn a_stale_board_keeps_the_last_rows_and_names_the_error() {
        // The plausible bug: unreachable coordinator blanks the table or
        // leaves the last sample looking current.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView {
                    workpiece: "issue-keep".to_owned(),
                    cursor: Some(CompositionCursorView {
                        stage: Some(StageId::Construct),
                        attempts: 1,
                        candidate: None,
                    }),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let mut shell = Shell::showing(&view, Some("connection refused"));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("STALE"), "{text}");
        assert!(text.contains("connection refused"), "{text}");
        assert!(text.contains("issue-keep"), "{text}");
    }

    #[test]
    fn a_resource_has_at_most_one_inflight_request() {
        // The plausible bug: every frame re-sends /view while the live lane
        // is still out, flooding the coordinator and losing the one-inflight
        // rule the Cell exists to enforce.
        let (mut shell, probe) = Shell::harness(Duration::from_secs(1));
        shell.pump();
        shell.pump();
        assert_eq!(probe.take_live().map(|request| request.key), Some(ResourceKey::View));
        assert!(probe.take_live().is_none());
        let mut bulk = Vec::new();
        while let Some(request) = probe.take_bulk() {
            bulk.push(request.key);
        }
        assert!(bulk.contains(&ResourceKey::MetricsSummary), "{bulk:?}");
        assert!(bulk.contains(&ResourceKey::MetricsDays), "{bulk:?}");
        assert!(bulk.contains(&ResourceKey::MetricsDispatches), "{bulk:?}");
        assert!(bulk.contains(&ResourceKey::Spend), "{bulk:?}");
        assert_eq!(bulk.len(), 4, "dashboard metrics must each have one inflight: {bulk:?}");
    }

    #[test]
    fn cadence_belongs_to_the_resource() {
        // The plausible bug: a reply clears inflight and the next pump
        // immediately re-issues /view, so cadence is "every frame".
        let (mut shell, probe) = Shell::harness(Duration::from_hours(1));
        shell.pump();
        assert!(probe.take_live().is_some());
        probe.reply(FetchReply { key: ResourceKey::View, outcome: Ok(ResourceBody::View(ViewDocument::default())) });
        shell.pump();
        shell.pump();
        assert!(probe.take_live().is_none());

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('r'))), Outcome::Handled);
        assert_eq!(probe.take_live().map(|request| request.key), Some(ResourceKey::View));
    }

    #[test]
    fn a_view_reply_reseats_the_board_cursor() {
        // The plausible bug: the store updates and the table paints but the
        // cursor stays empty, so the first row is never highlighted.
        let (mut shell, probe) = Shell::harness(Duration::from_secs(1));
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView {
                    workpiece: "wp-a".to_owned(),
                    cursor: Some(CompositionCursorView {
                        stage: Some(StageId::Construct),
                        attempts: 1,
                        candidate: None,
                    }),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        probe.reply(FetchReply { key: ResourceKey::View, outcome: Ok(ResourceBody::View(view)) });
        shell.pump();
        assert_eq!(shell.board().cursor().selected(), Some(&RowId::Bloom { id: digest(1) }));
    }

    #[test]
    fn the_artifact_key_is_not_offered_on_a_board_row() {
        // The plausible bug: a bloom id is a digest, so the footer paints `a`
        // and the key opens a 404 artifact frame on every board row.
        let view = ViewDocument {
            blooms: vec![BloomView { id: digest(1), ..BloomView::default() }],
            ..ViewDocument::default()
        };
        let mut shell = Shell::showing(&view, None);
        let mut terminal = Terminal::new(TestBackend::new(240, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(!text.contains("a artifact"), "artifact hint painted on a board row:\n{text}");
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('a'))), Outcome::Ignored);
        assert_eq!(shell.stack_depth(), 0);
    }

    #[test]
    fn an_open_artifact_does_not_stack_a_copy_of_itself() {
        // The plausible bug: the artifact screen reports its own digest as
        // under the cursor, so `a` pushes a second identical frame.
        let mut shell = Shell::probe(Nav::focus(Focus::artifact(digest(1))));
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('a'))), Outcome::Ignored);
        assert_eq!(shell.stack_depth(), 1);
    }

    #[test]
    fn a_stalled_coordinator_does_not_block_the_shell() {
        // The plausible bug: GET /view still runs on the event loop, so a
        // coordinator that never answers freezes j/k for the full live timeout.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a silent coordinator");
        let addr = listener.local_addr().expect("addr");
        thread::scope(|scope| {
            let mut shell =
                Shell::new(scope, Endpoint { host: addr.ip().to_string(), port: addr.port() }, Duration::from_secs(1));

            let start = Instant::now();
            shell.pump();
            for _ in 0..20 {
                assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('j'))), Outcome::Handled);
            }
            let elapsed = start.elapsed();
            assert!(
                elapsed < Duration::from_millis(200),
                "shell work took {elapsed:?}; a blocking /view would cost the 1s live timeout"
            );
            drop(listener);
        });
    }

    fn bloom_with(members: Vec<MemberView>, mutate: impl FnOnce(&mut BloomView)) -> ViewDocument {
        let mut bloom = BloomView { id: digest(0xab), members, ..BloomView::default() };
        mutate(&mut bloom);
        ViewDocument { blooms: vec![bloom], ..ViewDocument::default() }
    }

    fn draw(shell: &mut Shell) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        buffer_text(&terminal)
    }

    fn assert_interrupt_jumps(view: &ViewDocument, action: &str, focus: &Focus) {
        // The plausible bug: the source is an alert only, so the queue has no
        // row and Enter cannot jump to the subject that is stopped.
        let mut shell = Shell::showing(view, None);
        let text = draw(&mut shell);
        assert!(text.contains(action), "interrupt action {action} missing from:\n{text}");
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert_eq!(shell.top_focus().as_ref(), Some(focus), "Enter on {action} jumped to the wrong subject");
    }

    #[test]
    fn park_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| bloom.review_park = Some(ReviewParkView::default())),
            "accept or defer",
            &Focus::bloom(digest(0xab)),
        );
    }

    #[test]
    fn decision_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(
                vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    pending_decision: Some(PendingDecisionView::default()),
                    ..MemberView::default()
                }],
                |_| {},
            ),
            "answer",
            &Focus::member(digest(0xab), "issue-1"),
        );
    }

    #[test]
    fn findings_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| {
                bloom.composition = Some(CompositionView {
                    findings: vec![CompositionFinding::default()],
                    ..CompositionView::default()
                });
            }),
            "accept or defer",
            &Focus::composition(digest(0xab)),
        );
    }

    #[test]
    fn terminal_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| {
                bloom.executor_fault = Some(ExecutorFaultView { rolls: 3, budget: 3, terminal: true });
            }),
            "eject or re-approve",
            &Focus::bloom(digest(0xab)),
        );
    }

    #[test]
    fn wedge_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(
                vec![MemberView { workpiece: "issue-1".to_owned(), wedge: Some(Present {}), ..MemberView::default() }],
                |_| {},
            ),
            "widen the surface or eject",
            &Focus::member(digest(0xab), "issue-1"),
        );
    }

    #[test]
    fn landing_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| {
                bloom.landing_blocked = Some(LandingBlock { rolls: 2, budget: 2 });
            }),
            "eject or re-approve",
            &Focus::bloom(digest(0xab)),
        );
    }

    #[test]
    fn quiesce_interrupt_renders_and_enter_jumps() {
        let view = ViewDocument {
            spend_quiesce: Some(SpendQuiesce::Window {
                window: "bloomery/daily/2026-08-17".to_owned(),
                spent_micro_usd: 12,
                ceiling_micro_usd: 10,
            }),
            ..ViewDocument::default()
        };
        assert_interrupt_jumps(&view, "raise the ceiling or stand down", &Focus::Seal);
    }

    #[test]
    fn hold_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| {
                bloom.operator_hold =
                    Some(OperatorHoldView { reason: "wait".to_owned(), operator: "owner".to_owned() });
            }),
            "release",
            &Focus::bloom(digest(0xab)),
        );
    }

    #[test]
    fn a_resolved_member_leaves_the_board_for_quiet() {
        // The plausible bug: a finished member still occupies a live board
        // row, with machinery rolls under ELAPSED and a blocker id under COST,
        // while quiet says nothing about it.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                status: Some(BloomStatus::Sealed),
                members: vec![MemberView {
                    workpiece: "wp-done".to_owned(),
                    resolution: Some(Present {}),
                    machinery_rolls: 2,
                    machinery_budget: 3,
                    blocked_by: Some("wp-a".to_owned()),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        let mut shell = Shell::showing(&view, None);
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let board = left_column(&terminal);
        let quiet = right_column(&terminal);
        assert!(!board.contains("wp-done"), "resolved member stayed on the board:\n{board}");
        assert!(quiet.contains("resolved, awaiting land"), "quiet dropped the resolved count:\n{quiet}");
    }

    #[test]
    fn pane_geometry_does_not_read_pane_content() {
        // Tripwire: pane geometry not reading pane content — an empty needs-you
        // used to collapse its rows and slide the board up under the cursor.
        let empty_text = draw(&mut Shell::showing(&ViewDocument::default(), None));
        let parked_text = draw(&mut Shell::showing(&parked_blooms(10), None));
        assert_eq!(
            title_y(&empty_text, "board"),
            title_y(&parked_text, "board"),
            "empty:\n{empty_text}\nparked:\n{parked_text}"
        );
        assert!(empty_text.contains("needs you"), "empty workspace dropped needs you:\n{empty_text}");
        assert!(empty_text.contains("quiet"), "empty workspace dropped quiet:\n{empty_text}");
        assert!(!empty_text.contains("fleet"), "fleet box still occupies the workspace:\n{empty_text}");
    }

    #[test]
    fn a_pushed_frame_replaces_the_workspace() {
        // The plausible bug: a drill-in still paints workspace chrome, or
        // popping it fails to restore the three panes.
        let view = bloom_with(
            vec![MemberView { workpiece: "issue-1".to_owned(), wedge: Some(Present {}), ..MemberView::default() }],
            |bloom| bloom.review_park = Some(ReviewParkView::default()),
        );
        let mut shell = Shell::showing(&view, None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert!(shell.top_focus().is_some(), "Enter must push a frame over the workspace");
        let text = draw(&mut shell);
        assert!(!text.contains("needs you"), "workspace stayed on screen under the frame:\n{text}");
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Esc)), Outcome::Handled);
        let text = draw(&mut shell);
        assert!(text.contains("needs you"), "pop did not restore the workspace:\n{text}");
        assert!(text.contains("accept or defer"), "{text}");
        assert!(text.contains("widen the surface or eject"), "{text}");
    }

    #[test]
    fn the_header_survives_a_pushed_frame() {
        // The plausible bug: Shell::render paints only the pushed screen into
        // the body, so the endpoint and sample age leave the display.
        let view = bloom_with(
            vec![MemberView { workpiece: "issue-1".to_owned(), wedge: Some(Present {}), ..MemberView::default() }],
            |bloom| bloom.review_park = Some(ReviewParkView::default()),
        );
        let mut shell = Shell::showing(&view, None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        let text = draw(&mut shell);
        assert!(text.contains("127.0.0.1:8910"), "endpoint missing under a pushed frame:\n{text}");
        assert!(text.contains("sample"), "sample age missing under a pushed frame:\n{text}");
    }

    #[test]
    fn the_header_spans_the_full_width() {
        // Tripwire: the header being a half-width pane — any content past
        // column 50 is clipped when it is.
        let text = draw(&mut Shell::showing(&ViewDocument::default(), None));
        let line = text.lines().next().expect("header row");
        assert!(
            line.find("lanes").expect("lanes missing from header") >= 50,
            "metrics did not reach past the halfway point on:\n{line}"
        );
    }

    #[test]
    fn a_narrow_header_drops_the_sparklines_not_the_error() {
        // Tripwire: the drop order — if the row ever elides right-to-left by
        // truncation instead, the error goes first.
        let mut view = Cell::<ViewDocument>::default();
        view.apply_err("connection refused");
        let dashboard = Dashboard {
            spend_spark: "▁▂▃▄▅▆▇█▇▆▅▄▃▂".to_owned(),
            footer: "landed 0  cycle —  flight 0  lanes 0/0".to_owned(),
            ..Dashboard::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).expect("test backend");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    chrome::header("127.0.0.1:8910", &view, Some(&dashboard), frame.area().width),
                    frame.area(),
                );
            })
            .expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("STALE"), "{text}");
        assert!(text.contains("connection refused"), "{text}");
        assert!(!text.contains(&dashboard.spend_spark), "sparkline survived a narrow header:\n{text}");
    }

    #[test]
    fn a_member_enter_chain_reaches_the_transcript() {
        // The plausible bug: Enter on a member pushes Focus::Dispatch into a
        // titled detail frame, so Nav::transcript is never produced and the
        // viewer stays unreachable from the operator's seat.
        let bloom = digest(0xab);
        let mut shell = Shell::showing(
            &ViewDocument {
                blooms: vec![BloomView {
                    id: bloom,
                    members: vec![MemberView { workpiece: "wp-a".to_owned(), ..MemberView::default() }],
                    ..BloomView::default()
                }],
                ..ViewDocument::default()
            },
            None,
        );
        shell.push_nav(Nav::focus(Focus::member(bloom, "wp-a")));
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert_eq!(shell.top_focus(), Some(Focus::dispatch(bloom, "wp-a")));

        shell.apply_bloom_dispatches(
            bloom,
            BloomDispatchesView {
                dispatches: vec![BloomDispatchView {
                    nonce: "dispatch-1".to_owned(),
                    workpiece: "wp-a".to_owned(),
                    stage: StageId::Construct,
                    attempt: 1,
                    verdict: Some("pass".to_owned()),
                    cost: Some(1_000_000),
                    evidence_retained: true,
                }],
            },
        );
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert_eq!(shell.top_focus(), Some(Focus::transcript("dispatch-1")));
    }

    #[test]
    fn the_footer_trail_names_every_frame_on_the_stack() {
        // The plausible bug: a three-Enter transcript names only the nonce, so
        // the operator cannot tell which bloom or member the viewer belongs to.
        let bloom = digest(0xab);
        let mut shell = Shell::showing(
            &ViewDocument {
                blooms: vec![BloomView {
                    id: bloom,
                    members: vec![MemberView { workpiece: "issue-1".to_owned(), ..MemberView::default() }],
                    ..BloomView::default()
                }],
                ..ViewDocument::default()
            },
            None,
        );
        shell.push_nav(Nav::focus(Focus::bloom(bloom)));
        shell.push_nav(Nav::focus(Focus::member(bloom, "issue-1")));
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        shell.apply_bloom_dispatches(
            bloom,
            BloomDispatchesView {
                dispatches: vec![BloomDispatchView {
                    nonce: "dispatch-1".to_owned(),
                    workpiece: "issue-1".to_owned(),
                    stage: StageId::Construct,
                    attempt: 1,
                    verdict: Some("pass".to_owned()),
                    cost: Some(1_000_000),
                    evidence_retained: true,
                }],
            },
        );
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        let last = draw(&mut shell).lines().last().unwrap_or("").to_owned();
        assert!(last.contains("board › bloom "), "{last}");
        assert!(last.contains("› member issue-1"), "{last}");
        assert!(last.contains("› transcript"), "{last}");
        assert!(last.trim_end().ends_with("q quit"), "{last}");
    }

    #[test]
    fn at_rest_the_footer_carries_the_keys_alone() {
        // Tripwire: the workspace is the root; a `board` crumb at rest is
        // noise on every frame.
        let mut shell = Shell::showing(&ViewDocument::default(), None);
        let last = draw(&mut shell).lines().last().unwrap_or("").to_owned();
        assert_eq!(last.trim(), footer_line(INLINE_HINTS));
    }

    #[test]
    fn a_deep_trail_is_elided_from_the_left_and_keeps_the_keys() {
        // The plausible bug: a trail longer than the frame is clipped from
        // the right, so the deepest crumb and q quit vanish together.
        let mut shell = Shell::showing(&ViewDocument::default(), None);
        for _ in 0..12 {
            shell.push_nav(Nav::days());
        }
        shell.push_nav(Nav::transcript("deep-crumb"));
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let last = buffer_text(&terminal).lines().last().unwrap_or("").to_owned();
        assert!(last.starts_with('…'), "{last}");
        assert!(last.chars().count() <= 60, "{last}");
        assert!(last.contains("deep-crumb"), "{last}");
        assert!(last.trim_end().ends_with("q quit"), "{last}");
    }

    #[test]
    fn a_dropped_member_reseats_to_its_bloom() {
        // The plausible bug: a supersede that drops the member walks the
        // cursor onto an unrelated bloom's first row instead of the parent.
        let other = digest(0xcd);
        let start = ViewDocument {
            blooms: vec![
                BloomView {
                    id: digest(0xab),
                    members: vec![MemberView { workpiece: "issue-1".to_owned(), ..MemberView::default() }],
                    ..BloomView::default()
                },
                BloomView {
                    id: other,
                    members: vec![MemberView { workpiece: "issue-9".to_owned(), ..MemberView::default() }],
                    ..BloomView::default()
                },
            ],
            ..ViewDocument::default()
        };
        let mut shell = Shell::showing(&start, None);
        shell.push_nav(Nav::focus(Focus::member(digest(0xab), "issue-1")));
        assert_eq!(shell.top_focus(), Some(Focus::member(digest(0xab), "issue-1")));
        let depth = shell.stack_depth();

        shell.apply_view(ViewDocument {
            blooms: vec![
                BloomView {
                    id: digest(0xab),
                    status: Some(BloomStatus::Superseded),
                    superseded_by: Some(other),
                    members: Vec::new(),
                    ..BloomView::default()
                },
                BloomView {
                    id: other,
                    members: vec![MemberView { workpiece: "issue-9".to_owned(), ..MemberView::default() }],
                    ..BloomView::default()
                },
            ],
            ..ViewDocument::default()
        });
        assert_eq!(shell.stack_depth(), depth, "a refresh must not pop the frame");
        assert_eq!(shell.top_focus(), Some(Focus::bloom(digest(0xab))));
        assert_ne!(shell.top_focus(), Some(Focus::bloom(other)));
        assert_ne!(shell.top_focus(), Some(Focus::member(other, "issue-9")));
    }

    #[test]
    fn enter_on_a_digest_pushes_the_artifact_and_esc_restores_cursor() {
        // The plausible bug: opening an artifact replaces the detail frame,
        // so Esc lands on the board with a reset cursor and scroll.
        let subject = digest(0x66);
        let detail = digest(0x99);
        let view = bloom_with(Vec::new(), |bloom| {
            bloom.composition = Some(CompositionView {
                findings: vec![CompositionFinding { subject, detail, implicated: vec!["issue-1".to_owned()] }],
                ..CompositionView::default()
            });
        });
        let mut shell = Shell::showing(&view, None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert_eq!(shell.top_focus(), Some(Focus::composition(digest(0xab))));

        let mut hops = 0;
        while shell.top_digest() != Some(detail) {
            assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('j'))), Outcome::Handled);
            hops += 1;
            assert!(hops < 16, "never reached the finding digest");
        }
        let cursor = shell.top_selected();
        let scroll = shell.top_scroll();
        let depth = shell.stack_depth();

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert_eq!(shell.top_focus(), Some(Focus::artifact(detail)));
        assert_eq!(shell.stack_depth(), depth + 1);

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Esc)), Outcome::Handled);
        assert_eq!(shell.top_focus(), Some(Focus::composition(digest(0xab))));
        assert_eq!(shell.stack_depth(), depth);
        assert_eq!(shell.top_selected(), cursor);
        assert_eq!(shell.top_scroll(), scroll);
    }

    #[test]
    fn a_predating_coordinator_states_the_backlog_and_keeps_the_board_live() {
        // The plausible bug: a 404 on /commissions is treated as a store
        // failure that dims /view, so opening the backlog takes every other
        // pane down with it.
        let view = bloom_with(
            vec![MemberView {
                workpiece: "wp-keep".to_owned(),
                cursor: Some(CompositionCursorView { stage: Some(StageId::Construct), attempts: 1, candidate: None }),
                ..MemberView::default()
            }],
            |_| {},
        );
        let (mut shell, probe) = Shell::harness(Duration::from_secs(1));
        probe.reply(FetchReply { key: ResourceKey::View, outcome: Ok(ResourceBody::View(view)) });
        shell.pump();
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('b'))), Outcome::Handled);
        probe.reply(FetchReply { key: ResourceKey::Commissions, outcome: Ok(ResourceBody::CommissionsMissing) });
        shell.pump();

        let text = draw(&mut shell);
        assert!(text.contains("predates the commission store"), "{text}");
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Esc)), Outcome::Handled);
        let text = draw(&mut shell);
        assert!(text.contains("wp-keep"), "{text}");
        assert!(!text.contains("predates the commission store"), "{text}");

        while probe.take_live().is_some() {}
        while probe.take_bulk().is_some() {}
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('r'))), Outcome::Handled);
        assert_eq!(probe.take_live().map(|request| request.key), Some(ResourceKey::View));
    }

    fn parked_blooms(count: u8) -> ViewDocument {
        ViewDocument {
            blooms: (1..=count)
                .map(|n| BloomView {
                    id: digest(n),
                    review_park: Some(ReviewParkView::default()),
                    members: vec![MemberView {
                        workpiece: format!("wp-{n}"),
                        cursor: Some(CompositionCursorView {
                            stage: Some(StageId::Construct),
                            attempts: 1,
                            candidate: None,
                        }),
                        ..MemberView::default()
                    }],
                    ..BloomView::default()
                })
                .collect(),
            ..ViewDocument::default()
        }
    }

    #[test]
    fn holding_arrow_down_never_walks_chrome() {
        // The plausible bug: the band auto-selects on load, so holding Down
        // walks every interrupt and alert id before the table cursor moves.
        let mut shell = Shell::showing(&parked_blooms(10), None);
        assert_eq!(shell.chrome_selected(), None);
        let start = shell.board().cursor().selected().cloned();
        for _ in 0..5 {
            assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Down)), Outcome::Handled);
            assert_eq!(shell.chrome_selected(), None);
        }
        assert_ne!(shell.board().cursor().selected().cloned(), start);
    }

    #[test]
    fn arrow_up_from_the_first_row_does_not_enter_the_band() {
        // The plausible bug: Up from the first table row silently selects the
        // last chrome id, most of which have no visible representation.
        let mut shell = Shell::showing(&parked_blooms(2), None);
        let start = shell.board().cursor().selected().cloned();
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Up)), Outcome::Handled);
        assert_eq!(shell.chrome_selected(), None);
        assert_eq!(shell.board().cursor().selected().cloned(), start);
    }

    #[test]
    fn band_entry_and_exit_are_footer_advertised() {
        // The plausible bug: band entry is a silent wrap, and leaving it means
        // walking every remaining chrome id, so the footer never names either.
        let view = parked_blooms(2);
        let mut shell = Shell::showing(&view, None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        let outside = shell.footer_hints();
        assert!(
            outside.iter().any(|hint| hint.keys == "i" && hint.action == "queue"),
            "missing enter hint in {}",
            footer_line(&outside)
        );
        assert_footer_honest(&outside, |code| {
            let mut probe = Shell::showing(&view, None);
            assert_eq!(probe.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
            probe.handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        assert!(shell.chrome_selected().is_some());
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('j'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Esc)), Outcome::Handled);
        assert_eq!(shell.chrome_selected(), None);

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        let inside = shell.footer_hints();
        assert!(
            inside.iter().any(|hint| hint.keys == "Esc" && hint.action == "board"),
            "missing exit hint in {}",
            footer_line(&inside)
        );
        assert!(
            inside.iter().any(|hint| hint.keys == "Enter" && hint.action == "jump"),
            "missing jump hint in {}",
            footer_line(&inside)
        );
        assert!(footer_line(&inside).contains("x dismiss"), "missing dismiss hint in {}", footer_line(&inside));
        assert_footer_honest(&inside, |code| {
            let mut probe = Shell::showing(&view, None);
            assert_eq!(probe.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
            assert_eq!(probe.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
            probe.handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn a_dismissed_row_leaves_the_band_and_returns_when_its_facts_change() {
        // The plausible bug: a park the operator already judged occupies a
        // needs-you row forever, or a new stop on the same bloom stays hidden.
        let view = parked_blooms(1);
        let subject = digest(1).prefix();
        let mut shell = Shell::showing(&view, None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('x'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);

        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let text = right_column(&terminal);
        assert!(!text.contains(&subject), "dismissed subject still in the band:\n{text}");
        assert!(text.contains("·1 cleared"), "cleared count missing from:\n{text}");

        let mut wedged = parked_blooms(1);
        wedged.blooms[0].review_park = None;
        wedged.blooms[0].members[0].wedge = Some(Present {});
        wedged.blooms[0].members[0].wedge_cause = Some(WedgeCause::Work);
        shell.apply_view(wedged);
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let text = right_column(&terminal);
        assert!(text.contains("wp-1"), "new stop missing from the band:\n{text}");
    }

    #[test]
    fn a_dismissed_row_stays_walkable_while_the_band_has_focus() {
        // The plausible bug: hiding the row immediately makes a mis-keyed
        // dismissal unrecoverable without a restart.
        let view = parked_blooms(1);
        let subject = digest(1).prefix();
        let mut shell = Shell::showing(&view, None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('x'))), Outcome::Handled);

        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let text = right_column(&terminal);
        assert!(text.contains(&subject), "dismissed row missing while focused:\n{text}");
        let mods = right_column_modifiers(&terminal, &subject);
        assert!(mods.iter().any(|modifier| modifier.contains(Modifier::DIM)), "dismissed row was not dimmed: {mods:?}");
    }

    #[test]
    fn chrome_selection_stays_on_a_visible_interrupt() {
        // The plausible bug: j walks ids past the pane's inner rows, so the
        // highlight sits on a clipped row and the cursor looks gone.
        let mut shell = Shell::showing(&parked_blooms(10), None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        for _ in 0..9 {
            assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('j'))), Outcome::Handled);
        }
        assert!(shell.chrome_selected().is_some());
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let text = right_column(&terminal);
        let park_lines: Vec<&str> =
            text.lines().filter(|line| line.contains(" · ") && line.contains("accept or defer")).collect();
        let last = digest(10).prefix();
        let first = digest(1).prefix();
        assert!(
            park_lines.iter().any(|line| line.contains(&last)),
            "selected interrupt missing from band:\n{}",
            park_lines.join("\n")
        );
        assert!(
            park_lines.iter().all(|line| !line.contains(&first)),
            "scrolled-off interrupt still painted:\n{}",
            park_lines.join("\n")
        );
        assert!(text.lines().any(|line| line.contains('+') && line.contains("more")), "+N more missing from:\n{text}");
    }

    #[test]
    fn a_vanished_chrome_id_leaves_the_band() {
        // The plausible bug: a refresh that drops the selected id reseats onto
        // another chrome row, stealing focus the operator never asked for.
        let mut shell = Shell::showing(&parked_blooms(1), None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        assert!(shell.chrome_selected().is_some());
        let mut next = parked_blooms(1);
        next.blooms[0].id = digest(9);
        shell.apply_view(next);
        assert_eq!(shell.chrome_selected(), None);
    }

    #[test]
    fn tab_walks_the_focus_ring_and_routes_j() {
        // The plausible bug: Tab is ignored, or j always hits the board, so
        // the needs-you cursor cannot move without stealing the table selection.
        let mut shell = Shell::showing(&parked_blooms(10), None);
        assert_eq!(shell.focused_pane(), PaneId::Board);
        let board_start = shell.board().cursor().selected().cloned();

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.focused_pane(), PaneId::NeedsYou);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('j'))), Outcome::Handled);
        assert!(shell.chrome_selected().is_some());
        assert_eq!(shell.board().cursor().selected().cloned(), board_start);
        let chrome = shell.chrome_selected().cloned();

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.focused_pane(), PaneId::Quiet);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        assert_eq!(shell.focused_pane(), PaneId::Board);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('j'))), Outcome::Handled);
        assert_ne!(shell.board().cursor().selected().cloned(), board_start);
        assert_eq!(shell.chrome_selected().cloned(), chrome);
    }

    #[test]
    fn the_focused_pane_border_uses_the_focus_role() {
        // The plausible bug: every pane paints the unfocused frame color, so
        // Tab moves an invisible ring.
        let mut shell = Shell::showing(&ViewDocument::default(), None);
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        assert_eq!(title_role(buffer, "board"), Role::Focus);
        assert_eq!(title_role(buffer, "needs you"), Role::Frames);
        assert_eq!(title_role(buffer, "quiet"), Role::Frames);

        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Tab)), Outcome::Handled);
        terminal.draw(|frame| shell.render(frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        assert_eq!(title_role(buffer, "board"), Role::Frames);
        assert_eq!(title_role(buffer, "needs you"), Role::Focus);
        assert_eq!(title_role(buffer, "quiet"), Role::Frames);
    }

    #[test]
    fn journal_filter_esc_leaves_edit_not_the_frame() {
        // The plausible bug: Esc is taken by the shell before the journal, so
        // a filter edit cannot be cancelled without popping the frame.
        let mut shell = Shell::showing(&ViewDocument::default(), None);
        shell.push_nav(Nav::journal(None));
        assert_eq!(shell.stack_depth(), 1);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('f'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Esc)), Outcome::Handled);
        assert_eq!(shell.stack_depth(), 1);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Esc)), Outcome::Handled);
        assert_eq!(shell.stack_depth(), 0);
    }

    #[test]
    fn journal_filter_types_i_while_the_queue_is_loud() {
        // The plausible bug: i is taken by the interrupt band before the
        // journal, so the letter cannot be typed into a filter.
        let mut shell = Shell::showing(&parked_blooms(3), None);
        shell.push_nav(Nav::journal(None));
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('f'))), Outcome::Handled);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Char('i'))), Outcome::Handled);
        let text = draw(&mut shell);
        assert!(text.contains("filter  i"), "typed i missing from filter:\n{text}");
    }

    fn title_y(text: &str, title: &str) -> usize {
        text.lines().position(|line| line.contains(title)).unwrap_or_else(|| panic!("missing {title} in:\n{text}"))
    }

    fn left_column(terminal: &Terminal<TestBackend>) -> String {
        column_text(terminal, 0, terminal.backend().buffer().area().width / 2)
    }

    fn right_column(terminal: &Terminal<TestBackend>) -> String {
        let width = terminal.backend().buffer().area().width;
        column_text(terminal, width / 2, width)
    }

    fn right_column_modifiers(terminal: &Terminal<TestBackend>, needle: &str) -> Vec<Modifier> {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        let start = area.width / 2;
        for y in area.y..area.y + area.height {
            let mut line = String::new();
            let mut mods = Vec::new();
            for x in start..area.width {
                let cell = &buffer[(x, y)];
                line.push_str(cell.symbol());
                mods.push(cell.modifier);
            }
            if let Some(at) = line.find(needle) {
                return mods.into_iter().skip(at).take(needle.chars().count()).collect();
            }
        }
        Vec::new()
    }

    fn column_text(terminal: &Terminal<TestBackend>, start: u16, end: u16) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        let mut out = String::new();
        for y in area.y..area.y + area.height {
            for x in start..end {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn title_role(buffer: &Buffer, title: &str) -> Role {
        let area = buffer.area();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if !title_starts_at(buffer, x, y, title) {
                    continue;
                }
                let border_x = x.saturating_sub(1);
                return role_of(buffer[(border_x, y)].fg);
            }
        }
        panic!("title {title:?} not found");
    }

    fn title_starts_at(buffer: &Buffer, x: u16, y: u16, title: &str) -> bool {
        let area = buffer.area();
        let mut cursor = x;
        for ch in title.chars() {
            if cursor >= area.x + area.width {
                return false;
            }
            let symbol = buffer[(cursor, y)].symbol();
            if !symbol.starts_with(ch) {
                return false;
            }
            cursor = cursor.saturating_add(1);
        }
        true
    }

    fn role_of(color: Color) -> Role {
        Role::ALL
            .into_iter()
            .find(|role| role.color(depth()) == color)
            .unwrap_or_else(|| panic!("unrecognized {color:?}"))
    }
}
