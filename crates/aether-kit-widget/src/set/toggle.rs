// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (the full rationale is on the same allow in `lib.rs`).
#![allow(clippy::needless_pass_by_value)]

//! Boolean toggle control (issue 2926).
//!
//! A left press arms the switch and a release back inside toggles it once.
//! Enter toggles on its first key press; Space toggles on its matching release.
//! Focus loss, read-only state, and unavailability cancel every arm.

use alloc::string::String;
use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::mouse_button;
use aether_kinds::{Key, KeyRelease, MouseButton, MouseButtonRelease};
use aether_math::Rgba;

use crate::set::defaults::WidgetDefaults;
use crate::set::{ActivationArms, push_control_outlines, quad, reply_if_hidden, text_origin_y};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::Theme;
use crate::{
    Collect, SetWidgetState, ToggleChanged, ToggleConfig, WidgetControlState, WidgetDrawItem, WidgetDrawList,
    WidgetFrame,
};

/// A boolean switch with a track, knob, and optional label.
pub struct ToggleWidget {
    label: String,
    on: bool,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    arms: ActivationArms,
}

impl ToggleWidget {
    fn clear_arms(&mut self) {
        self.arms.clear();
    }

    fn pressed(&self) -> bool {
        self.arms.pressed()
    }

    fn toggle(&mut self) -> bool {
        self.on = !self.on;
        self.on
    }

    fn release_at(&mut self, x: f32, y: f32) -> Option<bool> {
        self.arms.release_pointer(&self.frame, self.state.can_mutate(), x, y).then(|| self.toggle())
    }

    fn press_key(&mut self, code: u32) -> Option<bool> {
        self.arms.press_key(self.state.can_mutate(), code).then(|| self.toggle())
    }

    fn release_key(&mut self, code: u32) -> Option<bool> {
        self.arms.release_key(self.state.can_mutate(), code).then(|| self.toggle())
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

    /// The colour the track is filled with: the accent when on, the raised
    /// surface when off.
    fn track_color(&self) -> Rgba {
        if self.on {
            self.theme.accent
        } else {
            self.theme.surface_raised
        }
    }

    /// The colour the knob is filled with — the one element that says which
    /// side the switch is on, so it has to be a **face** against the track it
    /// sits on rather than a hairline.
    ///
    /// The off knob drew in `outline`, which is the divider token `theme.rs`
    /// documents as "meant to be nearly invisible" — and it drew it on the
    /// `surface_raised` track, which is the very colour that token is
    /// documented as disappearing against: [`Theme::contrast_ratio`] measures
    /// the pair at 1.36. The switch read as an empty bar with no knob at all,
    /// and off-vs-on was carried only by the track changing colour.
    /// [`Theme::edge`] is the token derived for exactly this — `outline`
    /// carried toward the primary ink until it clears the 3.0 face-contrast
    /// target against that same raised surface — and it is the fix the button
    /// ladder's outlined rank already took. The on knob keeps `accent_text`,
    /// the ink paired with the accent the on track is filled with, at 9.96.
    fn knob_color(&self) -> Rgba {
        if self.on {
            self.theme.accent_text
        } else {
            self.theme.edge()
        }
    }

    /// The toggle's local draw: track, knob, optional label, and the focus /
    /// validation outlines every control shares.
    fn draw_items(&self) -> Vec<WidgetDrawItem> {
        let width = self.frame.width;
        let height = self.frame.height;
        let track_height = (height * 0.65).clamp(4.0, height.max(4.0));
        let track_width = (track_height * 1.8).min(width.max(0.0));
        let track_y = (height - track_height) * 0.5;
        let knob_size = (track_height - 4.0).max(1.0);
        let knob_x = if self.on {
            (track_width - knob_size - 2.0).max(2.0)
        } else {
            2.0
        };
        let state = self.state.theme_state(self.pressed());

        let mut items = Vec::new();
        items.push(quad(0.0, track_y, track_width, track_height, self.theme.fill(self.track_color(), state)));
        items.push(quad(
            knob_x,
            track_y + 2.0,
            knob_size,
            knob_size,
            self.theme.fill(self.knob_color(), self.state.supporting_theme_state(false)),
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
        items
    }
}

impl WidgetDefaults for ToggleWidget {
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

/// A toggle widget. Spawned inline by a panel root with a [`ToggleConfig`];
/// reports [`ToggleChanged`] after each completed activation.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
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
            arms: ActivationArms::default(),
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
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, press: MouseButton) {
        self.arms.press_mouse_button(&self.frame, self.state.can_mutate(), press);
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
    use aether_kinds::keycode::{KEY_ENTER, KEY_SPACE};

    fn toggle() -> ToggleWidget {
        ToggleWidget {
            label: String::from("snap"),
            on: false,
            theme: Theme::DEFAULT,
            frame: WidgetFrame { x: 10.0, y: 20.0, width: 100.0, height: 24.0 },
            state: InteractionState::new(WidgetControlState::default()),
            arms: ActivationArms::default(),
        }
    }

    /// The track's fill and the knob's, in draw order — the first two quads
    /// the toggle pushes, before any outline.
    fn track_and_knob(switch: &ToggleWidget) -> (Rgba, Rgba) {
        let items = switch.draw_items();
        let mut fills = items.iter().filter_map(|item| match item {
            WidgetDrawItem::Quad { color, .. } => Some(*color),
            _ => None,
        });
        (fills.next().expect("the track is drawn first"), fills.next().expect("the knob is drawn on it"))
    }

    // Tripwire: the knob is the one element that says which side the switch is
    // on, so it has to be a face against the track it sits on. The off knob
    // borrowed `outline` — the divider token `theme.rs` documents as "meant to
    // be nearly invisible" and pins *below* the 3.0 face target on purpose —
    // and drew it on the `surface_raised` track at 1.29, so the switch read as
    // an empty bar with no knob at all. The mix behind `Theme::edge` is solved
    // in `f32`, so a face landing exactly on the target measures back a few
    // parts in ten million under it; the tolerance is that rounding and
    // nothing else.
    #[test]
    fn the_knob_reads_as_a_face_against_its_own_track_in_both_positions() {
        let floor = 3.0 - 1e-4;
        let mut switch = toggle();

        let (track, knob) = track_and_knob(&switch);
        let off = Theme::contrast_ratio(knob, track);
        assert!(off >= floor, "the off knob reads at only {off} against its own track");

        switch.on = true;
        let (track, knob) = track_and_knob(&switch);
        let on = Theme::contrast_ratio(knob, track);
        assert!(on >= floor, "the on knob reads at only {on} against its own track");
    }

    #[test]
    fn pointer_activation_toggles_once_and_release_outside_cancels() {
        let mut toggle = toggle();
        toggle.arms.press_pointer(&toggle.frame, toggle.state.can_mutate(), 20.0, 30.0);
        assert_eq!(toggle.release_at(20.0, 30.0), Some(true));
        assert_eq!(toggle.release_at(20.0, 30.0), None, "an unarmed release cannot toggle twice");

        toggle.arms.press_pointer(&toggle.frame, toggle.state.can_mutate(), 20.0, 30.0);
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
        toggle.arms.press_pointer(&toggle.frame, toggle.state.can_mutate(), 20.0, 30.0);
        toggle.press_key(KEY_SPACE);
        let read_only = WidgetControlState { read_only: true, ..WidgetControlState::default() };
        assert!(toggle.adopt_control_state(read_only));
        assert!(!toggle.arms.pointer_pressed);
        assert_eq!(toggle.arms.keyboard_arm, None);
        assert_eq!(toggle.release_at(20.0, 30.0), None);
        assert_eq!(toggle.press_key(KEY_ENTER), None);

        let disabled = WidgetControlState { enabled: false, ..WidgetControlState::default() };
        assert!(toggle.adopt_control_state(disabled));
        toggle.arms.press_pointer(&toggle.frame, toggle.state.can_mutate(), 20.0, 30.0);
        assert!(!toggle.arms.pointer_pressed);
    }
}
