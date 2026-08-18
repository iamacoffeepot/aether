//! Time-axis bucketization for a one-row span line.
//!
//! A span shorter than one bucket still occupies one cell. An empty row
//! is never a legal rendering of a present span.

use super::glyph::{CellKind, Silence, family_of, glyph};
use crate::dto::TimelineSpan;

/// Default bucket on a reconstructed-or-live axis: one second.
pub const BUCKET_RESOLUTION_MILLIS: u64 = 1_000;

/// One painted cell plus the exact duration the numeric column shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketedSpan {
    pub cells: String,
    pub duration_millis: u64,
    pub duration_label: String,
}

/// Place `start..end` on `[range_start, range_end)` across `width` cells.
///
/// A span under the bucket resolution still occupies one cell.
#[must_use]
pub fn bucket_span(start: u64, end: u64, range_start: u64, range_end: u64, width: usize) -> BucketedSpan {
    let width = width.max(1);
    let end = end.max(start);
    let duration_millis = end.saturating_sub(start);
    let mut cells = vec![CellKind::Empty; width];
    let (first, last) = cell_range(start, end, range_start, range_end, width);
    for cell in cells.iter_mut().take(last.saturating_add(1)).skip(first) {
        *cell = CellKind::Stage(super::glyph::StageFamily::Host);
    }
    if cells.iter().all(|cell| *cell == CellKind::Empty) {
        cells[first.min(width.saturating_sub(1))] = CellKind::Stage(super::glyph::StageFamily::Host);
    }
    BucketedSpan {
        cells: cells.into_iter().map(glyph).collect(),
        duration_millis,
        duration_label: format_duration(duration_millis),
    }
}

/// Paint one member's spans into a block line, filling gaps with `silence`.
#[must_use]
pub fn paint_member_line(
    spans: &[TimelineSpan],
    range_start: u64,
    range_end: u64,
    width: usize,
    silence: Silence,
    now: bool,
    wedged: bool,
) -> String {
    let width = width.max(1);
    let mut cells = vec![CellKind::Silence(silence); width];
    let mut any = false;
    for span in spans {
        let start = span.started_unix_millis.unwrap_or(range_start);
        let end = next_end(spans, span).unwrap_or_else(|| range_end.max(start.saturating_add(1)));
        let (first, last) = cell_range(start, end, range_start, range_end, width);
        for cell in cells.iter_mut().take(last.saturating_add(1)).skip(first) {
            *cell = CellKind::Stage(family_of(span.stage));
            any = true;
        }
        if !any {
            cells[first.min(width.saturating_sub(1))] = CellKind::Stage(family_of(span.stage));
            any = true;
        }
    }
    if wedged {
        cells[width.saturating_sub(1)] = CellKind::Wedge;
    } else if now {
        cells[width.saturating_sub(1)] = CellKind::Now;
    }
    cells.into_iter().map(glyph).collect()
}

/// Inclusive cell range covering `[start, end)` on the axis.
#[must_use]
pub fn cell_range(start: u64, end: u64, range_start: u64, range_end: u64, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let first = cell_index(start, range_start, range_end, width);
    let last = cell_index(end.max(start.saturating_add(1)).saturating_sub(1), range_start, range_end, width);
    if first <= last {
        (first, last)
    } else {
        (last, first)
    }
}

fn cell_index(at: u64, range_start: u64, range_end: u64, width: usize) -> usize {
    let span = range_end.saturating_sub(range_start).max(1);
    let offset = at.saturating_sub(range_start).min(span.saturating_sub(1));
    let index = (u128::from(offset) * u128::from(width as u64)) / u128::from(span);
    usize::try_from(index).unwrap_or_else(|_| width.saturating_sub(1)).min(width.saturating_sub(1))
}

fn next_end(spans: &[TimelineSpan], current: &TimelineSpan) -> Option<u64> {
    spans
        .iter()
        .filter(|span| span.workpiece == current.workpiece && span.sequence > current.sequence)
        .filter_map(|span| span.started_unix_millis)
        .min()
}

/// Exact duration in the numeric column — never rounded to a bucket.
#[must_use]
pub fn format_duration(millis: u64) -> String {
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let secs = millis / 1_000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    format!("{}h", mins / 60)
}

/// The axis a set of spans covers. Reconstructed stamps do not invent a clock.
#[must_use]
pub fn axis_range(spans: &[TimelineSpan], now_millis: u64) -> Option<(u64, u64, bool)> {
    let reconstructed = spans.iter().any(|span| span.reconstructed || span.started_unix_millis.is_none());
    let stamps: Vec<u64> = spans.iter().filter_map(|span| span.started_unix_millis).collect();
    if stamps.is_empty() {
        return None;
    }
    let start = stamps.iter().copied().min().unwrap_or(0);
    let end = stamps.iter().copied().max().unwrap_or(start).saturating_add(BUCKET_RESOLUTION_MILLIS).max(now_millis);
    Some((start, end.max(start.saturating_add(1)), reconstructed))
}

/// Sequence-spaced stand-in for a reconstructed axis so order still paints.
#[must_use]
pub fn reconstructed_range(spans: &[TimelineSpan]) -> (u64, u64) {
    let last = spans.iter().map(|span| span.sequence).max().unwrap_or(0);
    (0, last.saturating_add(1).max(1))
}

#[must_use]
pub fn reconstructed_start(span: &TimelineSpan) -> u64 {
    span.sequence
}

/// Duration of one span on a real axis, falling back to the bucket floor.
#[must_use]
pub fn span_duration_millis(spans: &[TimelineSpan], span: &TimelineSpan, now_millis: u64) -> u64 {
    let Some(start) = span.started_unix_millis else {
        return 0;
    };
    next_end(spans, span).unwrap_or(now_millis).saturating_sub(start)
}

#[cfg(test)]
mod tests {
    use super::{bucket_span, format_duration};

    #[test]
    fn a_span_under_the_bucket_renders_one_cell() {
        // The plausible bug: a 200ms span on a 1s bucket paints no cell,
        // so the row looks empty and the duration column is the only hint.
        let painted = bucket_span(1_000, 1_200, 0, 10_000, 10);
        let occupied = painted.cells.chars().filter(|ch| *ch != ' ').count();
        assert_eq!(occupied, 1, "sub-resolution span must occupy exactly one cell, got {:?}", painted.cells);
        assert_eq!(painted.duration_millis, 200);
        assert_eq!(painted.duration_label, format_duration(200));
        assert!(!painted.cells.chars().all(|ch| ch == ' '), "a present span must never render an empty row");
    }

    #[test]
    fn a_zero_width_span_still_occupies_one_cell() {
        let painted = bucket_span(5_000, 5_000, 0, 10_000, 10);
        assert_eq!(painted.cells.chars().filter(|ch| *ch != ' ').count(), 1);
        assert_eq!(painted.duration_millis, 0);
    }
}
