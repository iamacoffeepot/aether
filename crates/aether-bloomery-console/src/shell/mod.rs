//! Shell: endpoint, store, fetch lanes, chrome, filter, and the screen stack.
//!
//! Screens receive the store read-only and cannot fetch or mutate it.

pub mod chrome;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::fetch::{FetchLanes, FetchReply, ResourceBody};
use crate::http::Endpoint;
use crate::keys::Outcome;
use crate::screen::Screen;
use crate::store::{ResourceKey, Store};

/// The running console: one stack, one store, two fetch lanes.
pub struct Shell {
    endpoint_label: String,
    store: Store,
    fetch: Option<FetchLanes>,
    stack: Vec<Screen>,
    filter: String,
}

impl Shell {
    #[must_use]
    pub fn new(endpoint: Endpoint, view_cadence: Duration) -> Self {
        let endpoint_label = endpoint.label();
        Self::assemble(endpoint_label, Store::new(view_cadence), Some(FetchLanes::spawn(endpoint)))
    }

    fn assemble(endpoint_label: String, store: Store, fetch: Option<FetchLanes>) -> Self {
        Self { endpoint_label, store, fetch, stack: vec![Screen::board()], filter: String::new() }
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
        let Some(top) = self.stack.last_mut() else {
            return Outcome::Ignored;
        };
        match top.handle_key(key, &self.store) {
            Outcome::Refresh => {
                self.refresh();
                Outcome::Handled
            }
            other => other,
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let filter_height = u16::from(!self.filter.is_empty());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(filter_height),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(frame.area());

        frame.render_widget(chrome::header(&self.endpoint_label, self.store.view()), chunks[0]);
        if filter_height > 0 {
            frame.render_widget(chrome::filter_line(&self.filter), chunks[1]);
        }
        if let Some(screen) = self.stack.last_mut() {
            screen.render(frame, chunks[2], &self.store);
        }
        let hints = self.stack.last().map(Screen::key_hints).unwrap_or(&[]);
        frame.render_widget(chrome::footer(hints), chunks[3]);
    }

    fn drain_replies(&mut self) {
        let Some(fetch) = &self.fetch else {
            return;
        };
        let replies: Vec<FetchReply> = fetch.drain().collect();
        let mut view_changed = false;
        for reply in replies {
            if reply.key == ResourceKey::View {
                view_changed = true;
                match reply.outcome {
                    Ok(ResourceBody::View(view)) => self.store.apply_view(Ok(view)),
                    Err(error) => self.store.apply_view(Err(error)),
                }
            }
        }
        if view_changed && let Some(top) = self.stack.last_mut() {
            top.reseat(&self.store);
        }
    }

    fn request_due(&mut self) {
        let keys = self.subscribed();
        for key in keys {
            if !self.store.due(key) {
                continue;
            }
            self.send_request(key);
        }
    }

    fn refresh(&mut self) {
        for key in self.subscribed() {
            if self.store.is_inflight(key) {
                continue;
            }
            self.send_request(key);
        }
    }

    fn send_request(&mut self, key: ResourceKey) {
        let Some(fetch) = &self.fetch else {
            return;
        };
        self.store.mark_inflight(key);
        if let Err(error) = fetch.request(key) {
            self.store.apply_err(key, error);
        }
    }

    fn subscribed(&self) -> Vec<ResourceKey> {
        let mut keys = Vec::new();
        for screen in &self.stack {
            for key in screen.subscriptions() {
                if !keys.contains(key) {
                    keys.push(*key);
                }
            }
        }
        keys
    }
}

#[cfg(test)]
impl Shell {
    fn harness(view_cadence: Duration) -> (Self, crate::fetch::FetchProbe) {
        let (fetch, probe) = FetchLanes::pair();
        (Self::assemble("127.0.0.1:8910".to_owned(), Store::new(view_cadence), Some(fetch)), probe)
    }

    fn showing(view: &crate::dto::ViewDocument, error: Option<&str>) -> Self {
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_view(Ok(view.clone()));
        if let Some(error) = error {
            store.apply_view(Err(error.to_owned()));
        }
        let mut shell = Self::assemble("127.0.0.1:8910".to_owned(), store, None);
        if let Some(top) = shell.stack.last_mut() {
            top.reseat(&shell.store);
        }
        shell
    }

    fn board(&self) -> &crate::screen::Board {
        match self.stack.first() {
            Some(Screen::Board(board)) => board,
            _ => panic!("the stack starts with the board"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Shell;
    use crate::dto::{BloomView, DigestHex, LandingBlock, MemberView, Present, ReviewParkView, ViewDocument};
    use crate::fetch::{FetchReply, ResourceBody};
    use crate::keys::Outcome;
    use crate::screen::RowId;
    use crate::store::ResourceKey;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::net::TcpListener;
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
                    host_fault: Some(Present {}),
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
        assert!(probe.take_bulk().is_none());
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
        let mut shell = Shell::new(
            crate::http::Endpoint { host: addr.ip().to_string(), port: addr.port() },
            Duration::from_secs(1),
        );

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
    }
}
