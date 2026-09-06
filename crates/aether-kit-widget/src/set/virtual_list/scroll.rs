//! Where the window stands and how far it travels: the scroll span the wheel
//! and the thumb both write, and the row a given offset means.
//!
//! The unit is rows for a list at one pitch and content pixels for one whose
//! rows differ, and everything here is a ratio of the three facts either way.

use crate::set::virtual_list::VirtualListWidget;

/// How far the viewport reaches, how far the whole vector reaches, and where
/// the reader stands in it — the three facts the scroll bar is drawn from.
///
/// The unit is **rows** for a list at one pitch and **content pixels** for one
/// whose rows differ, and the bar does not care which: it is a ratio of the
/// three either way. Stating it once is what keeps the two kinds of list
/// scrolling alike, and keeps a list of uniform rows measuring exactly the bar
/// it measured before rows had heights of their own.
///
/// A *span* rather than an extent because [`crate::ScrollExtent`] already
/// names something else in this crate — a viewport's size in pixels — and one
/// term reading as two concepts costs every reader both definitions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ScrollSpan {
    pub(super) offset: f32,
    pub(super) viewport: f32,
    pub(super) content: f32,
}

impl ScrollSpan {
    /// How far the offset can travel before the last of the content stands at
    /// the bottom of the viewport. `0.0` for content that fits.
    pub(super) fn travel(self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }
}

impl VirtualListWidget {
    /// What the scroll bar is drawn from: rows for the fast path — the
    /// vector's length, the configured viewport and `first_index`, exactly the
    /// three counts the bar was drawn from before rows had heights of their
    /// own — and content pixels once the offset table stands.
    #[allow(clippy::cast_precision_loss)] // a row count a reader could scroll cannot lose precision
    pub(super) fn scroll_span(&self) -> ScrollSpan {
        let first_index = self.first_index.min(self.max_first_index());
        self.row_tops.as_ref().map_or(
            ScrollSpan {
                offset: first_index as f32,
                viewport: self.visible_row_count as f32,
                content: self.items.len() as f32,
            },
            |tops| ScrollSpan {
                offset: tops.get(first_index).copied().unwrap_or(0.0),
                viewport: self.frame.height,
                content: tops.last().copied().unwrap_or(0.0),
            },
        )
    }

    /// The first row a scroll offset in [`ScrollSpan`]'s own unit means:
    /// that row count rounded on the fast path, and the last row whose top is
    /// at or above the offset once the table stands.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(super) fn first_index_at_offset(&self, offset: f32) -> usize {
        let max_first_index = self.max_first_index();
        if !offset.is_finite() || offset <= 0.0 {
            return 0;
        }
        let Some(tops) = &self.row_tops else {
            return (offset.round() as usize).min(max_first_index);
        };
        // The end of the travel is the *last window* rather than the row that
        // happens to start before it: a thumb dragged to the bottom of its
        // track and a wheel rolled past the end both mean "show the end of the
        // content", and rounding those down is the same row-short stop
        // `max_first_index` rounds up out of (gap 41a).
        if offset >= self.last_window_top() {
            return max_first_index;
        }
        tops.partition_point(|top| *top <= offset).saturating_sub(1).min(max_first_index)
    }

    /// Move the window to `first_index`, clamped. Selection is untouched: a
    /// reader scrolling to look at something has not chosen it.
    pub(super) fn scroll_to(&mut self, first_index: usize) {
        self.first_index = first_index.min(self.max_first_index());
    }

    /// Scroll by content pixels, carrying the remainder too small to move a
    /// row. Positive moves the window down the vector.
    ///
    /// The window starts on a row's own top either way, so the wheel picks the
    /// row the rolled pixels land in and carries what is left into the next
    /// roll. What a roll spends past either end of the vector is **dropped**
    /// rather than carried: the window cannot travel that way, so banking it
    /// would make the reader roll the debt back out before the list moved at
    /// all — a dead wheel for as many notches as they over-rolled. The
    /// fixed-pitch branch drops it by construction (its carry is what is left
    /// under one row, and the rows past the end are clamped away); the table
    /// branch has to say so.
    #[allow(clippy::cast_possible_truncation)] // the row delta is bounded by the wheel's own pixels
    pub(super) fn scroll_by_pixels(&mut self, pixels: f32) {
        if !pixels.is_finite() {
            return;
        }
        if self.row_tops.is_none() {
            let Some(row_height) = self.row_height() else {
                return;
            };
            let carried = self.wheel_residual_pixels + pixels;
            let rows = (carried / row_height).trunc();
            self.wheel_residual_pixels = row_height.mul_add(-rows, carried);
            let steps = rows as i64;
            let moved = if steps >= 0 {
                self.first_index.saturating_add(steps.unsigned_abs() as usize)
            } else {
                self.first_index.saturating_sub(steps.unsigned_abs() as usize)
            };
            self.scroll_to(moved);
            return;
        }
        let from = self.scroll_span().offset;
        let carried = self.wheel_residual_pixels + pixels;
        let next = self.first_index_at_offset(from + carried);
        let residual = carried - (self.content_top(next) - from);
        let spent_past_an_end = (next == 0 && residual < 0.0) || (next == self.max_first_index() && residual > 0.0);
        self.wheel_residual_pixels = if spent_past_an_end {
            0.0
        } else {
            residual.clamp(-self.frame.height, self.frame.height)
        };
        self.scroll_to(next);
    }
}

#[cfg(test)]
mod tests {
    use crate::VirtualListRow;
    use crate::set::virtual_list::fixture::{WRAPPING_NOTE, list, noted, table_list};
    use alloc::format;
    use alloc::vec::Vec;

    #[test]
    fn a_table_banks_no_dead_wheel_travel_past_the_end_of_its_vector() {
        // Tripwire: at either end the window cannot travel further, so what a
        // roll spends there is spent. Carried instead it is a debt — the
        // reader who over-rolls at the bottom rolls back up and the list sits
        // still for as many notches as they over-rolled, up to a whole
        // viewport of dead wheel. The fixed-pitch branch never banks more than
        // one row's worth, so the debt is the table branch's own.
        let items: Vec<VirtualListRow> =
            (0..10).map(|index| noted(&format!("stat {index}"), "a sentence under the statistic")).collect();
        let notch = 50.0;

        let mut over_rolled = table_list(items.clone(), 3);
        let bottom = over_rolled.max_first_index();
        over_rolled.scroll_to(bottom);
        for _ in 0..3 {
            over_rolled.scroll_by_pixels(notch);
        }
        assert_eq!(over_rolled.first_index, bottom, "the window stands at the end and rolling past it moves nothing");

        let mut control = table_list(items, 3);
        control.scroll_to(bottom);
        control.scroll_by_pixels(-notch);
        assert!(control.first_index < bottom, "one roll back up moves a window that never over-rolled");

        over_rolled.scroll_by_pixels(-notch);
        assert_eq!(
            over_rolled.first_index, control.first_index,
            "and carries the over-rolled one exactly as far rather than paying off a debt first",
        );
    }

    #[test]
    fn the_wheel_moves_the_window_in_whole_rows_and_carries_the_remainder() {
        // Tripwire: the window is a row index, so a trackpad's stream of
        // sub-row deltas would round to nothing and the list would sit still.
        // Selection is untouched either way — a reader scrolling to look at
        // something has not chosen it.
        let mut widget = list(200, 5, 3);
        let row_height = widget.row_height().expect("a laid-out list has a row height");
        assert_eq!(row_height, 24.0);

        for _ in 0..4 {
            widget.scroll_by_pixels(row_height * 0.25);
        }
        assert_eq!(widget.first_index, 1, "four quarter-rows are one row");
        assert_eq!(widget.selected_index, Some(3), "and the selection did not move with the window");

        widget.scroll_by_pixels(-row_height * 8.0);
        assert_eq!(widget.first_index, 0, "the window clamps at the top");
        widget.scroll_by_pixels(row_height * 1000.0);
        assert_eq!(widget.first_index, 195, "and at the last full page");
        widget.scroll_by_pixels(f32::NAN);
        assert_eq!(widget.first_index, 195, "a non-finite wheel moves nothing");
    }

    #[test]
    fn the_scroll_span_is_the_sum_of_the_heights_rather_than_a_count_of_rows() {
        // Tripwire: a bar whose thumb is `visible / item_count` says a list of
        // ten short rows and a list of ten two-line rows are the same length,
        // and its travel then lands the reader nowhere near where they
        // pointed. Once rows have heights of their own the span is pixels.
        let items: Vec<VirtualListRow> = (0..8)
            .map(|index| {
                if index % 2 == 0 {
                    noted(&format!("row {index}"), WRAPPING_NOTE)
                } else {
                    VirtualListRow::from(format!("row {index}"))
                }
            })
            .collect();
        let widget = table_list(items, 5);

        let span = widget.scroll_span();
        assert!((span.content - (4.0f32.mul_add(55.2, 4.0 * 24.0))).abs() < 1e-2, "{span:?}");
        assert!((span.viewport - widget.frame.height).abs() < f32::EPSILON, "the viewport is the frame, in pixels");
        assert!(widget.scroll_bar().is_some(), "and content taller than the frame stands a bar");
    }
}
