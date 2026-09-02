// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]
// An open list realizes at most a screenful of rows; the `usize as f32` for a
// row's pixel offset cannot lose precision at any realizable row count.
#![allow(clippy::cast_precision_loss)]

//! The dropdown: one current choice, the alternatives in a list that opens
//! on demand.
//!
//! Closed, it is one row reading the current option (or the placeholder)
//! with a chevron at its end. Open, it keeps that row and draws up to
//! `open_row_count` option rows below it in its **overlay**
//! ([`WidgetDrawList::overlay`]) so the list escapes the slot clip and lands
//! over every ordinary draw of the cluster. While open it asks the root for
//! the pointer grab through [`crate::DropdownOpenChanged`], so a press anywhere on
//! the window reaches it: a press on a row selects and closes, any other
//! press closes without a change. The current row is drawn in the selection
//! role, never the accent — a chosen thing is a state, not a button.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_ESCAPE, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, KeyRelease, MouseButton, MouseButtonRelease, MouseMove};
use aether_math::Rgba;

use crate::set::{
    ActivationArms, WidgetDefaults, push_control_outlines, push_rect_border, quad, reply_if_hidden, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::{Theme, ThemeState};
use crate::{
    Collect, DropdownConfig, DropdownOpenChanged, DropdownSelected, FocusLost, SetWidgetState, WidgetDrawItem,
    WidgetDrawList, WidgetFrame,
};

/// Which way a keyboard step moves the highlighted row of an open list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HighlightMove {
    Previous,
    Next,
}

/// What one state transition owes the parent: at most one choice change and
/// at most one open/closed edge. Returned by the pure transition methods so
/// the handlers own the sending and the tests own nothing but the logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct DropdownEffects {
    selected: Option<u32>,
    open_changed: Option<bool>,
}

impl DropdownEffects {
    fn opened() -> Self {
        Self { selected: None, open_changed: Some(true) }
    }

    fn closed() -> Self {
        Self { selected: None, open_changed: Some(false) }
    }

    /// The choice first, the open edge second: a consumer sees the value it
    /// asked for before the list reports itself gone.
    fn emit(self, ctx: &WasmCtx<'_>) {
        let Some(parent) = ctx.parent() else {
            return;
        };
        if let Some(index) = self.selected {
            parent.send(&DropdownSelected { index });
        }
        if let Some(open) = self.open_changed {
            parent.send(&DropdownOpenChanged { open });
        }
    }
}

/// The dropdown widget. Holds its options and current choice plus the
/// cached theme / frame.
pub struct DropdownWidget {
    options: Vec<String>,
    selected_index: Option<usize>,
    placeholder: String,
    open_row_count: usize,
    open: bool,
    /// First option realized by the open list — the scrolled window's origin.
    first_index: usize,
    /// The row the pointer or the arrow keys are on while open. Drawn with
    /// the hover overlay; Enter and Space commit it.
    highlighted_index: Option<usize>,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Shared pointer/keyboard activation state; a release-inside toggles the
    /// list while it is closed.
    arms: ActivationArms,
}

impl DropdownWidget {
    /// Rows the open list actually draws — the requested count clamped by the
    /// options there are to show.
    fn realized_row_count(&self) -> usize {
        self.open_row_count.min(self.options.len())
    }

    /// The scrolled window's origin, clamped so the window never runs past
    /// the end of the option vector.
    fn first_row(&self) -> usize {
        self.first_index.min(self.options.len().saturating_sub(self.realized_row_count()))
    }

    /// The option under a window-pixel pointer position, or `None` when the
    /// list is closed or the position misses every realized row. The rows
    /// hang directly below the closed row, so the frame's own height is the
    /// list's top edge.
    fn option_row_at(&self, x: f32, y: f32) -> Option<usize> {
        let row_height = self.theme.row_height;
        let rows = self.realized_row_count();
        if !self.open || rows == 0 || !row_height.is_finite() || row_height <= 0.0 {
            return None;
        }
        if !x.is_finite() || !y.is_finite() || !self.frame.width.is_finite() || !self.frame.height.is_finite() {
            return None;
        }
        if x < self.frame.x || x >= self.frame.x + self.frame.width {
            return None;
        }
        let local_y = y - (self.frame.y + self.frame.height);
        if local_y < 0.0 || local_y >= rows as f32 * row_height {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row_offset = (local_y / row_height).floor() as usize;
        (row_offset < rows).then(|| self.first_row() + row_offset)
    }

    /// Open the list on the current choice. Refused for a read-only or
    /// unavailable dropdown, for an already-open one, and for one with no
    /// rows to realize.
    fn open_list(&mut self) -> DropdownEffects {
        if self.open || !self.state.can_mutate() || self.realized_row_count() == 0 {
            return DropdownEffects::default();
        }
        self.open = true;
        let highlight = self.selected_index.unwrap_or(0);
        self.highlighted_index = Some(highlight);
        self.first_index = revealed_first_index(highlight, self.first_row(), self.open_row_count, self.options.len());
        DropdownEffects::opened()
    }

    /// Close the list without changing the choice. A no-op — and silent —
    /// when it is already closed, so the root's grab is ended exactly once.
    fn dismiss(&mut self) -> DropdownEffects {
        if !self.open {
            return DropdownEffects::default();
        }
        self.open = false;
        self.highlighted_index = None;
        DropdownEffects::closed()
    }

    /// Take `index` as the current choice and close. Reports the choice only
    /// when it actually changed.
    fn commit(&mut self, index: usize) -> DropdownEffects {
        let changed = self.state.can_mutate() && index < self.options.len() && self.selected_index != Some(index);
        if changed {
            self.selected_index = Some(index);
        }
        let mut effects = self.dismiss();
        if changed {
            effects.selected = u32::try_from(index).ok();
        }
        effects
    }

    /// Any left press while the list is open: a press on a row takes it, a
    /// press anywhere else dismisses.
    fn press_while_open(&mut self, x: f32, y: f32) -> DropdownEffects {
        match self.option_row_at(x, y) {
            Some(index) => self.commit(index),
            None => self.dismiss(),
        }
    }

    /// Enter / Space: open a closed list, or take the highlighted row of an
    /// open one. Either way the list ends up in the other state, so the key
    /// reads as a toggle whether or not the arrows moved the highlight first.
    fn toggle(&mut self) -> DropdownEffects {
        if !self.open {
            return self.open_list();
        }
        match self.highlighted_index {
            Some(index) => self.commit(index),
            None => self.dismiss(),
        }
    }

    /// Step the highlighted row, scrolling the realized window only enough to
    /// keep it visible. Silent: nothing is chosen until the list closes.
    fn move_highlight(&mut self, direction: HighlightMove) {
        if !self.open || self.options.is_empty() {
            return;
        }
        let last_index = self.options.len() - 1;
        let next = match (self.highlighted_index, direction) {
            (None, _) => self.selected_index.unwrap_or(0),
            (Some(index), HighlightMove::Previous) => index.saturating_sub(1),
            (Some(index), HighlightMove::Next) => index.saturating_add(1).min(last_index),
        };
        self.highlighted_index = Some(next);
        self.first_index = revealed_first_index(next, self.first_row(), self.open_row_count, self.options.len());
    }

    /// The text the closed row reads and the ink it reads in: the current
    /// option in primary ink, or the placeholder in muted ink.
    fn closed_row_text(&self) -> (&str, Rgba) {
        self.selected_index
            .and_then(|index| self.options.get(index))
            .map_or((self.placeholder.as_str(), self.theme.text_muted), |option| {
                (option.as_str(), self.theme.text_primary)
            })
    }

    /// The closed row: its fill, the current text, the chevron, and the
    /// common validation / focus outlines.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let width = self.frame.width;
        let height = self.frame.height;
        let theme_state = self.state.theme_state(self.arms.pressed());
        let size = self.theme.label_size_pixels;

        let mut items = Vec::new();
        items.push(quad(0.0, 0.0, width, height, self.theme.fill(self.theme.surface_raised, theme_state)));
        let (text, ink) = self.closed_row_text();
        if !text.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad,
                y: text_origin_y(0.0, height, size),
                font_id: self.theme.font_id,
                text: String::from(text),
                size_pixels: size,
                color: self.theme.fill(ink, theme_state),
                clip: None,
            });
        }
        push_chevron(
            &mut items,
            width - self.theme.pad,
            height * 0.5,
            size * CHEVRON_SIZE_RATIO,
            self.theme.fill(self.theme.text_muted, theme_state),
        );
        push_control_outlines(&mut items, width, height, &self.state, &self.theme);
        items
    }

    /// The open list, in the widget's own local coordinates below the closed
    /// row. Empty while closed. Nothing here is clipped by the slot — that is
    /// what the overlay layer buys.
    fn overlay_items(&self) -> Vec<WidgetDrawItem> {
        let rows = self.realized_row_count();
        let row_height = self.theme.row_height;
        let width = self.frame.width;
        if !self.open || rows == 0 || !row_height.is_finite() || row_height <= 0.0 {
            return Vec::new();
        }
        if !width.is_finite() || width <= 0.0 || !self.frame.height.is_finite() {
            return Vec::new();
        }

        let top = self.frame.height;
        let list_height = rows as f32 * row_height;
        let first_index = self.first_row();
        let mut items = Vec::with_capacity(rows.saturating_mul(2).saturating_add(5));
        items.push(quad(0.0, top, width, list_height, self.theme.surface_raised));
        for (row_offset, option) in self.options[first_index..first_index + rows].iter().enumerate() {
            let index = first_index + row_offset;
            let row_y = (row_offset as f32).mul_add(row_height, top);
            let current = self.selected_index == Some(index);
            let highlighted = self.highlighted_index == Some(index);
            if current || highlighted {
                let base = if current {
                    self.theme.selection
                } else {
                    self.theme.surface_raised
                };
                let row_state = if highlighted {
                    ThemeState::Hover
                } else {
                    ThemeState::Normal
                };
                items.push(quad(0.0, row_y, width, row_height, self.theme.fill(base, row_state)));
            }
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad,
                y: text_origin_y(row_y, row_height, self.theme.label_size_pixels),
                font_id: self.theme.font_id,
                text: option.clone(),
                size_pixels: self.theme.label_size_pixels,
                color: if current {
                    self.theme.selection_text
                } else {
                    self.theme.text_primary
                },
                clip: None,
            });
        }
        push_rect_border(&mut items, 0.0, top, width, list_height, 1.0, self.theme.outline);
        items
    }
}

impl WidgetDefaults for DropdownWidget {
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
        self.arms.clear();
        self.open = false;
        self.highlighted_index = None;
    }
}

/// A dropdown. Spawned inline by a panel root with a [`DropdownConfig`];
/// reports [`crate::DropdownSelected`] on a change of choice and
/// [`crate::DropdownOpenChanged`] as its list opens and closes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send
/// it its `DropdownConfig` again to replace the options or the choice in
/// place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for DropdownWidget {
    type Config = DropdownConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.dropdown";

    fn init(config: DropdownConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(DropdownWidget {
            selected_index: initial_selection(config.initial_selected_index, config.options.len()),
            options: config.options,
            placeholder: config.placeholder,
            open_row_count: usize::try_from(config.open_row_count).unwrap_or(usize::MAX),
            open: false,
            first_index: 0,
            highlighted_index: None,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            arms: ActivationArms::default(),
        })
    }

    /// Replace the options / choice / theme in place from a re-sent config.
    /// A list that was open closes, so the root gives up its pointer grab.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: DropdownConfig) {
        let closed = self.dismiss();
        self.selected_index = initial_selection(config.initial_selected_index, config.options.len());
        self.options = config.options;
        self.placeholder = config.placeholder;
        self.open_row_count = usize::try_from(config.open_row_count).unwrap_or(usize::MAX);
        self.first_index = 0;
        self.arms.clear();
        self.theme = config.theme;
        closed.emit(ctx);
        if self.state.replace(config.state) {
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Update external availability; a dropdown that can no longer be chosen
    /// from closes its list.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        if self.state.replace(set.state) {
            emit_state_changed(ctx, &self.state);
        }
        if !self.state.can_mutate() {
            self.arms.clear();
            self.dismiss().emit(ctx);
        }
    }

    /// Focus loss closes the list. Overrides the shared default because
    /// `cancel_activation` cannot report the close, and an unreported close
    /// would leave the root holding a grab for a list nobody can see.
    #[handler::single]
    fn on_focus_lost(&mut self, ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.arms.clear();
        self.dismiss().emit(ctx);
    }

    /// While open, any left press is the list's: on a row it chooses, off one
    /// it dismisses. While closed a press inside the row arms the toggle.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT {
            return;
        }
        if self.open {
            self.press_while_open(press.x, press.y).emit(ctx);
        } else {
            self.arms.press_mouse_button(&self.frame, self.state.can_mutate(), press);
        }
    }

    /// A left release back inside the closed row opens the list.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button != mouse_button::LEFT {
            return;
        }
        if self.arms.release_pointer(&self.frame, self.state.can_mutate(), release.x, release.y) {
            self.open_list().emit(ctx);
        }
    }

    /// Motion moves the highlighted row. The root forwards every move to the
    /// grabbed child, so this tracks the pointer over the overlay rows that
    /// lie outside the widget's own slot.
    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if self.open && self.state.is_available() {
            self.highlighted_index = self.option_row_at(moved.x, moved.y);
        }
    }

    /// Escape closes; Up/Down move the highlight of an open list; Enter
    /// toggles on its press and Space arms until its matching release.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        match key.code {
            KEY_ESCAPE => self.dismiss().emit(ctx),
            KEY_UP if self.open => self.move_highlight(HighlightMove::Previous),
            KEY_DOWN if self.open => self.move_highlight(HighlightMove::Next),
            code => {
                if self.arms.press_key(self.state.can_mutate(), code) {
                    self.toggle().emit(ctx);
                }
            }
        }
    }

    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        if self.arms.release_key(self.state.can_mutate(), release.code) {
            self.toggle().emit(ctx);
        }
    }

    /// Reply the dropdown's local draw: the closed row as ordinary items, the
    /// open list as overlay.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items: self.draw_items(), overlay: self.overlay_items() });
        }
    }
}

/// The chevron's height as a fraction of the label size.
const CHEVRON_SIZE_RATIO: f32 = 0.5;

/// The rows the solid chevron triangle is drawn from. Four bars read as a
/// triangle at every size the row heights in play produce, and stay legible
/// without depending on a glyph the configured font may not carry.
const CHEVRON_ROWS: usize = 4;

/// A downward solid triangle whose bottom-right lands at `right_x`, centered
/// vertically on `center_y` — the closed row's "there are alternatives" mark.
fn push_chevron(items: &mut Vec<WidgetDrawItem>, right_x: f32, center_y: f32, size: f32, color: Rgba) {
    if !size.is_finite() || size <= 0.0 || !right_x.is_finite() || !center_y.is_finite() {
        return;
    }
    let row_height = size / CHEVRON_ROWS as f32;
    let top = size.mul_add(-0.5, center_y);
    let center_x = size.mul_add(-0.5, right_x);
    for row in 0..CHEVRON_ROWS {
        let width = size * (1.0 - row as f32 / CHEVRON_ROWS as f32);
        let y = (row as f32).mul_add(row_height, top);
        items.push(quad(width.mul_add(-0.5, center_x), y, width, row_height, color));
    }
}

/// The boot selection clamped into the option vector; `None` when there is
/// nothing to select or nothing was asked for.
fn initial_selection(initial_selected_index: Option<u32>, option_count: usize) -> Option<usize> {
    let index = usize::try_from(initial_selected_index?).ok()?;
    (option_count > 0).then(|| index.min(option_count - 1))
}

/// The realized window's origin moved the least distance that makes
/// `highlight` visible: unchanged while the row is already inside the window,
/// otherwise pulled to the window's near edge. Clamped so the window never
/// runs past either end of the option vector.
fn revealed_first_index(
    highlight: usize,
    first_index: usize,
    requested_row_count: usize,
    option_count: usize,
) -> usize {
    let row_count = requested_row_count.min(option_count);
    let first_index = first_index.min(option_count.saturating_sub(row_count));
    if row_count == 0 || highlight >= option_count {
        return first_index;
    }
    if highlight < first_index {
        highlight
    } else if highlight >= first_index + row_count {
        highlight.saturating_add(1) - row_count
    } else {
        first_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;
    use alloc::format;
    use alloc::vec;

    fn dropdown(option_count: usize, open_row_count: usize, selected_index: Option<usize>) -> DropdownWidget {
        DropdownWidget {
            options: (0..option_count).map(|index| format!("option {index}")).collect(),
            selected_index,
            placeholder: String::from("Choose"),
            open_row_count,
            open: false,
            first_index: 0,
            highlighted_index: None,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 20.0, width: 100.0, height: 24.0 },
            state: InteractionState::new(WidgetControlState::default()),
            arms: ActivationArms::default(),
        }
    }

    fn opened(option_count: usize, open_row_count: usize, selected_index: Option<usize>) -> DropdownWidget {
        let mut widget = dropdown(option_count, open_row_count, selected_index);
        assert_eq!(widget.open_list(), DropdownEffects::opened());
        widget
    }

    fn row_text(items: &[WidgetDrawItem]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { text, .. } => Some(text.as_str()),
                WidgetDrawItem::Quad { .. } | WidgetDrawItem::TexturedQuad { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_pointer_click_opens_then_closes_the_list() {
        let mut widget = dropdown(4, 3, Some(0));
        widget.arms.press_pointer(&widget.frame, widget.state.can_mutate(), 20.0, 30.0);
        assert!(widget.arms.release_pointer(&widget.frame, widget.state.can_mutate(), 20.0, 30.0));
        assert_eq!(widget.open_list(), DropdownEffects::opened());
        assert!(widget.open);

        // A press back on the closed row hits no option row, so it dismisses.
        assert_eq!(widget.press_while_open(20.0, 30.0), DropdownEffects::closed());
        assert!(!widget.open);
        assert_eq!(widget.selected_index, Some(0), "a dismiss leaves the choice alone");
    }

    #[test]
    fn open_reports_once_and_a_second_open_is_silent() {
        let mut widget = dropdown(4, 3, None);
        assert_eq!(widget.open_list(), DropdownEffects::opened());
        assert_eq!(widget.open_list(), DropdownEffects::default(), "already open reports nothing");
        assert_eq!(widget.dismiss(), DropdownEffects::closed());
        assert_eq!(widget.dismiss(), DropdownEffects::default(), "already closed reports nothing");
    }

    #[test]
    fn a_press_on_a_row_takes_it_and_only_an_actual_change_reports() {
        let mut widget = opened(6, 3, Some(0));
        // Rows start at frame.y + frame.height = 44, one row_height (24) each.
        assert_eq!(
            widget.press_while_open(20.0, 44.0 + 24.0),
            DropdownEffects { selected: Some(1), open_changed: Some(false) }
        );
        assert_eq!(widget.selected_index, Some(1));

        widget.open_list();
        assert_eq!(
            widget.press_while_open(20.0, 44.0 + 24.0),
            DropdownEffects::closed(),
            "re-choosing the current option closes without a value event"
        );
    }

    #[test]
    fn overlay_rows_are_hit_tested_below_the_closed_row_with_an_exclusive_bottom() {
        let widget = opened(6, 3, Some(0));
        let list_top = widget.frame.y + widget.frame.height;
        assert_eq!(widget.option_row_at(20.0, list_top - 0.1), None, "the closed row is not an option row");
        assert_eq!(widget.option_row_at(20.0, list_top), Some(0));
        assert_eq!(widget.option_row_at(20.0, list_top + 23.999), Some(0));
        assert_eq!(widget.option_row_at(20.0, list_top + 24.0), Some(1));
        assert_eq!(widget.option_row_at(20.0, list_top + 71.999), Some(2));
        assert_eq!(widget.option_row_at(20.0, list_top + 72.0), None, "past the last realized row");
        assert_eq!(widget.option_row_at(9.9, list_top), None, "left of the frame");
        assert_eq!(widget.option_row_at(110.0, list_top), None, "the frame's right edge is exclusive");
        assert_eq!(widget.option_row_at(f32::NAN, list_top), None);

        let closed = dropdown(6, 3, Some(0));
        assert_eq!(closed.option_row_at(20.0, list_top), None, "a closed list has no rows to hit");
    }

    #[test]
    fn a_scrolled_window_hit_tests_the_options_it_realizes() {
        let mut widget = opened(10, 3, Some(0));
        widget.first_index = 5;
        let list_top = widget.frame.y + widget.frame.height;
        assert_eq!(widget.option_row_at(20.0, list_top), Some(5));
        assert_eq!(widget.option_row_at(20.0, list_top + 48.0), Some(7));
    }

    #[test]
    fn the_window_moves_only_enough_to_reveal_the_highlight() {
        assert_eq!(revealed_first_index(2, 0, 3, 10), 0, "already visible");
        assert_eq!(revealed_first_index(3, 0, 3, 10), 1);
        assert_eq!(revealed_first_index(9, 0, 3, 10), 7);
        assert_eq!(revealed_first_index(1, 5, 3, 10), 1);
        assert_eq!(revealed_first_index(0, 0, 3, 0), 0, "no options, no window");
        assert_eq!(revealed_first_index(4, 0, 0, 10), 0, "no rows requested, no window");
        assert_eq!(revealed_first_index(4, 99, 3, 10), 4, "a stale origin is clamped first");
        assert_eq!(revealed_first_index(0, 0, usize::MAX, 10), 0);
    }

    #[test]
    fn arrows_walk_the_highlight_scrolling_the_window_and_clamping_at_the_ends() {
        let mut widget = opened(10, 3, Some(0));
        widget.move_highlight(HighlightMove::Previous);
        assert_eq!(widget.highlighted_index, Some(0), "up at the first option is clamped");
        for expected in 1..=4 {
            widget.move_highlight(HighlightMove::Next);
            assert_eq!(widget.highlighted_index, Some(expected));
        }
        assert_eq!(widget.first_row(), 2, "the window followed the highlight down");
        assert_eq!(row_text(&widget.overlay_items()), vec!["option 2", "option 3", "option 4"]);

        let mut widget = opened(3, 3, Some(2));
        for _ in 0..4 {
            widget.move_highlight(HighlightMove::Next);
        }
        assert_eq!(widget.highlighted_index, Some(2), "down at the last option is clamped");
    }

    #[test]
    fn opening_reveals_the_current_option() {
        let widget = opened(10, 3, Some(8));
        assert_eq!(widget.first_row(), 6);
        assert_eq!(widget.highlighted_index, Some(8));
        assert_eq!(row_text(&widget.overlay_items()), vec!["option 6", "option 7", "option 8"]);
    }

    #[test]
    fn enter_and_space_commit_the_highlighted_row_and_close() {
        let mut widget = dropdown(6, 3, Some(0));
        assert_eq!(widget.toggle(), DropdownEffects::opened());
        widget.move_highlight(HighlightMove::Next);
        widget.move_highlight(HighlightMove::Next);
        assert_eq!(widget.toggle(), DropdownEffects { selected: Some(2), open_changed: Some(false) });
        assert_eq!(widget.selected_index, Some(2));
    }

    #[test]
    fn an_unavailable_or_read_only_dropdown_never_opens() {
        for control in [
            WidgetControlState { enabled: false, ..WidgetControlState::default() },
            WidgetControlState { read_only: true, ..WidgetControlState::default() },
            WidgetControlState { visible: false, ..WidgetControlState::default() },
        ] {
            let mut widget = dropdown(4, 3, Some(0));
            widget.state.replace(control);
            assert_eq!(widget.open_list(), DropdownEffects::default());
            assert!(!widget.open);
        }
    }

    #[test]
    fn an_empty_option_vector_and_a_zero_row_list_have_nothing_to_open() {
        let mut empty = dropdown(0, 3, None);
        assert_eq!(empty.open_list(), DropdownEffects::default());
        let mut no_rows = dropdown(4, 0, Some(0));
        assert_eq!(no_rows.open_list(), DropdownEffects::default());
    }

    #[test]
    fn the_overlay_is_empty_while_closed_and_the_current_row_reads_in_the_selection_role() {
        let mut widget = dropdown(6, 3, Some(1));
        assert!(widget.overlay_items().is_empty(), "a closed list draws no overlay");

        widget.open_list();
        widget.highlighted_index = Some(2);
        let items = widget.overlay_items();
        assert_eq!(row_text(&items), vec!["option 0", "option 1", "option 2"]);
        let fills: Vec<Rgba> = items
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Quad { color, .. } => Some(*color),
                _ => None,
            })
            .collect();
        assert!(fills.contains(&Theme::DEFAULT.selection), "the current option is filled in the selection role");
        assert!(!fills.contains(&Theme::DEFAULT.accent), "never the accent — a choice is a state, not a button");
        assert!(
            fills.contains(&Theme::DEFAULT.fill(Theme::DEFAULT.surface_raised, ThemeState::Hover)),
            "the pointed-at option takes the hover overlay",
        );
    }

    #[test]
    fn the_closed_row_reads_the_placeholder_in_muted_ink_until_something_is_chosen() {
        let mut widget = dropdown(6, 3, None);
        assert_eq!(widget.closed_row_text(), ("Choose", Theme::DEFAULT.text_muted));
        assert_eq!(row_text(&widget.draw_items()), vec!["Choose"]);

        widget.selected_index = Some(1);
        assert_eq!(widget.closed_row_text(), ("option 1", Theme::DEFAULT.text_primary));
    }

    #[test]
    fn the_boot_selection_clamps_for_nonempty_and_empty_option_vectors() {
        assert_eq!(initial_selection(None, 5), None, "no choice asked for is no choice");
        assert_eq!(initial_selection(Some(0), 0), None);
        assert_eq!(initial_selection(Some(99), 5), Some(4));
        assert_eq!(initial_selection(Some(2), 5), Some(2));
    }
}
