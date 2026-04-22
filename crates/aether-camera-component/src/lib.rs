//! Orbit camera that drives the substrate's `aether.camera` sink.
//!
//! Auto-orbits around the world origin on a yaw-only circle at a
//! fixed distance, updating each tick. Aspect ratio tracks the live
//! window size via `WindowSize`, so the projection stays square
//! regardless of window shape. First real second drawing-adjacent
//! component (after `aether-hello-component`) and the forcing
//! function for the `aether-math` + GPU-uniform plumbing.
//!
//! WASD / arrow-key input is deliberately deferred to a follow-up —
//! winit's `KeyCode as u32` discriminants aren't a stable named
//! contract through `aether-kinds`, so hardcoding raw numbers here
//! would be fragile. Auto-orbit gets the pipeline end-to-end
//! working; key-driven control lands once we have either a named
//! keycode export or a smoke-tested lookup.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_kinds::{Camera, Tick, WindowSize};
use aether_math::{Mat4, PI, Quat, Vec3};

/// Orbit radius — how far the eye sits from the world origin.
const DISTANCE: f32 = 3.0;
/// Yaw delta per tick. At 60 fps this is a full revolution every
/// ~12 seconds — slow enough to watch, fast enough to obviously be
/// moving.
const YAW_PER_TICK: f32 = PI / 360.0;
/// Fixed downward pitch so the camera looks slightly down at the
/// origin, giving a sense of depth. Positive pitch = tilt toward +Y.
const PITCH: f32 = 0.3;
/// Vertical field of view, 60°.
const FOV_Y: f32 = PI / 3.0;
const Z_NEAR: f32 = 0.1;
const Z_FAR: f32 = 100.0;
/// Fallback aspect ratio used before the first `WindowSize` arrives.
/// The substrate re-pulses `WindowSize` every tick so this is only
/// visible for a single frame after load.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;

pub struct Orbit {
    camera: Sink<Camera>,
    yaw: f32,
    aspect: f32,
}

/// Orbit camera that publishes a view*projection matrix to the
/// substrate every tick.
///
/// # Agent
/// Load this alongside a drawing component (e.g. `aether-hello-component`)
/// to see the triangle viewed through a moving 3D camera. Use
/// `capture_frame` frame-to-frame — the triangle should drift across
/// the view horizontally as the camera yaws around origin. No mail
/// endpoints to poke; it runs itself off the tick stream.
#[handlers]
impl Component for Orbit {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        Orbit {
            camera: ctx.resolve_sink::<Camera>("camera"),
            yaw: 0.0,
            aspect: DEFAULT_ASPECT,
        }
    }

    /// Advance the orbit and publish a fresh `view_proj`.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut Ctx<'_>, _tick: Tick) {
        self.yaw += YAW_PER_TICK;
        if self.yaw > PI * 2.0 {
            self.yaw -= PI * 2.0;
        }

        // Orbit Vec3::new(0, 0, DISTANCE) by yaw around Y and pitch
        // around local X. YXZ order: yaw applied first, then pitch
        // in the yaw-rotated frame — identical to a first-person
        // camera's look direction math.
        let orientation = Quat::from_euler_yxz(self.yaw, PITCH, 0.0);
        let eye = orientation * Vec3::new(0.0, 0.0, DISTANCE);

        let view = Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y);
        let proj = Mat4::perspective_rh(FOV_Y, self.aspect, Z_NEAR, Z_FAR);
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
}

aether_component::export!(Orbit);
