//! Application chrome: header, status, alert band, interrupt queue, footer.
//!
//! Owned by the shell so a drill-in keeps the endpoint, sample age, alerts,
//! and owner-authority queue on screen.

use std::time::Duration;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dto::{SpendQuiesce, ViewDocument};
use crate::keys::{KeyHint, footer_line};
use crate::palette::{self, Role};
use crate::screen::Dashboard;
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
pub fn header(endpoint_label: &str, view: &Cell<ViewDocument>, dashboard: Option<&Dashboard>) -> Paragraph<'static> {
    let age = format_age(view.sample_age());
    let mut spans = vec![
        Span::styled("bloomery-console", palette::body().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {endpoint_label}  sample {age}")),
    ];
    if view.is_stale() {
        spans.push(Span::styled("  STALE", palette::paint(Role::Loud).add_modifier(Modifier::BOLD)));
        if let Some(error) = &view.error {
            spans.push(Span::raw(format!("  {error}")));
        }
    }
    if let Some(dashboard) = dashboard {
        spans.push(Span::raw(format!(
            "  ${}  L{}  W{}",
            dashboard.spend_spark, dashboard.landed_spark, dashboard.wedge_spark
        )));
    }
    Paragraph::new(Line::from(spans)).style(palette::body())
}

#[must_use]
pub fn today(dashboard: &Dashboard) -> Paragraph<'static> {
    Paragraph::new(dashboard.today.clone()).style(palette::body())
}

#[must_use]
pub fn filter_line(filter: &str) -> Paragraph<'static> {
    Paragraph::new(filter.to_owned()).style(palette::body())
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
        spans.push(Span::styled("  diverged", palette::paint(Role::Attention).add_modifier(Modifier::BOLD)));
    }
    Paragraph::new(Line::from(spans)).style(palette::body())
}

#[must_use]
pub fn seal(quiesce: &SpendQuiesce) -> Paragraph<'static> {
    Paragraph::new(Span::styled(format_seal(quiesce), palette::paint(Role::Loud).add_modifier(Modifier::BOLD)))
        .style(palette::body())
}

#[must_use]
pub fn alert_band(alerts: &[Alert], selected: Option<usize>) -> Paragraph<'static> {
    let spans: Vec<Span<'static>> = alerts
        .iter()
        .enumerate()
        .flat_map(|(index, alert)| {
            let mut style = palette::paint(alert_role(alert.kind)).add_modifier(Modifier::BOLD);
            if selected == Some(index) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            [Span::styled(alert.token.clone(), style), Span::raw(format!(" {}  ", alert.detail))]
        })
        .collect();
    Paragraph::new(Line::from(spans)).style(palette::body())
}

/// Interrupt rows the band can paint. Extra queue entries stay reachable by
/// scrolling the selection onto this window.
pub const INTERRUPT_BAND_ROWS: usize = 8;

/// Visible slice of the interrupt queue and the selection index inside it.
#[must_use]
pub fn interrupt_window(entries: &[Interrupt], selected: Option<usize>) -> (&[Interrupt], Option<usize>) {
    let start = interrupt_window_start(entries.len(), selected);
    let end = (start + INTERRUPT_BAND_ROWS).min(entries.len());
    let window = &entries[start..end];
    let relative = selected.and_then(|index| index.checked_sub(start).filter(|&rel| rel < window.len()));
    (window, relative)
}

fn interrupt_window_start(len: usize, selected: Option<usize>) -> usize {
    if len <= INTERRUPT_BAND_ROWS {
        return 0;
    }
    let max_start = len - INTERRUPT_BAND_ROWS;
    selected.map_or(0, |index| index.saturating_sub(INTERRUPT_BAND_ROWS.saturating_sub(1)).min(max_start))
}

#[must_use]
pub fn interrupt_band(entries: &[Interrupt], selected: Option<usize>) -> Paragraph<'static> {
    let lines: Vec<Line<'static>> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut style = palette::paint(interrupt_role(entry.kind)).add_modifier(Modifier::BOLD);
            if selected == Some(index) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::from(vec![
                Span::styled(entry.kind.label().to_owned(), style),
                Span::raw(format!("  {}", entry.detail)),
            ])
        })
        .collect();
    Paragraph::new(lines).style(palette::body())
}

fn alert_role(kind: AlertKind) -> Role {
    match kind {
        AlertKind::Park | AlertKind::HostFault => Role::Attention,
        AlertKind::Landing | AlertKind::Fault | AlertKind::Wedge => Role::Loud,
    }
}

fn interrupt_role(kind: InterruptKind) -> Role {
    match kind {
        InterruptKind::Park | InterruptKind::Decision | InterruptKind::Hold | InterruptKind::Findings => {
            Role::Attention
        }
        InterruptKind::Terminal | InterruptKind::Wedge | InterruptKind::Landing | InterruptKind::Quiesce => Role::Loud,
    }
}

#[must_use]
pub fn footer(hints: &[KeyHint], metrics: Option<&str>) -> Paragraph<'static> {
    match metrics {
        Some(metrics) if !metrics.is_empty() => {
            Paragraph::new(format!("{metrics}   {}", footer_line(hints))).style(palette::body())
        }
        _ => Paragraph::new(footer_line(hints)).style(palette::body()),
    }
}

#[cfg(test)]
mod tests {
    use super::{format_age, format_seal, format_status, interrupt_window};
    use crate::dto::{DigestHex, SpendQuiesce, ViewDocument};
    use crate::warroom::{Focus, Interrupt, InterruptKind};
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn park_row(n: u8) -> Interrupt {
        Interrupt { kind: InterruptKind::Park, detail: format!("row-{n}"), focus: Focus::bloom(digest(n)) }
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

    #[test]
    fn interrupt_window_scrolls_the_selected_row_into_view() {
        // The plausible bug: a queue longer than the band keeps the highlight
        // on a clipped row, so j/k looks like the cursor vanished.
        let entries: Vec<Interrupt> = (0..10).map(park_row).collect();

        let (window, selected) = interrupt_window(&entries, None);
        assert_eq!(window.len(), 8);
        assert_eq!(window[0].detail, "row-0");
        assert_eq!(selected, None);

        let (window, selected) = interrupt_window(&entries, Some(0));
        assert_eq!(window[0].detail, "row-0");
        assert_eq!(selected, Some(0));

        let (window, selected) = interrupt_window(&entries, Some(9));
        assert_eq!(window.len(), 8);
        assert_eq!(window[0].detail, "row-2");
        assert_eq!(window[7].detail, "row-9");
        assert_eq!(selected, Some(7));
    }
}
