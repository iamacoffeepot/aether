//! Axis-switchable cost table with in-row bars.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::keys::{KeyHint, Outcome};
use crate::palette;
use crate::store::{ResourceKey, Store};

use super::cost::{CostAxis, CostGroup, groups_from_members, groups_from_seats, groups_from_spend, mean_of};

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "x", action: "axis" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Cost breakdown over bloom / member / stage / seat.
#[derive(Clone, Debug)]
pub struct Breakdown {
    axis: CostAxis,
    cursor: Cursor<String>,
    scroll: usize,
}

impl Default for Breakdown {
    fn default() -> Self {
        Self { axis: CostAxis::Seat, cursor: Cursor::new(), scroll: 0 }
    }
}

impl Breakdown {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::MetricsSeats, ResourceKey::MetricsDispatches, ResourceKey::Spend]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    #[must_use]
    pub fn selected_is_first(&self, store: &Store) -> bool {
        let rows = self.groups(store);
        matches!(self.cursor.selected_index(&rows, |row| row.label.clone()), Some(0) | None)
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let rows = self.groups(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, |row| row.label.clone());
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, |row| row.label.clone());
                Outcome::Handled
            }
            KeyCode::Char('x') => {
                self.axis = self.axis.next();
                self.cursor = Cursor::new();
                Outcome::Handled
            }
            KeyCode::Esc => Outcome::Handled,
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = self.groups(store);
        self.cursor.reseat(&rows, |row| row.label.clone(), |_, rows| rows.first().map(|row| row.label.clone()));
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = self.groups(store);
        if self.cursor.selected().is_none() {
            self.reseat(store);
        }
        let max = rows.iter().map(|row| row.cost_micro_usd).max().unwrap_or(0);
        let mean = mean_of(&rows).map_or_else(|| "—".to_owned(), super::cost::format_micro_usd);
        let header = Row::new([
            format!("COST  {}  mean {mean}", self.axis.label()),
            "COST".to_owned(),
            "MEAN".to_owned(),
            "N".to_owned(),
            "UNPR".to_owned(),
            "BAR".to_owned(),
        ])
        .style(palette::body().add_modifier(Modifier::BOLD));
        let table_rows = rows.iter().map(|row| {
            Row::new([
                Cell::from(row.label.clone()),
                Cell::from(row.cost_label()),
                Cell::from(row.mean_label()),
                Cell::from(row.priced_samples.to_string()),
                Cell::from(row.unpriced.to_string()),
                Cell::from(row.bar(12, max)),
            ])
        });
        let table = Table::new(
            table_rows,
            [
                Constraint::Min(16),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(12),
            ],
        )
        .style(palette::body())
        .header(header)
        .row_highlight_style(palette::cursor())
        .highlight_symbol("> ");
        let mut state = TableState::default()
            .with_selected(self.cursor.selected_index(&rows, |row| row.label.clone()))
            .with_offset(self.scroll);
        frame.render_stateful_widget(table, area, &mut state);
        self.scroll = state.offset();
    }

    fn groups(&self, store: &Store) -> Vec<CostGroup> {
        let seats = store.seats().value.as_ref().map_or(&[][..], Vec::as_slice);
        let spend = store.spend().value.as_ref();
        let dispatches = store.dispatches().value.as_ref().map_or(&[][..], Vec::as_slice);
        match self.axis {
            CostAxis::Seat | CostAxis::Stage => groups_from_seats(seats, self.axis),
            CostAxis::Bloom => spend.map(|window| groups_from_spend(window, CostAxis::Bloom)).unwrap_or_default(),
            CostAxis::Member => groups_from_members(dispatches),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Breakdown;
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::store::Store;
    use crossterm::event::KeyEvent;
    use std::time::Duration;

    #[test]
    fn breakdown_footer_keys_are_handled() {
        let store = Store::new(Duration::from_secs(1));
        let mut view = Breakdown::new();
        assert_footer_honest(Breakdown::key_hints(), |code| {
            view.handle_key(KeyEvent::from(code), &store) != Outcome::Ignored
        });
    }
}
