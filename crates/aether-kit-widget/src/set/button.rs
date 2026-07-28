// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The momentary push button (issue 2660).
//!
//! A left press inside the button arms it (the root holds the pointer
//! capture); the matching release fires [`ButtonClicked`] only if it lands
//! back inside — a press-then-release-inside, so a press that drags off and
//! releases elsewhere cancels. The armed state draws the pressed overlay.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::mouse_button;
use aether_kinds::{Key, KeyRelease, MouseButton, MouseButtonRelease};

use crate::set::{ActivationArms, push_border, quad, reply_if_hidden, text_origin_y};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::{SetTheme, Theme};
use crate::{
    ButtonClicked, ButtonConfig, Collect, FocusGained, FocusLost, HoverGained, HoverLost, SetWidgetState,
    WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame,
};

/// A momentary push button. Holds its label plus the cached theme / frame /
/// focus and the armed (`pressed`) state.
pub struct ButtonWidget {
    label: String,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Shared pointer/keyboard activation state; a release-inside fires the click.
    arms: ActivationArms,
}

impl ButtonWidget {
    /// Resolve a release: returns `true` (a click fired) only if the button
    /// was armed and the release landed back inside. Disarms either way.
    fn release_at(&mut self, x: f32, y: f32) -> bool {
        self.arms.release_pointer(&self.frame, self.state.is_available(), x, y)
    }

    fn pressed(&self) -> bool {
        self.arms.pressed()
    }

    fn clear_arms(&mut self) {
        self.arms.clear();
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.state.replace(next) {
            if !self.state.is_available() {
                self.clear_arms();
            }
            emit_state_changed(ctx, &self.state);
        }
    }

    fn emit_click(ctx: &WasmCtx<'_>) {
        if let Some(parent) = ctx.parent() {
            parent.send(&ButtonClicked);
        }
    }

    /// Apply one key press. Returns whether activation fires immediately.
    fn press_key(&mut self, code: u32) -> bool {
        self.arms.press_key(self.state.is_available(), code)
    }

    /// Apply one matching key release. Returns whether activation fires now.
    fn release_key(&mut self, code: u32) -> bool {
        self.arms.release_key(self.state.is_available(), code)
    }
}

/// A push-button widget. Spawned inline by a panel root with a
/// [`ButtonConfig`]; reports [`ButtonClicked`] up on a completed click.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `ButtonConfig` again to relabel or restyle it in place.
#[actor(instanced, composable)]
impl WasmActor for ButtonWidget {
    type Config = ButtonConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.button";

    fn init(config: ButtonConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ButtonWidget {
            label: config.label,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            arms: ActivationArms::default(),
        })
    }

    /// Relabel / restyle in place from a re-sent config.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: ButtonConfig) {
        self.label = config.label;
        self.theme = config.theme;
        self.apply_control_state(ctx, config.state);
    }

    /// Read-only and validation are deliberately inapplicable to a momentary
    /// button; visibility/enabled still control routing and presentation.
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

    /// A left press inside arms the button.
    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        self.arms.press_mouse_button(&self.frame, self.state.is_available(), press);
    }

    /// A left release fires the click if it lands back inside while armed.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button != mouse_button::LEFT {
            return;
        }
        if self.release_at(release.x, release.y) {
            Self::emit_click(ctx);
        }
    }

    /// Enter activates once on its first press; Space arms until its matching
    /// release. Key-repeat presses are ignored while either key is armed.
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if self.press_key(key.code) {
            Self::emit_click(ctx);
        }
    }

    #[handler::single]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, release: KeyRelease) {
        if self.release_key(release.code) {
            Self::emit_click(ctx);
        }
    }

    /// Reply the button's local draw: a filled rect (pressed overlay when
    /// armed), the label, and a focus ring.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        if reply_if_hidden(ctx, &self.state) {
            return;
        }
        let width = self.frame.width;
        let height = self.frame.height;
        let theme_state = self.state.theme_state(self.pressed());
        let size = self.theme.label_size_pixels;

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        items.push(quad(0.0, 0.0, width, height, self.theme.fill(self.theme.accent, theme_state)));
        if !self.label.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad,
                y: text_origin_y(0.0, height, size),
                font_id: self.theme.font_id,
                text: self.label.clone(),
                size_pixels: size,
                color: self.theme.fill(self.theme.accent_text, theme_state),
                clip: None,
            });
        }
        if self.state.focused() {
            push_border(&mut items, width, height, 2.0, self.theme.accent);
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic: None, items });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_kinds::keycode::{KEY_ENTER, KEY_SPACE};

    use crate::WidgetControlState;
    use crate::set::KeyboardArm;

    fn button() -> ButtonWidget {
        ButtonWidget {
            label: String::from("go"),
            theme: Theme::DEFAULT,
            state: InteractionState::new(WidgetControlState::default()),
            frame: WidgetFrame { x: 10.0, y: 10.0, width: 40.0, height: 20.0 },
            arms: ActivationArms::default(),
        }
    }

    #[test]
    fn press_inside_then_release_inside_clicks() {
        let mut b = button();
        b.arms.press_pointer(&b.frame, b.state.is_available(), 20.0, 20.0);
        assert!(b.arms.pointer_pressed);
        assert!(b.release_at(30.0, 25.0), "press-inside then release-inside is a click");
        assert!(!b.arms.pointer_pressed, "disarmed after release");
    }

    #[test]
    fn press_inside_then_release_outside_cancels() {
        let mut b = button();
        b.arms.press_pointer(&b.frame, b.state.is_available(), 20.0, 20.0);
        assert!(!b.release_at(200.0, 200.0), "a release that drifts off the button does not click");
        assert!(!b.arms.pointer_pressed, "disarmed even on a cancel");
    }

    #[test]
    fn press_outside_never_arms() {
        let mut b = button();
        b.arms.press_pointer(&b.frame, b.state.is_available(), 200.0, 200.0);
        assert!(!b.arms.pointer_pressed);
        assert!(!b.release_at(20.0, 20.0), "a release with no prior inside-press does not click");
    }

    #[test]
    fn enter_fires_once_per_press_release_pair_and_ignores_repeat() {
        let mut b = button();
        assert!(b.press_key(KEY_ENTER));
        assert!(!b.press_key(KEY_ENTER), "repeat while armed cannot duplicate");
        assert!(!b.release_key(KEY_ENTER), "Enter fires on press, not release");
        assert!(b.press_key(KEY_ENTER), "matching release rearms the next press");
    }

    #[test]
    fn space_fires_only_on_matching_release_and_cancels_with_focus_loss() {
        let mut b = button();
        assert!(!b.press_key(KEY_SPACE));
        assert_eq!(b.arms.keyboard_arm, Some(KeyboardArm::Space));
        assert!(b.release_key(KEY_SPACE));
        assert_eq!(b.arms.keyboard_arm, None);

        b.press_key(KEY_SPACE);
        b.state.lose_focus();
        b.clear_arms();
        assert!(!b.release_key(KEY_SPACE));
    }
}
