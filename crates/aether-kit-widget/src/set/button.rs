// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The momentary push button (issue 2660).
//!
//! A left press inside the button arms it (the root holds the pointer
//! capture); the matching release fires [`ButtonClicked`] only if it lands
//! back inside — a press-then-release-inside, so a press that drags off and
//! releases elsewhere cancels. The armed state draws the pressed overlay.
//!
//! The label sits centered in the frame, both axes. Centering it horizontally
//! needs the label's real width, so the button drives the same single-flight
//! [`FontMetricsRequest`](aether_text::FontMetricsRequest) the text controls
//! do and keeps the left-padded draw until the measurement lands — a guessed
//! width would center the label wrong and then visibly jump. The measurement
//! also gives the button its intrinsic size, so a layout can size a slot to
//! the label it holds.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::mouse_button;
use aether_kinds::{Key, KeyRelease, MouseButton, MouseButtonRelease};
use aether_text::FontMetricsResult;

use crate::set::defaults::WidgetDefaults;
use crate::set::{
    ActivationArms, accept_font_metrics_result, apply_text_theme, centered_text_x, measured_text_width,
    pump_text_font_metrics, push_border, quad, reply_if_hidden, text_origin_y,
};
use crate::state::{InteractionState, emit_state_changed};
use crate::text_edit::FontMetricsAdapter;
use crate::theme::{SetTheme, Theme};
use crate::{
    ButtonClicked, ButtonConfig, Collect, SetWidgetState, WidgetControlState, WidgetDrawItem, WidgetDrawList,
    WidgetFrame,
};

/// A momentary push button. Holds its label plus the cached theme / frame /
/// focus, the armed (`pressed`) state, and the single-flight font-metrics
/// adapter that feeds the centered label and the reported intrinsic size.
pub struct ButtonWidget {
    label: String,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Shared pointer/keyboard activation state; a release-inside fires the click.
    arms: ActivationArms,
    /// Single-flight exact metrics for the active theme font.
    font_metrics: FontMetricsAdapter,
}

impl ButtonWidget {
    /// The label's measured pixel width, `None` until the theme font's
    /// metrics resolve.
    ///
    /// This is the sum of the glyphs' advances, not their ink bounds — the
    /// metric table carries no ink extents — so a single-glyph label like `+`
    /// is centered on its advance and its ink can sit a hair off the frame's
    /// optical center.
    fn measured_label_width(&self) -> Option<f32> {
        self.font_metrics
            .resolved()
            .map(|metrics| measured_text_width(metrics, &self.label, self.theme.label_size_pixels))
    }

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

impl WidgetDefaults for ButtonWidget {
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
        self.clear_arms();
    }
}

/// A push-button widget. Spawned inline by a panel root with a
/// [`ButtonConfig`]; reports [`ButtonClicked`] up on a completed click.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `ButtonConfig` again to relabel or restyle it in place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for ButtonWidget {
    type Config = ButtonConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.button";

    fn init(config: ButtonConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let desired_font_id = config.theme.font_id;
        Ok(ButtonWidget {
            label: config.label,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            arms: ActivationArms::default(),
            font_metrics: FontMetricsAdapter::new(desired_font_id),
        })
    }

    /// Kick off the font-metrics request for the initial theme font.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Relabel / restyle in place from a re-sent config, and request metrics
    /// for the new theme font.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: ButtonConfig) {
        self.label = config.label;
        self.font_metrics.set_desired(config.theme.font_id);
        self.theme = config.theme;
        self.apply_control_state(ctx, config.state);
        pump_text_font_metrics(ctx, &mut self.font_metrics);
    }

    /// Restyle: adopt the fanned theme and request metrics for its font.
    #[handler::single]
    fn on_set_theme(&mut self, ctx: &mut WasmCtx<'_>, set: SetTheme) {
        apply_text_theme(ctx, &mut self.font_metrics, &mut self.theme, set.theme);
    }

    /// Install a font-metrics reply; the next `Collect` centers the label.
    #[handler::single]
    fn on_font_metrics_result(&mut self, ctx: &mut WasmCtx<'_>, result: FontMetricsResult) {
        accept_font_metrics_result(ctx, &mut self.font_metrics, result);
    }

    /// Read-only and validation are deliberately inapplicable to a momentary
    /// button; visibility/enabled still control routing and presentation.
    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
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
    /// armed), the centered label, and a focus ring, plus the intrinsic size
    /// the label asks for once it is measured.
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
        let measured = self.measured_label_width();

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        items.push(quad(0.0, 0.0, width, height, self.theme.fill(self.theme.accent, theme_state)));
        if !self.label.is_empty() {
            items.push(WidgetDrawItem::Text {
                // Left-padded until the measurement lands, centered after.
                x: measured.map_or(self.theme.pad, |text_width| centered_text_x(width, text_width)),
                y: text_origin_y(0.0, height, size),
                font_id: self.theme.font_id,
                text: self.label.clone(),
                size_pixels: size,
                color: self.theme.fill(self.theme.accent_text, theme_state),
                clip: None,
            });
        }
        // Keyboard focus only: the button a pointer just pressed shows its
        // press, and a ring left over from the click says nothing more.
        if self.state.focus_visible() {
            push_border(&mut items, width, height, 2.0, self.theme.accent);
        }

        // The label plus one pad each side, at the theme's row height: what a
        // layout needs to size a slot to this button's own label.
        let intrinsic = measured.map(|text_width| [self.theme.pad.mul_add(2.0, text_width), self.theme.row_height]);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { intrinsic, items, overlay: Vec::new() });
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
            font_metrics: FontMetricsAdapter::new(0),
        }
    }

    #[test]
    fn a_label_is_centered_at_every_frame_width_and_the_intrinsic_reserves_a_pad_each_side() {
        // Tripwire: the owner's asymmetric-Remove-button note. Whatever frame
        // the consumer hands down, the margins either side of the label must
        // be equal — the old `.max(pad)` clamp made them 8 and 2 on a frame a
        // few pixels under the intrinsic width.
        for width in [100.0_f32, 90.0, 84.0, 80.0, 41.0] {
            let text_width = 40.0_f32;
            let x = centered_text_x(width, text_width);
            let right_margin = width - (x + text_width);
            assert!((x - right_margin).abs() < 1e-4, "frame {width}: margins {x} / {right_margin} are not equal");
        }
        assert_eq!(centered_text_x(100.0, 40.0), 30.0, "even margins either side");
        assert_eq!(centered_text_x(40.0, 60.0), 0.0, "a label wider than its whole frame keeps its start visible");
    }

    #[test]
    fn the_reported_intrinsic_width_is_the_label_plus_one_pad_each_side() {
        // Tripwire: a layout sizes a slot from this number, so it has to be
        // exactly what `centered_text_x` then reproduces as equal margins.
        let button = button();
        let text_width = 40.0_f32;
        let intrinsic = button.theme.pad.mul_add(2.0, text_width);
        assert_eq!(centered_text_x(intrinsic, text_width), button.theme.pad, "the intrinsic width pads exactly once");
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
