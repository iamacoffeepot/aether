//! The Board: alert band, bloom/member table, footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::state::{BoardRow, BoardState, format_age};

/// Paint one frame of the board.
pub fn render(frame: &mut Frame<'_>, state: &BoardState) {
    let dimmed = state.is_stale();
    let alert_height = if state.alerts.is_empty() {
        0
    } else {
        u16::try_from(state.alerts.len().clamp(1, 4)).unwrap_or(1)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(alert_height),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    frame.render_widget(header(state, dimmed), chunks[0]);
    if alert_height > 0 {
        frame.render_widget(alert_band(state), chunks[1]);
    }
    render_table(frame, chunks[2], state, dimmed);
    frame.render_widget(footer(), chunks[3]);
}

fn header(state: &BoardState, dimmed: bool) -> Paragraph<'static> {
    let age = format_age(state.sample_age());
    let mut spans = vec![
        Span::styled("bloomery-console", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {}  sample {age}", state.endpoint_label)),
    ];
    if dimmed {
        spans.push(Span::styled("  STALE", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)));
        if let Some(error) = &state.last_error {
            spans.push(Span::raw(format!("  {error}")));
        }
    }
    Paragraph::new(Line::from(spans))
}

fn alert_band(state: &BoardState) -> Paragraph<'static> {
    let spans: Vec<Span<'static>> = state
        .alerts
        .iter()
        .flat_map(|alert| {
            [
                Span::styled(
                    alert.token.clone(),
                    Style::default().fg(alert_color(&alert.token)).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" {}  ", alert.detail)),
            ]
        })
        .collect();
    Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black))
}

fn alert_color(token: &str) -> Color {
    if token == "PARK" || token == "hostfault" {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn render_table(frame: &mut Frame<'_>, area: Rect, state: &BoardState, dimmed: bool) {
    let muted = if dimmed {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    let header = Row::new(["BLOOM / MEMBER", "STATE", "MACH", "BLOCKED BY", "WEDGE"])
        .style(Style::default().add_modifier(Modifier::BOLD).patch(muted));
    let rows = state.rows.iter().map(|row| match row {
        BoardRow::Bloom(bloom) => Row::new([
            Cell::from(bloom.id_prefix.clone()),
            Cell::from(bloom.status.clone()),
            Cell::from(format!("{} mem", bloom.member_count)),
            Cell::from(""),
            Cell::from(""),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD).patch(muted)),
        BoardRow::Member(member) => Row::new([
            Cell::from(format!("  {}", member.workpiece)),
            Cell::from(member.state.clone()),
            Cell::from(member.machinery.clone()),
            Cell::from(member.blocked_by.clone()),
            Cell::from(member.wedge_cause.clone()),
        ])
        .style(muted),
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(28),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(20),
            Constraint::Min(8),
        ],
    )
    .header(header)
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("> ");
    let mut table_state = TableState::default().with_selected(state.selected_index());
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn footer() -> Paragraph<'static> {
    Paragraph::new("j/k select   r refresh   q quit")
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::dto::{BloomView, DigestHex, LandingBlock, MemberView, Present, ViewDocument};
    use crate::state::BoardState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
                review_park: Some(Present {}),
                landing_blocked: Some(LandingBlock { rolls: 1, budget: 2 }),
                members: vec![MemberView {
                    workpiece: "issue-1".to_owned(),
                    wedge: Some(Present {}),
                    host_fault: Some(Present {}),
                    ..MemberView::default()
                }],
                ..BloomView::default()
            }],
        };
        let mut state = BoardState::new("127.0.0.1:8910".to_owned());
        state.apply_view(&view);
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).expect("test backend");
        terminal.draw(|frame| render(frame, &state)).expect("draw");
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
        };
        let mut state = BoardState::new("127.0.0.1:8910".to_owned());
        state.apply_view(&view);
        state.apply_error("connection refused");
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).expect("test backend");
        terminal.draw(|frame| render(frame, &state)).expect("draw");
        let text = buffer_text(&terminal);
        assert!(text.contains("STALE"), "{text}");
        assert!(text.contains("connection refused"), "{text}");
        assert!(text.contains("issue-keep"), "{text}");
    }
}
