//! Root workspace: four bordered panes and one Tab-cycled focus ring.

mod pane;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::Paragraph;

use super::chrome;
use crate::cursor::Cursor;
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::screen::{Board, Dashboard, compose};
use crate::store::{ResourceKey, Store};
use crate::warroom::{self, Alert, ChromeId, Interrupt};

pub use pane::PaneId;
use pane::pane_block;

const FLEET_ROWS: u16 = 3;

const TAB_HINT: KeyHint = KeyHint { keys: "Tab", action: "pane" };
const ENTER_HINT: KeyHint = KeyHint { keys: "Enter", action: "jump" };
const ENTER_BAND_HINT: KeyHint = KeyHint { keys: "i", action: "queue" };
const LEAVE_BAND_HINT: KeyHint = KeyHint { keys: "Esc", action: "board" };
const WALK_HINT: KeyHint = KeyHint { keys: "j/k", action: "select" };
const REFRESH_HINT: KeyHint = KeyHint { keys: "r", action: "refresh" };
const QUIT_HINT: KeyHint = KeyHint { keys: "q", action: "quit" };

/// The rest root: fleet over board on the left, needs-you over quiet on the right.
pub struct Workspace {
    board: Board,
    chrome: Cursor<ChromeId>,
    focus: PaneId,
}

impl Workspace {
    #[must_use]
    pub fn new() -> Self {
        Self { board: Board::new(), chrome: Cursor::new(), focus: PaneId::Board }
    }

    pub fn cycle(&mut self) {
        self.focus = self.focus.cycle();
    }

    #[cfg(test)]
    #[must_use]
    pub fn focus(&self) -> PaneId {
        self.focus
    }

    #[must_use]
    pub fn board(&self) -> &Board {
        &self.board
    }

    #[cfg(test)]
    #[must_use]
    pub fn chrome_selected(&self) -> Option<&ChromeId> {
        self.chrome.selected()
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        self.board.subscriptions()
    }

    #[must_use]
    pub fn key_hints(&self, store: &Store) -> Vec<KeyHint> {
        let mut hints = vec![TAB_HINT];
        match self.focus {
            PaneId::Board => hints.extend_from_slice(self.board.key_hints()),
            PaneId::NeedsYou => hints.extend(self.needs_you_hints(store)),
            PaneId::Quiet => {
                hints.push(REFRESH_HINT);
                hints.push(QUIT_HINT);
            }
        }
        hints
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let outcome = match self.focus {
            PaneId::Board => self.board.handle_key(key, store),
            PaneId::NeedsYou => self.handle_needs_you(key, store),
            PaneId::Quiet => Outcome::Ignored,
        };
        match outcome {
            Outcome::Ignored => match key.code {
                KeyCode::Char('r') => Outcome::Refresh,
                KeyCode::Char('q') => Outcome::Quit,
                _ => Outcome::Ignored,
            },
            other => other,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        self.board.reseat(store);
        let ids = self.chrome_ids(store);
        if let Some(id) = self.chrome.selected()
            && !ids.iter().any(|item| item == id)
        {
            self.chrome.select(None);
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store, endpoint_label: &str) {
        let dashboard = compose(store);
        let areas = split_panes(area);
        self.render_fleet(frame, areas.fleet, store, endpoint_label, &dashboard);
        self.render_board(frame, areas.board, store);
        self.render_needs_you(frame, areas.needs_you, store);
        self.render_quiet(frame, areas.quiet, store, &dashboard);
    }

    fn handle_needs_you(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let ids = self.chrome_ids(store);
        match key.code {
            KeyCode::Char('i') if !ids.is_empty() && self.chrome.selected().is_none() => {
                self.chrome.select(ids.first().cloned());
                Outcome::Handled
            }
            KeyCode::Char('j') if !ids.is_empty() => {
                self.chrome.select_next(&ids, Clone::clone);
                Outcome::Handled
            }
            KeyCode::Char('k') if !ids.is_empty() => {
                self.chrome.select_prev(&ids, Clone::clone);
                Outcome::Handled
            }
            KeyCode::Enter => {
                let Some(focus) = self.chrome.selected().map(|id| id.focus().clone()) else {
                    return Outcome::Ignored;
                };
                self.chrome.select(None);
                Outcome::Push(Nav::focus(focus))
            }
            KeyCode::Esc if self.chrome.selected().is_some() => {
                self.chrome.select(None);
                Outcome::Handled
            }
            _ => Outcome::Ignored,
        }
    }

    fn needs_you_hints(&self, store: &Store) -> Vec<KeyHint> {
        let mut hints = Vec::new();
        let ids = self.chrome_ids(store);
        if self.chrome.selected().is_some() {
            hints.push(ENTER_HINT);
            hints.push(LEAVE_BAND_HINT);
            hints.push(WALK_HINT);
        } else if !ids.is_empty() {
            hints.push(ENTER_BAND_HINT);
            hints.push(WALK_HINT);
        }
        hints.push(REFRESH_HINT);
        hints.push(QUIT_HINT);
        hints
    }

    fn chrome_ids(&self, store: &Store) -> Vec<ChromeId> {
        let (alerts, interrupts) = bands(store);
        warroom::chrome_ids(&interrupts, &alerts)
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

    fn render_fleet(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        store: &Store,
        endpoint_label: &str,
        dashboard: &Dashboard,
    ) {
        let block = pane_block("fleet", false);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(chrome::header(endpoint_label, store.view(), Some(dashboard)), inner);
    }

    fn render_board(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let block = pane_block(PaneId::Board.title(), self.focus == PaneId::Board);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.board.render(frame, inner, store);
    }

    fn render_needs_you(&self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let block = pane_block(PaneId::NeedsYou.title(), self.focus == PaneId::NeedsYou);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let (alerts, interrupts) = bands(store);
        if alerts.is_empty() && interrupts.is_empty() {
            frame.render_widget(Paragraph::new("empty").style(palette::body()), inner);
            return;
        }
        let alert_height = if alerts.is_empty() {
            0
        } else {
            2
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(alert_height), Constraint::Min(1)])
            .split(inner);
        if alert_height > 0 {
            frame.render_widget(chrome::alert_band(&alerts, self.selected_alert_index(&alerts)), chunks[0]);
        }
        let rows = usize::from(chunks[1].height);
        let selected = self.selected_interrupt_index(&interrupts);
        let (window, highlight) = chrome::interrupt_window(&interrupts, selected, rows);
        frame.render_widget(chrome::interrupt_band(window, highlight), chunks[1]);
    }

    fn render_quiet(&self, frame: &mut Frame<'_>, area: Rect, store: &Store, dashboard: &Dashboard) {
        let block = pane_block(PaneId::Quiet.title(), self.focus == PaneId::Quiet);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(view) = store.view().value.as_ref() else {
            return;
        };
        let seal = u16::from(view.spend_quiesce.is_some());
        let today = u16::from(!dashboard.today.is_empty());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(seal), Constraint::Length(today)])
            .split(inner);
        frame.render_widget(chrome::status(view), chunks[0]);
        if let Some(quiesce) = &view.spend_quiesce {
            frame.render_widget(chrome::seal(quiesce), chunks[1]);
        }
        if today > 0 {
            frame.render_widget(chrome::today(dashboard), chunks[2]);
        }
    }
}

struct PaneAreas {
    fleet: Rect,
    board: Rect,
    needs_you: Rect,
    quiet: Rect,
}

fn split_panes(area: Rect) -> PaneAreas {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(FLEET_ROWS), Constraint::Min(3)])
        .split(columns[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(columns[1]);
    PaneAreas { fleet: left[0], board: left[1], needs_you: right[0], quiet: right[1] }
}

fn bands(store: &Store) -> (Vec<Alert>, Vec<Interrupt>) {
    store
        .view()
        .value
        .as_ref()
        .map_or_else(|| (Vec::new(), Vec::new()), |view| (warroom::alerts(view), warroom::interrupts(view)))
}
