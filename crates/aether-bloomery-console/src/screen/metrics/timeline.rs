//! Per-bloom lane timeline: one row per member, last column a span line.

use std::time::{SystemTime, UNIX_EPOCH};

use aether_bloomery::WorkpieceId;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::cursor::Cursor;
use crate::dto::{BloomStatus, BloomView, DigestHex, MemberView, MetricsTimeline, TimelineSpan};
use crate::keys::{KeyHint, Outcome};
use crate::nav::Nav;
use crate::palette;
use crate::screen::board::member_status_state;
use crate::store::{ResourceKey, Store};
use crate::warroom::Focus;

use super::bucket::{
    axis_range, bucket_span, format_duration, paint_member_line, reconstructed_range, reconstructed_start,
    span_duration_millis,
};
use super::glyph::{CellKind, Silence, operator_action};

const HINTS: &[KeyHint] = &[
    KeyHint { keys: "j/k", action: "select" },
    KeyHint { keys: "Enter", action: "open" },
    KeyHint { keys: "b", action: "time" },
    KeyHint { keys: "Esc", action: "back" },
    KeyHint { keys: "r", action: "refresh" },
    KeyHint { keys: "q", action: "quit" },
];

/// One bloom's member-stage timeline.
pub struct Timeline {
    bloom: DigestHex,
    cursor: Cursor<String>,
    scroll: usize,
}

impl Timeline {
    #[must_use]
    pub fn new(bloom: DigestHex) -> Self {
        Self { bloom, cursor: Cursor::new(), scroll: 0 }
    }

    #[must_use]
    pub fn bloom(&self) -> DigestHex {
        self.bloom
    }

    #[must_use]
    pub fn subscriptions(&self) -> Vec<ResourceKey> {
        vec![ResourceKey::View, ResourceKey::MetricsTimeline(self.bloom)]
    }

    #[must_use]
    pub fn key_hints() -> &'static [KeyHint] {
        HINTS
    }

    #[must_use]
    pub fn selected_focus(&self) -> Option<Focus> {
        let workpiece = self.cursor.selected()?;
        Some(if workpiece == WorkpieceId::COMPOSITION {
            Focus::composition(self.bloom)
        } else {
            Focus::member(self.bloom, workpiece.clone())
        })
    }

    #[must_use]
    pub fn enter_pushes(&self) -> bool {
        self.selected_focus().is_some()
    }

    pub fn handle_key(&mut self, key: KeyEvent, store: &Store) -> Outcome {
        let rows = self.rows(store);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor.select_next(&rows, |row| row.workpiece.clone());
                Outcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor.select_prev(&rows, |row| row.workpiece.clone());
                Outcome::Handled
            }
            KeyCode::Enter => self.selected_focus().map_or(Outcome::Handled, |focus| Outcome::Push(Nav::focus(focus))),
            KeyCode::Char('b') => self
                .cursor
                .selected()
                .cloned()
                .map_or(Outcome::Handled, |workpiece| Outcome::Push(Nav::time(self.bloom, workpiece))),
            KeyCode::Char('r') => Outcome::Refresh,
            KeyCode::Char('q') => Outcome::Quit,
            _ => Outcome::Ignored,
        }
    }

    pub fn reseat(&mut self, store: &Store) {
        let rows = self.rows(store);
        self.cursor.reseat(&rows, |row| row.workpiece.clone(), |_, rows| rows.first().map(|row| row.workpiece.clone()));
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect, store: &Store) {
        let rows = self.rows(store);
        if self.cursor.selected().is_none() {
            self.reseat(store);
        }
        let reconstructed = store
            .timeline(self.bloom)
            .and_then(|cell| cell.value.as_ref())
            .is_some_and(|doc| doc.spans.iter().any(|span| span.reconstructed || span.started_unix_millis.is_none()));
        let header = Row::new([
            lane_title(&self.bloom.prefix(), reconstructed),
            "STAGE".to_owned(),
            "DUR".to_owned(),
            "SPANS".to_owned(),
        ])
        .style(palette::body().add_modifier(if reconstructed {
            Modifier::DIM | Modifier::BOLD
        } else {
            Modifier::BOLD
        }));
        let table_rows = rows.iter().map(|row| {
            Row::new([
                Cell::from(workpiece_label(&row.workpiece)),
                Cell::from(row.stage.clone()),
                Cell::from(row.duration.clone()),
                Cell::from(row.line.clone()),
            ])
        });
        let table = Table::new(
            table_rows,
            [
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Length(6),
                Constraint::Length(area.width.saturating_sub(36).max(8)),
            ],
        )
        .style(palette::body())
        .header(header)
        .row_highlight_style(palette::cursor())
        .highlight_symbol(super::super::caret(self.enter_pushes()));
        let mut state = TableState::default()
            .with_selected(self.cursor.selected_index(&rows, |row| row.workpiece.clone()))
            .with_offset(self.scroll);
        frame.render_stateful_widget(table, area, &mut state);
        self.scroll = state.offset();
    }

    fn rows(&self, store: &Store) -> Vec<LaneRow> {
        let Some(doc) = store.timeline(self.bloom).and_then(|cell| cell.value.as_ref()) else {
            return Vec::new();
        };
        rows_of(doc, store, 40)
    }
}

struct LaneRow {
    workpiece: String,
    stage: String,
    duration: String,
    line: String,
}

fn workpiece_label(workpiece: &str) -> String {
    if workpiece.is_empty() {
        "(bloom)".to_owned()
    } else {
        workpiece.to_owned()
    }
}

fn lane_title(prefix: &str, reconstructed: bool) -> String {
    let blocked = operator_action(CellKind::Silence(Silence::Blocked)).unwrap_or("");
    let queued = operator_action(CellKind::Silence(Silence::Queued)).unwrap_or("");
    if reconstructed {
        format!("LANE  {prefix}  axis: reconstructed  ░ {blocked}  ┄ {queued}")
    } else {
        format!("LANE  {prefix}  ░ {blocked}  ┄ {queued}")
    }
}

fn rows_of(doc: &MetricsTimeline, store: &Store, width: usize) -> Vec<LaneRow> {
    let now = now_millis();
    let view = store.view().value.as_ref();
    let live = view.and_then(|view| view.blooms.iter().find(|bloom| bloom.id == doc.bloom));
    let reconstructed = doc.spans.iter().any(|span| span.reconstructed || span.started_unix_millis.is_none());
    let (range_start, range_end) = if reconstructed {
        reconstructed_range(&doc.spans)
    } else {
        axis_range(&doc.spans, now).map_or((0, 1), |(start, end, _)| (start, end))
    };
    lane_names(doc, live)
        .into_iter()
        .map(|workpiece| {
            let member_spans: Vec<TimelineSpan> =
                doc.spans.iter().filter(|span| span.workpiece == workpiece).cloned().collect();
            let is_composition = workpiece == WorkpieceId::COMPOSITION;
            let member = live.and_then(|bloom| bloom.members.iter().find(|member| member.workpiece == workpiece));
            let composition = live.and_then(|bloom| bloom.composition.as_ref()).filter(|_| is_composition);
            let silence = silence_of(member);
            let wedged = if is_composition {
                composition.is_some_and(|composition| composition.wedge.is_some())
            } else {
                member.is_some_and(|member| member.wedge.is_some())
            };
            let live_bloom =
                live.is_some_and(|bloom| !matches!(bloom.status, Some(BloomStatus::Landed | BloomStatus::Superseded)));
            let spans_for_paint: Vec<TimelineSpan> = if reconstructed {
                member_spans
                    .iter()
                    .map(|span| TimelineSpan { started_unix_millis: Some(reconstructed_start(span)), ..span.clone() })
                    .collect()
            } else {
                member_spans.clone()
            };
            let last = member_spans.last();
            let duration = last.map_or_else(
                || "—".to_owned(),
                |span| {
                    if reconstructed {
                        format_duration(0)
                    } else {
                        let start = span.started_unix_millis.unwrap_or(range_start);
                        let end = start.saturating_add(span_duration_millis(&member_spans, span, now));
                        bucket_span(start, end, range_start, range_end, width).duration_label
                    }
                },
            );
            let stage = last.map_or_else(
                || {
                    if is_composition {
                        composition
                            .and_then(|composition| composition.cursor.as_ref())
                            .and_then(|cursor| cursor.stage)
                            .map(|stage| stage.to_string())
                            .unwrap_or_default()
                    } else {
                        member
                            .map(|member| {
                                member_status_state(member, view.is_some_and(|view| view.has_lane(doc.bloom, member)))
                                    .to_owned()
                            })
                            .unwrap_or_default()
                    }
                },
                |span| span.stage.to_string(),
            );
            LaneRow {
                workpiece,
                stage,
                duration,
                line: paint_member_line(
                    &spans_for_paint,
                    range_start,
                    range_end,
                    width,
                    silence,
                    live_bloom && !wedged,
                    wedged,
                ),
            }
        })
        .collect()
}

fn lane_names(doc: &MetricsTimeline, live: Option<&BloomView>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for span in &doc.spans {
        if !names.contains(&span.workpiece) {
            names.push(span.workpiece.clone());
        }
    }
    let Some(bloom) = live else {
        return names;
    };
    for member in &bloom.members {
        if !names.contains(&member.workpiece) {
            names.push(member.workpiece.clone());
        }
    }
    if bloom.composition.is_some() {
        let composition = String::from(WorkpieceId::COMPOSITION);
        if !names.contains(&composition) {
            names.push(composition);
        }
    }
    names
}

fn silence_of(member: Option<&MemberView>) -> Silence {
    match member {
        Some(member) if member.blocked_by.as_deref().is_some_and(|name| !name.is_empty()) => Silence::Blocked,
        _ => Silence::Queued,
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{Timeline, lane_title};
    use crate::dto::{DigestHex, MetricsTimeline, StageId, TimelineSpan};
    use crate::keys::{Outcome, assert_footer_honest};
    use crate::nav::Nav;
    use crate::shell::Shell;
    use crate::store::Store;
    use crate::warroom::Focus;
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    #[test]
    fn timeline_footer_keys_are_handled() {
        let nav = Nav::timeline(DigestHex::from_bytes([1; 32]));
        assert_footer_honest(Timeline::key_hints(), |code| {
            Shell::probe(nav.clone()).handle_key(KeyEvent::from(code)) != Outcome::Ignored
        });
    }

    #[test]
    fn a_reconstructed_axis_is_annotated() {
        // The plausible bug: a missing envelope stamp paints the same header
        // as a real wall-clock axis, so the operator reads sequence order as
        // time.
        let live = lane_title("abcd1234", false);
        let reconstructed = lane_title("abcd1234", true);
        assert!(reconstructed.contains("axis: reconstructed"), "{reconstructed}");
        assert!(!live.contains("axis: reconstructed"), "{live}");
        assert_ne!(live, reconstructed);
    }

    #[test]
    fn b_opens_the_member_time_breakdown() {
        // The plausible bug: the footer paints `b time` while Enter is still
        // the only drill-in, so the operator cannot reach the breakdown from
        // the lane they are already looking at.
        let bloom = DigestHex::from_bytes([1; 32]);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_timeline(
            bloom,
            Ok(MetricsTimeline {
                bloom,
                spans: vec![TimelineSpan {
                    workpiece: "wp-a".to_owned(),
                    stage: StageId::Construct,
                    started_unix_millis: Some(1_000),
                    ..TimelineSpan::default()
                }],
                ..MetricsTimeline::default()
            }),
        );
        let mut timeline = Timeline::new(bloom);
        timeline.reseat(&store);
        assert_eq!(
            timeline.handle_key(KeyEvent::from(KeyCode::Char('b')), &store),
            Outcome::Push(Nav::time(bloom, "wp-a"))
        );
    }

    #[test]
    fn enter_opens_the_member_under_the_cursor() {
        // The plausible bug: Enter is still ignored on the lane table, so the
        // operator has to pop back to the board to open the member they are
        // already looking at — or Enter always opens the first row.
        let bloom = DigestHex::from_bytes([1; 32]);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_timeline(
            bloom,
            Ok(MetricsTimeline {
                bloom,
                spans: vec![
                    TimelineSpan {
                        workpiece: "wp-a".to_owned(),
                        stage: StageId::Construct,
                        started_unix_millis: Some(1_000),
                        ..TimelineSpan::default()
                    },
                    TimelineSpan {
                        workpiece: "wp-b".to_owned(),
                        stage: StageId::Verify,
                        started_unix_millis: Some(2_000),
                        ..TimelineSpan::default()
                    },
                ],
                ..MetricsTimeline::default()
            }),
        );
        let mut timeline = Timeline::new(bloom);
        timeline.reseat(&store);
        assert_eq!(
            timeline.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::focus(Focus::member(bloom, "wp-a")))
        );
        assert_eq!(timeline.handle_key(KeyEvent::from(KeyCode::Char('j')), &store), Outcome::Handled);
        assert_eq!(
            timeline.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::focus(Focus::member(bloom, "wp-b")))
        );
    }

    #[test]
    fn enter_opens_the_composition_row() {
        // The plausible bug: the composition lane is opened as a member named
        // aether.bloomery.composition, which the detail frame cannot find.
        let bloom = DigestHex::from_bytes([1; 32]);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_timeline(
            bloom,
            Ok(MetricsTimeline {
                bloom,
                spans: vec![TimelineSpan {
                    workpiece: aether_bloomery::WorkpieceId::COMPOSITION.to_owned(),
                    stage: StageId::AggregateReview,
                    started_unix_millis: Some(1_000),
                    ..TimelineSpan::default()
                }],
                ..MetricsTimeline::default()
            }),
        );
        let mut timeline = Timeline::new(bloom);
        timeline.reseat(&store);
        assert_eq!(
            timeline.handle_key(KeyEvent::from(KeyCode::Enter), &store),
            Outcome::Push(Nav::focus(Focus::composition(bloom)))
        );
    }

    #[test]
    fn a_row_enter_pushes_paints_the_caret() {
        // The plausible bug: hiding the caret on rows Enter refuses also
        // hides it where Enter now pushes a member frame.
        let bloom = DigestHex::from_bytes([1; 32]);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_timeline(
            bloom,
            Ok(MetricsTimeline {
                bloom,
                spans: vec![TimelineSpan {
                    workpiece: "wp-a".to_owned(),
                    stage: StageId::Construct,
                    started_unix_millis: Some(1_000),
                    ..TimelineSpan::default()
                }],
                ..MetricsTimeline::default()
            }),
        );
        let mut timeline = Timeline::new(bloom);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| timeline.render(frame, frame.area(), &store)).expect("draw");
        assert_eq!(super::super::super::row_caret(&terminal, "wp-a"), "> ");
    }

    #[test]
    fn an_aggregate_review_span_paints_on_the_composition_row() {
        // The plausible bug: the critic's span carries an empty workpiece, so
        // the timeline paints a "(bloom)" tail beside the composition cursor
        // for the same integration subject.
        let bloom = DigestHex::from_bytes([1; 32]);
        let mut store = Store::new(Duration::from_secs(1));
        store.apply_timeline(
            bloom,
            Ok(MetricsTimeline {
                bloom,
                spans: vec![
                    TimelineSpan {
                        workpiece: "wp-a".to_owned(),
                        stage: StageId::Construct,
                        started_unix_millis: Some(500),
                        ..TimelineSpan::default()
                    },
                    TimelineSpan {
                        workpiece: aether_bloomery::WorkpieceId::COMPOSITION.to_owned(),
                        stage: StageId::AggregateReview,
                        started_unix_millis: Some(1_000),
                        ..TimelineSpan::default()
                    },
                ],
                ..MetricsTimeline::default()
            }),
        );
        let mut timeline = Timeline::new(bloom);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("test backend");
        terminal.draw(|frame| timeline.render(frame, frame.area(), &store)).expect("draw");
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        let painted: String =
            (0..area.height).flat_map(|y| (0..area.width).map(move |x| buffer[(x, y)].symbol().to_owned())).collect();
        assert!(painted.contains("aether.bloomery"), "the critic span belongs on the composition row: {painted}");
        assert!(
            !painted.contains("(bloom)"),
            "an empty-workpiece bloom tail must not sit beside the composition: {painted}"
        );
        assert!(painted.contains("Aggregate"), "the span keeps its aggregate dispatch identity: {painted}");
    }
}
