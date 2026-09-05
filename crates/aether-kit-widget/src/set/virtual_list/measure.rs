//! What the list measures: a note wrapped to the lines it takes, a name
//! elided to the width it has, the two right-hand columns the rows share, and
//! the intrinsic the whole vector asks a layout for.

use alloc::string::String;
use alloc::vec::Vec;

use crate::set::virtual_list::VirtualListWidget;
use crate::set::virtual_list::rows::VisibleRowWindow;
use crate::set::{elide_to_width, measured_text_width, wrap_to_width};
use crate::theme::TextRole;
use crate::{InkedSpan, VirtualListRow};

/// How much clear space stands between a row's leading run and its trailing
/// column, in spacing units. One — enough that the two read as two columns,
/// little enough that a short name and its amount still read as one row.
const TRAILING_GAP_UNITS: u8 = 1;

/// The theme's word gap — how much clear space stands between one span of a
/// trailing run and the next, in spacing units. One: the spans are words on
/// one line, so they are spaced like words rather than like columns.
pub(super) const TRAILING_SPAN_GAP_UNITS: u8 = 1;

/// How far a note is set in past its own row's indent, in spacing units. One:
/// enough that the sentence reads as hanging off the name above it, little
/// enough that it stays inside the same entry.
const NOTE_INDENT_UNITS: u8 = 1;

/// A note line's height as a multiple of the caption size. Tighter than a
/// row's pitch, because the lines of one note are one paragraph and the space
/// between them is leading rather than a gap between rows.
const NOTE_LINE_HEIGHT_RATIO: f32 = 1.3;

/// The most lines one row's note may take. Three: a note is a sentence about
/// the row above it, and a row that grows past three lines has stopped being
/// an entry in a table and become a paragraph the host should draw itself.
/// Past the cap the third line carries what is left of the sentence, elided
/// with an [`ELLIPSIS`](crate::set::ELLIPSIS), so the cut says it is a cut.
const MAX_NOTE_LINES: usize = 3;

impl VirtualListWidget {
    /// Drop the cached row measurement and the offset table. Called wherever
    /// the items, the font, or the type scale change, which is every input
    /// either of them has.
    pub(super) fn forget_measurements(&mut self) {
        self.widest_row_width = None;
        self.row_tops_frame = None;
    }

    /// How tall one line of a note is: the caption size by
    /// [`NOTE_LINE_HEIGHT_RATIO`].
    pub(super) fn note_line_height(&self) -> f32 {
        (self.theme.text_size_pixels(TextRole::Caption) * NOTE_LINE_HEIGHT_RATIO).max(0.0)
    }

    /// How far into the row's text budget a note starts: the row's own indent
    /// and one unit more.
    pub(super) fn note_indent(&self, row: &VirtualListRow) -> f32 {
        self.theme.space(row.indent.saturating_add(NOTE_INDENT_UNITS))
    }

    /// The note this row has to say. A note of nothing but space is not a
    /// note: it would grow the row by a line that draws nothing.
    fn note_of(row: &VirtualListRow) -> Option<&str> {
        row.note.as_deref().map(str::trim).filter(|note| !note.is_empty())
    }

    /// The width a note wraps to in a row `row_width` wide: the row's text
    /// budget less the note's own indent. A note runs the whole budget,
    /// because the trailing column and the verbs stand on the row's *first*
    /// line and the note is the line under them.
    pub(super) fn note_budget(&self, row: &VirtualListRow, row_width: f32) -> f32 {
        (text_budget_of(row_width, self.theme.pad) - self.note_indent(row)).max(0.0)
    }

    /// One row's note as the lines it will be drawn on: word-wrapped to
    /// `budget`, capped at [`MAX_NOTE_LINES`], with the last of those carrying
    /// what is left of the sentence and eliding it.
    ///
    /// A row that has a note always gets at least one line — a single word
    /// wider than the budget keeps its own line rather than being broken in
    /// half — and the whole note stands on one line until the font's advances
    /// land, because wrapping against a guess and again against the metrics
    /// would change every row's height a frame after it drew.
    pub(super) fn note_lines(&self, row: &VirtualListRow, budget: f32) -> Vec<String> {
        let Some(note) = Self::note_of(row) else {
            return Vec::new();
        };
        let size = self.theme.text_size_pixels(TextRole::Caption);
        let Some(metrics) = self.font_metrics.resolved() else {
            return alloc::vec![String::from(note)];
        };
        let mut lines = wrap_to_width(note, budget, |run| measured_text_width(metrics, run, size));
        if lines.len() > MAX_NOTE_LINES {
            let mut rest = String::new();
            for line in lines.split_off(MAX_NOTE_LINES - 1) {
                if !rest.is_empty() {
                    rest.push(' ');
                }
                rest.push_str(&line);
            }
            lines.push(self.fitted_text(&rest, size, budget));
        }
        lines
    }

    /// The width a row's two columns share: the row they are drawn in, less
    /// one `pad` at each end, so nothing in a row touches either edge of the
    /// space it was given.
    pub(super) fn text_width_budget(&self) -> f32 {
        text_budget_of(self.row_width(), self.theme.pad)
    }

    /// The width the *leading* run has once the row's right-hand furniture is
    /// reserved: the row's budget less the verb block, the trailing column, and
    /// one spacing unit of clear space before each. A window with neither
    /// reserves nothing, so an ordinary list is laid out exactly as it was.
    ///
    /// This is why the leading elides and nothing else does: both right-hand
    /// columns are subtracted *first* and the name takes what is left. An
    /// amount cut to `12…` is worse than no amount at all, a verb cut to `Rem…`
    /// is a control nobody can read, while a name cut to `Increased Critic…`
    /// still names the thing.
    pub(super) fn leading_width_budget(&self, trailing_column: f32, actions_reserve: f32) -> f32 {
        let reserved = if trailing_column > 0.0 {
            trailing_column + self.theme.space(TRAILING_GAP_UNITS)
        } else {
            0.0
        };
        (self.text_width_budget() - reserved - actions_reserve).max(0.0)
    }

    /// The width `spans` occupy on one line at `size`: each span measured, plus
    /// the theme's word gap between each pair. `0.0` for an empty run and while
    /// the font's advances are still in flight.
    pub(super) fn spans_width(&self, spans: &[InkedSpan], size: f32) -> f32 {
        let (Some(metrics), Some(pair_count)) = (self.font_metrics.resolved(), spans.len().checked_sub(1)) else {
            return 0.0;
        };
        #[allow(clippy::cast_precision_loss)] // a trailing run is a few words on one line
        let gaps = pair_count as f32 * self.theme.space(TRAILING_SPAN_GAP_UNITS);
        spans.iter().map(|span| measured_text_width(metrics, &span.text, size)).sum::<f32>() + gaps
    }

    /// The prefix of one row's trailing run that fits `budget`: whole spans
    /// dropped off its **end** until what is left fits.
    ///
    /// A span is a word that means something on its own — `Fire`, `21/20` — so
    /// half of one is worse than none of it. The run gives way by dropping tags
    /// off the end rather than by cutting one mid-word, which keeps the
    /// ellipsis on the leading run the only cut mark a row carries.
    ///
    /// The **head span always stays**, even when it alone is wider than the
    /// budget: the column exists for the fact in it, so a row whose one amount
    /// is wider than the row shows the amount and gives the name nothing,
    /// rather than showing an empty column and a full-width name.
    pub(super) fn fitted_trailing<'row>(&self, row: &'row VirtualListRow, budget: f32) -> &'row [InkedSpan] {
        let size = self.theme.text_size_pixels(row.role);
        let mut spans = row.trailing.as_slice();
        while spans.len() > 1 && self.spans_width(spans, size) > budget {
            spans = &spans[..spans.len() - 1];
        }
        spans
    }

    /// The widest a trailing column may be: the row's text budget less the verb
    /// block. A run of tags wider than the row it is in is not a column, and it
    /// is what gives way rather than the leading run being squeezed to nothing.
    pub(super) fn trailing_budget(&self, actions_reserve: f32) -> f32 {
        (self.text_width_budget() - actions_reserve).max(0.0)
    }

    /// One row's trailing run once it has been fitted to `budget`, or `0.0` for
    /// a row without one and while the font's advances are still in flight.
    fn trailing_width(&self, row: &VirtualListRow, budget: f32) -> f32 {
        self.spans_width(self.fitted_trailing(row, budget), self.theme.text_size_pixels(row.role))
    }

    /// The trailing column this window's rows share: the widest trailing run
    /// among the rows **on screen**. One column for the realized window rather
    /// than for the whole vector, because the reader compares what they can
    /// see — and a column sized by an off-screen row would leave a visible gap
    /// nothing stands in. `0.0` when no realized row has a trailing run, which
    /// is the ordinary single-column list.
    pub(super) fn trailing_column(&self, window: VisibleRowWindow, budget: f32) -> f32 {
        self.items[window.first_index..window.end_exclusive_index]
            .iter()
            .map(|row| self.trailing_width(row, budget))
            .fold(0.0_f32, f32::max)
    }

    /// One line as it will be drawn: elided to the row's own width with an
    /// [`ELLIPSIS`](crate::set::ELLIPSIS) once the theme font's metrics
    /// resolve, whole before that. The slot clip still bounds the row either
    /// way — this is what stops the clip from being the *first* thing that
    /// cuts, because a hard clip cuts mid-glyph and an ellipsis says a name
    /// was too long.
    pub(super) fn fitted_text(&self, text: &str, size_pixels: f32, budget: f32) -> String {
        self.font_metrics.resolved().map_or_else(
            || String::from(text),
            |metrics| elide_to_width(text, budget, |run| measured_text_width(metrics, run, size_pixels)),
        )
    }

    /// The `[width, height]` this list asks a layout for: the widest row it
    /// holds plus one `pad` either side and the scroll bar's track when the
    /// vector overflows, by the configured row height times the configured
    /// viewport. `None` until the font's metrics resolve, and for a list with
    /// no rows to measure — a slot sized from a guess would resize the moment
    /// the real advances landed.
    ///
    /// The gutter is counted whenever the vector overflows rather than only
    /// once a frame exists to hang a bar on: the intrinsic is what *makes* the
    /// frame, so a width that ignored the bar would size a slot the bar then
    /// took a gutter's worth of text out of.
    pub(super) fn intrinsic(&mut self) -> Option<[f32; 2]> {
        let widest = self.widest_row_width()?;
        let height = self.viewport_height();
        let gutter = if self.visible_row_count > 0 && self.items.len() > self.visible_row_count {
            self.bar_reserve_width()
        } else {
            0.0
        };
        let width = self.theme.pad.mul_add(2.0, widest) + gutter;
        (width.is_finite() && height.is_finite()).then_some([width, height])
    }

    /// The widest row in the whole item vector, measured once per change to
    /// the items or the font and cached. A row with a trailing run or a verb on
    /// it is as wide as all of its columns and the gaps between them: a slot
    /// sized from this has to hold the whole row, not just its name.
    ///
    /// A row's **note is not measured**. A note is prose, and prose sized to
    /// its own longest line opens a pane at its ceiling on a sentence and
    /// leaves the table it was supposed to size sitting in a column of empty
    /// plate; the note wraps to whatever width the *rows* ask for. A row's
    /// indent is counted, because that is width the name actually needs.
    fn widest_row_width(&mut self) -> Option<f32> {
        if let Some(widest) = self.widest_row_width {
            return Some(widest);
        }
        let metrics = self.font_metrics.resolved()?;
        if self.items.is_empty() {
            return None;
        }
        let gap = self.theme.space(TRAILING_GAP_UNITS);
        let widest = self
            .items
            .iter()
            .map(|row| {
                let size = self.theme.text_size_pixels(row.role);
                let trailing = match self.spans_width(&row.trailing, size) {
                    run if run > 0.0 => gap + run,
                    _ => 0.0,
                };
                self.theme.space(row.indent)
                    + measured_text_width(metrics, &row.text, size)
                    + trailing
                    + self.actions_reserve_for(row)
            })
            .fold(0.0_f32, f32::max);
        self.widest_row_width = Some(widest);
        Some(widest)
    }
}

/// The width the two text columns of a row `row_width` wide share: the row
/// less one `pad` at each end. Free of the widget because the offset table is
/// built at a width the list does not have yet — the gutter it will give up
/// depends on the heights the table is being built to find.
fn text_budget_of(row_width: f32, pad: f32) -> f32 {
    pad.mul_add(-2.0, row_width).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::ELLIPSIS;
    use crate::set::virtual_list::fixture::{
        WRAPPING_NOTE, drawn_runs, list, measured_list, noted, placed_runs, row_plates, row_text, table_list,
    };
    use alloc::string::String;
    use alloc::vec;

    #[test]
    fn a_row_too_long_for_the_frame_is_elided_and_only_once_the_font_resolves() {
        // Tripwire: an unmeasured row must draw whole — the slot clip is the
        // fallback, and eliding against a guessed advance would cut a name
        // that fits. Once the advances land the row is cut to the frame less
        // one pad each side, with the mark inside that budget.
        let long = String::from("a name far too long for this narrow list");
        let mut unmeasured = list(1, 5, 0);
        unmeasured.items = alloc::vec![VirtualListRow::from(long.clone())];
        assert_eq!(row_text(&unmeasured), alloc::vec![long.clone()], "no metrics, no elision");

        let mut widget = measured_list(1, 5);
        widget.items = alloc::vec![VirtualListRow::from(long)];
        let drawn = row_text(&widget);
        assert!(drawn[0].ends_with(ELLIPSIS), "the cut row says it was cut: {drawn:?}");
        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        assert!(
            measured_text_width(metrics, &drawn[0], size) <= widget.text_width_budget(),
            "and the mark is inside the budget, not appended past it: {drawn:?}",
        );
    }

    #[test]
    fn one_trailing_column_serves_every_visible_row_and_only_the_name_elides() {
        // Tripwire: two failures the second column exists to prevent. If each
        // row placed its own trailing run against the right pad, the two would
        // still line up — but the *leading* budget would differ per row, so the
        // names would elide at ragged points; and if the trailing were elided
        // like the leading, a reader would get `21/…`, which is a wrong number
        // rather than a shortened one. The column is the widest trailing among
        // the visible rows, subtracted from every row's leading budget first.
        let mut widget = measured_list(2, 5);
        widget.items = vec![
            VirtualListRow::from("a gem name far too long for this narrow list").with_trailing(vec!["21/20".into()]),
            VirtualListRow::from("short").with_trailing(vec!["1".into()]),
        ];
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let (column, wide_width, narrow_width) = {
            let metrics = widget.font_metrics.resolved().expect("the test table is installed");
            let wide = measured_text_width(metrics, "21/20", size);
            (wide, wide, measured_text_width(metrics, "1", size))
        };
        let budget = widget.trailing_budget(0.0);
        assert!(narrow_width < wide_width, "the two trailing runs are different widths");
        assert_eq!(widget.trailing_column(widget.window(), budget), column, "the widest of them is the column");

        let runs = drawn_runs(&widget);
        assert_eq!(runs.len(), 4, "two rows, two runs each: {runs:?}");
        assert_eq!(runs[1].1, "21/20");
        assert_eq!(runs[3].1, "1", "the narrow amount is drawn whole, not padded and not cut");
        let right_edge = widget.row_width() - widget.theme.pad;
        assert!((runs[1].0 + wide_width - right_edge).abs() < f32::EPSILON, "the wide amount ends at the right pad");
        assert!((runs[3].0 + narrow_width - right_edge).abs() < f32::EPSILON, "and so does the narrow one");

        assert!(runs[0].1.ends_with(ELLIPSIS), "the name gave way: {:?}", runs[0].1);
        let leading = widget.leading_width_budget(column, 0.0);
        assert_eq!(leading, widget.text_width_budget() - column - widget.theme.space(TRAILING_GAP_UNITS));
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        assert!(measured_text_width(metrics, &runs[0].1, size) <= leading, "and stopped clear of the column");
    }

    #[test]
    fn a_trailing_run_too_wide_for_its_row_drops_whole_spans_off_its_end() {
        // Tripwire: elided the way the leading run is, a run of tags answers
        // `Fire Cold Light…` — half a tag, which names nothing — and drawn
        // unfitted it runs out under the name and off the row's own edge. A
        // span is a word that means something on its own, so the run gives way
        // by whole spans from its end and the leading run's ellipsis stays the
        // one cut mark a row carries.
        let mut widget = measured_list(1, 1);
        widget.items = vec![VirtualListRow::from("Fireball").with_trailing(vec![
            "Fire".into(),
            "Cold".into(),
            "Lightning".into(),
            "Chaos".into(),
        ])];
        widget.forget_measurements();

        let budget = widget.trailing_budget(0.0);
        let fitted = widget.fitted_trailing(&widget.items[0], budget);
        assert!(!fitted.is_empty(), "the head tag stays whatever the budget is");
        assert!(fitted.len() < 4, "this row is too narrow for the whole run: {fitted:?}");
        assert_eq!(
            fitted,
            &widget.items[0].trailing[..fitted.len()],
            "what is left is the run's own head, in order and whole",
        );
        assert!(
            widget.spans_width(fitted, widget.theme.label_size_pixels) <= budget,
            "and it fits the column it was fitted to",
        );

        let runs = drawn_runs(&widget);
        assert_eq!(runs.len(), fitted.len() + 1, "the dropped tags are not drawn at all: {runs:?}");
        assert!(runs[1..].iter().all(|(_, run)| !run.contains(ELLIPSIS)), "no tag was cut mid-word: {runs:?}");
    }

    #[test]
    fn the_intrinsic_width_is_the_widest_row_in_the_whole_vector_plus_a_pad_each_side() {
        // Tripwire: the intrinsic must measure the *items*, not the realized
        // window — a width that changed as the reader scrolled would resize
        // the column under them. It is also the one thing here that touches
        // every item, so it is cached until an input to it changes.
        let mut widget = measured_list(40, 5);
        widget.items[17] = VirtualListRow::from("the widest row of them all");
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let expected = {
            let metrics = widget.font_metrics.resolved().expect("the test table is installed");
            widget.theme.pad.mul_add(2.0, measured_text_width(metrics, &widget.items[17].text, size))
                + widget.track_width()
                + widget.scroll_bar_gap()
        };
        let [width, height] = widget.intrinsic().expect("a measured, non-empty list reports an intrinsic");
        assert!((width - expected).abs() < f32::EPSILON, "{width} is not the widest row plus a pad each side");
        assert_eq!(height, widget.theme.row_height * 5.0, "the height is the configured viewport, not the item count");

        widget.first_index = 30;
        assert_eq!(widget.intrinsic().map(|size| size[0]), Some(width), "scrolling past the widest row keeps it");

        assert_eq!(list(40, 5, 0).intrinsic(), None, "an unmeasured list asks for nothing");
        assert_eq!(measured_list(0, 5).intrinsic(), None, "and neither does one with no rows to measure");
    }

    #[test]
    fn a_note_is_a_second_line_of_its_row_and_the_row_grows_by_the_lines_it_took() {
        // Tripwire: the studio's gap 38. A note pushed into the vector as a
        // row of its own reads as a statistic whose value failed to draw, and
        // a note drawn on a row whose height still came from
        // `frame.height / visible_row_count` would print over the row beneath
        // it. The row has to *grow*, and the growth has to reach the offset
        // table the next row's top is read from.
        let widget = table_list(alloc::vec![noted("Armour", WRAPPING_NOTE), VirtualListRow::from("Evasion")], 5);

        let lines: Vec<String> = placed_runs(&widget)
            .into_iter()
            .filter(|(_, _, _, size)| (*size - widget.theme.caption_size_pixels).abs() < f32::EPSILON)
            .map(|(text, _, _, _)| text)
            .collect();
        assert_eq!(lines.len(), 2, "the note wrapped onto two lines: {lines:?}");
        assert!(lines[0].starts_with("armour is"), "and it broke between words: {lines:?}");
        assert_eq!(lines.concat().replace(' ', ""), WRAPPING_NOTE.replace(' ', ""), "nothing was cut");

        // Body pitch 24, two caption lines at 12 × 1.3: 24 + 31.2.
        let grown = widget.content_top(1) - widget.content_top(0);
        assert!((grown - 55.2).abs() < 1e-3, "the noted row stands {grown} tall");
        assert!(
            (widget.content_top(2) - widget.content_top(1) - 24.0).abs() < 1e-3,
            "and the row under it is the plain body pitch again",
        );

        let plates = row_plates(&widget);
        assert!((plates[1].1 - grown).abs() < 1e-3, "the next row starts below the note, not over it: {plates:?}");
    }

    #[test]
    fn a_note_past_its_cap_ends_on_an_ellipsis_rather_than_growing_the_row_without_end() {
        // Tripwire: a row is an entry in a table, and prose let to wrap
        // forever turns one entry into a paragraph that pushes every other row
        // off the viewport. The cap is three lines and the third says it was
        // cut, which is what stops a note from silently losing its tail.
        let long = "a monster's spells cannot be evaded and nor can a boss attack the game flashes red before it lands";
        let widget = table_list(alloc::vec![noted("Evasion", long)], 5);

        let lines: Vec<String> = placed_runs(&widget)
            .into_iter()
            .filter(|(_, _, _, size)| (*size - widget.theme.caption_size_pixels).abs() < f32::EPSILON)
            .map(|(text, _, _, _)| text)
            .collect();
        assert_eq!(lines.len(), MAX_NOTE_LINES, "three lines and no more: {lines:?}");
        assert!(lines[MAX_NOTE_LINES - 1].ends_with(ELLIPSIS), "and the last says it was cut: {lines:?}");
    }
}
