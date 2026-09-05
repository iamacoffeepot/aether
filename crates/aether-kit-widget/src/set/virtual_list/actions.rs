//! The verbs a row carries: the block's width, where each face stands, and
//! how one draws. A verb is the kit's own button face inside a row this
//! widget owns, so one emphasis ladder and one hover answer serve it whether
//! it stands in a slot of its own or on a row.

use alloc::vec::Vec;

use aether_actor::WasmCtx;

use crate::set::virtual_list::VirtualListWidget;
use crate::set::virtual_list::draw::ROW_RULE_THICKNESS;
use crate::set::virtual_list::rows::{RowBands, VisibleRowWindow};
use crate::set::{ButtonFace, approx_text_width, button_face_width, push_button_face, quad};
use crate::theme::ThemeState;
use crate::{RowAction, VirtualListAction, VirtualListRow, WidgetDrawItem};

/// How much clear space stands between the verb block and the text columns
/// beside it, in spacing units. One — the same gap the trailing column keeps,
/// so a row of two columns and a block of verbs reads as three things in a row.
///
/// Nothing stands between one verb and the next: they are **flush**, and the
/// last one ends on the row's own right edge rather than on its right pad
/// (round-12 note 1).
const ACTION_BLOCK_GAP_UNITS: u8 = 1;

/// One verb of one row, addressed the way [`VirtualListAction`] reports it: an
/// index into the item vector, and an index into that row's own actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowActionIndex {
    pub(super) row_index: usize,
    pub(super) action_index: usize,
}

/// Where one row verb stands, in widget-local pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ActionRect {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) width: f32,
    pub(super) height: f32,
}

impl ActionRect {
    pub(super) fn contains(self, local_x: f32, local_y: f32) -> bool {
        local_x >= self.x
            && local_x < self.x + self.width
            && local_y >= self.y
            && local_y < self.y + self.height
            && self.width > 0.0
    }
}

impl VirtualListWidget {
    /// Report one row's verb. Not a selection, and never accompanied by one:
    /// the press that fires this chose nothing.
    pub(super) fn emit_action(ctx: &WasmCtx<'_>, index: RowActionIndex) {
        let (Ok(row_index), Ok(action_index)) = (u32::try_from(index.row_index), u32::try_from(index.action_index))
        else {
            return;
        };
        if let Some(parent) = ctx.parent() {
            parent.send(&VirtualListAction { row_index, action_index });
        }
    }

    /// One verb's width: its measured label plus one `pad` each side — exactly
    /// the intrinsic a [`ButtonWidget`](crate::set::ButtonWidget) reports, so a
    /// `×` on a row is the size it would be in a slot of its own. Approximated
    /// from the character count until the font's advances land, because a verb
    /// that occupied no width until then would let the name elide into the
    /// space it is about to take and then cut it again on the next frame.
    fn action_width(&self, action: &RowAction) -> f32 {
        self.font_metrics.resolved().map_or_else(
            || {
                self.theme
                    .pad
                    .mul_add(2.0, approx_text_width(action.label.chars().count(), self.theme.label_size_pixels))
            },
            |metrics| button_face_width(&action.label, &self.theme, metrics),
        )
    }

    /// The whole verb block one row carries: every verb, edge to edge. `0.0`
    /// for a row with no verbs.
    ///
    /// Nothing is added between them — the owner's round-12 note 1, "flush
    /// means touching" — so a two-verb block is exactly its two faces wide.
    fn actions_width(&self, row: &VirtualListRow) -> f32 {
        row.actions.iter().map(|action| self.action_width(action)).sum()
    }

    /// What a verb block of `block` pixels takes off the right end of a row's
    /// **text budget**, which is the row less one pad at each end.
    ///
    /// The block ends on the row's own right edge rather than on its right pad,
    /// so the pad the text budget already gave up is pad the block is standing
    /// in: the reserve is the block plus its one gap of clear space, *less*
    /// that pad. `0.0` for a row with no verbs, and never negative — a theme
    /// whose pad is wider than a whole verb block reserves nothing rather than
    /// handing the text more room than the row has.
    fn block_reserve(&self, block: f32) -> f32 {
        match block {
            block if block > 0.0 => (block + self.theme.space(ACTION_BLOCK_GAP_UNITS) - self.theme.pad).max(0.0),
            _ => 0.0,
        }
    }

    /// What one row's own verbs take off its text budget — the intrinsic's
    /// half of [`Self::actions_reserve`], which measures the shared column.
    pub(super) fn actions_reserve_for(&self, row: &VirtualListRow) -> f32 {
        self.block_reserve(self.actions_width(row))
    }

    /// The verb block this window's rows share: the widest among the rows **on
    /// screen**, like the trailing column and for the same reason — a column
    /// sized by an off-screen row leaves a gap nothing stands in, and one row
    /// eliding at a different point from its neighbours reads as a ragged edge.
    fn actions_column(&self, window: VisibleRowWindow) -> f32 {
        self.items[window.first_index..window.end_exclusive_index]
            .iter()
            .map(|row| self.actions_width(row))
            .fold(0.0_f32, f32::max)
    }

    /// What the verbs take off the right end of every row's text: the shared
    /// block's reserve, or nothing when no realized row carries a verb.
    pub(super) fn actions_reserve(&self, window: VisibleRowWindow) -> f32 {
        self.block_reserve(self.actions_column(window))
    }

    /// Where each verb of one row stands. The block is right-aligned against
    /// the **row's own right edge** — not its right pad — and the verbs run
    /// left to right in the order they were written, touching, so the last one
    /// written is the one on the edge: the owner's `[Change gem][x]`, with the
    /// `×` outermost and nothing after it.
    ///
    /// Round 11 read "flush" as one spacing unit between the verbs and the
    /// block sitting on the row's right pad; round-12 note 1 says the pad and
    /// the gaps both go, so a pressable face runs to the row's edge and the
    /// pair reads as one block of verbs rather than two loose controls.
    /// A verb stands on the row's **first line** rather than over its whole
    /// height: a row with a note is two lines of one entry, and a face drawn
    /// down both of them would read as a control over the sentence too.
    pub(super) fn action_rects(&self, row: &VirtualListRow, bands: RowBands) -> Vec<ActionRect> {
        let mut x = self.row_width() - self.actions_width(row);
        let mut rects = Vec::with_capacity(row.actions.len());
        for action in &row.actions {
            let width = self.action_width(action);
            rects.push(ActionRect { x, y: bands.plate_top, width, height: bands.line_height });
            x += width;
        }
        rects
    }

    /// How one verb answers the pointer. A list that cannot be changed draws
    /// every verb disabled — read-only as well as disabled, because a verb that
    /// looks live and does nothing is worse than one that says it is dead —
    /// and otherwise each verb carries its own Pressed → Hover → Normal.
    fn action_theme_state(&self, index: RowActionIndex) -> ThemeState {
        if !self.state.can_mutate() {
            ThemeState::Disabled
        } else if self.pressed_action == Some(index) {
            ThemeState::Pressed
        } else if self.hovered_action == Some(index) {
            ThemeState::Hover
        } else {
            ThemeState::Normal
        }
    }

    /// Draw one row's verbs as button faces, at the rects the block resolves to.
    pub(super) fn push_row_actions(
        &self,
        items: &mut Vec<WidgetDrawItem>,
        row: &VirtualListRow,
        item_index: usize,
        bands: RowBands,
    ) {
        for (action_index, (action, rect)) in row.actions.iter().zip(self.action_rects(row, bands)).enumerate() {
            let face = ButtonFace {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                label: &action.label,
                emphasis: action.emphasis,
                tone: action.tone,
            };
            let theme_state = self.action_theme_state(RowActionIndex { row_index: item_index, action_index });
            push_button_face(items, &face, &self.theme, theme_state, self.font_metrics.resolved());
            if action_index > 0 {
                items.push(quad(rect.x, rect.y, ROW_RULE_THICKNESS, rect.height, self.theme.edge()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;
    use crate::set::ELLIPSIS;
    use crate::set::defaults::WidgetDefaults;
    use crate::set::measured_text_width;
    use crate::set::virtual_list::fixture::{actioned_list, drawn_runs, realized_action_rects, row_middle_y};
    use alloc::string::String;
    use alloc::vec;

    #[test]
    fn every_row_ends_its_verbs_flush_with_each_other_and_with_the_rows_own_edge() {
        // Tripwire: the owner's round-12 note 1 — "the buttons for the skill
        // remove and skill change aren't flush with each other (touching), and
        // the 'x' button isn't touching the end of the entry". Round 11 read
        // "flush" as one spacing unit between the verbs with the block on the
        // row's right pad, which is what this inverts: nothing between one face
        // and the next, and the last face on the row's own right edge. Three
        // further ways to lose it. Left-align a row's block inside the *shared*
        // column and every row carrying fewer or narrower verbs than the widest
        // one ends short of the edge with a band of slack after it. Right-align
        // against the frame instead of the row and the block slides under the
        // scroll bar's gutter. Keep the pad and the `×` still floats off the
        // end, which is the half of the note the geometry has to answer.
        let mut widget = actioned_list(40, 240.0);
        widget.items[1] = VirtualListRow::from("one verb").with_actions(vec![RowAction::text("Change")]);
        widget.items[2] = VirtualListRow::from("three verbs").with_actions(vec![
            RowAction::text("Change"),
            RowAction::text("Copy"),
            RowAction::danger("x"),
        ]);
        widget.items[3] = VirtualListRow::from("no verbs at all");
        widget.forget_measurements();

        let right_edge = widget.row_width();
        assert!(widget.row_width() < widget.frame.width, "this list scrolls, so the gutter really is off the row");

        for row_offset in 0..widget.window().len() {
            let rects = realized_action_rects(&widget, row_offset);
            let Some(last) = rects.last() else {
                continue;
            };
            assert!(
                (last.x + last.width - right_edge).abs() < 1e-3,
                "row {row_offset} leaves slack after its last verb: {rects:?}",
            );
            for pair in rects.windows(2) {
                assert!(
                    (pair[1].x - (pair[0].x + pair[0].width)).abs() < 1e-3,
                    "row {row_offset} holds its verbs apart instead of flush: {rects:?}",
                );
            }
        }
    }

    #[test]
    fn touching_verbs_are_told_apart_by_one_hairline_on_each_boundary_inside_the_block() {
        // Tripwire: round-12 note 1 makes the faces touch, and the rank a row
        // verb takes is `ButtonEmphasis::Text` — a label and no face at all —
        // so two touching verbs are two labels with nothing between them, which
        // reads as one word. The hairline is what tells them apart. One per
        // boundary *inside* the block and none on its outer edges: a rule at
        // the row's own right edge is a second border, and one before the first
        // verb is a column rule nobody asked for.
        let mut widget = actioned_list(1, 300.0);
        widget.items[0] = VirtualListRow::from("Spark").with_actions(vec![
            RowAction::text("Change"),
            RowAction::text("Copy"),
            RowAction::danger("x"),
        ]);
        widget.forget_measurements();

        let rects = realized_action_rects(&widget, 0);
        let hairlines: Vec<f32> = widget
            .draw_items()
            .into_iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { x, width, color, .. }
                    if width == ROW_RULE_THICKNESS && color == widget.theme.edge() =>
                {
                    Some(x)
                }
                _ => None,
            })
            .collect();

        assert_eq!(hairlines, vec![rects[1].x, rects[2].x], "one hairline per boundary between two verbs");
    }

    #[test]
    fn a_row_name_elides_clear_of_the_verbs_and_the_verbs_are_drawn_whole() {
        // Tripwire: the verbs are the row's third column and are reserved
        // *first*, exactly as the trailing column is. Charge the name against
        // the whole row and it runs under the buttons — the round-5 note-8
        // defect one column further in; elide the verb instead and the reader
        // gets a control labelled `Ch…`, which is not a control.
        let mut widget = actioned_list(2, 240.0);
        widget.items[0] = VirtualListRow::from("a skill gem with a name far too long for this row")
            .with_trailing(vec!["21/20".into()])
            .with_actions(vec![RowAction::text("Change"), RowAction::danger("x")]);
        widget.forget_measurements();

        let window = widget.window();
        let reserve = widget.actions_reserve(window);
        assert_eq!(
            reserve,
            widget.actions_column(window) + widget.theme.space(ACTION_BLOCK_GAP_UNITS) - widget.theme.pad,
            "the reserve is the shared block plus one gap of clear space, less the pad the block stands in",
        );

        let runs = drawn_runs(&widget);
        let size = widget.theme.label_size_pixels;
        let metrics = widget.font_metrics.resolved().expect("the test table is installed");
        let (name_x, name) = runs[0].clone();
        assert!(name.ends_with(ELLIPSIS), "the name gave way to the verbs: {name:?}");
        let budget =
            widget.leading_width_budget(widget.trailing_column(window, widget.trailing_budget(reserve)), reserve);
        assert!(measured_text_width(metrics, &name, size) <= budget, "and stopped inside the budget: {name:?}");

        let (trailing_x, trailing) = runs[1].clone();
        assert_eq!(trailing, "21/20", "the amount is drawn whole");
        assert!(
            (trailing_x + measured_text_width(metrics, &trailing, size)
                - (widget.row_width() - widget.theme.pad - reserve))
                .abs()
                < f32::EPSILON,
            "and ends against the verb block rather than under it",
        );

        let labels: Vec<String> = runs[2..4].iter().map(|(_, run)| run.clone()).collect();
        assert_eq!(labels, vec![String::from("Change"), String::from("x")], "both verbs read whole: {labels:?}");
        assert!(name_x < trailing_x, "the row still reads name, amount, verbs from the left");

        // The second row carries the same verbs, so both rows give up the same
        // width and their names elide on one edge.
        assert_eq!(widget.actions_width(&widget.items[0]), widget.actions_width(&widget.items[1]));
    }

    #[test]
    fn a_list_that_cannot_be_changed_draws_its_verbs_dead_and_refuses_their_presses() {
        // Tripwire: a read-only list is as unable to remove a skill as a
        // disabled one, so both must *say* so. A verb that kept its live ink
        // and swallowed the press is the worst of the three outcomes — the
        // reader presses `×`, nothing happens, and nothing said it would not.
        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        for control in [disabled, read_only] {
            let mut widget = actioned_list(200, 240.0);
            let inside_the_cross = realized_action_rects(&widget, 1)[1].x + 1.0;
            let middle_y = row_middle_y(&widget, 1);
            widget.replace_control_state(control.clone());

            assert_eq!(
                widget.action_theme_state(RowActionIndex { row_index: 1, action_index: 1 }),
                ThemeState::Disabled,
                "the verb draws dead",
            );
            assert_eq!(widget.press_target(inside_the_cross, middle_y), None, "and takes no press");
            assert!(
                widget
                    .draw_items()
                    .iter()
                    .any(|item| matches!(item, WidgetDrawItem::Text { text, .. } if text == "x"),),
                "it is still drawn — a verb that vanished would say the row lost its remove, not that it is dead",
            );
        }

        // Hover and press are per verb, and the pointer leaving the list drops
        // the one it was over: a stale hover would light a verb the pointer is
        // nowhere near.
        let mut widget = actioned_list(200, 240.0);
        let hovered = RowActionIndex { row_index: 3, action_index: 0 };
        widget.hovered_action = Some(hovered);
        widget.pressed_action = Some(RowActionIndex { row_index: 3, action_index: 1 });
        assert_eq!(widget.action_theme_state(hovered), ThemeState::Hover);
        assert_eq!(
            widget.action_theme_state(RowActionIndex { row_index: 3, action_index: 1 }),
            ThemeState::Pressed,
            "the armed verb outranks the hovered one on its own rect",
        );
        assert_eq!(
            widget.action_theme_state(RowActionIndex { row_index: 4, action_index: 0 }),
            ThemeState::Normal,
            "and a verb the pointer is not on answers nothing",
        );
        widget.cancel_activation();
        assert_eq!(widget.pressed_action, None, "focus loss disarms the verb like any other activation");
    }
}
