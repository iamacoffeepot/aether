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
use super::sparkline::dated;

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

const BAR_WIDTH: u16 = 3;
const BAR_GAP: u16 = 1;
const WINDOW_DAYS: usize = 14;

/// How many bars fit in `inner_width`. Callers take this many days from the
/// end of the series so the newest days survive a narrow pane.
fn visible_span(inner_width: u16, bar_width: u16, bar_gap: u16) -> usize {
    let stride = bar_width.saturating_add(bar_gap).max(1);
    usize::from(inner_width.saturating_add(bar_gap) / stride).min(WINDOW_DAYS)
}

/// The last `span` days the coordinator actually served. Reconstructed
/// placeholder days carry no date and are dropped before the window is taken,
/// so a narrow pane spends its bars on real days rather than a synthetic floor.
fn dated_window(days: &[MetricDay], span: usize) -> Vec<&MetricDay> {
    let dated = dated(days);
    let skip = dated.len().saturating_sub(span);
    dated.into_iter().skip(skip).collect()
}

fn spend_bar(day: &MetricDay) -> u64 {
    day.spend_micro_usd
}

fn chart_block(title: impl Into<String>) -> Block<'static> {
    Block::default().borders(Borders::ALL).border_style(palette::border()).style(palette::body()).title(title.into())
}

fn render_spend(frame: &mut Frame<'_>, area: Rect, days: &[MetricDay], ceiling: Option<u64>) {
    let window = dated_window(days, visible_span(area.width.saturating_sub(2), BAR_WIDTH, BAR_GAP));
    let title =
        ceiling.map_or_else(|| "SPEND".to_owned(), |ceiling| format!("SPEND  ceiling {}", format_micro_usd(ceiling)));
    if window.is_empty() || window.iter().all(|day| spend_bar(day) == 0) {
        frame.render_widget(
            Paragraph::new("no priced spend in this window")
                .block(chart_block(title))
                .style(palette::body().add_modifier(Modifier::DIM)),
            area,
        );
        return;
    }
    let labels: Vec<String> = window.iter().map(|day| bar_label(day)).collect();
    let data: Vec<(&str, u64)> =
        labels.iter().zip(window.iter()).map(|(label, day)| (label.as_str(), spend_bar(day))).collect();
    let chart = BarChart::default()
        .block(chart_block(title))
        .data(&data)
        .bar_width(BAR_WIDTH)
        .bar_gap(BAR_GAP)
        .bar_style(palette::paint(Role::Working));
    frame.render_widget(chart, area);
}

fn render_landed(frame: &mut Frame<'_>, area: Rect, days: &[MetricDay]) {
    let window = dated_window(days, visible_span(area.width.saturating_sub(2), BAR_WIDTH, BAR_GAP));
    if window.is_empty() || window.iter().all(|day| day.landed == 0) {
        frame.render_widget(
            Paragraph::new("no landings recorded in this window")
                .block(chart_block("LANDED".to_owned()))
                .style(palette::body().add_modifier(Modifier::DIM)),
            area,
        );
        return;
    }
    let labels: Vec<String> = window.iter().map(|day| bar_label(day)).collect();
    let data: Vec<(&str, u64)> =
        labels.iter().zip(window.iter()).map(|(label, day)| (label.as_str(), day.landed)).collect();
    let chart = BarChart::default()
        .block(chart_block("LANDED".to_owned()))
        .data(&data)
        .bar_width(BAR_WIDTH)
        .bar_gap(BAR_GAP)
        .bar_style(palette::paint(Role::Settled));
    frame.render_widget(chart, area);
}

fn render_cycle(frame: &mut Frame<'_>, area: Rect, days: &[MetricDay]) {
    let window = dated_window(days, visible_span(area.width.saturating_sub(2), BAR_WIDTH, BAR_GAP));
    let line = if window.is_empty() || window.iter().all(|day| day.cycle_time_millis.is_none()) {
        "no cycle time recorded in this window".to_owned()
    } else {
        let mut line = String::from("cycle  ");
        for day in window {
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
        line
    };
    frame.render_widget(
        Paragraph::new(line)
            .block(chart_block("CYCLE TIME".to_owned()))
            .style(palette::body().add_modifier(Modifier::DIM)),
        area,
    );
}

fn bar_label(day: &MetricDay) -> String {
    let after_slash = day.label.rsplit('/').next().unwrap_or(&day.label);
    let after_dash = after_slash.rsplit('-').next().unwrap_or(after_slash);
    let base = if after_dash.is_empty() || after_dash.len() > 3 {
        after_slash
    } else {
        after_dash
    };
    if day.quiesced {
        format!("{base}Q")
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{Days, bar_label, dated_window, spend_bar, visible_span};
    use crate::dto::MetricDay;
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::Store;
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

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

    fn paint_days(days: Vec<MetricDay>) -> String {
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_days(Ok(days));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test backend");
        terminal.draw(|frame| Days::new().render(frame, frame.area(), &store)).expect("draw");
        buffer_text(&terminal)
    }

    #[test]
    fn days_footer_keys_are_handled() {
        assert_footer_honest(Days::key_hints(), |code| {
            Shell::probe(Nav::days()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn the_spend_chart_never_substitutes_a_dispatch_count_for_dollars() {
        // The plausible bug: a bar chart titled SPEND rendering a count of
        // attempts as a dollar figure.
        let days = [
            MetricDay {
                label: "bloomery/daily/2026-08-19".into(),
                dispatches: 41,
                spend_micro_usd: 0,
                ..MetricDay::default()
            },
            MetricDay {
                label: "reconstructed".into(),
                reconstructed: true,
                dispatches: 9,
                spend_micro_usd: 100,
                ..MetricDay::default()
            },
        ];
        let window = dated_window(&days, 14);
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].label, "bloomery/daily/2026-08-19");
        assert_eq!(spend_bar(window[0]), 0);
    }

    #[test]
    fn a_bar_label_fits_the_bar_width() {
        // Tripwire: a label wider than BAR_WIDTH is truncated by ratatui to a
        // meaningless prefix.
        let day = MetricDay { label: "bloomery/daily/2026-08-20".into(), ..MetricDay::default() };
        assert_eq!(bar_label(&day), "20");
        let quiesced = MetricDay { quiesced: true, ..day };
        assert_eq!(bar_label(&quiesced), "20Q");
    }

    #[test]
    fn visible_span_never_overflows_the_pane() {
        // Tripwire: fourteen bars at width 3 with gap 1 need 56 columns; a
        // narrower pane used to drop overflow bars with no marker.
        assert_eq!(visible_span(58, 3, 1), 14);
        assert_eq!(visible_span(20, 3, 1), 5);
        assert_eq!(visible_span(0, 3, 1), 0);
        assert_eq!(visible_span(200, 3, 1), 14);
    }

    #[test]
    fn the_spend_chart_does_not_plot_the_dispatch_count() {
        // Tripwire: spend_micro_usd.max(dispatches) plotted dispatch counts
        // under a dollar-ceiling title whenever spend was unpriced.
        let text = paint_days(vec![MetricDay {
            label: "bloomery/daily/2026-08-20".into(),
            spend_micro_usd: 0,
            dispatches: 900,
            ..MetricDay::default()
        }]);
        assert!(text.contains("no priced spend in this window"), "{text}");
        assert!(!text.contains('█'), "{text}");
    }

    #[test]
    fn an_empty_cycle_series_says_so() {
        // Tripwire: a day with no cycle_time_millis used to paint `·` and
        // look the same as a recorded zero.
        let text = paint_days(vec![MetricDay {
            label: "bloomery/daily/2026-08-20".into(),
            cycle_time_millis: None,
            ..MetricDay::default()
        }]);
        assert!(text.contains("no cycle time recorded"), "{text}");
        assert!(!text.contains("· ·"), "{text}");
    }
}
