// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! A fixed-row virtual list (issue 2921).
//!
//! The actor owns the complete item vector but realizes only the bounded row
//! window visible in its assigned frame. Selection is retained independently
//! from realization and keyboard movement reveals it without drawing the
//! offscreen rows.
//!
//! Selection is a state, not an affordance: the current row draws in the
//! theme's selection role, never in the accent that means "the primary
//! action". A model that holds no selection lights no row, and a list with no
//! items at all says so in one muted caption line instead of drawing an empty
//! rectangle.
//!
//! A row is always one configured row tall. A list holding fewer items than
//! its viewport draws that many short rows and leaves the rest of the frame
//! empty — it never spreads them to fill it, which would turn a two-item list
//! into a pair of slabs and its selected row into a half-screen block.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease};

use crate::set::defaults::WidgetDefaults;
use crate::set::{push_control_outlines, quad, release_left, reply_with_draw_items, text_origin_y};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::{TextRole, Theme};
use crate::{
    Collect, SetWidgetState, VirtualListConfig, VirtualListSelected, WidgetControlState, WidgetDrawItem, WidgetFrame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleRowWindow {
    first_index: usize,
    end_exclusive_index: usize,
}

impl VisibleRowWindow {
    fn len(self) -> usize {
        self.end_exclusive_index.saturating_sub(self.first_index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMove {
    Up,
    Down,
    PageUp,
    PageDown,
}

/// A fixed-row virtual list. The item vector is retained, but every collect
/// allocates draw items for only the current `VisibleRowWindow`.
pub struct VirtualListWidget {
    items: Vec<String>,
    /// The one line drawn in place of rows while `items` is empty; empty text
    /// draws nothing.
    empty_text: String,
    selected_index: Option<usize>,
    first_index: usize,
    visible_row_count: usize,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    pressed: bool,
}

impl VirtualListWidget {
    fn window(&self) -> VisibleRowWindow {
        clamped_window(self.first_index, self.visible_row_count, self.items.len())
    }

    fn reveal_selection(&mut self) {
        let Some(selected_index) = self.selected_index else {
            self.first_index = 0;
            return;
        };
        self.first_index =
            reveal_window(selected_index, self.first_index, self.visible_row_count, self.items.len()).first_index;
    }

    fn select(&mut self, selected_index: usize) -> Option<u32> {
        if selected_index >= self.items.len() || self.selected_index == Some(selected_index) {
            return None;
        }
        self.selected_index = Some(selected_index);
        self.reveal_selection();
        u32::try_from(selected_index).ok()
    }

    fn select_if_mutable(&mut self, selected_index: usize) -> Option<u32> {
        if !self.state.can_mutate() {
            return None;
        }
        self.select(selected_index)
    }

    fn move_selection(&mut self, movement: SelectionMove) -> Option<u32> {
        let next = moved_selection(self.selected_index, movement, self.visible_row_count, self.items.len())?;
        self.select(next)
    }

    fn move_selection_if_mutable(&mut self, movement: SelectionMove) -> Option<u32> {
        if !self.state.can_mutate() {
            return None;
        }
        self.move_selection(movement)
    }

    fn emit(ctx: &WasmCtx<'_>, selected_index: u32) {
        if let Some(parent) = ctx.parent() {
            parent.send(&VirtualListSelected { selected_index });
        }
    }

    fn replace_control_state(&mut self, next: WidgetControlState) -> bool {
        let changed = self.state.replace(next);
        if changed && !self.state.can_mutate() {
            self.pressed = false;
        }
        changed
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.replace_control_state(next) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// One row's height: the viewport divided by the row count the list was
    /// *configured* for, never by the number it happens to have realized. A
    /// list holding fewer items than its viewport therefore draws its rows at
    /// their normal height with the rest of the viewport left empty — dividing
    /// by the realized count instead stretched two items over the whole frame,
    /// so a short list rendered as one giant row.
    fn row_height(&self) -> Option<f32> {
        if self.visible_row_count == 0 || !valid_frame(&self.frame) {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let divisor = self.visible_row_count as f32;
        let row_height = self.frame.height / divisor;
        (row_height.is_finite() && row_height > 0.0).then_some(row_height)
    }

    fn row_at_local_y(&self, local_y: f32) -> Option<usize> {
        let window = self.window();
        if !local_y.is_finite() || local_y < 0.0 || local_y >= self.frame.height {
            return None;
        }
        let row_height = self.row_height()?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row_offset = (local_y / row_height).floor() as usize;
        (row_offset < window.len()).then(|| window.first_index + row_offset)
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
            text: self.empty_text.clone(),
            size_pixels: size,
            color: self.theme.fill(self.theme.text_muted, self.state.supporting_theme_state(false)),
            clip: None,
        }]
    }

    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        if !self.state.is_visible() {
            return Vec::new();
        }
        if self.items.is_empty() {
            return self.empty_draw_items();
        }
        let window = self.window();
        let visible_row_count = window.len();
        let Some(row_height) = self.row_height() else {
            return Vec::new();
        };

        let mut items = Vec::with_capacity(visible_row_count.saturating_mul(2).saturating_add(8));
        for (row_offset, item) in self.items[window.first_index..window.end_exclusive_index].iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let row_y = row_offset as f32 * row_height;
            let item_index = window.first_index + row_offset;
            let selected = self.selected_index == Some(item_index);
            let base = if selected {
                self.theme.selection
            } else {
                self.theme.surface_raised
            };
            let row_state = if selected {
                self.state.theme_state(self.pressed)
            } else {
                self.state.supporting_theme_state(false)
            };
            items.push(quad(0.0, row_y, self.frame.width, row_height, self.theme.fill(base, row_state)));
            let text_base = if selected {
                self.theme.selection_text
            } else {
                self.theme.text_primary
            };
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad,
                y: text_origin_y(row_y, row_height, self.theme.label_size_pixels),
                font_id: self.theme.font_id,
                text: item.clone(),
                size_pixels: self.theme.label_size_pixels,
                color: self.theme.fill(text_base, self.state.supporting_theme_state(false)),
                clip: None,
            });
        }
        push_control_outlines(&mut items, self.frame.width, self.frame.height, &self.state, &self.theme);
        items
    }
}

impl WidgetDefaults for VirtualListWidget {
    fn widget_frame(&mut self) -> &mut WidgetFrame {
        &mut self.frame
    }

    fn widget_theme(&mut self) -> &mut Theme {
        &mut self.theme
    }

    fn widget_state(&mut self) -> &mut InteractionState {
        &mut self.state
    }

    fn cancel_activation(&mut self) {
        self.pressed = false;
    }
}

/// A fixed-row virtual list. Spawned inline by a panel root with a
/// [`VirtualListConfig`]; reports [`VirtualListSelected`] when selection
/// changes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `VirtualListConfig` again to replace the item vector or viewport.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for VirtualListWidget {
    type Config = VirtualListConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.virtual_list";

    fn init(config: VirtualListConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let visible_row_count = usize_from_u32(config.visible_row_count);
        let selected_index = initial_selection(config.initial_selected_index, config.items.len());
        let first_index = selected_index.map_or(0, |selected_index| {
            reveal_window(selected_index, 0, visible_row_count, config.items.len()).first_index
        });
        Ok(Self {
            items: config.items,
            empty_text: config.empty_text,
            selected_index,
            first_index,
            visible_row_count,
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            pressed: false,
        })
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: VirtualListConfig) {
        self.items = config.items;
        self.empty_text = config.empty_text;
        self.visible_row_count = usize_from_u32(config.visible_row_count);
        self.selected_index = initial_selection(config.initial_selected_index, self.items.len());
        self.first_index = 0;
        self.reveal_selection();
        self.theme = config.theme;
        self.apply_control_state(ctx, config.state);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.can_mutate() {
            return;
        }
        self.pressed = true;
        if let Some(selected_index) = self.row_at_local_y(press.y - self.frame.y)
            && let Some(selected_index) = self.select_if_mutable(selected_index)
        {
            Self::emit(ctx, selected_index);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        release_left(&mut self.pressed, false, release);
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.can_mutate() {
            return;
        }
        let movement = match key.code {
            KEY_UP => SelectionMove::Up,
            KEY_DOWN => SelectionMove::Down,
            KEY_PAGE_UP => SelectionMove::PageUp,
            KEY_PAGE_DOWN => SelectionMove::PageDown,
            _ => return,
        };
        if let Some(selected_index) = self.move_selection_if_mutable(movement) {
            Self::emit(ctx, selected_index);
        }
    }

    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        reply_with_draw_items(ctx, &self.state, || self.draw_items());
    }
}

fn usize_from_u32(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn initial_selection(initial_selected_index: Option<u32>, item_count: usize) -> Option<usize> {
    let initial_selected_index = initial_selected_index?;
    if item_count == 0 {
        return None;
    }
    Some(usize_from_u32(initial_selected_index).min(item_count - 1))
}

fn clamped_window(first_index: usize, requested_visible_row_count: usize, item_count: usize) -> VisibleRowWindow {
    let visible_row_count = requested_visible_row_count.min(item_count);
    let max_first_index = item_count.saturating_sub(visible_row_count);
    let first_index = first_index.min(max_first_index);
    VisibleRowWindow { first_index, end_exclusive_index: first_index.saturating_add(visible_row_count).min(item_count) }
}

fn reveal_window(
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

fn valid_frame(frame: &WidgetFrame) -> bool {
    frame.x.is_finite()
        && frame.y.is_finite()
        && frame.width.is_finite()
        && frame.height.is_finite()
        && frame.width > 0.0
        && frame.height > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeState;
    use crate::{WidgetDrawItem, WidgetValidation};
    use alloc::format;
    use alloc::vec;

    fn list(item_count: usize, visible_row_count: usize, selected_index: usize) -> VirtualListWidget {
        let items = (0..item_count).map(|index| format!("row {index}")).collect();
        let selected_index = (item_count > 0).then_some(selected_index.min(item_count.saturating_sub(1)));
        VirtualListWidget {
            items,
            empty_text: String::new(),
            selected_index,
            first_index: 0,
            visible_row_count,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 20.0, width: 100.0, height: 120.0 },
            state: InteractionState::new(WidgetControlState::default()),
            pressed: false,
        }
    }

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
    fn initial_selection_is_none_for_empty_and_clamped_for_nonempty() {
        assert_eq!(initial_selection(Some(0), 0), None);
        assert_eq!(initial_selection(None, 5), None, "no selection asked for is no selection");
        assert_eq!(initial_selection(Some(0), 1), Some(0));
        assert_eq!(initial_selection(Some(99), 5), Some(4));
        assert_eq!(initial_selection(Some(u32::MAX), usize::MAX), Some(usize_from_u32(u32::MAX)));
    }

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
        assert_eq!(items.len(), 10, "five row quads and five labels only");
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
        widget.state.gain_focus();
        assert!(widget.state.focused(), "read-only remains keyboard-focusable");
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
    fn hover_focus_and_control_state_follow_shared_interaction_rules() {
        let mut widget = list(20, 5, 0);
        widget.state.set_hovered(true);
        widget.state.gain_focus();
        assert_eq!(widget.state.theme_state(false), ThemeState::Hover);
        assert!(widget.state.focused());
        widget.state.lose_focus();
        assert_eq!(widget.state.theme_state(false), ThemeState::Hover);
        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        assert!(widget.replace_control_state(disabled));
        assert!(!widget.state.focused());
        assert_eq!(widget.state.theme_state(false), ThemeState::Disabled);
    }

    #[test]
    fn validation_outline_precedes_the_inset_focus_outline() {
        let mut widget = list(20, 5, 0);
        let control = WidgetControlState {
            validation: WidgetValidation::Warning { message: String::from("warning") },
            ..WidgetControlState::default()
        };
        widget.replace_control_state(control);
        widget.state.gain_focus();
        let items = widget.draw_items();
        assert_eq!(items.len(), 18, "ten row items plus two four-quad outlines");
        for item in &items[10..14] {
            assert!(matches!(item, WidgetDrawItem::Quad { color, .. } if *color == widget.theme.warning));
        }
        for item in &items[14..18] {
            assert!(matches!(item, WidgetDrawItem::Quad { color, .. } if *color == widget.theme.accent));
        }
    }
}
