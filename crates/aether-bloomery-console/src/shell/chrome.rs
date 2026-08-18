//! Application chrome: header, optional filter line, footer.
//!
//! Owned by the shell so a later drill-in keeps the endpoint, sample age,
//! and key hints on screen.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dto::ViewDocument;
use crate::keys::{KeyHint, footer_line};
use crate::store::Cell;

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

#[must_use]
pub fn footer(hints: &[KeyHint]) -> Paragraph<'static> {
    Paragraph::new(footer_line(hints))
}

#[cfg(test)]
mod tests {
    use super::format_age;
    use std::time::Duration;

    #[test]
    fn format_age_names_never_before_the_first_sample() {
        assert_eq!(format_age(None), "never");
        assert_eq!(format_age(Some(Duration::from_secs(0))), "0s");
        assert_eq!(format_age(Some(Duration::from_secs(59))), "59s");
        assert_eq!(format_age(Some(Duration::from_mins(1))), "1m");
        assert_eq!(format_age(Some(Duration::from_hours(1))), "1h");
    }
}
