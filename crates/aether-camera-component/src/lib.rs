//! Multi-camera component. Hosts N named cameras (each in one of two
//! modes — orbit or orthographic top-down), advances every camera each
//! tick, and publishes the active camera's `view_proj` to
//! `"aether.render"` (the camera mailbox folded into render per ADR-0074 §Decision 7; the kind name `aether.camera` is unchanged).
//!
//! Boots with one default camera, `name = "main"`, in orbit mode and
//! marked active, so loading the component still produces a visible
//! 3D camera with no further mail. Create / destroy / activate / mode-
//! switch via the `aether.camera.*` mail family.
//!
//! # Mail surface
//!
//! - `aether.camera.create { name, mode }` — add a new camera. Errors
//!   (warn-log) if `name` already exists.
//! - `aether.camera.destroy { name }` — drop a camera. If the active
//!   one is destroyed, publishing pauses until another is activated.
//! - `aether.camera.set_active { name }` — promote a camera to be the
//!   one whose `view_proj` publishes each tick.
//! - `aether.camera.set_mode { name, mode }` — replace an existing
//!   camera's mode in place. Prior-mode state is discarded.
//! - `aether.camera.orbit.set { name, params }` — apply orbit-mode
//!   field deltas (Option per field). No-op (warn-log) if the camera
//!   is in a different mode.
//! - `aether.camera.topdown.set { name, params }` — same for topdown
//!   mode.
//!
//! Inactive cameras still tick — orbit yaw keeps accumulating — so
//! re-activating a camera doesn't snap it to a stale yaw.
//!
//! WASD / arrow-key direct input is deliberately deferred (same
//! reason as the prior single-mode camera: winit `KeyCode` ints
//! aren't a stable named contract through `aether-kinds`). Control
//! mail is the driver surface.

use std::collections::HashMap;

use aether_actor::{BootError, Mailbox, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_camera::{
    CameraCreate, CameraDestroy, CameraOrbitSet, CameraSetActive, CameraSetMode, CameraTopdownSet,
    ModeInit, OrbitParams, TopdownParams,
};
use aether_kinds::{Camera, Tick, WindowSize};
use aether_math::{Mat4, PI, Quat, Vec2, Vec3};

const Z_NEAR: f32 = 0.1;
const Z_FAR: f32 = 100.0;
/// Aspect used before the first `WindowSize` arrives. The substrate
/// re-pulses `WindowSize` every tick so this only shows for one frame.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;

/// Compiled defaults used when a created camera leaves an `Option`
/// field unset, or when a mode-switch lands without all fields seeded.
mod defaults {
    use super::*;

    pub const ORBIT_DISTANCE: f32 = 3.0;
    /// Roughly one full revolution every 12 seconds at 60 fps —
    /// matches the prior single-mode orbit camera so existing demos
    /// don't visibly change cadence.
    pub const ORBIT_SPEED: f32 = PI / 360.0;
    pub const ORBIT_PITCH: f32 = 0.3;
    pub const ORBIT_FOV: f32 = PI / 3.0;
    pub const ORBIT_TARGET: Vec3 = Vec3::ZERO;
    pub const ORBIT_YAW: f32 = 0.0;

    pub const TOPDOWN_CENTER: Vec2 = Vec2::ZERO;
    pub const TOPDOWN_EXTENT: f32 = 3.0;
    /// Eye height along `+Z`. Orthographic projection is translation-
    /// invariant along the view axis; just needs to be positive and
    /// inside the far plane.
    pub const TOPDOWN_EYE_HEIGHT: f32 = 10.0;
    /// Floor for `TopdownParams::extent` to keep the projection from
    /// degenerating into NaN on a zero / negative request.
    pub const TOPDOWN_EXTENT_FLOOR: f32 = 0.001;
}

#[derive(Debug, Clone, Copy)]
struct OrbitState {
    distance: f32,
    pitch: f32,
    yaw: f32,
    speed: f32,
    fov_y_rad: f32,
    target: Vec3,
}

impl OrbitState {
    fn from_params(p: &OrbitParams) -> Self {
        OrbitState {
            distance: p.distance.unwrap_or(defaults::ORBIT_DISTANCE),
            pitch: p.pitch.unwrap_or(defaults::ORBIT_PITCH),
            yaw: p.yaw.unwrap_or(defaults::ORBIT_YAW),
            speed: p.speed.unwrap_or(defaults::ORBIT_SPEED),
            fov_y_rad: p.fov_y_rad.unwrap_or(defaults::ORBIT_FOV),
            target: p
                .target
                .map(Vec3::from_array)
                .unwrap_or(defaults::ORBIT_TARGET),
        }
    }

    fn apply(&mut self, p: &OrbitParams) {
        if let Some(v) = p.distance {
            self.distance = v;
        }
        if let Some(v) = p.pitch {
            self.pitch = v;
        }
        if let Some(v) = p.yaw {
            self.yaw = v;
        }
        if let Some(v) = p.speed {
            self.speed = v;
        }
        if let Some(v) = p.fov_y_rad {
            self.fov_y_rad = v;
        }
        if let Some(v) = p.target {
            self.target = Vec3::from_array(v);
        }
    }

    fn tick(&mut self) {
        self.yaw += self.speed;
        if self.yaw > PI * 2.0 {
            self.yaw -= PI * 2.0;
        } else if self.yaw < 0.0 {
            self.yaw += PI * 2.0;
        }
    }

    fn view_proj(&self, aspect: f32) -> [f32; 16] {
        let orientation = Quat::from_euler_yxz(self.yaw, self.pitch, 0.0);
        let eye = self.target + orientation * Vec3::new(0.0, 0.0, self.distance);
        let view = Mat4::look_at_rh(eye, self.target, Vec3::Y);
        let proj = Mat4::perspective_rh(self.fov_y_rad, aspect, Z_NEAR, Z_FAR);
        (proj * view).to_cols_array()
    }
}

#[derive(Debug, Clone, Copy)]
struct TopdownState {
    center: Vec2,
    extent: f32,
}

impl TopdownState {
    fn from_params(p: &TopdownParams) -> Self {
        TopdownState {
            center: p
                .center
                .map(|c| Vec2::new(c[0], c[1]))
                .unwrap_or(defaults::TOPDOWN_CENTER),
            extent: p
                .extent
                .map(|e| e.max(defaults::TOPDOWN_EXTENT_FLOOR))
                .unwrap_or(defaults::TOPDOWN_EXTENT),
        }
    }

    fn apply(&mut self, p: &TopdownParams) {
        if let Some(c) = p.center {
            self.center = Vec2::new(c[0], c[1]);
        }
        if let Some(e) = p.extent {
            self.extent = e.max(defaults::TOPDOWN_EXTENT_FLOOR);
        }
    }

    fn view_proj(&self, aspect: f32) -> [f32; 16] {
        let half_w = self.extent * aspect;
        let proj = Mat4::orthographic_rh(-half_w, half_w, -self.extent, self.extent, Z_NEAR, Z_FAR);
        let eye = Vec3::new(self.center.x, self.center.y, defaults::TOPDOWN_EYE_HEIGHT);
        let target = Vec3::new(self.center.x, self.center.y, 0.0);
        let view = Mat4::look_at_rh(eye, target, Vec3::Y);
        (proj * view).to_cols_array()
    }
}

#[derive(Debug, Clone, Copy)]
enum ModeState {
    Orbit(OrbitState),
    Topdown(TopdownState),
}

impl ModeState {
    fn from_init(mode: &ModeInit) -> Self {
        match mode {
            ModeInit::Orbit(p) => ModeState::Orbit(OrbitState::from_params(p)),
            ModeInit::Topdown(p) => ModeState::Topdown(TopdownState::from_params(p)),
        }
    }

    fn tick(&mut self) {
        if let ModeState::Orbit(state) = self {
            state.tick();
        }
    }

    fn view_proj(&self, aspect: f32) -> [f32; 16] {
        match self {
            ModeState::Orbit(s) => s.view_proj(aspect),
            ModeState::Topdown(s) => s.view_proj(aspect),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            ModeState::Orbit(_) => "orbit",
            ModeState::Topdown(_) => "topdown",
        }
    }
}

/// A single camera the component is hosting. Identity is its key in
/// `CameraComponent::cameras`; the per-mode state is the only payload.
struct CameraState {
    mode: ModeState,
}

pub struct CameraComponent {
    cameras: HashMap<String, CameraState>,
    active: Option<String>,
    aspect: f32,
    camera: Mailbox<Camera>,
}

/// Multi-camera component. Hosts N named cameras, ticks all, publishes
/// the active one's `view_proj` each frame.
///
/// # Agent
/// Boots with a default camera named `"main"` in orbit mode, marked
/// active. Iterate from there:
///
/// - `aether.camera.create { name, mode: Orbit(OrbitParams { … }) }`
///   to add another camera (e.g. a topdown overview).
/// - `aether.camera.set_active { name }` to switch which camera's
///   `view_proj` reaches the GPU.
/// - `aether.camera.orbit.set { name, params: { distance: Some(5.0) } }`
///   for live deltas — every `Some` field overwrites, `None` leaves
///   the camera's current value alone.
/// - `aether.camera.set_mode { name, mode }` to re-shape an existing
///   camera in place.
///
/// Use `capture_frame` between sends to verify each change.
#[actor]
impl WasmActor for CameraComponent {
    const NAMESPACE: &'static str = "camera";

    fn init(ctx: &mut WasmInitCtx<'_>) -> Result<Self, BootError> {
        let mut cameras = HashMap::new();
        cameras.insert(
            "main".to_owned(),
            CameraState {
                mode: ModeState::Orbit(OrbitState::from_params(&OrbitParams::default())),
            },
        );
        Ok(CameraComponent {
            cameras,
            active: Some("main".to_owned()),
            aspect: DEFAULT_ASPECT,
            camera: ctx.resolve_mailbox::<Camera>("aether.render"),
        })
    }

    /// Advance every camera's per-mode state, then publish the active
    /// camera's `view_proj` to `"aether.render"`. Inactive cameras
    /// still tick (so orbit yaw keeps accumulating); only the active
    /// one writes to the sink.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        for cam in self.cameras.values_mut() {
            cam.mode.tick();
        }
        if let Some(name) = &self.active
            && let Some(cam) = self.cameras.get(name)
        {
            let view_proj = cam.mode.view_proj(self.aspect);
            ctx.send(&self.camera, &Camera { view_proj });
        }
    }

    /// Track live window aspect so 3D / orthographic projections stay
    /// unsquashed on non-square windows.
    ///
    /// # Agent
    /// Publish-subscribe; the substrate pulses this every tick. Not
    /// useful to send manually.
    #[handler]
    fn on_window_size(&mut self, _ctx: &mut WasmCtx<'_>, size: WindowSize) {
        if size.width > 0 && size.height > 0 {
            self.aspect = size.width as f32 / size.height as f32;
        }
    }

    /// Add a new named camera in the supplied mode. Errors (warn-log)
    /// if `name` is already bound — use `set_mode` to swap an existing
    /// camera instead. Newly-created cameras are not made active
    /// automatically; pair with `set_active` to switch publishing.
    #[handler]
    fn on_create(&mut self, _ctx: &mut WasmCtx<'_>, msg: CameraCreate) {
        if self.cameras.contains_key(&msg.name) {
            tracing::warn!(
                target: "aether_camera",
                name = %msg.name,
                "camera.create rejected: name already bound; use set_mode to swap modes",
            );
            return;
        }
        self.cameras.insert(
            msg.name,
            CameraState {
                mode: ModeState::from_init(&msg.mode),
            },
        );
    }

    /// Drop a camera by name. Idempotent — silently no-ops if the
    /// camera doesn't exist. If the active camera is destroyed,
    /// publishing pauses (no `aether.camera` mail goes out) until
    /// `set_active` picks a survivor.
    #[handler]
    fn on_destroy(&mut self, _ctx: &mut WasmCtx<'_>, msg: CameraDestroy) {
        self.cameras.remove(&msg.name);
        if self.active.as_deref() == Some(msg.name.as_str()) {
            self.active = None;
        }
    }

    /// Promote `name` to be the camera whose `view_proj` publishes to
    /// `"aether.render"` each tick. Errors (warn-log, no state
    /// change) if `name` isn't bound.
    #[handler]
    fn on_set_active(&mut self, _ctx: &mut WasmCtx<'_>, msg: CameraSetActive) {
        if self.cameras.contains_key(&msg.name) {
            self.active = Some(msg.name);
        } else {
            tracing::warn!(
                target: "aether_camera",
                name = %msg.name,
                "camera.set_active rejected: no camera bound under that name",
            );
        }
    }

    /// Replace an existing camera's mode in place. Prior-mode state is
    /// discarded; the new mode is seeded from the supplied params plus
    /// per-mode compiled defaults. No-op (warn-log) if `name` isn't
    /// bound.
    #[handler]
    fn on_set_mode(&mut self, _ctx: &mut WasmCtx<'_>, msg: CameraSetMode) {
        match self.cameras.get_mut(&msg.name) {
            Some(cam) => cam.mode = ModeState::from_init(&msg.mode),
            None => tracing::warn!(
                target: "aether_camera",
                name = %msg.name,
                "camera.set_mode rejected: no camera bound under that name",
            ),
        }
    }

    /// Apply orbit-mode field deltas to the named camera. Every `Some`
    /// field overwrites; `None` leaves the current value alone. No-op
    /// (warn-log) if the camera doesn't exist or is in a different
    /// mode.
    #[handler]
    fn on_orbit_set(&mut self, _ctx: &mut WasmCtx<'_>, msg: CameraOrbitSet) {
        match self.cameras.get_mut(&msg.name) {
            Some(cam) => match &mut cam.mode {
                ModeState::Orbit(state) => state.apply(&msg.params),
                other => tracing::warn!(
                    target: "aether_camera",
                    name = %msg.name,
                    actual = %other.name(),
                    "camera.orbit.set rejected: camera is in a different mode; switch with set_mode first",
                ),
            },
            None => tracing::warn!(
                target: "aether_camera",
                name = %msg.name,
                "camera.orbit.set rejected: no camera bound under that name",
            ),
        }
    }

    /// Apply topdown-mode field deltas to the named camera. Same
    /// semantics as `orbit.set` for the orthographic mode's `center`
    /// / `extent`.
    #[handler]
    fn on_topdown_set(&mut self, _ctx: &mut WasmCtx<'_>, msg: CameraTopdownSet) {
        match self.cameras.get_mut(&msg.name) {
            Some(cam) => match &mut cam.mode {
                ModeState::Topdown(state) => state.apply(&msg.params),
                other => tracing::warn!(
                    target: "aether_camera",
                    name = %msg.name,
                    actual = %other.name(),
                    "camera.topdown.set rejected: camera is in a different mode; switch with set_mode first",
                ),
            },
            None => tracing::warn!(
                target: "aether_camera",
                name = %msg.name,
                "camera.topdown.set rejected: no camera bound under that name",
            ),
        }
    }
}

aether_actor::export!(CameraComponent);
