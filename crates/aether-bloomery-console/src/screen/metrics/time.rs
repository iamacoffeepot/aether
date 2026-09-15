//! Per-member stage and substage time breakdown, with a bloom-level rollup.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::DigestHex;
use crate::keys::{KeyHint, Outcome};
use crate::palette;
use crate::store::{ResourceKey, Store};

use super::bucket::format_duration;
use super::life::{self, MemberLife, duration_bar};

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

const BAR_WIDTH: usize = 12;

/// One member's wall-clock breakdown over the bloom timeline.
pub struct Time {
    bloom: DigestHex,
    workpiece: String,
    cursor: Cursor<String>,
    scroll: usize,
}

impl Time {
    #[must_use]
    pub fn new(bloom: DigestHex, workpiece: impl Into<String>) -> Self {
        Self { bloom, workpiece: workpiece.into(), cursor: Cursor::new(), scroll: 0 }
    }

    #[must_use]
    pub fn bloom(&self) -> DigestHex {
        self.bloom
    }

    #[must_use]
    pub fn workpiece(&self) -> &str {
        &self.workpiece
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::MetricsTimeline(self.bloom)]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    /// The breakdown has no Enter; the caret follows that.
    #[must_use]
    pub fn enter_pushes() -> bool {
        false
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let rows = self.life(store).rows;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, |row| row.key.clone());
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, |row| row.key.clone());
                Outcome::Handled
            }
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = self.life(store).rows;
        self.cursor.reseat(&rows, |row| row.key.clone(), |_, rows| rows.first().map(|row| row.key.clone()));
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let life = self.life(store);
        if self.cursor.selected().is_none() {
            self.reseat(store);
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(5), Constraint::Min(4)])
            .split(area);
        render_header(frame, chunks[0], &life);
        self.render_member(frame, chunks[1], &life);
        render_rollup(frame, chunks[2], &life);
    }

    fn life(&self, store: &Store) -> MemberLife {
        let (spans, truncated) = store
            .timeline(self.bloom)
            .and_then(|cell| cell.value.as_ref())
            .map_or((&[][..], false), |doc| (doc.spans.as_slice(), doc.truncated));
        life::compose(spans, &self.workpiece, truncated)
    }

    fn render_member(&mut self, frame: &mut Frame<'_>, area: Rect, life: &MemberLife) {
        let max = life.rows.iter().map(|row| row.duration_millis).max().unwrap_or(0);
        let header = Row::new(["STAGE".to_owned(), "DUR".to_owned(), "SHARE".to_owned(), "BAR".to_owned()])
            .style(palette::body().add_modifier(Modifier::BOLD));
        let table_rows = life.rows.iter().map(|row| {
            Row::new([
                Cell::from(indented(&row.label, row.depth)),
                Cell::from(format_duration(row.duration_millis)),
                Cell::from(format!("{}%", row.share)),
                Cell::from(duration_bar(row.duration_millis, max, BAR_WIDTH)),
            ])
        });
        let table = Table::new(
            table_rows,
            [Constraint::Min(18), Constraint::Length(6), Constraint::Length(6), Constraint::Length(12)],
        )
        .style(palette::body())
        .header(header)
        .row_highlight_style(palette::cursor())
        .highlight_symbol(super::super::caret(Self::enter_pushes()));
        let mut state = TableState::default()
            .with_selected(self.cursor.selected_index(&life.rows, |row| row.key.clone()))
            .with_offset(self.scroll);
        frame.render_stateful_widget(table, area, &mut state);
        self.scroll = state.offset();
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, life: &MemberLife) {
    let total = if life.total_millis == 0 {
        "—".to_owned()
    } else {
        format_duration(life.total_millis)
    };
    let mut title = format!("TIME  {}  {total}", life.workpiece);
    if life.truncated {
        title.push_str("  truncated");
    }
    let text = if life.bar.is_empty() {
        title
    } else {
        format!("{title}\n{}", life.bar)
    };
    frame.render_widget(Paragraph::new(text).style(palette::body()), area);
}

fn render_rollup(frame: &mut Frame<'_>, area: Rect, life: &MemberLife) {
    let rollup = if life.truncated {
        "ROLLUP (partial)"
    } else {
        "ROLLUP"
    };
    let header =
        Row::new([rollup.to_owned(), "SUM".to_owned(), "MIN".to_owned(), "MAX".to_owned(), "HOLDER".to_owned()])
            .style(palette::body().add_modifier(Modifier::BOLD));
    let table_rows = life.rollup.iter().map(|row| {
        Row::new([
            Cell::from(row.stage.label().to_owned()),
            Cell::from(format_duration(row.sum_millis)),
            Cell::from(format_duration(row.min_millis)),
            Cell::from(format_duration(row.max_millis)),
            Cell::from(row.holder.clone()),
        ])
    });
    let table = Table::new(
        table_rows,
        [Constraint::Min(16), Constraint::Length(6), Constraint::Length(6), Constraint::Length(6), Constraint::Min(8)],
    )
    .style(palette::body())
    .header(header);
    frame.render_widget(table, area);
}

fn indented(label: &str, depth: u8) -> String {
    format!("{}{label}", "  ".repeat(usize::from(depth)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::Time;
    use crate::dto::{DigestHex, MetricsTimeline, StageId, TimelineSpan};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::screen::metrics::life::{self, LifeStage};
    use crate::shell::Shell;
    use crate::store::Store;
    use aether_bloomery::{SPAN_OUTCOME_INTEGRATED, SPAN_OUTCOME_RETIRED};
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    fn bloom() -> DigestHex {
        DigestHex::from_bytes([0xab; 32])
    }

    fn run(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn span(workpiece: &str, stage: StageId, start: u64, end: u64) -> TimelineSpan {
        TimelineSpan {
            workpiece: workpiece.to_owned(),
            stage,
            sequence: start,
            started_unix_millis: Some(start),
            ended_unix_millis: Some(end),
            ..TimelineSpan::default()
        }
    }

    fn verify_run(workpiece: &str, start: u64, end: u64, run_id: DigestHex, outcome: &str) -> TimelineSpan {
        TimelineSpan {
            run: Some(run_id),
            outcome: Some(outcome.to_owned()),
            ..span(workpiece, StageId::Verify, start, end)
        }
    }

    fn fixture() -> Store {
        fixture_truncated(false)
    }

    fn fixture_truncated(truncated: bool) -> Store {
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_timeline(
            bloom(),
            Ok(MetricsTimeline {
                bloom: bloom(),
                spans: vec![
                    span("wp-a", StageId::Construct, 0, 8_000),
                    verify_run("wp-a", 8_000, 16_000, run(1), SPAN_OUTCOME_RETIRED),
                    verify_run("wp-a", 16_000, 28_000, run(2), SPAN_OUTCOME_INTEGRATED),
                    TimelineSpan {
                        substage: Some("verify.fmt".to_owned()),
                        run: Some(run(2)),
                        sequence: 16_000,
                        ..span("wp-a", StageId::Verify, 16_000, 17_000)
                    },
                    TimelineSpan {
                        substage: Some("verify.test".to_owned()),
                        run: Some(run(2)),
                        sequence: 16_000,
                        ..span("wp-a", StageId::Verify, 17_000, 28_000)
                    },
                    span("wp-a", StageId::Review, 28_000, 36_000),
                    span("wp-b", StageId::Construct, 0, 8_000),
                    verify_run("wp-b", 8_000, 40_000, run(3), SPAN_OUTCOME_INTEGRATED),
                ],
                truncated,
            }),
        );
        store
    }

    fn painted(store: &Store) -> String {
        let mut time = Time::new(bloom(), "wp-a");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("test backend");
        terminal.draw(|frame| time.render(frame, frame.area(), store)).expect("draw");
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        (0..area.height).flat_map(|y| (0..area.width).map(move |x| buffer[(x, y)].symbol().to_owned())).collect()
    }

    #[test]
    fn time_footer_keys_are_handled() {
        let nav = Nav::time(bloom(), "wp-a");
        assert_footer_honest(Time::key_hints(), |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn two_verify_runs_paint_retirement_and_the_rollup_names_the_long_pole() {
        // The plausible bug: two runs collapse into one verify row so a
        // retirement is invisible, and the rollup reports this member as the
        // max because substages were counted twice.
        let store = fixture();
        let spans = store.timeline(bloom()).unwrap().value.as_ref().unwrap();
        let life = life::compose(&spans.spans, "wp-a", spans.truncated);
        assert_eq!(
            life.rows
                .iter()
                .filter(|row| row.depth == 1 && (row.label.contains("retired") || row.label.contains("integrated")))
                .count(),
            2,
            "each physical run is its own row: {:?}",
            life.rows
        );
        assert!(
            life.rows.iter().any(|row| row.depth == 1 && row.label.contains("retired by head move")),
            "retirement must be labelled: {:?}",
            life.rows
        );
        let verify = life.rows.iter().find(|row| row.depth == 0 && row.label == "verify").expect("verify stage");
        assert_eq!(verify.duration_millis, 20_000, "substages must not inflate the parent: {:?}", life.rows);
        let rollup = life.rollup.iter().find(|row| row.stage == LifeStage::Verify).expect("verify rollup");
        assert_eq!(rollup.holder, "wp-b", "the long pole is the other member: {:?}", life.rollup);
        assert_eq!(rollup.max_millis, 32_000);

        let painted = painted(&store);
        assert!(painted.contains("retired"), "the painted frame must carry the retirement: {painted}");
        assert!(painted.contains("wp-b"), "the rollup must name the max-verify member: {painted}");
    }

    #[test]
    fn a_truncated_timeline_marks_the_header_and_the_rollup_as_partial() {
        // The plausible bug: the ledger cuts spans at its cap while the Time
        // screen sums whatever came back, so the header total and the rollup
        // read as complete while silently undercounting.
        let whole = painted(&fixture());
        assert!(!whole.contains("truncated"), "a whole timeline carries no cut marker: {whole}");
        assert!(!whole.contains("partial"), "a whole rollup is not partial: {whole}");

        let painted = painted(&fixture_truncated(true));
        assert!(painted.contains("truncated"), "the header must mark the cut: {painted}");
        assert!(painted.contains("partial"), "the rollup must read as a partial sum: {painted}");
    }
}
