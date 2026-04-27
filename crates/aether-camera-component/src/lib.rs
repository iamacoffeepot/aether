//! Orbit camera that drives the substrate's `aether.camera` sink.
//!
//! Yaws around a configurable world-space target at a configurable
//! distance, updating each tick. Aspect ratio tracks the live window
//! size via `WindowSize`, so the projection stays square regardless
//! of window shape.
//!
//! Runtime-controllable via `aether.camera.orbit.set_*` mail kinds:
//! distance, pitch, yaw, speed (auto-rotation rate), FoV, and the
//! world-space target point. Sending `OrbitSetSpeed { rad_per_tick:
//! 0.0 }` freezes the camera at its current yaw so an absolute
//! `OrbitSetYaw` holds; any non-zero speed resumes auto-rotation
//! from whatever the current yaw is.
//!
//! WASD / arrow-key input is deliberately deferred — winit's
//! `KeyCode as u32` discriminants aren't a stable named contract
//! through `aether-kinds`, so hardcoding raw numbers here would be
//! fragile. Control mail is the current driver surface.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{
    Camera, OrbitSetDistance, OrbitSetFov, OrbitSetPitch, OrbitSetSpeed, OrbitSetTarget,
    OrbitSetYaw, Tick, WindowSize,
};
use aether_math::{Mat4, PI, Quat, Vec3};

/// Default orbit radius — how far the eye sits from the target.
const DEFAULT_DISTANCE: f32 = 3.0;
/// Default yaw delta per tick. At 60 fps this is a full revolution
/// every ~12 seconds — slow enough to watch, fast enough to obviously
/// be moving.
const DEFAULT_SPEED: f32 = PI / 360.0;
/// Default downward pitch so the camera looks slightly down at the
/// target, giving a sense of depth. Positive pitch = tilt toward +Y.
const DEFAULT_PITCH: f32 = 0.3;
/// Default vertical field of view, 60°.
const DEFAULT_FOV: f32 = PI / 3.0;
const Z_NEAR: f32 = 0.1;
const Z_FAR: f32 = 100.0;
/// Fallback aspect ratio used before the first `WindowSize` arrives.
/// The substrate re-pulses `WindowSize` every tick so this is only
/// visible for a single frame after load.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;

pub struct Orbit {
    camera: Sink<Camera>,
    yaw: f32,
    pitch: f32,
    distance: f32,
    speed: f32,
    fov: f32,
    target: Vec3,
    aspect: f32,
}

/// Orbit camera that publishes a view*projection matrix to the
/// substrate every tick. Configurable at runtime via the
/// `aether.camera.orbit.set_*` mail family.
///
/// # Agent
/// Load this alongside a drawing component (e.g. `aether-hello-component`)
/// to see its geometry viewed through a moving 3D camera. To poke the
/// camera, send control mail to this component's mailbox:
///
/// - `OrbitSetDistance { distance }` — zoom (default 3.0)
/// - `OrbitSetPitch { pitch }` — vertical tilt radians (default 0.3)
/// - `OrbitSetYaw { yaw }` — absolute yaw radians; pair with
///   `OrbitSetSpeed { 0.0 }` to pin
/// - `OrbitSetSpeed { rad_per_tick }` — auto-rotation rate;
///   `0.0` freezes
/// - `OrbitSetFov { fov_y_rad }` — vertical field of view
/// - `OrbitSetTarget { x, y, z }` — world-space pivot
///
/// Use `capture_frame` between sends to verify each change.
#[handlers]
impl Component for Orbit {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Orbit {
            camera: ctx.resolve_sink::<Camera>("aether.sink.camera"),
            yaw: 0.0,
            pitch: DEFAULT_PITCH,
            distance: DEFAULT_DISTANCE,
            speed: DEFAULT_SPEED,
            fov: DEFAULT_FOV,
            target: Vec3::ZERO,
            aspect: DEFAULT_ASPECT,
        }
    }

    /// Advance the orbit and publish a fresh `view_proj`.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        self.yaw += self.speed;
        if self.yaw > PI * 2.0 {
            self.yaw -= PI * 2.0;
        } else if self.yaw < 0.0 {
            self.yaw += PI * 2.0;
        }

        // Orbit `(0, 0, distance)` by yaw around Y and pitch around
        // local X, then translate to the target. YXZ order: yaw first,
        // then pitch in the yaw-rotated frame — identical to a
        // first-person camera's look direction math.
        let orientation = Quat::from_euler_yxz(self.yaw, self.pitch, 0.0);
        let eye = self.target + orientation * Vec3::new(0.0, 0.0, self.distance);

        let view = Mat4::look_at_rh(eye, self.target, Vec3::Y);
        let proj = Mat4::perspective_rh(self.fov, self.aspect, Z_NEAR, Z_FAR);
        let view_proj = proj * view;

        ctx.send(
            &self.camera,
            &Camera {
                view_proj: view_proj.to_cols_array(),
            },
        );
    }

    /// Update aspect from live window size. Avoids division by zero
    /// on degenerate dimensions (0-height windows during restore, etc).
    ///
    /// # Agent
    /// Publish-subscribe; the substrate pulses this every tick, you
    /// don't need to drive it.
    #[handler]
    fn on_window_size(&mut self, _ctx: &mut Ctx<'_>, size: WindowSize) {
        if size.width > 0 && size.height > 0 {
            self.aspect = size.width as f32 / size.height as f32;
        }
    }

    /// Set orbit distance from target.
    ///
    /// # Agent
    /// Sending larger values zooms out; smaller zooms in. Visible
    /// next frame.
    #[handler]
    fn on_set_distance(&mut self, _ctx: &mut Ctx<'_>, msg: OrbitSetDistance) {
        self.distance = msg.distance;
    }

    /// Set vertical tilt (radians).
    ///
    /// # Agent
    /// Positive tilts the eye up (camera looks down); negative tilts
    /// the eye down (camera looks up). `±π/2` are degenerate.
    #[handler]
    fn on_set_pitch(&mut self, _ctx: &mut Ctx<'_>, msg: OrbitSetPitch) {
        self.pitch = msg.pitch;
    }

    /// Set absolute yaw (radians). Auto-advance keeps ticking from
    /// the new value on subsequent frames.
    ///
    /// # Agent
    /// Combine with `OrbitSetSpeed { 0.0 }` to pin the camera to a
    /// specific yaw — otherwise the next tick advances it.
    #[handler]
    fn on_set_yaw(&mut self, _ctx: &mut Ctx<'_>, msg: OrbitSetYaw) {
        self.yaw = msg.yaw;
    }

    /// Set auto-rotation rate (radians per tick). `0.0` freezes.
    ///
    /// # Agent
    /// Negative reverses direction. Combined with `OrbitSetYaw`,
    /// pins the camera at a chosen angle.
    #[handler]
    fn on_set_speed(&mut self, _ctx: &mut Ctx<'_>, msg: OrbitSetSpeed) {
        self.speed = msg.rad_per_tick;
    }

    /// Set vertical field of view (radians).
    ///
    /// # Agent
    /// Typical values `π/4` (45°, tight) to `π/2` (90°, wide).
    /// Above `π` the view inverts; below `0` is degenerate.
    #[handler]
    fn on_set_fov(&mut self, _ctx: &mut Ctx<'_>, msg: OrbitSetFov) {
        self.fov = msg.fov_y_rad;
    }

    /// Set the world-space point the camera orbits around.
    ///
    /// # Agent
    /// Re-target per tick to follow a moving object.
    #[handler]
    fn on_set_target(&mut self, _ctx: &mut Ctx<'_>, msg: OrbitSetTarget) {
        self.target = Vec3::new(msg.x, msg.y, msg.z);
    }
}

aether_component::export!(Orbit);
