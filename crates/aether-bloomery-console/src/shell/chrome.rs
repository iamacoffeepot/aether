//! Application chrome: header, status, needs-you band, footer.
//!
//! The header and footer are shell chrome. Needs-you paints the merged
//! queue; quiet paints the status, today, and rest-count lines.

use std::time::Duration;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dto::{SpendQuiesce, ViewDocument};
use crate::keys::{KeyHint, footer_line};
use crate::palette::{self, Role};
use crate::screen::{Dashboard, QuietLine};
use crate::store::Cell;
use crate::warroom::{NeedsYouRow, Severity};

/// One painted needs-you line, plus whether the operator has dismissed it.
pub struct BandRow<'a> {
    pub row: &'a NeedsYouRow,
    pub dismissed: bool,
}

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

// Three 14-cell series plus the metrics do not fit a narrower row, so the decoration goes before the facts.
const SPARKLINE_MIN_WIDTH: u16 = 100;

/// Identity, endpoint, sample age, then staleness (deliberately before the metrics), then metrics and sparklines.
#[must_use]
pub fn header(
    endpoint_label: &str,
    view: &Cell<ViewDocument>,
    dashboard: Option<&Dashboard>,
    width: u16,
) -> Paragraph<'static> {
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
        spans.push(Span::raw(format!("  {}", dashboard.footer)));
        if width >= SPARKLINE_MIN_WIDTH {
            spans.push(Span::raw(format!(
                "  ${}  L{}  W{}",
                dashboard.spend_spark, dashboard.landed_spark, dashboard.wedge_spark
            )));
        }
    }
    Paragraph::new(Line::from(spans)).style(palette::body())
}

#[must_use]
pub fn today(dashboard: &Dashboard) -> Paragraph<'static> {
    Paragraph::new(dashboard.today.clone()).style(palette::body())
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
pub fn quiet(lines: &[QuietLine]) -> Paragraph<'static> {
    let dim = palette::body().add_modifier(Modifier::DIM);
    Paragraph::new(lines.iter().map(|line| Line::from(Span::styled(line.text(), dim))).collect::<Vec<_>>())
        .style(palette::body())
}

/// Visible slice of the needs-you queue, the selection index inside it, and
/// how many rows sit outside the window. `height` is the pane's inner height.
/// When the queue overflows, the last line is reserved for `+N more`.
#[must_use]
pub fn needs_you_window<T>(rows: &[T], selected: Option<usize>, height: usize) -> (&[T], Option<usize>, usize) {
    if height == 0 || rows.is_empty() {
        return (&[], None, 0);
    }
    let visible = if rows.len() > height {
        height.saturating_sub(1)
    } else {
        height
    };
    if visible == 0 {
        return (&[], None, rows.len());
    }
    let start = needs_you_window_start(rows.len(), selected, visible);
    let end = (start + visible).min(rows.len());
    let window = &rows[start..end];
    let relative = selected.and_then(|index| index.checked_sub(start).filter(|&rel| rel < window.len()));
    (window, relative, rows.len() - window.len())
}

fn needs_you_window_start(len: usize, selected: Option<usize>, rows: usize) -> usize {
    if len <= rows {
        return 0;
    }
    let max_start = len - rows;
    selected.map_or(0, |index| index.saturating_sub(rows.saturating_sub(1)).min(max_start))
}

#[must_use]
pub fn needs_you_band(
    window: &[BandRow<'_>],
    selected: Option<usize>,
    hidden: usize,
    cleared: usize,
) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = window
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let mut style = palette::paint(severity_role(row.row.severity)).add_modifier(if row.dismissed {
                Modifier::DIM
            } else {
                Modifier::BOLD
            });
            if selected == Some(index) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Line::from(Span::styled(format!("{} · {} · {}", row.row.subject, row.row.happened, row.row.action), style))
        })
        .collect();
    let mut clauses = Vec::new();
    if hidden > 0 {
        clauses.push(format!("+{hidden} more"));
    }
    if cleared > 0 {
        clauses.push(format!("·{cleared} cleared"));
    }
    if !clauses.is_empty() {
        lines.push(Line::from(Span::raw(clauses.join("  "))));
    }
    Paragraph::new(lines).style(palette::body())
}

fn severity_role(severity: Severity) -> Role {
    match severity {
        Severity::Attention => Role::Attention,
        Severity::Loud => Role::Loud,
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
    use super::{format_age, format_seal, format_status, needs_you_window};
    use crate::dto::{DigestHex, SpendQuiesce, ViewDocument};
    use crate::warroom::{Focus, NeedsYouRow, Severity};
    use std::time::Duration;

    fn digest(byte: u8) -> DigestHex {
        DigestHex::from_bytes([byte; 32])
    }

    fn park_row(n: u8) -> NeedsYouRow {
        NeedsYouRow {
            focus: Focus::bloom(digest(n)),
            subject: digest(n).prefix(),
            happened: "park".to_owned(),
            action: "accept or defer".to_owned(),
            severity: Severity::Attention,
        }
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
    fn needs_you_window_reserves_a_line_for_the_overflow_marker() {
        // The plausible bug: a queue longer than the pane paints only the
        // visible slice, so the operator cannot tell rows exist outside it;
        // or the marker overwrites a real row because the window was not shrunk.
        let rows: Vec<NeedsYouRow> = (0..11).map(park_row).collect();

        let (window, selected, hidden) = needs_you_window(&rows, None, 8);
        assert_eq!(window.len(), 7);
        assert_eq!(hidden, 4);
        assert_eq!(selected, None);

        let (window, selected, hidden) = needs_you_window(&rows, Some(10), 8);
        assert_eq!(window.len(), 7);
        assert_eq!(hidden, 4);
        assert_eq!(window[6].focus, Focus::bloom(digest(10)));
        assert_eq!(selected, Some(6));
    }
}
