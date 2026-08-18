//! Application chrome: header, status, alert band, interrupt queue, footer.
//!
//! Owned by the shell so a drill-in keeps the endpoint, sample age, alerts,
//! and owner-authority queue on screen.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dto::{SpendQuiesce, ViewDocument};
use crate::keys::{KeyHint, footer_line};
use crate::store::Cell;
use crate::warroom::{Alert, AlertKind, Interrupt, InterruptKind};

/// Age of the last successful sample, for the header.
#[must_use]
pub fn format_age(age: Option<Duration>) -> String {
    let Some(age) = age else {
        return "never".to_owned();
    };
    let secs = age.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    format!("{}h", mins / 60)
}

#[must_use]
pub fn header(endpoint_label: &str, view: &Cell<ViewDocument>) -> Paragraph<'static> {
    let age = format_age(view.sample_age());
    let mut spans = vec![
        Span::styled("bloomery-console", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {endpoint_label}  sample {age}")),
    ];
    if view.is_stale() {
        spans.push(Span::styled("  STALE", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)));
        if let Some(error) = &view.error {
            spans.push(Span::raw(format!("  {error}")));
        }
    }
    Paragraph::new(Line::from(spans))
}

#[must_use]
pub fn filter_line(filter: &str) -> Paragraph<'static> {
    Paragraph::new(filter.to_owned())
}

/// `mainline` / `observed` prefixes, plus a divergence token when they differ.
#[must_use]
pub fn format_status(view: &ViewDocument) -> String {
    let line = format!("mainline {}  observed {}", view.mainline.prefix(), view.observed.prefix());
    if view.mainline == view.observed {
        line
    } else {
        format!("{line}  diverged")
    }
}

/// Seal-door-closed marker: window and ceiling from `spend_quiesce`.
#[must_use]
pub fn format_seal(quiesce: &SpendQuiesce) -> String {
    quiesce.label()
}

#[must_use]
pub fn status(view: &ViewDocument) -> Paragraph<'static> {
    let mut spans =
        vec![Span::raw(format!("mainline {}  observed {}", view.mainline.prefix(), view.observed.prefix()))];
    if view.mainline != view.observed {
        spans.push(Span::styled("  diverged", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    }
    Paragraph::new(Line::from(spans))
}

#[must_use]
pub fn seal(quiesce: &SpendQuiesce) -> Paragraph<'static> {
    Paragraph::new(Span::styled(format_seal(quiesce), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)))
}

#[must_use]
pub fn alert_band(alerts: &[Alert], selected: Option<usize>) -> Paragraph<'static> {
    let spans: Vec<Span<'static>> = alerts
        .iter()
        .enumerate()
        .flat_map(|(index, alert)| {
            let mut style = Style::default().fg(alert_color(alert.kind)).add_modifier(Modifier::BOLD);
            if selected == Some(index) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            [Span::styled(alert.token.clone(), style), Span::raw(format!(" {}  ", alert.detail))]
        })
        .collect();
    Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black))
}

#[must_use]
pub fn interrupt_band(entries: &[Interrupt], selected: Option<usize>) -> Paragraph<'static> {
    let lines: Vec<Line<'static>> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut style = Style::default().fg(interrupt_color(entry.kind)).add_modifier(Modifier::BOLD);
            if selected == Some(index) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::from(vec![
                Span::styled(entry.kind.label().to_owned(), style),
                Span::raw(format!("  {}", entry.detail)),
            ])
        })
        .collect();
    Paragraph::new(lines)
}

fn alert_color(kind: AlertKind) -> Color {
    match kind {
        AlertKind::Park | AlertKind::HostFault => Color::Yellow,
        AlertKind::Landing | AlertKind::Fault | AlertKind::Wedge => Color::Red,
    }
}

fn interrupt_color(kind: InterruptKind) -> Color {
    match kind {
        InterruptKind::Park | InterruptKind::Decision | InterruptKind::Hold | InterruptKind::Findings => Color::Yellow,
        InterruptKind::Terminal | InterruptKind::Wedge | InterruptKind::Landing | InterruptKind::Quiesce => Color::Red,
    }
}

#[must_use]
pub fn footer(hints: &[KeyHint]) -> Paragraph<'static> {
    Paragraph::new(footer_line(hints))
}

#[cfg(test)]
mod tests {
    use super::{format_age, format_seal, format_status};
    use crate::dto::{DigestHex, SpendQuiesce, ViewDocument};
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    #[test]
    fn format_age_names_never_before_the_first_sample() {
        assert_eq!(format_age(None), "never");
        assert_eq!(format_age(Some(Duration::from_secs(0))), "0s");
        assert_eq!(format_age(Some(Duration::from_secs(59))), "59s");
        assert_eq!(format_age(Some(Duration::from_mins(1))), "1m");
        assert_eq!(format_age(Some(Duration::from_hours(1))), "1h");
    }

    #[test]
    fn status_names_divergence_and_the_closed_seal() {
        // The plausible bug: equal heads still paint "diverged", or the seal
        // line drops the window and ceiling the operator has to raise.
        let aligned = ViewDocument { mainline: digest(1), observed: digest(1), ..ViewDocument::default() };
        assert_eq!(
            format_status(&aligned),
            format!("mainline {}  observed {}", digest(1).prefix(), digest(1).prefix())
        );
        assert!(!format_status(&aligned).contains("diverged"));

        let drifted = ViewDocument { mainline: digest(1), observed: digest(2), ..ViewDocument::default() };
        assert!(format_status(&drifted).contains("diverged"), "{}", format_status(&drifted));

        assert_eq!(
            format_seal(&SpendQuiesce::Window {
                window: "bloomery/daily/2026-08-17".to_owned(),
                spent_micro_usd: 12,
                ceiling_micro_usd: 10,
            }),
            "SEAL CLOSED  bloomery/daily/2026-08-17  12/10"
        );
    }
}
