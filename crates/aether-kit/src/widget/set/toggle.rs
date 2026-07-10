// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! Boolean toggle control (issue 2926).
//!
//! A left press arms the switch and a release back inside toggles it once.
//! Enter toggles on its first key press; Space toggles on its matching release.
//! Focus loss, read-only state, and unavailability cancel every arm.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_ENTER, KEY_SPACE};
use aether_kinds::mouse_button;
use aether_kinds::{Key, KeyRelease, MouseButton, MouseButtonRelease};

use crate::widget::set::{push_control_outlines, quad, reply_if_hidden, text_origin_y};
use crate::widget::state::{InteractionState, emit_state_changed};
use crate::widget::theme::{SetTheme, Theme};
use crate::widget::{
    Collect, FocusGained, FocusLost, HoverGained, HoverLost, SetWidgetState, ToggleChanged, ToggleConfig,
    WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardArm {
    Enter,
    Space,
}

/// A boolean switch with a track, knob, and optional label.
pub struct ToggleWidget {
    label: String,
    on: bool,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    pointer_pressed: bool,
    keyboard_arm: Option<KeyboardArm>,
}

impl ToggleWidget {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.frame.x
            && x <= self.frame.x + self.frame.width
            && y >= self.frame.y
            && y <= self.frame.y + self.frame.height
    }

    fn clear_arms(&mut self) {
        self.pointer_pressed = false;
        self.keyboard_arm = None;
    }

    fn pressed(&self) -> bool {
        self.pointer_pressed || self.keyboard_arm == Some(KeyboardArm::Space)
    }

    fn toggle(&mut self) -> bool {
        self.on = !self.on;
        self.on
    }

    fn press_at(&mut self, x: f32, y: f32) {
        if self.state.can_mutate() && self.contains(x, y) {
            self.pointer_pressed = true;
        }
    }

    fn release_at(&mut self, x: f32, y: f32) -> Option<bool> {
        let activates = self.state.can_mutate() && self.pointer_pressed && self.contains(x, y);
        self.pointer_pressed = false;
        activates.then(|| self.toggle())
    }

    fn press_key(&mut self, code: u32) -> Option<bool> {
        if !self.state.can_mutate() || self.keyboard_arm.is_some() {
            return None;
        }
        match code {
            KEY_ENTER => {
                self.keyboard_arm = Some(KeyboardArm::Enter);
                Some(self.toggle())
            }
            KEY_SPACE => {
                self.keyboard_arm = Some(KeyboardArm::Space);
                None
            }
            _ => None,
        }
    }

    fn release_key(&mut self, code: u32) -> Option<bool> {
        match (code, self.keyboard_arm) {
            (KEY_ENTER, Some(KeyboardArm::Enter)) => {
                self.keyboard_arm = None;
                None
            }
            (KEY_SPACE, Some(KeyboardArm::Space)) => {
                self.keyboard_arm = None;
                self.state.can_mutate().then(|| self.toggle())
            }
            _ => None,
        }
    }

    fn adopt_control_state(&mut self, next: WidgetControlState) -> bool {
        if !self.state.replace(next) {
            return false;
        }
        if !self.state.can_mutate() {
            self.clear_arms();
        }
        true
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.adopt_control_state(next) {
            emit_state_changed(ctx, &self.state);
        }
    }

    fn emit(ctx: &WasmCtx<'_>, on: bool) {
        if let Some(parent) = ctx.parent() {
            parent.send(&ToggleChanged { on });
        }
    }
}

/// A toggle widget. Spawned inline by a panel root with a [`ToggleConfig`];
/// reports [`ToggleChanged`] after each completed activation.
#[actor(instanced)]
impl WasmActor for ToggleWidget {
    type Config = ToggleConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.toggle";

    fn init(config: ToggleConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            label: config.label,
            on: config.initial,
            theme: config.theme,
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            state: InteractionState::new(config.state),
            pointer_pressed: false,
            keyboard_arm: None,
        })
    }

    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: ToggleConfig) {
        self.label = config.label;
        self.on = config.initial;
        self.theme = config.theme;
        self.clear_arms();
        self.apply_control_state(ctx, config.state);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    #[handler::single]
    fn on_set_theme(&mut self, _ctx: &mut WasmCtx<'_>, set: SetTheme) {
        self.theme = set.theme;
    }

    #[handler::single]
    fn on_frame(&mut self, _ctx: &mut WasmCtx<'_>, frame: WidgetFrame) {
        self.frame = frame;
    }

    #[handler::single]
    fn on_focus_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: FocusGained) {
        self.state.gain_focus();
    }

    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.state.lose_focus();
        self.clear_arms();
    }

    #[handler::single]
    fn on_hover_gained(&mut self, _ctx: &mut WasmCtx<'_>, _gained: HoverGained) {
        self.state.set_hovered(true);
    }

    #[handler::single]
    fn on_hover_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: HoverLost) {
        self.state.set_hovered(false);
    }

    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button == mouse_button::LEFT {
            self.press_at(press.x, press.y);
        }
    }

    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button == mouse_button::LEFT
            && let Some(on) = self.release_at(release.x, release.y)
        {
            Self::emit(ctx, on);
        }
    }

    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if let Some(on) = self.press_key(key.code) {
            Self::emit(ctx, on);
        }
    }

    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        if let Some(on) = self.release_key(release.code) {
            Self::emit(ctx, on);
        }
    }

    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let width = self.frame.width;
        let height = self.frame.height;
        let track_height = (height * 0.65).clamp(4.0, height.max(4.0));
        let track_width = (track_height * 1.8).min(width.max(0.0));
        let track_y = (height - track_height) * 0.5;
        let knob_size = (track_height - 4.0).max(1.0);
        let knob_x = if self.on { (track_width - knob_size - 2.0).max(2.0) } else { 2.0 };
        let state = self.state.theme_state(self.pressed());
        let track_color = if self.on { self.theme.accent } else { self.theme.surface_raised };
        let knob_color = if self.on { self.theme.accent_text } else { self.theme.outline };

        let mut items = Vec::new();
        items.push(quad(0.0, track_y, track_width, track_height, self.theme.fill(track_color, state)));
        items.push(quad(
            knob_x,
            track_y + 2.0,
            knob_size,
            knob_size,
            self.theme.fill(knob_color, self.state.supporting_theme_state(false)),
        ));
        if !self.label.is_empty() {
            let size = self.theme.label_size_pixels;
            items.push(WidgetDrawItem::Text {
                x: track_width + self.theme.pad,
                y: text_origin_y(0.0, height, size),
                font_id: self.theme.font_id,
                text: self.label.clone(),
                size_pixels: size,
                color: self.theme.fill(self.theme.text_primary, self.state.supporting_theme_state(false)),
                clip: None,
            });
        }
        push_control_outlines(&mut items, width, height, &self.state, &self.theme);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toggle() -> ToggleWidget {
        ToggleWidget {
            label: String::from("snap"),
            on: false,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 20.0, width: 100.0, height: 24.0 },
            state: InteractionState::new(WidgetControlState::default()),
            pointer_pressed: false,
            keyboard_arm: None,
        }
    }

    #[test]
    fn pointer_activation_toggles_once_and_release_outside_cancels() {
        let mut toggle = toggle();
        toggle.press_at(20.0, 30.0);
        assert_eq!(toggle.release_at(20.0, 30.0), Some(true));
        assert_eq!(toggle.release_at(20.0, 30.0), None, "an unarmed release cannot toggle twice");

        toggle.press_at(20.0, 30.0);
        assert_eq!(toggle.release_at(200.0, 30.0), None);
        assert!(toggle.on, "release outside preserves the prior value");
    }

    #[test]
    fn enter_and_space_suppress_repeat_and_toggle_on_their_owned_edge() {
        let mut toggle = toggle();
        assert_eq!(toggle.press_key(KEY_ENTER), Some(true));
        assert_eq!(toggle.press_key(KEY_ENTER), None, "repeat while armed is suppressed");
        assert_eq!(toggle.release_key(KEY_ENTER), None);

        assert_eq!(toggle.press_key(KEY_SPACE), None);
        assert_eq!(toggle.press_key(KEY_SPACE), None, "repeat while armed is suppressed");
        assert_eq!(toggle.release_key(KEY_SPACE), Some(false));
    }

    #[test]
    fn read_only_or_unavailable_state_cancels_live_arms_and_blocks_mutation() {
        let mut toggle = toggle();
        toggle.press_at(20.0, 30.0);
        toggle.press_key(KEY_SPACE);
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        assert!(toggle.adopt_control_state(read_only));
        assert!(!toggle.pointer_pressed);
        assert_eq!(toggle.keyboard_arm, None);
        assert_eq!(toggle.release_at(20.0, 30.0), None);
        assert_eq!(toggle.press_key(KEY_ENTER), None);

        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        assert!(toggle.adopt_control_state(disabled));
        toggle.press_at(20.0, 30.0);
        assert!(!toggle.pointer_pressed);
    }
}
