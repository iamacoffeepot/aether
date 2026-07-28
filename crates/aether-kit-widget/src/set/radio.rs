// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The radio group (issue 2660).
//!
//! A vertical list of mutually-exclusive options, one row per option at the
//! theme's row height. A left click selects the row under the cursor; while
//! focused, Up / Down move the selection (clamped at the ends). The selected
//! index reports up as [`RadioSelected`].

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease};

use crate::set::{clamp_option_index, push_control_outlines, quad, release_left, reply_if_hidden, text_origin_y};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::{SetTheme, Theme};
use crate::{
    Collect, FocusGained, FocusLost, HoverGained, HoverLost, RadioConfig, RadioSelected, SetWidgetState,
    WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

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
}

/// A radio-group widget. Spawned inline by a panel root with a [`RadioConfig`];
/// reports [`RadioSelected`] up as the selection changes.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `RadioConfig` again to replace the options or theme in place.
#[actor(instanced, composable)]
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

    /// Restyle: adopt the fanned theme.
    #[handler::single]
    fn on_set_theme(&mut self, _ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
    }

    /// Cache the layout rect the root assigned.
    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    /// Take keyboard focus.
    #[handler::single]
    fn on_focus_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: FocusGained) {
        self.state.gain_focus();
    }

    /// Release keyboard focus.
    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.pressed = false;
    }

    #[handler::single]
    fn on_hover_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        self.state.set_hovered(true);
    }

    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
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
        if self.options.is_empty() || !self.state.can_mutate() {
            return;
        }
        let next = match key.code {
            KEY_UP => self.selected.saturating_sub(1),
            KEY_DOWN => (self.selected + 1).min(self.options.len() - 1),
            _ => return,
        };
        if next != self.selected {
            self.selected = next;
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
        let row_height = self.theme.row_height.max(1.0);
        let marker = (row_height * 0.5).max(4.0);
        let pad = self.theme.pad;
        let size = self.theme.label_size_pixels;
        let mut items: Vec<WidgetDrawItem> = Vec::new();
        for (i, option) in self.options.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let row_y = i as f32 * row_height;
            let marker_y = (row_height - marker).mul_add(0.5, row_y);
            let base = if i == self.selected {
                self.theme.accent
            } else {
                self.theme.surface_raised
            };
            let marker_state = if i == self.selected {
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
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;
    use alloc::vec;

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
        // Exercise the clamp arithmetic the key handler uses.
        let g = group(&["a", "b", "c"], 0);
        assert_eq!(g.selected.saturating_sub(1), 0, "up at the top stays");
        let g = group(&["a", "b", "c"], 2);
        assert_eq!((g.selected + 1).min(g.options.len() - 1), 2, "down at the bottom stays");
        let g = group(&["a", "b", "c"], 1);
        assert_eq!((g.selected + 1).min(g.options.len() - 1), 2);
        assert_eq!(g.selected.saturating_sub(1), 0);
    }

    #[test]
    fn empty_group_has_no_rows() {
        let g = group(&[], 0);
        assert_eq!(g.row_at_local_y(0.0), None);
    }

    #[test]
    fn options_survive_construction() {
        let g = group(&["x", "y"], 1);
        assert_eq!(g.options, vec![String::from("x"), String::from("y")]);
        assert_eq!(g.selected, 1);
    }
}
