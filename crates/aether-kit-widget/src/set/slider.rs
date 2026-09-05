// `#[handler]` methods take their decoded mail by value per the ADR-0033
// dispatch ABI (see `widget/mod.rs`).
#![allow(clippy::needless_pass_by_value)]

//! The horizontal value slider (issue 2660).
//!
//! A left press within the track begins a drag (the root holds the pointer
//! capture), setting the value from the cursor's x mapped through the cached
//! frame and streaming an uncommitted [`SliderChanged`]; each move updates it;
//! the release commits. Arrow keys nudge by `step` while focused and commit at
//! once. The value maps `min..=max` snapped to `step`; the consumer maps the
//! reported `f32` onto its own domain.

use alloc::vec::Vec;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::keycode::{KEY_DOWN, KEY_LEFT, KEY_RIGHT, KEY_UP};
use aether_kinds::mouse_button;
use aether_kinds::{Key, MouseButton, MouseButtonRelease, MouseMove};

use crate::set::defaults::WidgetDefaults;
use crate::set::{push_control_outlines, quad, reply_if_hidden};
use crate::state::{InteractionState, emit_state_changed};
use crate::theme::Theme;
use crate::{
    Collect, SetWidgetState, SliderChanged, SliderConfig, WidgetControlState, WidgetDrawItem, WidgetDrawList,
    WidgetFrame,
};

/// The `min..=max` a slider actually runs over, normalised once from the raw
/// [`SliderConfig`] pair.
///
/// `f32::clamp` asserts `min <= max` and rejects a NaN bound, so the config's
/// two bare `f32`s reached it unchecked: a descending axis (`min: 1.0, max:
/// 0.0`) or a bound computed from an empty data set trapped the widget actor
/// at `init` and again on every press, move, release and arrow key.
///
/// A crossed pair is the same interval written backwards, so it swaps. A
/// non-finite end names no interval at all, so the slider **degrades to the
/// single value** its finite end carries (`0.0` when neither end is finite):
/// the zero-span paths already exist and already behave — `fill_fraction`
/// draws an empty track, `nudge_amount` is `0.0`, and the pointer maps
/// everywhere to that one value — so a malformed range yields a slider that
/// does not move rather than one that is not there.
fn normalized_bounds(min: f32, max: f32) -> (f32, f32) {
    match (min.is_finite(), max.is_finite()) {
        (true, true) if min <= max => (min, max),
        (true, true) => (max, min),
        (true, false) => (min, min),
        (false, true) => (max, max),
        (false, false) => (0.0, 0.0),
    }
}

/// The snap increment a slider actually uses. A non-finite `step` is no
/// increment: an infinite one drove `steps.mul_add(step, min)` through
/// `0.0 * inf` and mailed a NaN value down, so it leaves the slider
/// continuous exactly as a non-positive step does.
fn normalized_step(step: f32) -> f32 {
    if step.is_finite() {
        step
    } else {
        0.0
    }
}

/// A horizontal value slider. Local draw is a track with a fill from the left
/// to the current value, plus a focus ring when focused.
pub struct SliderWidget {
    /// The low end of the normalised range — see [`normalized_bounds`]. Never
    /// NaN, and never above [`SliderWidget::max`].
    min: f32,
    max: f32,
    /// The normalised snap increment — see [`normalized_step`]. Never NaN and
    /// never infinite; `0.0` or less leaves the value continuous.
    step: f32,
    value: f32,
    theme: Theme,
    frame: WidgetFrame,
    state: InteractionState,
    /// Whether a drag is in flight (a left press landed and no release has
    /// cleared it). Streams uncommitted values while set.
    dragging: bool,
}

impl SliderWidget {
    /// Clamp `raw` into `min..=max` and snap it to the nearest `step`. A
    /// non-positive `step` leaves the value continuous (clamp only).
    ///
    /// The bounds are the normalised pair, so the clamp cannot trap. A NaN
    /// `raw` — a `SliderConfig` whose `initial` came back NaN — has no place
    /// on the axis and takes `min`, so this is total: a finite value out for
    /// any value in.
    fn snapped(&self, raw: f32) -> f32 {
        let clamped = if raw.is_nan() {
            self.min
        } else {
            raw.clamp(self.min, self.max)
        };
        if self.step > 0.0 {
            let steps = ((clamped - self.min) / self.step).round();
            steps.mul_add(self.step, self.min).clamp(self.min, self.max)
        } else {
            clamped
        }
    }

    /// The value a local-x within the track maps to: the fraction of the
    /// track width, remapped across `min..=max` and snapped. Owned math — the
    /// unit tests pin it.
    fn value_from_local_x(&self, local_x: f32) -> f32 {
        let width = self.frame.width.max(1.0);
        let frac = (local_x / width).clamp(0.0, 1.0);
        self.snapped(frac.mul_add(self.max - self.min, self.min))
    }

    /// The value a window-space pointer x maps to, via the cached frame's
    /// left edge.
    fn value_from_pointer_x(&self, pointer_x: f32) -> f32 {
        self.value_from_local_x(pointer_x - self.frame.x)
    }

    /// The nudge one arrow press applies: `step` when set, else a hundredth of
    /// the range so a continuous slider still moves.
    fn nudge_amount(&self) -> f32 {
        if self.step > 0.0 {
            self.step
        } else {
            (self.max - self.min) * 0.01
        }
    }

    /// The value as a `0.0..=1.0` fraction of the range, for the fill width.
    fn fill_fraction(&self) -> f32 {
        let span = self.max - self.min;
        if span > 0.0 {
            ((self.value - self.min) / span).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Emit the current value up to the panel root, `committed` distinguishing
    /// a drag stream from a settled value.
    fn emit(&self, ctx: &WasmCtx<'_>, committed: bool) {
        if let Some(parent) = ctx.parent() {
            parent.send(&SliderChanged { value: self.value, committed });
        }
    }

    fn apply_control_state(&mut self, ctx: &WasmCtx<'_>, next: WidgetControlState) {
        if self.state.replace(next) {
            if !self.state.can_mutate() {
                self.dragging = false;
            }
            emit_state_changed(ctx, &self.state);
        }
    }
}

impl WidgetDefaults for SliderWidget {
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
        self.dragging = false;
    }
}

/// A slider widget. Spawned inline by a panel root with a [`SliderConfig`];
/// reports [`SliderChanged`] up as it is dragged or nudged.
///
/// # Agent
/// Not loaded directly — the panel root spawns it as an inline child. Send it
/// its `SliderConfig` again to reconfigure the range or theme in place.
#[actor(instanced, composable, handler_set(WidgetDefaults))]
impl WasmActor for SliderWidget {
    type Config = SliderConfig;
    const NAMESPACE: &'static str = "aether.kit.widget.slider";

    fn init(config: SliderConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let (min, max) = normalized_bounds(config.min, config.max);
        let mut slider = SliderWidget {
            min,
            max,
            step: normalized_step(config.step),
            value: config.initial,
            theme: config.theme,
            state: InteractionState::new(config.state),
            frame: WidgetFrame { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            dragging: false,
        };
        slider.value = slider.snapped(config.initial);
        Ok(slider)
    }

    /// Reconfigure the range / step / theme in place, re-clamping the current
    /// value into the new range. `initial` is ignored on re-config (it seeds
    /// the value only at init) so a restyle does not jump the value.
    ///
    /// A live reconfigure normalises the range the same way `init` does: it is
    /// the second path a malformed pair arrives on, and it re-clamps against
    /// the new bounds immediately.
    #[handler::single]
    fn on_config(&mut self, ctx: &mut WasmCtx<'_>, config: SliderConfig) {
        let (min, max) = normalized_bounds(config.min, config.max);
        self.min = min;
        self.max = max;
        self.step = normalized_step(config.step);
        self.theme = config.theme;
        self.value = self.snapped(self.value);
        self.apply_control_state(ctx, config.state);
    }

    #[handler::single]
    fn on_set_widget_state(&mut self, ctx: &mut WasmCtx<'_>, set: SetWidgetState) {
        self.apply_control_state(ctx, set.state);
    }

    /// A left press begins a drag and sets the value from the cursor.
    #[handler::single]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if press.button != mouse_button::LEFT || !self.state.can_mutate() {
            return;
        }
        self.dragging = true;
        self.value = self.value_from_pointer_x(press.x);
        self.emit(ctx, false);
    }

    /// A move while dragging updates the value and streams it uncommitted.
    #[handler::single]
    fn on_mouse_move(&mut self, ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if !self.dragging || !self.state.can_mutate() {
            return;
        }
        self.value = self.value_from_pointer_x(moved.x);
        self.emit(ctx, false);
    }

    /// A left release ends the drag and commits the value.
    #[handler::single]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if release.button != mouse_button::LEFT || !self.dragging || !self.state.can_mutate() {
            return;
        }
        self.value = self.value_from_pointer_x(release.x);
        self.dragging = false;
        self.emit(ctx, true);
    }

    /// Arrow keys nudge by `step` and commit at once (focused only — the root
    /// forwards keyboard mail to the focused child).
    #[handler::single]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if !self.state.can_mutate() {
            return;
        }
        let delta = match key.code {
            KEY_LEFT | KEY_DOWN => -self.nudge_amount(),
            KEY_RIGHT | KEY_UP => self.nudge_amount(),
            _ => return,
        };
        self.value = self.snapped(self.value + delta);
        self.emit(ctx, true);
    }

    /// Reply the slider's local draw: track, fill, and a focus ring when
    /// focused.
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
        let theme_state = self.state.theme_state(self.dragging);
        let track_state = self.state.supporting_theme_state(false);
        let track_height = (height * 0.35).clamp(4.0, height.max(4.0));
        let track_y = (height - track_height) * 0.5;

        let mut items: Vec<WidgetDrawItem> = Vec::new();
        items.push(quad(0.0, track_y, width, track_height, self.theme.fill(self.theme.surface_raised, track_state)));
        items.push(quad(
            0.0,
            track_y,
            width * self.fill_fraction(),
            track_height,
            self.theme.fill(self.theme.accent, theme_state),
        ));
        push_control_outlines(&mut items, width, height, &self.state, &self.theme);
        if let Some(parent) = ctx.parent() {
            parent.send(&WidgetDrawList { content_height: None, intrinsic: None, items, overlay: Vec::new() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WidgetControlState;

    fn slider(min: f32, max: f32, step: f32, initial: f32) -> SliderWidget {
        SliderWidget {
            min,
            max,
            step,
            value: initial,
            theme: Theme::DEFAULT,
            state: InteractionState::new(WidgetControlState::default()),
            frame: WidgetFrame { x: 100.0, y: 0.0, width: 200.0, height: 24.0 },
            dragging: false,
        }
    }

    #[test]
    fn pointer_maps_across_the_track_and_snaps_to_step() {
        let s = slider(0.0, 100.0, 10.0, 0.0);
        // Left edge of the frame → min.
        assert_eq!(s.value_from_pointer_x(100.0), 0.0);
        // Right edge → max.
        assert_eq!(s.value_from_pointer_x(300.0), 100.0);
        // Middle → 50, already on a step boundary.
        assert_eq!(s.value_from_pointer_x(200.0), 50.0);
        // A point at 27% of the track (x=154 → local 54 → 27.0 raw) snaps to 30.
        assert_eq!(s.value_from_pointer_x(154.0), 30.0);
    }

    #[test]
    fn pointer_past_the_edges_clamps_into_range() {
        let s = slider(-1.0, 1.0, 0.0, 0.0);
        assert_eq!(s.value_from_pointer_x(50.0), -1.0, "left of the track clamps to min");
        assert_eq!(s.value_from_pointer_x(500.0), 1.0, "right of the track clamps to max");
    }

    #[test]
    fn continuous_slider_does_not_snap() {
        let s = slider(0.0, 1.0, 0.0, 0.0);
        // Quarter of the track → 0.25, no snapping.
        assert_eq!(s.value_from_pointer_x(150.0), 0.25);
    }

    #[test]
    fn nudge_amount_falls_back_to_a_hundredth_of_range() {
        assert_eq!(slider(0.0, 100.0, 5.0, 0.0).nudge_amount(), 5.0);
        assert_eq!(slider(0.0, 100.0, 0.0, 0.0).nudge_amount(), 1.0);
    }

    /// A slider built the way `init` builds one — through the same bounds and
    /// step normalisation — so a test drives the config path rather than a
    /// hand-written pair the widget could never hold.
    fn configured(min: f32, max: f32, step: f32, initial: f32) -> SliderWidget {
        let (min, max) = normalized_bounds(min, max);
        let mut built = slider(min, max, normalized_step(step), initial);
        built.value = built.snapped(initial);
        built
    }

    // Tripwire: `f32::clamp` asserts `min <= max` and rejects a NaN bound, so
    // a `SliderConfig` carrying a descending axis or a bound computed from an
    // empty data set trapped the wasm actor at `init` and on every pointer
    // event — the panel lost the child rather than drawing a clamped slider.
    // Pinning the normalised pair is what keeps the clamp reachable only with
    // an ordered, finite one.
    #[test]
    fn an_inverted_or_non_finite_config_range_does_not_trap_the_widget() {
        let descending = configured(1.0, 0.0, 0.0, 0.5);
        assert_eq!((descending.min, descending.max), (0.0, 1.0), "a crossed pair is one interval, written backwards");
        assert_eq!(descending.value, 0.5);
        assert_eq!(descending.value_from_pointer_x(300.0), 1.0, "the right edge still maps to the high end");

        let half_known = configured(f32::NAN, 10.0, 0.0, 0.5);
        assert_eq!((half_known.min, half_known.max), (10.0, 10.0), "a range with no low end is one value, not a trap");
        assert_eq!(half_known.value, 10.0);
        assert_eq!(half_known.value_from_pointer_x(150.0), 10.0, "a single-valued slider reports it everywhere");
        assert_eq!(half_known.nudge_amount(), 0.0);
        assert_eq!(half_known.fill_fraction(), 0.0);

        let unknown = configured(f32::NAN, f32::NAN, f32::NAN, f32::NAN);
        assert_eq!((unknown.min, unknown.max, unknown.step, unknown.value), (0.0, 0.0, 0.0, 0.0));

        let unbounded_step = configured(0.0, 100.0, f32::INFINITY, 30.0);
        assert_eq!(unbounded_step.step, 0.0, "an infinite step is no step, not a NaN value");
        assert_eq!(unbounded_step.value, 30.0);
    }

    #[test]
    fn fill_fraction_tracks_value_within_range() {
        let mut s = slider(0.0, 100.0, 0.0, 0.0);
        assert_eq!(s.fill_fraction(), 0.0);
        s.value = 25.0;
        assert_eq!(s.fill_fraction(), 0.25);
        s.value = 200.0;
        assert_eq!(s.fill_fraction(), 1.0, "an out-of-range value clamps the fill");
    }
}
