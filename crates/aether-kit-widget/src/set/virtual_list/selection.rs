//! The chosen row: which one it is, how the arrows and page keys move it,
//! and how far the window travels to reveal it.
//!
//! Selection is retained independently from realization — a list keeps the
//! row a reader chose whether or not it is on screen — and a reveal moves the
//! window only far enough to bring it back.

use aether_actor::WasmCtx;

use crate::VirtualListSelected;
use crate::set::virtual_list::VirtualListWidget;
use crate::set::virtual_list::rows::{VisibleRowWindow, clamped_window};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionMove {
    Up,
    Down,
    PageUp,
    PageDown,
}

impl VirtualListWidget {
    /// Move the window so the selected row stands in it, without moving it
    /// further than that. A count of rows on the fast path; once the offset
    /// table stands, the topmost row that still leaves the selected row's own
    /// bottom edge inside the viewport, because a row below the selection may
    /// be two lines tall and "one viewport of rows" is no longer a count.
    pub(super) fn reveal_selection(&mut self) {
        let Some(selected_index) = self.selected_index else {
            self.first_index = 0;
            return;
        };
        if self.row_tops.is_none() {
            self.first_index =
                reveal_window(selected_index, self.first_index, self.visible_row_count, self.items.len()).first_index;
            return;
        }
        let window = self.window();
        if selected_index < window.first_index {
            self.first_index = selected_index;
            return;
        }
        if selected_index < window.end_exclusive_index {
            self.first_index = window.first_index;
            return;
        }
        let bottom = self.content_top(selected_index.saturating_add(1)) - self.frame.height;
        let Some(tops) = &self.row_tops else {
            return;
        };
        self.first_index = tops.partition_point(|top| *top < bottom).min(selected_index);
    }

    fn select(&mut self, selected_index: usize) -> Option<u32> {
        if selected_index >= self.items.len() || self.selected_index == Some(selected_index) {
            return None;
        }
        self.selected_index = Some(selected_index);
        self.reveal_selection();
        u32::try_from(selected_index).ok()
    }

    pub(super) fn select_if_mutable(&mut self, selected_index: usize) -> Option<u32> {
        if !self.state.can_mutate() {
            return None;
        }
        self.select(selected_index)
    }

    fn move_selection(&mut self, movement: SelectionMove) -> Option<u32> {
        let next = moved_selection(self.selected_index, movement, self.visible_row_count, self.items.len())?;
        self.select(next)
    }

    pub(super) fn move_selection_if_mutable(&mut self, movement: SelectionMove) -> Option<u32> {
        if !self.state.can_mutate() {
            return None;
        }
        self.move_selection(movement)
    }

    pub(super) fn emit(ctx: &WasmCtx<'_>, selected_index: u32) {
        if let Some(parent) = ctx.parent() {
            parent.send(&VirtualListSelected { selected_index });
        }
    }
}

pub(super) fn reveal_window(
    selected_index: usize,
    first_index: usize,
    requested_visible_row_count: usize,
    item_count: usize,
) -> VisibleRowWindow {
    let mut window = clamped_window(first_index, requested_visible_row_count, item_count);
    let visible_row_count = window.len();
    if visible_row_count == 0 || selected_index >= item_count {
        return window;
    }
    if selected_index < window.first_index {
        window = clamped_window(selected_index, visible_row_count, item_count);
    } else if selected_index >= window.end_exclusive_index {
        let first_index = selected_index.saturating_add(1).saturating_sub(visible_row_count);
        window = clamped_window(first_index, visible_row_count, item_count);
    }
    window
}

fn moved_selection(
    selected_index: Option<usize>,
    movement: SelectionMove,
    visible_row_count: usize,
    item_count: usize,
) -> Option<usize> {
    let selected_index = selected_index?;
    if item_count == 0 || visible_row_count == 0 {
        return None;
    }
    let last_index = item_count - 1;
    Some(match movement {
        SelectionMove::Up => selected_index.saturating_sub(1),
        SelectionMove::Down => selected_index.saturating_add(1).min(last_index),
        SelectionMove::PageUp => selected_index.saturating_sub(visible_row_count),
        SelectionMove::PageDown => selected_index.saturating_add(visible_row_count).min(last_index),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;
    use crate::set::virtual_list::fixture::list;

    #[test]
    fn reveal_moves_only_enough_to_include_selection() {
        assert_eq!(reveal_window(4, 0, 5, 100), VisibleRowWindow { first_index: 0, end_exclusive_index: 5 });
        assert_eq!(reveal_window(5, 0, 5, 100), VisibleRowWindow { first_index: 1, end_exclusive_index: 6 });
        assert_eq!(reveal_window(2, 10, 5, 100), VisibleRowWindow { first_index: 2, end_exclusive_index: 7 });
        assert_eq!(reveal_window(99, 90, 5, 100), VisibleRowWindow { first_index: 95, end_exclusive_index: 100 });
        assert_eq!(reveal_window(0, 0, 0, 100), VisibleRowWindow { first_index: 0, end_exclusive_index: 0 });
    }

    #[test]
    fn arrow_and_page_movement_clamp_and_require_a_nonzero_viewport() {
        assert_eq!(moved_selection(Some(0), SelectionMove::Up, 5, 100), Some(0));
        assert_eq!(moved_selection(Some(0), SelectionMove::Down, 5, 100), Some(1));
        assert_eq!(moved_selection(Some(5), SelectionMove::PageUp, 5, 100), Some(0));
        assert_eq!(moved_selection(Some(5), SelectionMove::PageDown, 5, 100), Some(10));
        assert_eq!(moved_selection(Some(99), SelectionMove::PageDown, 5, 100), Some(99));
        assert_eq!(moved_selection(Some(0), SelectionMove::Down, 0, 100), None);
        assert_eq!(moved_selection(None, SelectionMove::Down, 5, 0), None);
    }

    #[test]
    fn selection_reports_only_actual_changes_and_reveals_them() {
        let mut widget = list(200, 5, 0);
        assert_eq!(widget.select(0), None);
        assert_eq!(widget.move_selection(SelectionMove::PageDown), Some(5));
        assert_eq!(widget.window(), VisibleRowWindow { first_index: 1, end_exclusive_index: 6 });
        assert_eq!(widget.move_selection(SelectionMove::Down), Some(6));
        assert_eq!(widget.window(), VisibleRowWindow { first_index: 2, end_exclusive_index: 7 });
        assert_eq!(widget.select(200), None);
    }

    #[test]
    fn disabled_and_read_only_state_block_selection_mutation() {
        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        for control in [disabled, read_only] {
            let mut widget = list(20, 5, 0);
            widget.replace_control_state(control);
            assert!(!widget.state.can_mutate());
            assert_eq!(widget.move_selection_if_mutable(SelectionMove::PageDown), None);
            assert_eq!(widget.select_if_mutable(4), None);
            assert_eq!(widget.selected_index, Some(0));
        }

        let mut widget = list(20, 5, 0);
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        assert!(widget.replace_control_state(read_only.clone()));
        assert!(!widget.replace_control_state(read_only), "same state emits no second change");
        assert!(!widget.state.can_mutate());
        widget.state.gain_focus(true);
        assert!(widget.state.focused(), "read-only remains keyboard-focusable");
    }
}
