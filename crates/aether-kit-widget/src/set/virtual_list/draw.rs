//! The draw list a `Collect` replies with: the fill each row wears, the ink
//! each of its runs is set in, the rules between and above rows, the note
//! under a name, and the empty state a list with nothing in it says instead.

use alloc::string::String;
use alloc::vec::Vec;

use aether_math::Rgba;

use crate::set::virtual_list::VirtualListWidget;
use crate::set::virtual_list::measure::TRAILING_SPAN_GAP_UNITS;
use crate::set::virtual_list::rows::{RowBands, VisibleRowWindow};
use crate::set::virtual_list::valid_frame;
use crate::set::{measured_text_width, push_control_outlines, quad, text_origin_y};
use crate::theme::{TextInk, TextRole, ThemeState};
use crate::{VirtualListRow, WidgetDrawItem};

/// How thick the rule between two rows of a `ruled` list is. A hairline: the
/// rule is there to separate entries, and anything heavier reads as a table
/// border the rows are trapped in.
pub(super) const ROW_RULE_THICKNESS: f32 = 1.0;

impl VirtualListWidget {
    /// The fill one row draws, from the two facts that can be true of it.
    ///
    /// Four faces, and the ladder between them is the point. A row the pointer
    /// is on takes the kit's role-agnostic hover wash over the plain surface —
    /// the same face a dropdown's open list has always drawn under the pointer,
    /// so the two lists answer a pointer alike. A chosen row is the selection
    /// role, a *state* rather than a wash. Chosen **and** pointed at composes
    /// the two: the selection carrying that same hover.
    ///
    /// Before this the widget-wide hover flag lit the *selected* row wherever
    /// in the list the pointer was, so pointing at the fourth gem lit the
    /// first — the owner's round-11 note 13, "the current behavior only has
    /// the selected element being activated when hovering over ANY item".
    pub(super) fn row_fill(&self, selected: bool, hovered: bool) -> Rgba {
        let base = match (selected, hovered) {
            (true, _) => self.theme.selection,
            (false, true) => self.theme.fill(self.theme.surface_raised, ThemeState::Hover),
            (false, false) => self.theme.surface_raised,
        };
        let state = match self.state.supporting_theme_state(selected && self.pressed) {
            ThemeState::Normal if selected && hovered => ThemeState::Hover,
            state => state,
        };
        self.theme.fill(base, state)
    }

    /// The ink one run of a row is set in.
    ///
    /// A run with no ink of its own follows the row: `selection_text` on the
    /// chosen row, the muted ink at [`TextRole::Caption`] — a caption row is a
    /// quieter detail line and draws exactly as a caption-role label does —
    /// and the primary ink otherwise. A run that **names** an ink keeps it on
    /// the chosen row too: a name is written in its tier's colour because that
    /// is what the tier is, and a tier that disappears the moment the reader
    /// clicks the row is a tier the reader cannot compare.
    fn run_ink(&self, ink: TextInk, row: &VirtualListRow, selected: bool) -> Rgba {
        self.ink_at(ink, row.role, selected)
    }

    /// [`Self::run_ink`] for a run set at a role of its own rather than at its
    /// row's — which is a row's note, always a caption whatever the name above
    /// it is set at.
    fn ink_at(&self, ink: TextInk, role: TextRole, selected: bool) -> Rgba {
        let base = match ink {
            TextInk::Inherited if selected => self.theme.selection_text,
            ink => self.theme.text_ink(ink, role),
        };
        self.theme.fill(base, self.state.supporting_theme_state(false))
    }

    /// The hairlines standing between the realized rows of a `ruled` list —
    /// `n - 1` of them for `n` rows, each on the row boundary it divides. A
    /// rule under the last row would underline the list rather than separate
    /// anything, and one above the first would be a second top edge.
    fn rule_items(&self, window: VisibleRowWindow, row_width: f32) -> Vec<WidgetDrawItem> {
        if !self.ruled || window.len() < 2 {
            return Vec::new();
        }
        ((window.first_index + 1)..window.end_exclusive_index)
            .filter_map(|item_index| {
                let bands = self.row_bands(item_index)?;
                Some(quad(0.0, bands.slot_top, row_width, ROW_RULE_THICKNESS, self.theme.outline))
            })
            .collect()
    }

    /// The hairline one row draws to open a block: across the row's own text
    /// budget, at the **top of its space** — the rule first, then the ground,
    /// then the row — so a block boundary reads as a line with air under it
    /// rather than as a line stuck to a name. It spans the budget rather than
    /// the whole row so that it starts and ends where the text does, which is
    /// what tells it apart from the frame's own edge.
    fn rule_above_item(&self, row: &VirtualListRow, bands: RowBands) -> Option<WidgetDrawItem> {
        row.rule_above.then(|| {
            let width = self.text_width_budget();
            quad(self.theme.pad, bands.slot_top, width, ROW_RULE_THICKNESS, self.theme.outline)
        })
    }

    /// One row's note, drawn on the lines under its name: caption size, the
    /// muted ink — which follows the row into `selection_text` when it is
    /// chosen, exactly as a caption-role row's own text does — and set in by
    /// the row's indent and one unit more.
    fn push_row_note(&self, items: &mut Vec<WidgetDrawItem>, row: &VirtualListRow, bands: RowBands, selected: bool) {
        let size = self.theme.text_size_pixels(TextRole::Caption);
        let line_height = self.note_line_height();
        let indent = self.note_indent(row);
        for (line_offset, line) in self.note_lines(row, self.note_budget(row, self.row_width())).into_iter().enumerate()
        {
            #[allow(clippy::cast_precision_loss)] // a note is at most MAX_NOTE_LINES lines
            let line_top = line_height.mul_add(line_offset as f32, bands.plate_top + bands.line_height);
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad + indent,
                y: text_origin_y(line_top, line_height, size),
                font_id: self.theme.font_id,
                text: line,
                size_pixels: size,
                color: self.ink_at(TextInk::Inherited, TextRole::Caption, selected),
                clip: None,
            });
        }
    }

    /// The empty state: one caption-role, muted line at the top of the
    /// viewport. A list with nothing in it reads as told-you-so rather than as
    /// a control that failed to draw.
    fn empty_draw_items(&self) -> Vec<WidgetDrawItem> {
        if self.empty_text.is_empty() || !valid_frame(&self.frame) {
            return Vec::new();
        }
        let size = self.theme.text_size_pixels(TextRole::Caption);
        alloc::vec![WidgetDrawItem::Text {
            x: self.theme.pad,
            y: text_origin_y(0.0, self.theme.row_height.min(self.frame.height), size),
            font_id: self.theme.font_id,
            text: self.fitted_text(&self.empty_text, size, self.text_width_budget()),
            size_pixels: size,
            color: self.theme.fill(self.theme.text_muted, self.state.supporting_theme_state(false)),
            clip: None,
        }]
    }

    pub(super) fn draw_items(&self) -> Vec<WidgetDrawItem> {
        if !self.state.is_visible() {
            return Vec::new();
        }
        if self.items.is_empty() {
            return self.empty_draw_items();
        }
        let window = self.window();
        let visible_row_count = window.len();
        if visible_row_count == 0 || !valid_frame(&self.frame) {
            return Vec::new();
        }

        let row_width = self.row_width();
        let actions_reserve = self.actions_reserve(window);
        let trailing_budget = self.trailing_budget(actions_reserve);
        let trailing_column = self.trailing_column(window, trailing_budget);
        let leading_budget = self.leading_width_budget(trailing_column, actions_reserve);
        let mut items = Vec::with_capacity(visible_row_count.saturating_mul(3).saturating_add(8));
        for (row_offset, item) in self.items[window.first_index..window.end_exclusive_index].iter().enumerate() {
            let item_index = window.first_index + row_offset;
            let Some(bands) = self.row_bands(item_index) else {
                continue;
            };
            let selected = self.selected_index == Some(item_index);
            let hovered = self.hovered_row == Some(item_index);
            // The row's space is ground, not a taller plate: the fill starts
            // below it, so the gap between two blocks is the surface showing
            // through rather than one fat row.
            items.extend(self.rule_above_item(item, bands));
            items.push(quad(0.0, bands.plate_top, row_width, bands.plate_height, self.row_fill(selected, hovered)));

            let indent = self.theme.space(item.indent);
            let size = self.theme.text_size_pixels(item.role);
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad + indent,
                y: text_origin_y(bands.plate_top, bands.line_height, size),
                font_id: self.theme.font_id,
                text: self.fitted_text(&item.text, size, (leading_budget - indent).max(0.0)),
                size_pixels: size,
                color: self.run_ink(item.ink, item, selected),
                clip: None,
            });
            self.push_row_note(&mut items, item, bands, selected);
            // The trailing run is set flush against the row's right pad — or
            // against the verb block when one stands there — so every row's
            // second column ends on one edge. Its spans run left to right from
            // there, each in its own ink, one word gap apart.
            if trailing_column > 0.0
                && let Some(metrics) = self.font_metrics.resolved()
            {
                let fitted = self.fitted_trailing(item, trailing_budget);
                let mut x = row_width - self.theme.pad - actions_reserve - self.spans_width(fitted, size);
                for span in fitted {
                    items.push(WidgetDrawItem::Text {
                        x,
                        y: text_origin_y(bands.plate_top, bands.line_height, size),
                        font_id: self.theme.font_id,
                        text: String::from(span.text.as_str()),
                        size_pixels: size,
                        color: self.run_ink(span.ink, item, selected),
                        clip: None,
                    });
                    x += measured_text_width(metrics, &span.text, size) + self.theme.space(TRAILING_SPAN_GAP_UNITS);
                }
            }
            self.push_row_actions(&mut items, item, item_index, bands);
        }
        items.extend(self.rule_items(window, row_width));
        if let Some(bar) = self.scroll_bar() {
            items.extend(self.scroll_bar_items(bar));
        }
        push_control_outlines(&mut items, self.frame.width, self.frame.height, &self.state, &self.theme);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::virtual_list::fixture::{
        WRAPPING_NOTE, drawn_quads, drawn_runs, list, measured_list, noted, placed_runs, row_plates, row_runs,
        table_list,
    };
    use crate::theme::Theme;
    use crate::{InkedSpan, WidgetControlState, WidgetValidation};
    use alloc::vec;

    #[test]
    fn pointed_chosen_and_both_at_once_are_three_fills_and_none_of_them_is_the_plain_row() {
        // Tripwire: the owner's round-11 note 13 — "the current behavior only
        // has the selected element being activated when hovering over ANY item
        // in the list". The widget-wide hover flag lit the *chosen* row
        // wherever the pointer was, so a list answered the pointer by
        // brightening a row somewhere else. The four faces have to be four:
        // collapse hovered onto plain and the row under the pointer says
        // nothing, collapse hovered onto selected and pointing at a row claims
        // it was chosen, and collapse the composite onto either and the reader
        // cannot tell whether the row they are pointing at is the current one.
        let widget = measured_list(10, 5);
        let faces = [
            widget.row_fill(false, false),
            widget.row_fill(false, true),
            widget.row_fill(true, false),
            widget.row_fill(true, true),
        ];

        for (first, second) in (0..faces.len()).flat_map(|i| (i + 1..faces.len()).map(move |j| (i, j))) {
            assert_ne!(faces[first], faces[second], "row face {first} and row face {second} are one fill");
        }
    }

    #[test]
    fn a_named_ink_colours_the_name_alone_and_outlives_the_row_being_chosen() {
        // Tripwire: the owner's round-11 note 7 — an item's rarity is said by
        // the colour of its name and by nothing else. Two ways to lose it.
        // Apply the row's ink to the whole row and the trailing column comes
        // out in four colours, so a reader can no longer compare the numbers
        // they are lined up to compare. Let the chosen row's `selection_text`
        // win over a named ink — which is what the ink resolution did before
        // there was one — and the tier vanishes on the one row the reader
        // pointed at, which is the row they are asking about.
        let theme = Theme::DEFAULT;
        let mut widget = measured_list(2, 2);
        widget.items = vec![
            VirtualListRow::from("Astral Plate").with_trailing(vec!["21/20".into()]).with_ink(TextInk::RarityLegendary),
            VirtualListRow::from("Iron Ring").with_trailing(vec!["1".into()]),
        ];
        widget.selected_index = Some(0);
        widget.forget_measurements();

        let runs = row_runs(&widget);
        assert_eq!(runs.len(), 4, "two rows of two columns: {runs:?}");
        assert_eq!(runs[0].1, theme.rarity_legendary, "the chosen row's name kept its tier");
        assert_eq!(runs[1].1, theme.selection_text, "its amount did not take the tier with it");
        assert_eq!(runs[2].1, theme.text_primary, "an inkless row is written exactly as it was");
        assert_eq!(runs[3].1, theme.text_primary);
    }

    #[test]
    fn draw_realizes_exactly_the_current_window_text() {
        let mut widget = list(200, 5, 6);
        widget.first_index = 2;
        let items = widget.draw_items();
        let text: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, .. } => Some(text.as_str()),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect();
        assert_eq!(text, vec!["row 2", "row 3", "row 4", "row 5", "row 6"]);
        assert_eq!(items.len(), 12, "five row quads, five labels, and the bar's track and thumb only");
    }

    #[test]
    fn an_empty_list_draws_its_line_as_one_muted_caption_and_otherwise_nothing() {
        let mut widget = list(0, 5, 0);
        assert!(widget.draw_items().is_empty(), "an empty list with no line to say draws nothing at all");

        widget.empty_text = String::from("No saved builds");
        let items = widget.draw_items();
        assert_eq!(items.len(), 1, "the empty state is one line, with no row chrome behind it");
        let WidgetDrawItem::Text { text, size_pixels, color, .. } = &items[0] else {
            panic!("the empty state must draw text, not a quad");
        };
        assert_eq!(text, "No saved builds");
        assert_eq!(*size_pixels, widget.theme.caption_size_pixels, "the empty line is set at the caption step");
        assert_eq!(*color, widget.theme.text_muted, "and inked muted");
    }

    #[test]
    fn the_selection_role_fills_the_current_row_and_no_row_without_one() {
        // Tripwire: the accent means "the primary action" and nothing else, so
        // a chosen row must never carry it, and a model holding no selection
        // must light no row at all.
        let mut widget = list(4, 4, 2);
        let row_fills = |widget: &VirtualListWidget| {
            widget
                .draw_items()
                .into_iter()
                .filter_map(|item| match item {
                    WidgetDrawItem::Quad { color, .. } => Some(color),
                    WidgetDrawItem::Text { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            row_fills(&widget),
            vec![
                widget.theme.surface_raised,
                widget.theme.surface_raised,
                widget.theme.selection,
                widget.theme.surface_raised
            ]
        );

        widget.selected_index = None;
        assert!(row_fills(&widget).iter().all(|color| *color == widget.theme.surface_raised));
    }

    #[test]
    fn each_trailing_span_keeps_its_own_ink_and_a_named_one_survives_the_chosen_row() {
        // Tripwire: the owner's round-12 note 6 — "spell tags are all the same
        // colour regardless of tag". Two ways to keep that defect. Join the
        // spans into one run before drawing and every tag comes out in the
        // row's single ink, which is the gap itself. Let the chosen row's
        // `selection_text` win over a span that names an ink and the tags go
        // monochrome on the one row the reader is pointing at, which is the row
        // they are asking about. A span that names no ink still follows the
        // row — that is what keeps a column of amounts one ink down its edge.
        let theme = Theme::DEFAULT;
        let mut widget = measured_list(1, 1);
        widget.frame.width = 400.0;
        widget.items = vec![VirtualListRow::from("Fireball").with_trailing(vec![
            InkedSpan::new("Fire", TextInk::HueWarm),
            InkedSpan::new("Cold", TextInk::HueCool),
            InkedSpan::from("lvl 20"),
        ])];
        widget.selected_index = Some(0);
        widget.forget_measurements();

        let runs = row_runs(&widget);
        assert_eq!(runs.len(), 4, "the name and its three spans: {runs:?}");
        assert_eq!(runs[1], (String::from("Fire"), theme.hue_warm));
        assert_eq!(runs[2], (String::from("Cold"), theme.hue_cool));
        assert_eq!(runs[3], (String::from("lvl 20"), theme.selection_text), "an inkless span follows the row");
    }

    #[test]
    fn a_trailing_run_of_spans_sits_one_word_gap_apart_and_right_aligns_as_a_whole() {
        // Tripwire: three layouts that look right on one row and wrong on the
        // next. Right-align every span against the row's pad and they draw on
        // top of each other. Lay the run out from the left of the shared column
        // and a short run floats away from the edge every other row's run ends
        // on. Drop the word gap and `Fire` `Cold` come out as `FireCold`, which
        // is one word and the reason they are spans at all.
        let mut widget = measured_list(1, 1);
        widget.frame.width = 400.0;
        widget.items =
            vec![VirtualListRow::from("Fireball").with_trailing(vec!["Fire".into(), "Cold".into(), "lvl 20".into()])];
        widget.forget_measurements();

        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        let gap = widget.theme.space(TRAILING_SPAN_GAP_UNITS);
        let runs = drawn_runs(&widget);
        assert_eq!(runs.len(), 4, "the name and its three spans: {runs:?}");

        for pair in runs[1..].windows(2) {
            let advance = measured_text_width(metrics, &pair[0].1, size) + gap;
            assert!((pair[1].0 - (pair[0].0 + advance)).abs() < 1e-3, "the spans are not one word gap apart: {runs:?}");
        }
        let (last_x, last) = runs[3].clone();
        assert!(
            (last_x + measured_text_width(metrics, &last, size) - (widget.row_width() - widget.theme.pad)).abs() < 1e-3,
            "the run as a whole does not end on the row's right pad: {runs:?}",
        );
    }

    #[test]
    fn a_ruled_list_draws_one_hairline_between_each_pair_of_realized_rows() {
        // Tripwire: `n - 1`, never `n`. A rule under the last row underlines
        // the list rather than dividing anything, and a rule above the first
        // draws a second top edge on the frame the panel already bounded.
        let mut widget = list(3, 5, 0);
        let window = widget.window();
        assert!(widget.rule_items(window, 100.0).is_empty(), "an unruled list draws none");

        widget.ruled = true;
        let rules = widget.rule_items(window, 100.0);
        assert_eq!(rules.len(), 2, "three rows, two rules");
        for (index, rule) in rules.iter().enumerate() {
            let WidgetDrawItem::Quad { x, y, width, height, color, .. } = rule else {
                panic!("a rule is a quad: {rule:?}");
            };
            #[allow(clippy::cast_precision_loss)]
            let expected_y = (index + 1) as f32 * 24.0;
            assert_eq!((*x, *y, *width, *height), (0.0, expected_y, 100.0, ROW_RULE_THICKNESS));
            assert_eq!(*color, widget.theme.outline, "a divider is the outline role, not a colour of its own");
        }

        assert!(
            widget.rule_items(VisibleRowWindow { first_index: 0, end_exclusive_index: 1 }, 100.0).is_empty(),
            "one row has nothing to divide",
        );
        assert_eq!(
            widget.draw_items().iter().filter(|item| matches!(item, WidgetDrawItem::Quad { height, .. } if (*height - ROW_RULE_THICKNESS).abs() < f32::EPSILON)).count(),
            2,
            "and the rules reach the list's own draw",
        );
    }

    #[test]
    fn hidden_draw_is_empty_while_retaining_the_bounded_window() {
        let mut widget = list(200, 5, 6);
        widget.first_index = 2;
        let hidden = WidgetControlState { visible: false, ..WidgetControlState::default() };
        widget.replace_control_state(hidden);
        assert!(widget.draw_items().is_empty());
        assert_eq!(widget.window(), VisibleRowWindow { first_index: 2, end_exclusive_index: 7 });
    }

    #[test]
    fn validation_outline_precedes_the_inset_focus_outline() {
        let mut widget = list(20, 5, 0);
        let control = WidgetControlState {
            validation: WidgetValidation::Warning { message: String::from("warning") },
            ..WidgetControlState::default()
        };
        widget.replace_control_state(control);
        widget.state.gain_focus(true);
        let items = widget.draw_items();
        assert_eq!(items.len(), 20, "ten row items, the bar's two quads, and two four-quad outlines");
        for item in &items[12..16] {
            assert!(matches!(item, WidgetDrawItem::Quad { color, .. } if *color == widget.theme.warning));
        }
        for item in &items[16..20] {
            assert!(matches!(item, WidgetDrawItem::Quad { color, .. } if *color == widget.theme.accent));
        }
    }

    #[test]
    fn the_pointed_row_is_washed_over_the_whole_height_it_actually_has() {
        // Tripwire: the fill is the only mark that says which row the pointer
        // found, and one drawn at the configured pitch would wash the top 24
        // pixels of a 55-pixel entry and leave its note on the plain plate —
        // a row that reads as half-lit, and a hover rect that disagrees with
        // the row the list just reported.
        let mut widget = table_list(alloc::vec![noted("Armour", WRAPPING_NOTE), VirtualListRow::from("Evasion")], 5);
        widget.hovered_row = Some(0);

        let plates = row_plates(&widget);
        let washed = plates.iter().find(|(_, _, _, _, color)| *color == widget.row_fill(false, true));
        let (x, y, width, height, _) = *washed.expect("the pointed row draws the hover wash");
        assert!((x, y, width) == (0.0, 0.0, widget.row_width()), "the wash covers the row across");
        assert!((height - 55.2).abs() < 1e-3, "and down its whole height, note included: {height}");
    }

    #[test]
    fn a_rule_above_stands_at_the_top_of_the_rows_space_with_the_gap_under_it() {
        // Tripwire: `designing-a-screen.md` §4 puts whitespace before a rule,
        // so a block reads as a line and then air and then its heading. A rule
        // drawn against the heading instead — under the gap rather than over
        // it — belongs to the block it just closed, which is the boundary read
        // backwards. It spans the text budget, not the frame, so it is not
        // mistaken for the list's own edge.
        let heading = VirtualListRow::from("Resistances").with_space_before(3).with_rule_above();
        let widget = table_list(alloc::vec![VirtualListRow::from("Armour"), heading], 5);

        let rules: Vec<(f32, f32, f32, f32, Rgba)> =
            drawn_quads(&widget).into_iter().filter(|quad| quad.4 == widget.theme.outline).collect();
        assert_eq!(rules.len(), 1, "one rule, from the one row that asked for it: {rules:?}");
        assert_eq!(
            (rules[0].0, rules[0].1, rules[0].2, rules[0].3),
            (widget.theme.pad, 24.0, widget.text_width_budget(), ROW_RULE_THICKNESS),
            "the rule opens the block at the top of its space, across the text budget",
        );

        let plates = row_plates(&widget);
        assert!(
            (plates[1].1 - (24.0 + widget.theme.space(3))).abs() < 1e-3,
            "and the plate starts below the space, which is ground rather than a taller row: {plates:?}",
        );
    }

    #[test]
    fn an_indent_moves_the_name_and_its_note_and_leaves_the_value_where_it_was() {
        // Tripwire: the studio's gap 39. An indented row is a fact hanging off
        // the one above it, and the signal is the left edge — but a value
        // right-aligns on one column whatever rung its name sits on, because
        // that column is what a reader compares two figures down. Moving the
        // trailing run with the name would ruin the one alignment the table
        // has, and faking the indent with spaces would put it in the text.
        let derived = VirtualListRow::from("Physical damage mitigated")
            .with_indent(2)
            .with_note(WRAPPING_NOTE)
            .with_trailing(alloc::vec!["0%".into()]);
        let widget = table_list(alloc::vec![derived], 5);
        let plain = table_list(alloc::vec![VirtualListRow::from("Armour").with_trailing(alloc::vec!["0%".into()])], 5);

        let runs = placed_runs(&widget);
        let name = runs.iter().find(|(text, _, _, _)| text.starts_with("Physical")).expect("the name");
        let note = runs.iter().find(|(text, _, _, _)| text.starts_with("armour")).expect("the note");
        assert!((name.1 - (widget.theme.pad + widget.theme.space(2))).abs() < f32::EPSILON, "the name steps in");
        assert!((note.1 - (widget.theme.pad + widget.theme.space(3))).abs() < f32::EPSILON, "the note one unit more");

        let value_x = |widget: &VirtualListWidget| {
            placed_runs(widget).into_iter().find(|(text, _, _, _)| text == "0%").expect("the value").1
        };
        assert!((value_x(&widget) - value_x(&plain)).abs() < f32::EPSILON, "and the value column did not move");
    }
}
