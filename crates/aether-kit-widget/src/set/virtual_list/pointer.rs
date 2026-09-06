//! What a point in the list lands on, and the row the list reports the
//! pointer is resting on.
//!
//! The resolution order is stated once: the bar owns its gutter, a verb owns
//! its own rect, and the row owns everything else.

use aether_actor::WasmCtx;

use crate::VirtualListHover;
use crate::set::virtual_list::VirtualListWidget;
use crate::set::virtual_list::actions::RowActionIndex;
use crate::set::virtual_list::scroll_bar::ScrollBar;

/// What a left press inside the list lands on.
///
/// The resolution order is the whole of it, and it is stated once here rather
/// than spelled down the press handler: the bar owns its gutter, a **verb owns
/// its own rect**, and the row owns everything else. A verb resolved after the
/// row would remove a skill *and* leave the reader holding a selection they
/// never asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum PressTarget {
    ScrollBar(ScrollBar),
    Action(RowActionIndex),
    /// The row under the point, or `None` for the empty viewport below the last
    /// realized row — the list takes the press either way, and only an actual
    /// row is chosen by it.
    Row(Option<usize>),
}

impl VirtualListWidget {
    /// The row the pointer resolves to right now, or `None` when the pointer
    /// is off the list, over the scroll bar's gutter, or past the last
    /// realized row.
    ///
    /// The gutter is not a row: while a thumb drag is carrying the window the
    /// pointer is on the bar, and reporting whichever row happens to pass
    /// under it would stand a tooltip on a row the reader is not looking at.
    pub(super) fn pointer_row(&self) -> Option<usize> {
        let (local_x, local_y) = self.pointer_local.filter(|_| self.state.is_available())?;
        (local_x >= 0.0 && local_x < self.row_width()).then(|| self.row_at_local_y(local_y)).flatten()
    }

    /// Recompute the hovered row and report it if it changed.
    ///
    /// Called from everything that can move a row out from under the pointer —
    /// the pointer itself, the wheel, a thumb drag, a fresh item vector — so
    /// the fact the host is told stays true while the list scrolls under a
    /// still pointer, which is the half of the studio's gap 19 that a host
    /// redoing the geometry itself could never get right.
    pub(super) fn settle_hovered_row(&mut self, ctx: &WasmCtx<'_>) {
        let next = self.pointer_row();
        if self.hovered_row == next {
            return;
        }
        self.hovered_row = next;
        if let Some(parent) = ctx.parent() {
            parent.send(&VirtualListHover { row: next.and_then(|row| u32::try_from(row).ok()) });
        }
    }

    /// The verb under a point, if the point is on one. Consulted *before* the
    /// row fill, so a press on a verb never also selects the row under it.
    pub(super) fn action_at(&self, local_x: f32, local_y: f32) -> Option<RowActionIndex> {
        let row_index = self.row_at_local_y(local_y)?;
        let row = self.items.get(row_index)?;
        let bands = self.row_bands(row_index)?;
        self.action_rects(row, bands)
            .into_iter()
            .position(|rect| rect.contains(local_x, local_y))
            .map(|action_index| RowActionIndex { row_index, action_index })
    }

    /// What a left press at this widget-local point lands on, or `None` for a
    /// press a list that cannot be changed refuses. The bar is resolved first
    /// and needs no mutability: reading where you are in a read-only list is
    /// not a change to it.
    pub(super) fn press_target(&self, local_x: f32, local_y: f32) -> Option<PressTarget> {
        if let Some(bar) = self.scroll_bar()
            && bar.contains(local_x, local_y)
        {
            return Some(PressTarget::ScrollBar(bar));
        }
        if !self.state.can_mutate() {
            return None;
        }
        if let Some(index) = self.action_at(local_x, local_y) {
            return Some(PressTarget::Action(index));
        }
        Some(PressTarget::Row(self.row_at_local_y(local_y)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::virtual_list::fixture::{actioned_list, measured_list, realized_action_rects, row_middle_y};

    #[test]
    fn the_row_under_the_pointer_follows_the_realized_window() {
        // Tripwire: the studio's gap 19 — the list keeps its rows out of the
        // host's hit table, so the only way a host could follow the pointer
        // was to divide the list's rectangle by its visible row count itself,
        // which names the right item only while every item is realized. This
        // is that arithmetic done where the window actually lives: the
        // pointer has not moved, the wheel has, and the answer moves with it.
        // A hover computed from the pointer alone would still say row 2 after
        // the scroll and stand a tooltip on the wrong gem.
        let mut widget = measured_list(200, 5);
        let row_height = widget.row_height().expect("a laid-out list has a row height");
        widget.pointer_local = Some((widget.theme.pad, row_height.mul_add(2.0, 1.0)));
        assert_eq!(widget.pointer_row(), Some(2));

        widget.scroll_by_pixels(row_height * 7.0);
        assert_eq!(widget.window().first_index, 7, "the wheel moved the window under a pointer that did not move");
        assert_eq!(widget.pointer_row(), Some(9), "so the same point is a different item");

        widget.pointer_local = Some((widget.row_width() + 1.0, row_height.mul_add(2.0, 1.0)));
        assert_eq!(widget.pointer_row(), None, "the scroll bar's gutter is not a row");

        widget.pointer_local = None;
        assert_eq!(widget.pointer_row(), None, "and a pointer that left the list is on nothing");
    }

    #[test]
    fn a_press_on_a_row_verb_is_that_verb_and_never_also_the_row_under_it() {
        // Tripwire: the studio's gap 32 — round-9 note 4, "skills should be
        // removed via 'x' button bound to row". Two failures live in the
        // resolution order. Resolve the row first and the `×` selects the skill
        // it is about to remove, leaving the reader holding a selection they
        // never asked for; resolve no verb at all and the row is back to being
        // a plate that only selects, which is the gap. The verb owns its rect,
        // the row owns the rest.
        let widget = actioned_list(200, 240.0);
        let rects = realized_action_rects(&widget, 2);
        let middle_y = row_middle_y(&widget, 2);

        assert_eq!(rects.len(), 2, "both verbs stand: {rects:?}");
        assert!(
            (rects[1].x + rects[1].width - widget.row_width()).abs() < f32::EPSILON,
            "the verb written last ends on the row's own right edge: {rects:?}",
        );
        assert_eq!(rects[1].x, rects[0].x + rects[0].width, "and the pair touches");

        assert_eq!(
            widget.press_target(rects[1].x + 1.0, middle_y),
            Some(PressTarget::Action(RowActionIndex { row_index: 2, action_index: 1 })),
            "a press inside the × is the ×",
        );
        assert_eq!(
            widget.press_target(rects[0].x + 1.0, middle_y),
            Some(PressTarget::Action(RowActionIndex { row_index: 2, action_index: 0 })),
            "and a press inside the first verb is that one, not the one beside it",
        );
        assert_eq!(
            widget.press_target(widget.theme.pad, middle_y),
            Some(PressTarget::Row(Some(2))),
            "a press on the row's own text still chooses the row",
        );
        assert_eq!(
            widget.press_target(rects[0].x - 1.0, middle_y),
            Some(PressTarget::Row(Some(2))),
            "and so does the pixel just short of the block — the gap belongs to the row",
        );
    }

    #[test]
    fn scrolling_carries_a_verb_with_its_own_row_and_reports_the_item_it_belongs_to() {
        // Tripwire: the list realizes a window, so the row *offset* under the
        // pointer is not the item index. A verb that reported its offset would
        // remove the wrong skill the moment the reader scrolled — and one whose
        // rect was computed from the item index rather than the offset would
        // stand off the bottom of the frame entirely.
        let mut widget = actioned_list(200, 240.0);
        let top_row_middle = row_middle_y(&widget, 0);
        let inside_the_cross = realized_action_rects(&widget, 0)[1].x + 1.0;
        assert_eq!(
            widget.press_target(inside_the_cross, top_row_middle),
            Some(PressTarget::Action(RowActionIndex { row_index: 0, action_index: 1 })),
        );

        widget.first_index = 7;
        assert_eq!(
            widget.press_target(inside_the_cross, top_row_middle),
            Some(PressTarget::Action(RowActionIndex { row_index: 7, action_index: 1 })),
            "the same point is now the eighth skill's ×, because the window moved under it",
        );
        assert_eq!(realized_action_rects(&widget, 0)[1].y, 0.0, "and the verb is drawn at the top of the frame");
    }
}
