//! Shell: endpoint, store, fetch lanes, chrome, filter, and the screen stack.
//!
//! Screens receive the store read-only and cannot fetch or mutate it.
//! Alert band, interrupt queue, and status line are chrome — they render
//! at every stack depth.

pub mod chrome;

use std::thread;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::cursor::Cursor;
use crate::dto::ViewDocument;
use crate::fetch::{FetchLanes, FetchReply, ResourceBody};
use crate::http::Endpoint;
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::screen::{Screen, compose};
use crate::store::{ResourceKey, Store};
use crate::warroom::{self, Alert, ChromeId, Focus, Interrupt};

#[cfg(test)]
use crate::dto::DigestHex;
#[cfg(test)]
use crate::fetch::FetchProbe;
#[cfg(test)]
use crate::screen::Board;

const ENTER_HINT: KeyHint = KeyHint { keys: "Enter", action: "jump" };
const ARTIFACT_HINT: KeyHint = KeyHint { keys: "a", action: "artifact" };

/// The running console: one stack, one store, two fetch lanes.
pub struct Shell {
    endpoint_label: String,
    store: Store,
    fetch: Option<FetchLanes>,
    stack: Vec<Screen>,
    filter: String,
    chrome: Cursor<ChromeId>,
}

impl Shell {
    #[must_use]
    pub fn new<'scope>(scope: &'scope thread::Scope<'scope, '_>, endpoint: Endpoint, view_cadence: Duration) -> Self {
        let endpoint_label = endpoint.label();
        Self::assemble(endpoint_label, Store::new(view_cadence), Some(FetchLanes::spawn(scope, endpoint)))
    }

    fn assemble(endpoint_label: String, store: Store, fetch: Option<FetchLanes>) -> Self {
        Self {
            endpoint_label,
            store,
            fetch,
            stack: vec![Screen::board()],
            filter: String::new(),
            chrome: Cursor::new(),
        }
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
        if key.code == KeyCode::Esc && self.stack.len() > 1 {
            self.stack.pop();
            return Outcome::Handled;
        }

        let ids = self.chrome_ids();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if self.chrome.selected().is_some() => {
                self.chrome_next(&ids);
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => self.handle_up(&ids, key),
            KeyCode::Enter => self.handle_enter(key),
            KeyCode::Char('a') => self.handle_artifact(),
            _ => self.delegate_key(key),
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let (alerts, interrupts) = self.bands();
        let dashboard = compose(&self.store);
        let filter_height = u16::from(!self.filter.is_empty());
        let status_height = self.status_height();
        let today_height = u16::from(!dashboard.today.is_empty());
        let alert_height = u16::from(!alerts.is_empty());
        let interrupt_height = u16::try_from(interrupts.len().min(8)).unwrap_or(0);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(status_height),
                Constraint::Length(today_height),
                Constraint::Length(filter_height),
                Constraint::Length(alert_height),
                Constraint::Length(interrupt_height),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(frame.area());

        frame.render_widget(chrome::header(&self.endpoint_label, self.store.view(), Some(&dashboard)), chunks[0]);
        if let Some(view) = self.store.view().value.as_ref() {
            Self::render_status(frame, chunks[1], view, status_height);
        }
        if today_height > 0 {
            frame.render_widget(chrome::today(&dashboard), chunks[2]);
        }
        if filter_height > 0 {
            frame.render_widget(chrome::filter_line(&self.filter), chunks[3]);
        }
        if alert_height > 0 {
            frame.render_widget(chrome::alert_band(&alerts, self.selected_alert_index(&alerts)), chunks[4]);
        }
        if interrupt_height > 0 {
            frame.render_widget(
                chrome::interrupt_band(&interrupts, self.selected_interrupt_index(&interrupts)),
                chunks[5],
            );
        }
        if let Some(screen) = self.stack.last_mut() {
            screen.render(frame, chunks[6], &self.store);
        }
        frame.render_widget(chrome::footer(&self.footer_hints(), Some(&dashboard.footer)), chunks[7]);
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
        let mut keys = Vec::new();
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
        let Some(top) = self.stack.last_mut() else {
            return Outcome::Ignored;
        };
        match top.handle_key(key, &self.store) {
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

    fn handle_up(&mut self, ids: &[ChromeId], key: KeyEvent) -> Outcome {
        if self.chrome.selected().is_some() {
            self.chrome.select_prev(ids, Clone::clone);
            return Outcome::Handled;
        }
        if !ids.is_empty() && self.stack.last().is_some_and(|screen| screen.selected_is_first(&self.store)) {
            self.chrome.select(ids.last().cloned());
            return Outcome::Handled;
        }
        self.delegate_key(key)
    }

    fn handle_enter(&mut self, key: KeyEvent) -> Outcome {
        if let Some(id) = self.chrome.selected() {
            self.push_nav(Nav::focus(id.focus().clone()));
            return Outcome::Handled;
        }
        self.delegate_key(key)
    }

    fn handle_artifact(&mut self) -> Outcome {
        let Some(digest) = self.stack.last().and_then(Screen::digest_under_cursor) else {
            return self.delegate_key(KeyEvent::from(KeyCode::Char('a')));
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

    fn chrome_next(&mut self, ids: &[ChromeId]) {
        match self.chrome.selected().and_then(|id| ids.iter().position(|item| item == id)) {
            Some(index) if index + 1 < ids.len() => self.chrome.select(Some(ids[index + 1].clone())),
            Some(_) => self.chrome.select(None),
            None => self.chrome.select(ids.first().cloned()),
        }
    }

    fn chrome_ids(&self) -> Vec<ChromeId> {
        let (alerts, interrupts) = self.bands();
        warroom::chrome_ids(&interrupts, &alerts)
    }

    fn bands(&self) -> (Vec<Alert>, Vec<Interrupt>) {
        self.store
            .view()
            .value
            .as_ref()
            .map_or_else(|| (Vec::new(), Vec::new()), |view| (warroom::alerts(view), warroom::interrupts(view)))
    }

    fn reseat_top(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            top.reseat(&self.store);
        }
        self.reseat_chrome();
    }

    fn reseat_chrome(&mut self) {
        let ids = self.chrome_ids();
        self.chrome.reseat(&ids, Clone::clone, |_, ids| ids.first().cloned());
    }

    fn status_height(&self) -> u16 {
        match self.store.view().value.as_ref() {
            Some(view) if view.spend_quiesce.is_some() => 2,
            Some(_) => 1,
            None => 0,
        }
    }

    fn render_status(frame: &mut Frame<'_>, area: Rect, view: &ViewDocument, height: u16) {
        if height == 0 {
            return;
        }
        if height == 1 {
            frame.render_widget(chrome::status(view), area);
            return;
        }
        let lines = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        frame.render_widget(chrome::status(view), lines[0]);
        if let Some(quiesce) = &view.spend_quiesce {
            frame.render_widget(chrome::seal(quiesce), lines[1]);
        }
    }

    fn selected_interrupt_index(&self, interrupts: &[Interrupt]) -> Option<usize> {
        match self.chrome.selected() {
            Some(ChromeId::Interrupt { kind, focus }) => {
                interrupts.iter().position(|entry| entry.kind == *kind && entry.focus == *focus)
            }
            _ => None,
        }
    }

    fn selected_alert_index(&self, alerts: &[Alert]) -> Option<usize> {
        match self.chrome.selected() {
            Some(ChromeId::Alert { kind, focus }) => {
                alerts.iter().position(|alert| alert.kind == *kind && alert.focus == *focus)
            }
            _ => None,
        }
    }

    fn footer_hints(&self) -> Vec<KeyHint> {
        let mut hints = Vec::new();
        if self.chrome.selected().is_some() {
            hints.push(ENTER_HINT);
        }
        if self.stack.last().and_then(Screen::digest_under_cursor).is_some() {
            hints.push(ARTIFACT_HINT);
        }
        if let Some(screen) = self.stack.last() {
            hints.extend_from_slice(screen.key_hints());
        }
        hints
    }
}

#[cfg(test)]
impl Shell {
    fn harness(view_cadence: Duration) -> (Self, FetchProbe) {
        let (fetch, probe) = FetchLanes::pair();
        (Self::assemble("127.0.0.1:8910".to_owned(), Store::new(view_cadence), Some(fetch)), probe)
    }

    fn showing(view: &ViewDocument, error: Option<&str>) -> Self {
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

    fn board(&self) -> &Board {
        match self.stack.first() {
            Some(Screen::Board(board)) => board,
            _ => panic!("the stack starts with the board"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Shell;
    use crate::dto::{
        BloomStatus, BloomView, CompositionFinding, CompositionView, DigestHex, ExecutorFaultView, HostFaultView,
        LandingBlock, MemberView, OperatorHoldView, PendingDecisionView, Present, ReviewParkView, SpendQuiesce,
        ViewDocument,
    };
    use crate::fetch::{FetchReply, ResourceBody};
    use crate::http::Endpoint;
    use crate::keys::Outcome;
    use crate::nav::Nav;
    use crate::screen::RowId;
    use crate::store::ResourceKey;
    use crate::warroom::Focus;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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
        assert!(text.contains("PARK"), "{text}");
        assert!(text.contains("land: blocked 1/2"), "{text}");
        assert!(text.contains("WEDGED"), "{text}");
        assert!(text.contains("hostfault"), "{text}");
        assert!(text.contains("issue-1"), "{text}");
    }

    #[test]
    fn a_stale_board_keeps_the_last_rows_and_names_the_error() {
        // The plausible bug: unreachable coordinator blanks the table or
        // leaves the last sample looking current.
        let view = ViewDocument {
            blooms: vec![BloomView {
                id: digest(1),
                members: vec![MemberView { workpiece: "issue-keep".to_owned(), ..MemberView::default() }],
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
                members: vec![MemberView { workpiece: "wp-a".to_owned(), ..MemberView::default() }],
                ..BloomView::default()
            }],
            ..ViewDocument::default()
        };
        probe.reply(FetchReply { key: ResourceKey::View, outcome: Ok(ResourceBody::View(view)) });
        shell.pump();
        assert_eq!(shell.board().cursor().selected(), Some(&RowId::Bloom { id: digest(1) }));
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

    fn assert_interrupt_jumps(view: &ViewDocument, label: &str, focus: &Focus) {
        // The plausible bug: the source is an alert only, so the queue has no
        // row and Enter cannot jump to the subject that is stopped.
        let mut shell = Shell::showing(view, None);
        let text = draw(&mut shell);
        assert!(text.contains(label), "interrupt {label} missing from:\n{text}");
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert_eq!(shell.top_focus().as_ref(), Some(focus), "Enter on {label} jumped to the wrong subject");
    }

    #[test]
    fn park_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| bloom.review_park = Some(ReviewParkView::default())),
            "park",
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
            "decision",
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
            "findings",
            &Focus::composition(digest(0xab)),
        );
    }

    #[test]
    fn terminal_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| {
                bloom.executor_fault = Some(ExecutorFaultView { rolls: 3, budget: 3, terminal: true });
            }),
            "terminal",
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
            "wedge",
            &Focus::member(digest(0xab), "issue-1"),
        );
    }

    #[test]
    fn landing_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| {
                bloom.landing_blocked = Some(LandingBlock { rolls: 2, budget: 2 });
            }),
            "landing",
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
        assert_interrupt_jumps(&view, "quiesce", &Focus::Seal);
    }

    #[test]
    fn hold_interrupt_renders_and_enter_jumps() {
        assert_interrupt_jumps(
            &bloom_with(Vec::new(), |bloom| {
                bloom.operator_hold =
                    Some(OperatorHoldView { reason: "wait".to_owned(), operator: "owner".to_owned() });
            }),
            "hold",
            &Focus::bloom(digest(0xab)),
        );
    }

    #[test]
    fn an_empty_interrupt_queue_contributes_no_rows() {
        // The plausible bug: the band reserves a row for an empty queue, so a
        // quiet document still paints a blank interrupt strip.
        let view = bloom_with(
            vec![MemberView {
                workpiece: "issue-1".to_owned(),
                host_fault: Some(HostFaultView::default()),
                ..MemberView::default()
            }],
            |bloom| bloom.landing_blocked = Some(LandingBlock { rolls: 1, budget: 3 }),
        );
        let mut shell = Shell::showing(&view, None);
        let text = draw(&mut shell);
        assert!(text.contains("hostfault"), "{text}");
        assert!(text.contains("land: blocked 1/3"), "{text}");
        for label in ["park", "decision", "findings", "terminal", "wedge", "landing", "quiesce", "hold"] {
            assert!(!text.contains(&format!("{label}  ")), "empty queue painted {label} in:\n{text}");
        }

        let lines: Vec<&str> = text.lines().collect();
        let alert_at = lines.iter().position(|line| line.contains("hostfault")).expect("alert line");
        let table_at = lines.iter().position(|line| line.contains("BLOOM / MEMBER")).expect("table header");
        assert_eq!(table_at, alert_at + 1, "a collapsed queue leaves no row between alerts and the table:\n{text}");
    }

    #[test]
    fn alerts_render_while_a_pushed_frame_is_on_top() {
        // The plausible bug: the alert band still lives on the board, so a
        // drill-in blanks PARK / WEDGED even though chrome should keep them.
        let view = bloom_with(
            vec![MemberView { workpiece: "issue-1".to_owned(), wedge: Some(Present {}), ..MemberView::default() }],
            |bloom| bloom.review_park = Some(ReviewParkView::default()),
        );
        let mut shell = Shell::showing(&view, None);
        assert_eq!(shell.handle_key(KeyEvent::from(KeyCode::Enter)), Outcome::Handled);
        assert!(shell.top_focus().is_some(), "Enter must push a frame over the board");
        let text = draw(&mut shell);
        assert!(text.contains("PARK"), "{text}");
        assert!(text.contains("WEDGED"), "{text}");
        assert!(text.contains("bloom"), "{text}");
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
        let view = bloom_with(vec![MemberView { workpiece: "wp-keep".to_owned(), ..MemberView::default() }], |_| {});
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
}
