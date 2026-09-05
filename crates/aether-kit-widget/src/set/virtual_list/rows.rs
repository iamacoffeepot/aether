//! The offset table and the realized row window.
//!
//! A vector no row of which asks for a height of its own is the fixed-pitch
//! fast path — the frame divided by the configured row count, no table kept.
//! Any row carrying a note, an indent, a space or a rule puts every row on
//! its own height and the list keeps a prefix-sum table the window, the hit
//! test, the bar and the reported hover are all read from by bisection.

use alloc::vec::Vec;

use crate::VirtualListRow;
use crate::set::virtual_list::{VirtualListWidget, valid_frame};
use crate::theme::TextRole;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VisibleRowWindow {
    pub(super) first_index: usize,
    pub(super) end_exclusive_index: usize,
}

impl VisibleRowWindow {
    pub(super) fn len(self) -> usize {
        self.end_exclusive_index.saturating_sub(self.first_index)
    }
}

/// Where the parts of one realized row stand, in widget-local pixels.
///
/// A row is a slot, a plate inside it, and a first line inside that. The three
/// are one rectangle for the ordinary row and three for a table entry: the
/// slot opens with the row's `space_before` **ground**, the plate is the fill
/// under the whole entry, and the first line is the band its name, its
/// trailing column and its verbs are centred in, with the note's lines under
/// it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RowBands {
    /// The top of the whole slot — the top of the row's space, which is where
    /// a `rule_above` hairline stands.
    pub(super) slot_top: f32,
    /// The top of the plate, below that space.
    pub(super) plate_top: f32,
    /// How tall the plate is: the row less its space.
    pub(super) plate_height: f32,
    /// The band the row's own line stands in — its role's pitch, which is the
    /// whole plate for a row without a note.
    pub(super) line_height: f32,
}

/// Whether any of `items` asks for a height of its own.
pub(super) fn rows_vary(items: &[VirtualListRow]) -> bool {
    items.iter().any(|row| row.note.is_some() || row.indent > 0 || row.space_before > 0 || row.rule_above)
}

impl VirtualListWidget {
    /// The rows realized right now: the configured count from `first_index`
    /// while every row is one height, and every row the frame reaches once the
    /// offset table stands — a table of tall and short rows shows as many as
    /// fit rather than as many as were asked for. At least one row is realized
    /// either way, so a row taller than the whole viewport still draws.
    pub(super) fn window(&self) -> VisibleRowWindow {
        let Some(tops) = &self.row_tops else {
            return clamped_window(self.first_index, self.visible_row_count, self.items.len());
        };
        if self.items.is_empty() || self.visible_row_count == 0 || !valid_frame(&self.frame) {
            return VisibleRowWindow { first_index: 0, end_exclusive_index: 0 };
        }
        let first_index = self.first_index.min(self.max_first_index());
        let limit = tops[first_index] + self.frame.height;
        let end_exclusive_index = tops.partition_point(|top| *top < limit).clamp(first_index + 1, self.items.len());
        VisibleRowWindow { first_index, end_exclusive_index }
    }

    /// One row's height while every row is one height: the viewport divided by
    /// the row count the list was *configured* for, never by the number it
    /// happens to have realized. A list holding fewer items than its viewport
    /// therefore draws its rows at their normal height with the rest of the
    /// viewport left empty — dividing by the realized count instead stretched
    /// two items over the whole frame, so a short list rendered as one giant
    /// row.
    ///
    /// `None` once the list keeps an offset table, and for a frame no row can
    /// stand in.
    pub(super) fn row_height(&self) -> Option<f32> {
        if self.row_tops.is_some() || self.visible_row_count == 0 || !valid_frame(&self.frame) {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let divisor = self.visible_row_count as f32;
        let row_height = self.frame.height / divisor;
        (row_height.is_finite() && row_height > 0.0).then_some(row_height)
    }

    /// The row the point `local_y` lands in, or `None` for a point off the
    /// rows.
    ///
    /// A row's `space_before` gap belongs to the row **under** it — it is that
    /// row's own space, and a press in it is a press aimed at the row it opens
    /// rather than at the one it closed.
    pub(super) fn row_at_local_y(&self, local_y: f32) -> Option<usize> {
        let window = self.window();
        if !local_y.is_finite() || local_y < 0.0 || local_y >= self.frame.height {
            return None;
        }
        let Some(tops) = &self.row_tops else {
            let row_height = self.row_height()?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let row_offset = (local_y / row_height).floor() as usize;
            return (row_offset < window.len()).then(|| window.first_index + row_offset);
        };
        let content_y = tops.get(window.first_index)? + local_y;
        let index = tops.partition_point(|top| *top <= content_y).checked_sub(1)?;
        (index >= window.first_index && index < window.end_exclusive_index).then_some(index)
    }

    /// The pitch one row of `role` stands at.
    ///
    /// The theme's `row_height` is the **body** pitch and every other role
    /// scales by its own type step against the body size, so a caption row is
    /// shorter and a heading row taller in exactly the proportion their sizes
    /// differ. One number to tune rather than four, and a role that grows in a
    /// restyled theme takes its rows with it. A theme whose body size is zero
    /// falls back to the pitch itself rather than collapsing every row.
    fn role_row_height(&self, role: TextRole) -> f32 {
        if self.theme.label_size_pixels <= 0.0 {
            return self.theme.row_height.max(0.0);
        }
        (self.theme.row_height * self.theme.text_size_pixels(role) / self.theme.label_size_pixels).max(0.0)
    }

    /// One row's whole height in a row `row_width` wide: the ground above it,
    /// its role's pitch, and a line for each line of its note.
    fn item_height(&self, row: &VirtualListRow, row_width: f32) -> f32 {
        #[allow(clippy::cast_precision_loss)] // a note is at most MAX_NOTE_LINES lines
        let note_lines = self.note_lines(row, self.note_budget(row, row_width)).len() as f32;
        note_lines.mul_add(self.note_line_height(), self.theme.space(row.space_before) + self.role_row_height(row.role))
    }

    /// The offset table for the whole vector at `row_width`: `items.len() + 1`
    /// running sums, the last of which is the content height.
    fn build_row_tops(&self, row_width: f32) -> Vec<f32> {
        let mut tops = Vec::with_capacity(self.items.len().saturating_add(1));
        let mut top = 0.0;
        tops.push(top);
        for row in &self.items {
            top += self.item_height(row, row_width);
            tops.push(top);
        }
        tops
    }

    /// Rebuild the offset table when the frame it was built for is not the
    /// frame the list has now, and keep no table at all for a vector no row of
    /// which asks for a height of its own — the fixed-pitch fast path, where
    /// the geometry is one multiply and the list costs exactly what it always
    /// did.
    ///
    /// Called from every handler that goes on to consult the geometry, because
    /// the heights are a function of the frame and the frame arrives through a
    /// shared handler that knows nothing about rows.
    ///
    /// The gutter the scroll bar takes is itself a function of the heights, so
    /// the table is built once at the full frame and again a gutter narrower
    /// when that first pass overflows. Narrowing a row can only wrap a note
    /// onto *more* lines, so content that overflowed still overflows and the
    /// second pass is the last one.
    ///
    /// The **first** table a list builds re-reveals the selection, because the
    /// window it was booted with was picked before any table existed: `init`
    /// has no frame to measure against, so it counts rows at the one pitch,
    /// and a table's rows are not that pitch. Without this a list opened at a
    /// selected row deep in its vector draws a window the selection is not in
    /// and shows no highlight at all until the reader scrolls.
    pub(super) fn refresh_row_layout(&mut self) {
        // A frame no row can stand in draws nothing either way, and wrapping a
        // note against a width of zero would break it into one line per word.
        if !self.rows_vary || !valid_frame(&self.frame) {
            self.row_tops = None;
            self.row_tops_frame = None;
            return;
        }
        let frame = (self.frame.width, self.frame.height);
        if self.row_tops.is_some() && self.row_tops_frame == Some(frame) {
            return;
        }
        let full_width = self.frame.width.max(0.0);
        let mut tops = self.build_row_tops(full_width);
        if tops.last().copied().unwrap_or(0.0) > self.frame.height {
            let gutter = self.bar_reserve_width();
            tops = self.build_row_tops((full_width - gutter).max(0.0));
        }
        let first_table = self.row_tops.is_none();
        self.row_tops = Some(tops);
        self.row_tops_frame = Some(frame);
        if first_table {
            self.reveal_selection();
        }
    }

    /// The content-space top of one row's slot — the distance from the top of
    /// the whole vector to the top of that row's `space_before` gap. `0.0`
    /// without a table, where content space is counted in rows rather than in
    /// pixels.
    pub(super) fn content_top(&self, item_index: usize) -> f32 {
        self.row_tops.as_ref().and_then(|tops| tops.get(item_index).copied()).unwrap_or(0.0)
    }

    /// Where one realized row's parts stand in widget-local pixels, or `None`
    /// for an item outside the realized window.
    pub(super) fn row_bands(&self, item_index: usize) -> Option<RowBands> {
        let window = self.window();
        let row_offset = item_index.checked_sub(window.first_index).filter(|offset| *offset < window.len())?;
        let Some(tops) = &self.row_tops else {
            let row_height = self.row_height()?;
            #[allow(clippy::cast_precision_loss)] // a realized row offset is at most a viewport's worth
            let slot_top = row_offset as f32 * row_height;
            return Some(RowBands { slot_top, plate_top: slot_top, plate_height: row_height, line_height: row_height });
        };
        let row = self.items.get(item_index)?;
        let slot_top = tops.get(item_index)? - tops.get(window.first_index)?;
        let gap = self.theme.space(row.space_before);
        Some(RowBands {
            slot_top,
            plate_top: slot_top + gap,
            plate_height: (tops.get(item_index + 1)? - tops.get(item_index)? - gap).max(0.0),
            line_height: self.role_row_height(row.role),
        })
    }

    /// How wide a row is: the frame less whatever the scroll bar's gutter
    /// takes off its right end. A row stops where the gutter starts — it does
    /// not run under the bar and get covered by it (round-5 note 8), which is
    /// what a full-frame row fill did.
    pub(super) fn row_width(&self) -> f32 {
        (self.frame.width - self.bar_gutter_width()).max(0.0)
    }

    /// Where the **last** window stands in content space: a viewport short of
    /// the content's end, and `0.0` for content that fits. Zero on the fast
    /// path, whose content is counted in rows rather than pixels.
    pub(super) fn last_window_top(&self) -> f32 {
        self.row_tops.as_ref().map_or(0.0, |tops| tops.last().copied().unwrap_or(0.0) - self.frame.height)
    }

    /// The topmost row the window can start at: the one past which the rest of
    /// the content no longer fills the viewport. A count on the fast path,
    /// where a viewport is a whole number of rows by construction, and the
    /// **first** row whose top clears a viewport of the content's end once the
    /// offset table stands.
    ///
    /// That last window is rounded **up** (the studio's gap 41a, round-17 note
    /// 1 — "on defense extended stats cannot scroll to bottom"). Rounded down
    /// it started on the last row whose top is at or before the content's end,
    /// so unless the frame happened to be an exact prefix sum of the rows the
    /// window stopped short by up to a row's height and the final statistic
    /// hung below the frame's edge with nothing left to roll. Up, the last row
    /// lands inside the frame and the slack — never more than one row — falls
    /// above the window's start, which is what every scrolling view does with
    /// the end of its content.
    pub(super) fn max_first_index(&self) -> usize {
        let Some(tops) = &self.row_tops else {
            return self.items.len().saturating_sub(self.visible_row_count);
        };
        let last_top = self.last_window_top();
        if !last_top.is_finite() || last_top <= 0.0 {
            return 0;
        }
        tops.partition_point(|top| *top < last_top).min(self.items.len().saturating_sub(1))
    }

    /// The whole item vector's height in pixels: the offset table's last sum
    /// once the rows have heights of their own, and the pitch the rows are
    /// actually drawn at ([`Self::row_height`]) by the item count while every
    /// row is one height.
    ///
    /// The **drawn** pitch rather than the theme's, because a fixed-pitch list
    /// divides the frame it was given by the row count it was configured for:
    /// in a frame that is not `theme.row_height × visible_row_count` tall the
    /// rows stand taller or shorter than the theme's pitch and the vector
    /// scrolls through the sum of those. A plate sized from the theme number
    /// would be cut short of the rows it is meant to hold, which is the very
    /// gap this reports to close. The theme's pitch is the fallback for a
    /// frame no row can stand in, where there is nothing drawn to disagree
    /// with.
    ///
    /// This is the scroll span's `content` said in pixels. The span counts
    /// **rows** on the fixed-pitch path, because the bar is a ratio either
    /// way; a host drawing a container around the list needs the pixels, so
    /// the one number is stated in both units from the same two branches
    /// rather than re-derived on the host's side out of a mirrored row
    /// arithmetic that drifts (the studio's gap 41).
    ///
    /// `None` for a table whose rows the list has not measured yet — the
    /// offset table missing while some row asks for a height of its own, or
    /// the font's advances still in flight, either of which would answer with
    /// a note wrapped onto a line count it is about to change. A plate sized
    /// from that would resize under the reader a frame later.
    pub(super) fn content_height(&self) -> Option<f32> {
        let Some(tops) = &self.row_tops else {
            let pitch = self.row_height().unwrap_or(self.theme.row_height);
            #[allow(clippy::cast_precision_loss)] // a row count a reader could scroll cannot lose precision
            return (!self.rows_vary).then_some(pitch * self.items.len() as f32);
        };
        self.font_metrics.resolved()?;
        Some(tops.last().copied().unwrap_or(0.0))
    }

    /// The height the viewport asks for: the configured row count at the one
    /// pitch, and the first that many rows' own heights once they have any.
    ///
    /// Measured from the top of the vector rather than from the realized
    /// window, so the height a slot was sized to does not change under the
    /// reader as they scroll a table of tall and short rows.
    pub(super) fn viewport_height(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)] // a viewport of rows a reader could scroll cannot lose precision
        let uniform = self.theme.row_height * self.visible_row_count as f32;
        self.row_tops
            .as_ref()
            .map_or(uniform, |tops| tops.get(self.visible_row_count).or_else(|| tops.last()).copied().unwrap_or(0.0))
    }
}

pub(super) fn clamped_window(
    first_index: usize,
    requested_visible_row_count: usize,
    item_count: usize,
) -> VisibleRowWindow {
    let visible_row_count = requested_visible_row_count.min(item_count);
    let max_first_index = item_count.saturating_sub(visible_row_count);
    let first_index = first_index.min(max_first_index);
    VisibleRowWindow { first_index, end_exclusive_index: first_index.saturating_add(visible_row_count).min(item_count) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::virtual_list::fixture::{
        WRAPPING_NOTE, config_list, list, measured_list, noted, row_plates, table_list,
    };
    use crate::{VirtualListConfig, WidgetDrawItem, WidgetFrame};
    use alloc::format;
    use alloc::vec::Vec;

    #[test]
    fn window_clamps_zero_one_beginning_middle_and_tail() {
        assert_eq!(clamped_window(0, 5, 0), VisibleRowWindow { first_index: 0, end_exclusive_index: 0 });
        assert_eq!(clamped_window(8, 0, 10), VisibleRowWindow { first_index: 8, end_exclusive_index: 8 });
        assert_eq!(clamped_window(8, 1, 10), VisibleRowWindow { first_index: 8, end_exclusive_index: 9 });
        assert_eq!(clamped_window(0, 5, 100), VisibleRowWindow { first_index: 0, end_exclusive_index: 5 });
        assert_eq!(clamped_window(40, 5, 100), VisibleRowWindow { first_index: 40, end_exclusive_index: 45 });
        assert_eq!(clamped_window(99, 5, 100), VisibleRowWindow { first_index: 95, end_exclusive_index: 100 });
        assert_eq!(
            clamped_window(usize::MAX, usize::MAX, 100),
            VisibleRowWindow { first_index: 0, end_exclusive_index: 100 }
        );
    }

    #[test]
    fn every_window_is_bounded_and_has_at_most_the_requested_rows() {
        for item_count in 0..32 {
            for requested in 0..12 {
                for first_index in 0..40 {
                    let window = clamped_window(first_index, requested, item_count);
                    assert!(window.first_index <= window.end_exclusive_index);
                    assert!(window.end_exclusive_index <= item_count);
                    assert!(window.len() <= requested);
                    assert_eq!(window.len(), requested.min(item_count));
                }
            }
        }
    }

    #[test]
    fn row_hit_uses_realized_rows_and_rejects_invalid_or_exclusive_bottom() {
        let mut widget = list(200, 5, 0);
        assert_eq!(widget.row_at_local_y(0.0), Some(0));
        assert_eq!(widget.row_at_local_y(23.999), Some(0));
        assert_eq!(widget.row_at_local_y(24.0), Some(1));
        assert_eq!(widget.row_at_local_y(119.999), Some(4));
        assert_eq!(widget.row_at_local_y(120.0), None);
        assert_eq!(widget.row_at_local_y(-0.1), None);
        assert_eq!(widget.row_at_local_y(f32::NAN), None);
        widget.frame.height = f32::INFINITY;
        assert_eq!(widget.row_at_local_y(0.0), None);

        let short = list(2, 5, 0);
        assert_eq!(short.row_at_local_y(23.999), Some(0));
        assert_eq!(short.row_at_local_y(24.0), Some(1));
        assert_eq!(short.row_at_local_y(48.0), None, "the empty viewport under the last item is not a row");
    }

    #[test]
    fn a_short_list_keeps_its_rows_one_configured_row_tall() {
        // Tripwire: row height divides the viewport by the *configured* row
        // count. Dividing by the realized count stretched a two-item list over
        // its whole frame — two slabs, and a selected row half the viewport
        // high, which is what a short list looked like in the studio.
        let widget = list(2, 5, 1);
        let items = widget.draw_items();
        let quads: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { y, height, color, .. } => Some((*y, *height, *color)),
                WidgetDrawItem::Text { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect();

        assert_eq!(quads.len(), 2, "two items realize two row quads and no filler for the empty viewport");
        assert_eq!(quads[0], (0.0, 24.0, widget.theme.surface_raised));
        assert_eq!(quads[1], (24.0, 24.0, widget.theme.selection), "the selected row is one row high");
    }

    #[test]
    fn the_last_row_of_a_table_lands_inside_the_frame_however_the_frame_divides_its_rows() {
        // Tripwire: round-17 note 1, verbatim — "On defense extended stats
        // cannot scroll to bottom" (the studio's gap 41a). The window starts
        // on a row's own top, so the last window has to be the first one that
        // reaches the content's end. Chosen as the *last* row starting at or
        // before that end instead, the window stops short by up to a row and
        // the final statistic hangs below the frame's edge with nothing left
        // to roll — invisible on any frame that happens to be an exact prefix
        // sum of its rows, which a plate capped by a pane's height never is.
        let items = alloc::vec![
            noted("Armour", "the share of a hit of the size this fight expects that this takes off it"),
            VirtualListRow::from("Evasion").with_trailing(vec!["1240".into()]),
            noted("Fire resistance", "the lines come to -60%, with no headroom over the maximum at all"),
            VirtualListRow::from("Stun threshold").with_space_before(3).with_rule_above(),
            noted("Block", "nothing on this build blocks, so the chance is the character's own"),
        ];
        let mut widget = table_list(items, 5);
        let tops = widget.row_tops.clone().expect("a table keeps an offset table");

        // A frame that ends half way down the second row's slot: the content's
        // end lands strictly inside a row rather than on one's top edge.
        let half_row = (tops[2] - tops[1]) * 0.5;
        widget.frame = WidgetFrame { height: tops.last().expect("content") - tops[1] - half_row, ..widget.frame };
        widget.refresh_row_layout();
        let tops = widget.row_tops.clone().expect("a table keeps an offset table");
        let last_top = widget.last_window_top();
        assert!(
            last_top > tops[1] && last_top < tops[2],
            "the frame is not an exact prefix sum of the rows: {last_top} between {} and {}",
            tops[1],
            tops[2],
        );

        widget.scroll_to(usize::MAX);
        let last_index = widget.items.len() - 1;
        assert_eq!(widget.window().end_exclusive_index, widget.items.len(), "the last window realizes the last row");
        let bands = widget.row_bands(last_index).expect("the last row is realized at the end of the vector");
        assert!(
            bands.plate_top + bands.plate_height <= widget.frame.height + 1e-3,
            "the last row's bottom is inside the frame: {} of {}",
            bands.plate_top + bands.plate_height,
            widget.frame.height,
        );

        // And the bar reaches the same place: a thumb dragged to the bottom of
        // its track means the end of the content, not the row that happens to
        // start before it.
        widget.first_index = 0;
        let bar = widget.scroll_bar().expect("a table past its frame stands a bar");
        widget.press_scroll_bar(bar, bar.height);
        assert_eq!(widget.first_index, widget.max_first_index(), "the thumb's own end is the list's end");
    }

    #[test]
    fn a_fixed_pitch_list_still_ends_on_its_last_row_exactly() {
        // Tripwire: the fast path divides the frame by the configured row
        // count, so its viewport is a whole number of rows and its last window
        // is the item count less that — rounding it the way a table's is
        // rounded would scroll one row past the end and draw a blank strip
        // under the last row.
        let mut widget = measured_list(200, 5);
        widget.scroll_to(usize::MAX);
        assert_eq!(widget.first_index, 195);
        assert_eq!(widget.window().end_exclusive_index, 200, "and the window ends on the last item");
        let bands = widget.row_bands(199).expect("the last row is realized");
        assert!(
            (bands.plate_top + bands.plate_height - widget.frame.height).abs() < 1e-3,
            "its bottom is the frame's own bottom: {}",
            bands.plate_top + bands.plate_height,
        );
    }

    #[test]
    fn the_content_height_is_the_span_the_table_scrolls_through_rather_than_a_pitch_by_rows() {
        // Tripwire: the studio's gap 41. A host draws the plate under a list
        // from this number and the list scrolls through the span, so the
        // two have to be the one number — a content height re-derived as
        // pitch × rows under-measures every table whose rows carry a note or
        // open a block, and the plate is then cut short of its own last rows
        // while the list happily scrolls to them.
        let items = alloc::vec![
            VirtualListRow::from("Armour").with_trailing(vec!["1240".into()]),
            noted(
                "Physical damage mitigated",
                "the share of a hit of the size this fight expects that the armour value above takes off",
            ),
            VirtualListRow::from("Resistances").with_space_before(3).with_rule_above(),
        ];
        let widget = table_list(items, 5);

        assert_eq!(
            widget.content_height(),
            Some(widget.scroll_span().content),
            "the plate is drawn to the height the list scrolls through",
        );
        assert!(
            widget.scroll_span().content > widget.theme.row_height * 3.0,
            "and a table of notes and block gaps stands taller than three rows of pitch",
        );

        let plain = measured_list(200, 5);
        assert_eq!(plain.scroll_span().content, 200.0, "a fixed-pitch list counts its span in rows");
        assert_eq!(
            plain.content_height(),
            Some(plain.theme.row_height * 200.0),
            "and reports the same content in the pixels a host draws with",
        );

        // A frame the host did not size to the intrinsic: the five rows are
        // drawn to it, so the vector stands taller than the theme's pitch by
        // rows and a plate drawn to that number would be cut short of them.
        let mut off_pitch = measured_list(20, 5);
        off_pitch.frame.height = 200.0;
        assert_eq!(off_pitch.row_height(), Some(40.0), "a taller frame draws its five rows taller");
        assert_eq!(
            off_pitch.content_height(),
            Some(40.0 * 20.0),
            "and the content height follows the pitch the rows are drawn at, not the theme's",
        );

        let mut unwrapped = list(1, 5, 0);
        unwrapped.items = alloc::vec![noted("Armour", "a sentence long enough to wrap onto a second line")];
        unwrapped.rows_vary = true;
        unwrapped.refresh_row_layout();
        assert_eq!(
            unwrapped.content_height(),
            None,
            "a table whose notes have not been wrapped against real advances says nothing rather than a number it is about to change",
        );
    }

    #[test]
    fn a_table_opens_with_the_row_the_host_selected_realized() {
        // Tripwire: `init` has no frame, so it counts the boot window in rows
        // at the one pitch — the only arithmetic there is before anything is
        // measured. A table's rows are not that pitch: a row carrying a note
        // stands taller, so the window that count picks realizes fewer rows
        // than it counted and the selected row falls out of the bottom of it.
        // The list then opens on a table with no highlight anywhere in it and
        // stays that way until the reader scrolls or the host resends the
        // config.
        let mut widget = config_list(VirtualListConfig {
            items: (0..50).map(|index| noted(&format!("stat {index}"), "a sentence under the statistic")).collect(),
            initial_selected_index: Some(40),
            visible_row_count: 5,
            ..VirtualListConfig::default()
        });
        assert_eq!(widget.first_index, 36, "the boot window is five rows of pitch ending on the selection");

        widget.refresh_row_layout();

        let window = widget.window();
        assert!(
            (window.first_index..window.end_exclusive_index).contains(&40),
            "the selected row stands in the realized window {window:?} rather than below it",
        );
    }

    #[test]
    fn a_point_inside_a_tall_row_names_that_row_rather_than_the_one_a_pitch_would_name() {
        // Tripwire: every hit the list answers used to be `local_y / pitch`,
        // which names the right row only while every row is one height. At a
        // 24-pixel pitch the point 40 pixels down is row 1; in this list row 0
        // is 55 pixels tall and 40 is still inside it. A press resolving to
        // the wrong row selects the wrong entry, and the same arithmetic backs
        // the reported hover.
        let mut widget = table_list(alloc::vec![noted("Armour", WRAPPING_NOTE), VirtualListRow::from("Evasion")], 5);
        assert_eq!(widget.row_at_local_y(40.0), Some(0), "40 is inside the noted row");
        assert_eq!(widget.row_at_local_y(60.0), Some(1), "and 60 is past it");

        widget.pointer_local = Some((widget.theme.pad, 40.0));
        assert_eq!(widget.pointer_row(), Some(0), "so the hover the host is told about is the row the pointer is on");
    }

    #[test]
    fn a_vector_that_asks_for_no_height_of_its_own_keeps_the_pitch_the_frame_divides_into() {
        // Tripwire: the fast path. Every list written before rows had heights
        // draws at `frame.height / visible_row_count` with no table kept, and
        // a change that walked heights for all of them would both re-lay every
        // existing list and spend one `f32` per item on vectors that never
        // asked for it. This pins the geometry and the absence of the table.
        let mut widget = measured_list(9, 5);
        widget.refresh_row_layout();
        assert!(widget.row_tops.is_none(), "a plain vector keeps no offset table");

        let pitch = widget.row_height().expect("a laid-out plain list has one pitch");
        assert!((pitch - widget.frame.height / 5.0).abs() < f32::EPSILON, "which is the frame divided by the count");
        for (offset, plate) in row_plates(&widget).into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let expected_y = offset as f32 * pitch;
            assert!((plate.1 - expected_y).abs() < f32::EPSILON, "row {offset} stands at {}", plate.1);
            assert!((plate.3 - pitch).abs() < f32::EPSILON, "row {offset} is one pitch tall");
        }

        let span = widget.scroll_span();
        assert_eq!(
            (span.offset, span.viewport, span.content),
            (0.0, 5.0, 9.0),
            "and the bar is still drawn from the three counts",
        );
    }
}
