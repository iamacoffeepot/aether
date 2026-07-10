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
use aether_kinds::{MouseButton, MouseButtonRelease};

use crate::widget::set::{push_border, quad, text_origin_y};
use crate::widget::theme::{SetTheme, Theme, WidgetState};
use crate::widget::{
    ButtonClicked, ButtonConfig, Collect, FocusGained, FocusLost, WidgetDrawItem, WidgetDrawList,
    WidgetFrame,
};

/// A momentary push button. Holds its label plus the cached theme / frame /
/// focus and the armed (`pressed`) state.
pub struct ButtonWidget {
    label: String,
    theme: Theme,
    frame: WidgetFrame,
    focused: bool,
    /// Armed by a press inside; a release-inside while armed fires the click.
    pressed: bool,
}

impl ButtonWidget {
    /// Whether a window-space point falls inside the button's frame.
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.frame.x
            && x <= self.frame.x + self.frame.width
            && y >= self.frame.y
            && y <= self.frame.y + self.frame.height
    }

    /// Arm the button if the press landed inside. Owned state-machine step —
    /// unit-tested.
    fn press_at(&mut self, x: f32, y: f32) {
        if self.contains(x, y) {
            self.pressed = true;
        }
    }

    /// Resolve a release: returns `true` (a click fired) only if the button
    /// was armed and the release landed back inside. Disarms either way.
    fn release_at(&mut self, x: f32, y: f32) -> bool {
        let clicked = self.pressed && self.contains(x, y);
        self.pressed = false;
        clicked
    }
}

/// A push-button widget. Spawned inline by a panel root with a
/// [`ButtonConfig`]; reports [`ButtonClicked`] up on a completed click.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `ButtonConfig` again to relabel or restyle it in place.
#[actor(instanced)]
impl WasmActor for ButtonWidget {
    type Config = ButtonConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.button";

    fn init(config: ButtonConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ButtonWidget {
            label: config.label,
            theme: config.theme,
            frame: WidgetFrame {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
            focused: false,
            pressed: false,
        })
    }

    /// Relabel / restyle in place from a re-sent config.
    #[handler::single]
    fn on_config(&mut self, _ctx: &mut WasmCtx<'_>, config: ButtonConfig) {
        self.label = config.label;
        self.theme = config.theme;
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
        self.focused = true;
    }

    /// Release keyboard focus.
    #[handler::single]
    fn on_focus_lost(&mut self, _ctx: &mut WasmCtx<'_>, _lost: FocusLost) {
        self.focused = false;
    }

    /// A left press inside arms the button.
    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button == mouse_button::LEFT {
            self.press_at(press.x, press.y);
        }
    }

    /// A left release fires the click if it lands back inside while armed.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button != mouse_button::LEFT {
            return;
        }
        if self.release_at(release.x, release.y)
            && let Some(parent) = ctx.parent()
        {
            parent.send(&ButtonClicked);
        }
    }

    /// Reply the button's local draw: a filled rect (pressed overlay when
    /// armed), the label, and a focus ring.
    ///
    /// # Agent
    /// The panel root's per-frame poll; not useful to send manually.
    #[handler::single]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_>, _collect: Collect) {
        let width = self.frame.width;
        let height = self.frame.height;
        let state = if self.pressed {
            WidgetState::Pressed
        } else {
            WidgetState::Normal
        };
        let size = self.theme.label_size_pixels;

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        items.push(quad(
            0.0,
            0.0,
            width,
            height,
            self.theme.fill(self.theme.accent, state),
        ));
        if !self.label.is_empty() {
            items.push(WidgetDrawItem::Text {
                x: self.theme.pad,
                y: text_origin_y(0.0, height, size),
                font_id: self.theme.font_id,
                text: self.label.clone(),
                size_pixels: size,
                color: self.theme.accent_text,
                clip: None,
            });
        }
        if self.focused {
            push_border(&mut items, width, height, 2.0, self.theme.accent);
        }
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList {
                intrinsic: None,
                items,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button() -> ButtonWidget {
        ButtonWidget {
            label: String::from("go"),
            theme: Theme::DEFAULT,
            frame: WidgetFrame {
                x: 10.0,
                y: 10.0,
                width: 40.0,
                height: 20.0,
            },
            focused: false,
            pressed: false,
        }
    }

    #[test]
    fn press_inside_then_release_inside_clicks() {
        let mut b = button();
        b.press_at(20.0, 20.0);
        assert!(b.pressed);
        assert!(
            b.release_at(30.0, 25.0),
            "press-inside then release-inside is a click"
        );
        assert!(!b.pressed, "disarmed after release");
    }

    #[test]
    fn press_inside_then_release_outside_cancels() {
        let mut b = button();
        b.press_at(20.0, 20.0);
        assert!(
            !b.release_at(200.0, 200.0),
            "a release that drifts off the button does not click",
        );
        assert!(!b.pressed, "disarmed even on a cancel");
    }

    #[test]
    fn press_outside_never_arms() {
        let mut b = button();
        b.press_at(200.0, 200.0);
        assert!(!b.pressed);
        assert!(
            !b.release_at(20.0, 20.0),
            "a release with no prior inside-press does not click"
        );
    }
}
