// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (the full rationale is on the same allow in `lib.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The radio group (issue 2660).
//!
//! A vertical list of mutually-exclusive options, one row per option at the
//! theme's row height. A left click selects the row under the cursor; while
//! focused, Up / Down move the selection (clamped at the ends). The selected
//! index reports up as [`RadioSelected`].
//!
//! The chosen option is a state, not an affordance: its marker fills with the
//! theme's selection role, so an unselected marker reads as an empty slot and
//! the accent goes on meaning "the primary action" and nothing else.
//!
//! Only the marker is filled — a row is never plated — so every label, chosen
//! or not, is drawn on the panel's own `surface` and takes `text_primary`.
//! `selection_text` is defined as the ink that reads *on* a `selection` fill
//! ([`SegmentedWidget`](super::segmented) is the case that has one), and a
//! label on an unfilled row is not that; borrowing it here inverted the
//! chosen row's ink under any theme that pairs a light selection with dark
//! ink on it, the way this theme already pairs `accent` / `accent_text`.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease};

use crate::set::defaults::WidgetDefaults;
use crate::set::{clamp_option_index, push_control_outlines, quad, release_left, reply_if_hidden, text_origin_y};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::Theme;
use crate::{
    Collect, RadioConfig, RadioSelected, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList,
    WidgetFrame,
};

/// Which way an arrow key moves the selection.
#[derive(Debug, Clone, Copy)]
enum RadioDirection {
    Previous,
    Next,
}

/// A radio group. Holds the option labels, the selected index, and the cached
/// theme / frame / focus.
pub struct RadioGroupWidget {
    options: Vec<String>,
    selected: usize,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    pressed: bool,
}

impl RadioGroupWidget {
    /// The row a local-y falls in, or `None` past the last option. Owned
    /// hit math — unit-tested.
    fn row_at_local_y(&self, local_y: f32) -> Option<usize> {
        let row_height = self.theme.row_height.max(1.0);
        if local_y < 0.0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row = (local_y / row_height) as usize;
        (row < self.options.len()).then_some(row)
    }

    /// Emit the current selection up to the panel root.
    fn emit(&self, ctx: &WasmCtx<'_>) {
        if let Some(parent) = ctx.parent() {
            #[allow(clippy::cast_possible_truncation)]
            let index = self.selected as u32;
            parent.send(&RadioSelected { index });
        }
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.state.replace(next) {
            if !self.state.can_mutate() {
                self.pressed = false;
            }
            emit_state_changed(ctx, &self.state);
        }
    }

    /// Move the selection one row, returning the row it moved to — or `None`
    /// when it moved nothing, which is what the ends clamp to. Mirrors
    /// `SegmentedWidget::step`, the other exclusive-choice control, so the
    /// clamp is a function a test can call rather than arithmetic inlined in
    /// a handler.
    fn step(&mut self, direction: RadioDirection) -> Option<usize> {
        if self.options.is_empty() || !self.state.can_mutate() {
            return None;
        }
        let next = match direction {
            RadioDirection::Previous => self.selected.saturating_sub(1),
            RadioDirection::Next => (self.selected + 1).min(self.options.len() - 1),
        };
        if next == self.selected {
            return None;
        }
        self.selected = next;
        Some(next)
    }

    /// The group's local draw: one marker + label per option row, the
    /// selected row's marker filled, plus the shared control outlines.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let row_height = self.theme.row_height.max(1.0);
        let marker = (row_height * 0.5).max(4.0);
        let pad = self.theme.pad;
        let size = self.theme.label_size_pixels;

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        for (i, option) in self.options.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let row_y = i as f32 * row_height;
            let marker_y = (row_height - marker).mul_add(0.5, row_y);
            let selected = i == self.selected;
            let base = if selected {
                self.theme.selection
            } else {
                self.theme.surface_raised
            };
            let marker_state = if selected {
                self.state.theme_state(self.pressed)
            } else {
                self.state.supporting_theme_state(false)
            };
            items.push(quad(pad, marker_y, marker, marker, self.theme.fill(base, marker_state)));
            items.push(WidgetDrawItem::Text {
                x: pad.mul_add(2.0, marker),
                y: text_origin_y(row_y, row_height, size),
                font_id: self.theme.font_id,
                text: option.clone(),
                size_pixels: size,
                color: self.theme.fill(self.theme.text_primary, self.state.supporting_theme_state(false)),
                clip: None,
            });
        }

        push_control_outlines(&mut items, self.frame.width, self.frame.height, &self.state, &self.theme);
        items
    }
}

impl WidgetDefaults for RadioGroupWidget {
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

/// A radio-group widget. Spawned inline by a panel root with a [`RadioConfig`];
/// reports [`RadioSelected`] up as the selection changes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `RadioConfig` again to replace the options or theme in place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for RadioGroupWidget {
    type Config = RadioConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.radio";

    fn init(config: RadioConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let selected = clamp_option_index(config.initial_index, config.options.len());
        Ok(RadioGroupWidget {
            options: config.options,
            selected,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            pressed: false,
        })
    }

    /// Replace the options / theme in place, re-clamping the selection.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: RadioConfig) {
        self.selected = clamp_option_index(config.initial_index, config.options.len());
        self.options = config.options;
        self.theme = config.theme;
        self.apply_control_state(ctx, config.state);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    /// A left click selects the row under the cursor.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.can_mutate() {
            return;
        }
        self.pressed = true;
        if let Some(row) = self.row_at_local_y(press.y - self.frame.y)
            && row != self.selected
        {
            self.selected = row;
            self.emit(ctx);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, _ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        release_left(&mut self.pressed, false, release);
    }

    /// Up / Down move the selection while focused (clamped at the ends).
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        let direction = match key.code {
            KEY_UP => RadioDirection::Previous,
            KEY_DOWN => RadioDirection::Next,
            _ => return,
        };
        if self.step(direction).is_some() {
            self.emit(ctx);
        }
    }

    /// Reply the group's local draw: one marker + label per option row, the
    /// selected row's marker filled, plus a focus ring.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList {
                content_height: None,
                intrinsic: None,
                items: self.draw_items(),
                overlay: Vec::new(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_math::Rgba;

    use crate::WidgetControlState;

    fn group(options: &[&str], selected: usize) -> RadioGroupWidget {
        RadioGroupWidget {
            options: options.iter().map(|s| String::from(*s)).collect(),
            selected,
            theme: Theme::DEFAULT, // row_height = 24
            state: InteractionState::new(WidgetControlState::default()),
            frame: WidgetFrame { x: 0.0, y: 10.0, width: 100.0, height: 72.0 },
            pressed: false,
        }
    }

    #[test]
    fn row_hit_maps_local_y_to_index_and_misses_past_the_end() {
        let g = group(&["a", "b", "c"], 0);
        assert_eq!(g.row_at_local_y(0.0), Some(0));
        assert_eq!(g.row_at_local_y(30.0), Some(1)); // 30 / 24 = 1
        assert_eq!(g.row_at_local_y(60.0), Some(2)); // 60 / 24 = 2
        assert_eq!(g.row_at_local_y(72.0), None, "past the third row hits nothing");
        assert_eq!(g.row_at_local_y(-1.0), None, "above the group hits nothing");
    }

    #[test]
    fn clamp_index_pins_an_over_range_initial() {
        assert_eq!(clamp_option_index(0, 3), 0);
        assert_eq!(clamp_option_index(5, 3), 2, "past the end clamps to the last option");
        assert_eq!(clamp_option_index(1, 0), 0, "an empty group clamps to 0");
    }

    #[test]
    fn up_down_selection_clamps_at_the_ends() {
        let mut g = group(&["a", "b", "c"], 0);
        assert_eq!(g.step(RadioDirection::Previous), None, "up at the top stays");
        assert_eq!(g.step(RadioDirection::Next), Some(1));
        assert_eq!(g.step(RadioDirection::Next), Some(2));
        assert_eq!(g.step(RadioDirection::Next), None, "down at the bottom stays");
        assert_eq!(g.selected, 2);

        let mut empty = group(&[], 0);
        assert_eq!(empty.step(RadioDirection::Next), None, "an empty group has nothing to move to");

        let mut read_only = group(&["a", "b"], 0);
        assert!(read_only.state.replace(WidgetControlState { read_only: true, ..WidgetControlState::default() }));
        assert_eq!(read_only.step(RadioDirection::Next), None, "a read-only group does not move");
        assert_eq!(read_only.selected, 0);
    }

    #[test]
    fn empty_group_has_no_rows() {
        let g = group(&[], 0);
        assert_eq!(g.row_at_local_y(0.0), None);
    }

    /// The ink of every label the group draws, in row order.
    fn label_inks(g: &RadioGroupWidget) -> Vec<Rgba> {
        g.draw_items()
            .iter()
            .filter_map(|item| match item {
                WidgetDrawItem::Text { color, .. } => Some(*color),
                _ => None,
            })
            .collect()
    }

    // Tripwire: `selection_text` is defined as the ink drawn *on* a
    // `selection`-filled row, and a radio group fills only the small marker —
    // every label sits on the panel's own `surface`. The default theme hides
    // the misuse (its `selection_text` is `text_primary`), so this pins it
    // with a themed palette that pairs a light selection with dark ink on it,
    // exactly how `accent` / `accent_text` are already paired: under it the
    // chosen row was near-black text on the dark panel, the one unreadable
    // row in the group.
    #[test]
    fn a_selected_label_inks_for_the_surface_it_is_actually_drawn_on() {
        let mut g = group(&["a", "b"], 0);
        g.theme = Theme {
            selection: Rgba::from_srgb8(0xa8, 0xc9, 0x7a, 0xff),
            selection_text: Rgba::from_srgb8(0x19, 0x1b, 0x15, 0xff),
            ..Theme::DEFAULT
        };

        for (row, ink) in label_inks(&g).into_iter().enumerate() {
            let ratio = Theme::contrast_ratio(ink, g.theme.surface);
            assert!(ratio >= 4.5, "row {row}'s label is body text and reads at only {ratio} on the panel under it");
        }
    }
}
