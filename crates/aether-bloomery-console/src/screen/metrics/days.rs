//! Day-level series: spend, landed, cycle-time.

use std::fmt::Write;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{BarChart, Block, Borders, Paragraph};

use crate::dto::{MetricDay, SpendQuiesce};
use crate::keys::{KeyHint, Outcome};
use crate::palette::{self, Role};
use crate::store::{ResourceKey, Store};

use super::bucket::format_duration;
use super::cost::format_micro_usd;

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// Fleet day series over spend, landed, and cycle time.
#[derive(Clone, Debug, Default)]
pub struct Days;

impl Days {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::MetricsDays, ResourceKey::Spend, ResourceKey::View]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    pub fn handle_key(&mut self, key: KeyEvent, _store: &Store) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::Handled,
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let days = store.days().value.as_ref().map_or(&[][..], Vec::as_slice);
        let ceiling = store.view().value.as_ref().and_then(|view| match &view.spend_quiesce {
            Some(SpendQuiesce::Window { ceiling_micro_usd, .. } | SpendQuiesce::Bloom { ceiling_micro_usd, .. }) => {
                Some(*ceiling_micro_usd)
            }
            _ => None,
        });
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(30), Constraint::Percentage(30)])
            .split(area);
        render_spend(frame, chunks[0], days, ceiling);
        render_landed(frame, chunks[1], days);
        render_cycle(frame, chunks[2], days);
    }
}

fn render_spend(frame: &mut Frame<'_>, area: Rect, days: &[MetricDay], ceiling: Option<u64>) {
    let window: Vec<&MetricDay> = days.iter().rev().take(14).rev().collect();
    let labels: Vec<String> = window.iter().map(|day| bar_label(day)).collect();
    let data: Vec<(&str, u64)> = labels
        .iter()
        .zip(window.iter())
        .map(|(label, day)| (label.as_str(), day.spend_micro_usd.max(day.dispatches)))
        .collect();
    let title =
        ceiling.map_or_else(|| "SPEND".to_owned(), |ceiling| format!("SPEND  ceiling {}", format_micro_usd(ceiling)));
    let chart = BarChart::default()
        .block(
            Block::default().borders(Borders::ALL).border_style(palette::border()).style(palette::body()).title(title),
        )
        .data(&data)
        .bar_width(3)
        .bar_gap(1)
        .bar_style(palette::paint(Role::Working));
    frame.render_widget(chart, area);
}

fn render_landed(frame: &mut Frame<'_>, area: Rect, days: &[MetricDay]) {
    let window: Vec<&MetricDay> = days.iter().rev().take(14).rev().collect();
    let labels: Vec<String> = window.iter().map(|day| bar_label(day)).collect();
    let data: Vec<(&str, u64)> =
        labels.iter().zip(window.iter()).map(|(label, day)| (label.as_str(), day.landed)).collect();
    let chart = BarChart::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(palette::border())
                .style(palette::body())
                .title("LANDED"),
        )
        .data(&data)
        .bar_width(3)
        .bar_gap(1)
        .bar_style(palette::paint(Role::Settled));
    frame.render_widget(chart, area);
}

fn render_cycle(frame: &mut Frame<'_>, area: Rect, days: &[MetricDay]) {
    let mut line = String::from("cycle  ");
    for day in days.iter().rev().take(14).rev() {
        match day.cycle_time_millis {
            Some(millis) => {
                let _ = write!(line, "{} ", format_duration(millis));
            }
            None => line.push_str("· "),
        }
        if day.quiesced {
            line.push_str("Q ");
        }
    }
    if days.is_empty() {
        line.push_str("(no days)");
    }
    frame.render_widget(
        Paragraph::new(line)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(palette::border())
                    .style(palette::body())
                    .title("CYCLE TIME"),
            )
            .style(palette::body().add_modifier(Modifier::DIM)),
        area,
    );
}

fn bar_label(day: &MetricDay) -> String {
    let base = day.label.rsplit('/').next().unwrap_or(&day.label);
    if day.quiesced {
        format!("{base}Q")
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::Days;
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::store::Store;
    use crossterm::event::KeyEvent;
    use std::time::Duration;

    #[test]
    fn days_footer_keys_are_handled() {
        let store = Store::new(Duration::from_secs(1));
        let mut view = Days::new();
        assert_footer_honest(Days::key_hints(), |code| {
            view.handle_key(KeyEvent::from(code), &store) != Outcome::Ignored
        });
    }
}
