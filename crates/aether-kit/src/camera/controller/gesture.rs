//! Input to pose: what the controller's handlers record and the pure
//! functions that turn it into the camera's next pose.
//!
//! [`Input`] is the record: the held keys, the drag in progress and the
//! wheel steps not yet applied. The handlers only write to it. Once a tick,
//! [`Input::step`] takes the pose the gesture has reached and returns the
//! next one, using up what a step consumes: the wheel steps and the cursor
//! travel of an orbit.
//!
//! Each kind of motion is its own function from a pose to a pose. A value a
//! step pushed out of its range keeps the pose's own.

use core::f32::consts::FRAC_PI_2;
use core::mem;

use aether_kinds::{keycode, mouse_button};
use aether_math::{Quat, Vec2, Vec3};
use aether_render::ViewProjection;

use super::kinds::ControllerConfig;
use crate::camera::pose::pixel_ray;
use crate::camera::{Distance, Pitch, Pose, Yaw};

/// Wheel travel that counts as one step, in the pixels `MouseWheel` carries.
/// The window manager turns one line of a notched wheel into this many
/// pixels, so a notch is a step and a touchpad's pixel deltas are fractions
/// of one.
const WHEEL_PIXELS_PER_STEP: f32 = 40.0;

/// The keys the controller reads. A key's place here is its bit in [`Keys`].
const BOUND_KEYS: [u32; 10] = [
    keycode::KEY_W,
    keycode::KEY_UP,
    keycode::KEY_S,
    keycode::KEY_DOWN,
    keycode::KEY_A,
    keycode::KEY_LEFT,
    keycode::KEY_D,
    keycode::KEY_RIGHT,
    keycode::KEY_Q,
    keycode::KEY_E,
];

/// Which bound keys are held, one bit per physical key: W and the up arrow
/// both pan forward, and releasing one while the other is held keeps
/// panning. Opposite keys cancel to a zero axis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Keys(u16);

impl Keys {
    /// The bit of the bound key `code`, or `None` for a key that is not
    /// bound.
    fn bit(code: u32) -> Option<u16> {
        BOUND_KEYS.iter().position(|&bound| bound == code).map(|place| 1 << place)
    }

    /// Record `code` as held or released. A key that is not bound changes
    /// nothing.
    fn set(&mut self, code: u32, down: bool) {
        let Some(bit) = Self::bit(code) else {
            return;
        };

        if down {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    fn none(self) -> bool {
        self.0 == 0
    }

    fn held(self, code: u32) -> bool {
        Self::bit(code).is_some_and(|bit| self.0 & bit != 0)
    }

    /// `1.0` when either of `toward` is held, less `1.0` when either of
    /// `away` is.
    fn axis(self, toward: [u32; 2], away: [u32; 2]) -> f32 {
        let toward = toward.into_iter().any(|code| self.held(code));
        let away = away.into_iter().any(|code| self.held(code));

        f32::from(toward) - f32::from(away)
    }

    /// The ground pan the held keys ask for: `x` to the camera's right, `y`
    /// ahead of it.
    fn pan(self) -> Vec2 {
        let right = self.axis([keycode::KEY_D, keycode::KEY_RIGHT], [keycode::KEY_A, keycode::KEY_LEFT]);
        let ahead = self.axis([keycode::KEY_W, keycode::KEY_UP], [keycode::KEY_S, keycode::KEY_DOWN]);

        Vec2::new(right, ahead)
    }

    /// The turn the held keys ask for: E turns the yaw up, Q down.
    fn turn(self) -> f32 {
        f32::from(self.held(keycode::KEY_E)) - f32::from(self.held(keycode::KEY_Q))
    }
}

/// A point of the scene held under the cursor by a drag pan: the view the
/// drag began in, the target it began at, and the point that was grabbed.
#[derive(Debug, Clone)]
struct Grab {
    /// The camera's view when the drag began. Every ray of the drag is cast
    /// through it, so the camera moving under the drag does not move the
    /// grabbed point.
    view: ViewProjection,
    /// The pose's target when the drag began.
    target: Vec3,
    /// The normal of the plane the drag slides the target in: through
    /// `target`, facing the eye.
    normal: Vec3,
    /// Where the ray through the pressed pixel met that plane.
    anchor: Vec3,
}

impl Grab {
    /// Grab the point of the plane through `pose`'s target under `pixel` of
    /// `view`, the view the camera publishes for `pose`. `None` when no ray
    /// passes through the pixel or the ray misses the plane.
    fn take(pose: Pose, view: &ViewProjection, pixel: Vec2) -> Option<Self> {
        let target = pose.target;
        let normal = Quat::from_euler_yxz(pose.yaw.get(), pose.pitch.get(), 0.0) * Vec3::Z;

        plane_point(view, pixel, target, normal).map(|anchor| Self { view: view.clone(), target, normal, anchor })
    }

    /// The target that puts the grabbed point under `cursor`. Moving the
    /// target by `anchor - hit` slides the whole view by that much within
    /// the plane, which carries the point that was under the cursor, `hit`,
    /// onto the anchor.
    fn target_under(&self, cursor: Vec2) -> Option<Vec3> {
        plane_point(&self.view, cursor, self.target, self.normal).map(|hit| self.target + self.anchor - hit)
    }
}

/// Where the ray through `pixel` of `view` meets the plane through `point`
/// with `normal`. `None` when no ray passes through the pixel or the ray
/// does not meet the plane.
fn plane_point(view: &ViewProjection, pixel: Vec2, point: Vec3, normal: Vec3) -> Option<Vec3> {
    let ray = pixel_ray(view, pixel)?;

    ray.plane_hit(point, normal).map(|distance| ray.at(distance))
}

/// The mouse drag in progress. One button owns a drag from its press to its
/// release; a press of another button meanwhile is not a second drag.
#[derive(Debug, Clone, Default)]
enum Drag {
    #[default]
    None,
    /// The left button is held: the camera turns by the cursor's travel.
    Orbit {
        /// The cursor position the last step turned up to.
        turned_to: Vec2,
        cursor: Vec2,
    },
    /// The right or the middle button went down at `pressed` and no step has
    /// run since to grab the point under it.
    Pressed { button: u32, pressed: Vec2, cursor: Vec2 },
    /// The right or the middle button is held on a grabbed point.
    Pan { button: u32, grab: Grab, cursor: Vec2 },
}

impl Drag {
    /// The drag a press of `button` at `at` begins: none for a button that
    /// drags nothing.
    fn begun_by(button: u32, at: Vec2) -> Self {
        match button {
            mouse_button::LEFT => Self::Orbit { turned_to: at, cursor: at },
            mouse_button::RIGHT | mouse_button::MIDDLE => Self::Pressed { button, pressed: at, cursor: at },
            _ => Self::None,
        }
    }

    /// Whether `button` is the one this drag belongs to.
    fn owned_by(&self, button: u32) -> bool {
        match self {
            Self::None => false,
            Self::Orbit { .. } => button == mouse_button::LEFT,
            Self::Pressed { button: owner, .. } | Self::Pan { button: owner, .. } => *owner == button,
        }
    }

    fn move_cursor(&mut self, to: Vec2) {
        match self {
            Self::None => {}
            Self::Orbit { cursor, .. } | Self::Pressed { cursor, .. } | Self::Pan { cursor, .. } => *cursor = to,
        }
    }

    /// This drag with a pressed pan button's point grabbed, given the pose
    /// the step starts from and the view the camera publishes for it. With
    /// no view to cast through, or no point under the pixel, the press moves
    /// up to the cursor and the next step tries from there.
    fn grabbing(self, pose: Pose, view: Option<&ViewProjection>) -> Self {
        let Self::Pressed { button, pressed, cursor } = self else {
            return self;
        };
        let waiting = Self::Pressed { button, pressed: cursor, cursor };

        view.and_then(|view| Grab::take(pose, view, pressed)).map_or(waiting, |grab| Self::Pan { button, grab, cursor })
    }
}

/// What the controller's handlers have recorded and no step has used up.
#[derive(Debug, Clone, Default)]
pub(super) struct Input {
    keys: Keys,
    drag: Drag,
    /// Wheel steps since the last step, toward the scene positive.
    wheel_steps: f32,
}

impl Input {
    /// A key went down or came up.
    pub(super) fn key(&mut self, code: u32, down: bool) {
        self.keys.set(code, down);
    }

    /// A mouse button went down at `at`. It begins a drag when none is in
    /// progress.
    pub(super) fn press(&mut self, button: u32, at: Vec2) {
        if matches!(self.drag, Drag::None) {
            self.drag = Drag::begun_by(button, at);
        }
    }

    /// A mouse button came up. It ends the drag it began.
    pub(super) fn release(&mut self, button: u32) {
        if self.drag.owned_by(button) {
            self.drag = Drag::None;
        }
    }

    /// The cursor moved to `to`.
    pub(super) fn cursor(&mut self, to: Vec2) {
        self.drag.move_cursor(to);
    }

    /// The wheel moved `delta_pixels`, away from the user positive.
    pub(super) fn wheel(&mut self, delta_pixels: f32) {
        self.wheel_steps += delta_pixels / WHEEL_PIXELS_PER_STEP;
    }

    /// Whether nothing is held and nothing waits to be applied: no gesture
    /// is in progress.
    pub(super) fn at_rest(&self) -> bool {
        let no_drag = matches!(self.drag, Drag::None);
        let no_wheel = self.wheel_steps == 0.0;

        self.keys.none() && no_drag && no_wheel
    }

    /// `pose` after `elapsed_secs` of this input. `view` is the last view
    /// the camera published, which a drag pan casts its rays through; with
    /// none, a pan waits.
    ///
    /// The wheel and the turn keys apply first, then the drag. A drag pan
    /// holds the target, so the pan keys wait for it to end.
    pub(super) fn step(
        &mut self,
        pose: Pose,
        view: Option<&ViewProjection>,
        elapsed_secs: f32,
        config: &ControllerConfig,
    ) -> Pose {
        let wheel_steps = mem::take(&mut self.wheel_steps);
        let turn = self.keys.turn() * config.key_turn_radians_per_sec.get() * elapsed_secs;
        let pan = self.keys.pan();
        let pan_distances = config.key_pan_distances_per_sec.get() * elapsed_secs;
        let turned = turned(zoomed(pose, wheel_steps, config), turn);

        self.drag = mem::take(&mut self.drag).grabbing(pose, view);
        match &mut self.drag {
            Drag::None => ground_panned(turned, pan, pan_distances),
            Drag::Orbit { turned_to, cursor } => {
                let travel = *cursor - mem::replace(turned_to, *cursor);
                ground_panned(orbited(turned, travel, config.orbit_radians_per_pixel.get()), pan, pan_distances)
            }
            Drag::Pressed { .. } => turned,
            Drag::Pan { grab, cursor, .. } => {
                Pose { target: grab.target_under(*cursor).unwrap_or(turned.target), ..turned }
            }
        }
    }
}

/// `pose` with its yaw moved by `radians`.
fn turned(pose: Pose, radians: f32) -> Pose {
    Pose { yaw: Yaw::new(pose.yaw.get() + radians).unwrap_or(pose.yaw), ..pose }
}

/// `pose` turned by a left-drag of `travel` pixels. The scene follows the
/// cursor: a drag to the right turns the yaw down, and a drag down the
/// screen raises the eye, saturating where it looks straight down or up.
fn orbited(pose: Pose, travel: Vec2, radians_per_pixel: f32) -> Pose {
    let pitch = travel.y.mul_add(-radians_per_pixel, pose.pitch.get()).clamp(-FRAC_PI_2, FRAC_PI_2);

    Pose { pitch: Pitch::new(pitch).unwrap_or(pose.pitch), ..turned(pose, -travel.x * radians_per_pixel) }
}

/// `pose` with its distance multiplied once per wheel step and held within
/// the config's `nearest` and `farthest`. No steps leaves the distance as it
/// is, wherever a script put it.
fn zoomed(pose: Pose, steps: f32, config: &ControllerConfig) -> Pose {
    if steps == 0.0 {
        return pose;
    }
    let scaled = pose.distance.get() * config.zoom_per_wheel_step.get().powf(steps);
    let distance = scaled.max(config.nearest.get()).min(config.farthest.get());

    Pose { distance: Distance::new(distance).unwrap_or(pose.distance), ..pose }
}

/// `pose` with its target moved across the ground: `axes.x` to the camera's
/// right and `axes.y` ahead of it, by `distances` of the camera's distance.
/// At yaw 0 ahead is world `-Z` and right is world `+X`, and both turn with
/// the yaw, so the keys stay screen-relative. The direction is normalized,
/// so a diagonal covers the same ground as a cardinal.
fn ground_panned(pose: Pose, axes: Vec2, distances: f32) -> Pose {
    if axes == Vec2::ZERO {
        return pose;
    }
    let (sin_yaw, cos_yaw) = pose.yaw.get().sin_cos();
    let ahead = Vec3::new(-sin_yaw, 0.0, -cos_yaw);
    let right = Vec3::new(cos_yaw, 0.0, -sin_yaw);
    let direction = (right * axes.x + ahead * axes.y).normalize();

    Pose { target: pose.target + direction * (distances * pose.distance.get()), ..pose }
}

#[cfg(test)]
mod tests {
    use core::f32::consts::FRAC_PI_3;

    use aether_render::ViewportExtent;

    use super::*;
    use crate::camera::pose::view_projection;
    use crate::camera::{FieldOfView, Lens, OrthoExtent};

    const EXTENT: ViewportExtent = ViewportExtent { width: 1600, height: 900 };
    const TICK_SECS: f32 = 1.0 / 60.0;

    fn pose(target: Vec3, yaw: f32, pitch: f32, distance: f32) -> Pose {
        Pose {
            target,
            yaw: Yaw::new(yaw).expect("a finite yaw"),
            pitch: Pitch::new(pitch).expect("a pitch in range"),
            distance: Distance::new(distance).expect("a positive distance"),
        }
    }

    fn perspective() -> Lens {
        Lens::Perspective { fov: FieldOfView::new(FRAC_PI_3).expect("a field of view in range") }
    }

    fn orthographic() -> Lens {
        Lens::Orthographic { extent: OrthoExtent::new(0.5).expect("a positive extent") }
    }

    /// Where `view` draws the world point `point`, in physical pixels from
    /// the top-left corner.
    fn pixel_of(view: &ViewProjection, point: Vec3) -> Vec2 {
        let ndc = (view.projection * view.view).project_point(point).expect("a point in front of the eye projects");

        Vec2::new((ndc.x + 1.0) * 0.5 * 1600.0, (1.0 - ndc.y) * 0.5 * 900.0)
    }

    /// One step of `input` from `from` with no view, at the default config.
    fn stepped(input: &mut Input, from: Pose) -> Pose {
        input.step(from, None, TICK_SECS, &ControllerConfig::default())
    }

    /// A drag pan keeps the grabbed point under the cursor: after a
    /// right-drag from one pixel to another, the world point that was under
    /// the first pixel is drawn at the second, under a perspective and an
    /// orthographic lens, from a tilted and turned pose. A pan across the
    /// ground plane, or by pixels times a guessed scale, lands it elsewhere
    /// whenever the camera is not top-down or the cursor is off centre.
    #[test]
    fn a_drag_pan_keeps_the_grabbed_point_under_the_cursor() {
        let pressed = Vec2::new(420.0, 610.0);
        let released = Vec2::new(1130.0, 240.0);

        for lens in [perspective(), orthographic()] {
            let from = pose(Vec3::new(2.0, 0.5, -1.0), 0.7, -0.6, 8.0);
            let view = view_projection(from, lens, EXTENT);
            let grabbed = Grab::take(from, &view, pressed).expect("the pressed pixel is over the plane").anchor;
            assert!((pixel_of(&view, grabbed) - pressed).length() < 0.1, "the anchor is under the pressed pixel");

            let mut input = Input::default();
            input.press(mouse_button::RIGHT, pressed);
            input.cursor(released);
            let panned = input.step(from, Some(&view), TICK_SECS, &ControllerConfig::default());

            let drawn = pixel_of(&view_projection(panned, lens, EXTENT), grabbed);
            assert!((drawn - released).length() < 0.1, "{lens:?}: the grabbed point is drawn at {drawn:?}");
            assert_eq!((panned.yaw, panned.pitch, panned.distance), (from.yaw, from.pitch, from.distance));
        }
    }

    /// A held pan key covers the same share of the screen at any distance:
    /// one step of D from 3 units back and from 300 moves the old target to
    /// the same pixel. A pan at a fixed world rate crosses the view a
    /// hundred times slower from 300.
    #[test]
    fn a_key_pan_covers_the_same_share_of_the_screen_at_any_distance() {
        let shift_at = |distance: f32| -> Vec2 {
            let from = pose(Vec3::ZERO, 0.4, -0.5, distance);
            let mut input = Input::default();
            input.key(keycode::KEY_D, true);
            let panned = stepped(&mut input, from);

            pixel_of(&view_projection(panned, perspective(), EXTENT), from.target)
        };

        let close = shift_at(3.0);
        let far = shift_at(300.0);
        assert!((close.x - 800.0).abs() > 5.0, "the pan moved the view; the old target is drawn at {close:?}");
        assert!((close - far).length() < 0.1, "from 3 the old target is drawn at {close:?}, from 300 at {far:?}");
    }

    /// The wheel stops at the config's nearest and farthest distances. An
    /// unbounded zoom walks the eye into the target, where the depth planes
    /// collapse, or out past what the scene's depth can resolve.
    #[test]
    fn the_wheel_stops_at_the_nearest_and_the_farthest_distance() {
        let config = ControllerConfig::default();
        let from = pose(Vec3::ZERO, 0.0, -0.3, 5.0);

        let mut input = Input::default();
        input.wheel(1000.0 * WHEEL_PIXELS_PER_STEP);
        assert_eq!(stepped(&mut input, from).distance, config.nearest);

        input.wheel(-1000.0 * WHEEL_PIXELS_PER_STEP);
        assert_eq!(stepped(&mut input, from).distance, config.farthest);

        input.wheel(WHEEL_PIXELS_PER_STEP);
        let one_step_in = stepped(&mut input, from).distance.get();
        let expected = 5.0 * config.zoom_per_wheel_step.get();
        assert!((one_step_in - expected).abs() < 1e-5, "one step gave {one_step_in}");
        assert_eq!(stepped(&mut input, from), from, "a wheel step is applied once");
    }

    /// A diagonal pan covers the same ground as a cardinal one, with no
    /// `sqrt(2)` speed-up from adding the two axes.
    #[test]
    fn a_diagonal_key_pan_covers_the_same_ground_as_a_cardinal() {
        let from = pose(Vec3::ZERO, 0.9, -0.5, 4.0);
        let moved = |codes: &[u32]| -> f32 {
            let mut input = Input::default();
            for &code in codes {
                input.key(code, true);
            }
            (stepped(&mut input, from).target - from.target).length()
        };

        let cardinal = moved(&[keycode::KEY_W]);
        let diagonal = moved(&[keycode::KEY_W, keycode::KEY_D]);
        let one_tick_at_four_back = 4.0 * TICK_SECS;
        assert!((cardinal - one_tick_at_four_back).abs() < 1e-5, "a cardinal step covered {cardinal}");
        assert!((diagonal - cardinal).abs() < 1e-5, "a diagonal step covered {diagonal}, a cardinal {cardinal}");
    }

    /// A long drag down or up the screen stops at the poles and stays a
    /// pose. A pitch stepped past a quarter turn is refused by `Pitch`, and
    /// an orbit that kept the old pitch then would stick short of the pole.
    #[test]
    fn a_long_orbit_stops_at_the_poles() {
        let from = pose(Vec3::ZERO, 0.0, -0.3, 5.0);
        let mut input = Input::default();
        input.press(mouse_button::LEFT, Vec2::new(800.0, 450.0));

        input.cursor(Vec2::new(800.0, 100_450.0));
        let above = stepped(&mut input, from);
        assert_eq!(above.pitch, Pitch::DOWN);

        input.cursor(Vec2::new(800.0, -100_450.0));
        assert_eq!(stepped(&mut input, above).pitch, Pitch::new(FRAC_PI_2).expect("straight up is a pose"));
    }

    /// An orbit turns by the cursor's travel since the last step, once. A
    /// step that measured from the press every time would keep turning the
    /// camera while the button is held and the mouse is still.
    #[test]
    fn an_orbit_turns_by_the_cursor_s_travel_once() {
        let from = pose(Vec3::ZERO, 0.0, -0.3, 5.0);
        let mut input = Input::default();
        input.press(mouse_button::LEFT, Vec2::new(800.0, 450.0));
        input.cursor(Vec2::new(700.0, 450.0));

        let turned = stepped(&mut input, from);
        assert!((turned.yaw.get() - 0.5).abs() < 1e-5, "100 pixels to the left turned the yaw to {:?}", turned.yaw);
        assert_eq!(stepped(&mut input, turned), turned, "a held button with a still mouse turns nothing");
    }

    /// Two keys bound to one motion are tracked apart, and a drag ends only
    /// on its own button's release. One flag per motion would stop a pan
    /// when the arrow came up with W still down, and a drag that ended on
    /// any release would drop an orbit when a stray right-click came up.
    #[test]
    fn a_release_ends_only_what_its_own_press_began() {
        let mut input = Input::default();
        input.key(keycode::KEY_W, true);
        input.key(keycode::KEY_UP, true);
        input.key(keycode::KEY_UP, false);
        assert_eq!(input.keys.pan(), Vec2::new(0.0, 1.0));

        input.key(keycode::KEY_W, false);
        input.press(mouse_button::LEFT, Vec2::ZERO);
        input.press(mouse_button::RIGHT, Vec2::ZERO);
        input.release(mouse_button::RIGHT);
        assert!(!input.at_rest(), "the left button still holds its orbit");

        input.release(mouse_button::LEFT);
        assert!(input.at_rest());
    }
}
