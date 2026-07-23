// Octimeter → world-meter casts are domain-correct fixed-point-to-float
// conversions at the render boundary only.
#![allow(clippy::cast_precision_loss)]
// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! [`WorldMover`] — an input-driven body that walks the painted world.
//!
//! The first live-gameplay body: one controllable marker on the
//! [`crate::world`] cell lattice the [`WorldView`](crate::world::WorldView) actor
//! paints. Where `WorldView` owns the ground and replays it to
//! `"aether.render"` each frame, `WorldMover` owns exactly the moving body —
//! a fixed-point octimeter position, a trailing follow-camera, and a marker
//! drawn over the ground. The two compose through the two shared sinks the
//! crate's actors already couple on: the `aether.render` draw sink, and the
//! latest-wins `view_proj` camera uniform `WorldMover` publishes (which
//! drives the ground projection too, so the world and the body always share
//! one camera).
//!
//! # Movement
//!
//! Movement is cell-committed on the [`CellPos`] rules. From the current
//! cell the actor commits to the adjacent cell center in the held direction
//! ([`CellPos::center_octimeters`]), glides toward it at
//! `SPEED_OCTIMETERS_PER_TICK` with a velocity-normalized step (so a
//! diagonal covers the same ground per tick as a cardinal — no √2 speed-up),
//! snaps on arrival, and re-commits. A left-click ray-picks the ground cell
//! and walks the straight line to it; there is no pathfinder because blocking
//! is out of scope this rung — a WASD press cancels the click walk.
//!
//! # Camera and picking
//!
//! The actor owns a perspective camera that trails the body at a
//! three-quarter overhead angle the arrow keys orbit (yaw freely, pitch
//! within a slice above the horizon). It publishes the `view_proj` each
//! [`Render`] and casts a ray from the click pixel through that same camera
//! onto the flat ground plane to resolve the target cell, so picking stays
//! correct at any orbit angle. The body rides the flat cell plane at a fixed
//! ground `Y`; terrain-height follow is deferred to the height/step rung.
//!
//! # Mail surface
//!
//! - [`Key`] / [`KeyRelease`] — WASD set / clear a held movement direction;
//!   the arrow keys orbit the camera.
//! - [`MouseMove`] / [`MouseButton`] — track the cursor; a left-click walks
//!   to that cell. [`WindowSize`] feeds the camera aspect and picking.
//! - [`Tick`] — advance the body one step.
//! - [`Render`] — publish the camera + draw the marker to `"aether.render"`.
//! - [`MoverTeleport`] — jump the body to a cell center.

mod kinds;
pub use kinds::*;

use core::f32::consts::{FRAC_PI_2, PI, TAU};

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_input::{InputCapability, InputMailboxExt};
use aether_kinds::{Key, KeyRelease, MouseButton, MouseMove, Render, Tick, WindowSize, keycode, mouse_button};
use aether_lifecycle::LifecycleCapability;
use aether_lifecycle::LifecycleMailboxExt;
use aether_math::{Mat4, Rgb, Vec3};
use aether_render::{DrawTriangle, RenderCapability, Vertex, ViewProjection};

use crate::OCTIMETERS_PER_TILE;
use crate::world::CellPos;

/// Ground speed: octimeters/tick the body travels toward its committed cell.
/// `8` ≈ 1.9 m/s at a 60 Hz tick — the locked cadence for this rung.
const SPEED_OCTIMETERS_PER_TICK: i32 = 8;

/// One cell is one meter is 256 octimeters — the render-boundary scale from
/// fixed-point octimeters to world meters. Matches the `WorldView` mesher's
/// `1 cell = 1 m` convention, so the marker sits where the ground is painted.
const OCTIMETERS_PER_CELL: f32 = OCTIMETERS_PER_TILE as f32;

/// Vertical field of view of the follow camera.
const CAMERA_FOV_Y: f32 = PI / 3.0;
/// Starting pitch above the horizontal, in radians (~52°) — a three-quarter
/// overhead angle that reads as 3D while keeping the ground legible.
const CAMERA_PITCH: f32 = 0.9;
/// Pitch clamp: the eye stays in a slice above the horizon (never at or below
/// it, never straight overhead — `look_at` degenerates near world `Y`).
const CAMERA_PITCH_MIN: f32 = 0.15;
const CAMERA_PITCH_MAX: f32 = 1.45;
/// Arrow-key orbit rates, radians per tick — slow for a gentle orbit.
const CAMERA_YAW_SPEED: f32 = 0.0135;
const CAMERA_PITCH_SPEED: f32 = 0.0067;
/// Distance from the camera target (the body) to the eye, in meters.
const CAMERA_DISTANCE: f32 = 12.0;
/// Height above the ground the camera looks at — roughly the body's
/// mid-height, so the marker sits centered in frame.
const CAMERA_TARGET_HEIGHT: f32 = 0.9;
const CAMERA_Z_NEAR: f32 = 0.1;
const CAMERA_Z_FAR: f32 = 100.0;
/// Aspect used until the first `WindowSize` arrives.
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;

/// Marker capsule (a capped cylinder) at human dimensions: 1.8 m tall,
/// 0.3 m radius. The bottom cap rests on the ground (`y = 0`).
const PLAYER_HEIGHT: f32 = 1.8;
const PLAYER_RADIUS: f32 = 0.3;

/// Marker tint — a warm blue that reads over the meadow-and-lake palette.
const MARKER_COLOR: Rgb = Rgb::new(0.24, 0.55, 0.95);

/// Direction the scene light travels. The capsule bakes a simple Lambert
/// shade against it into vertex colors so it reads as a solid 3D form (the
/// render pipeline carries no lighting of its own).
const LIGHT_DIR: Vec3 = Vec3::new(-0.4, -1.0, -0.3);

/// The cell the body spawns on before any placement mail — an arbitrary
/// interior cell so a bare load drops the marker on the painted plane.
const SPAWN_CELL: CellPos = CellPos { x: 8, z: 8 };

/// Which direction keys are held. Four independent flags so pressing opposite
/// keys (A+D) resolves to a zero axis rather than the last one winning.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    neg_x: bool,
    pos_x: bool,
    neg_z: bool,
    pos_z: bool,
}

impl Held {
    fn dir_x(self) -> i32 {
        i32::from(self.pos_x) - i32::from(self.neg_x)
    }

    fn dir_z(self) -> i32 {
        i32::from(self.pos_z) - i32::from(self.neg_z)
    }
}

/// Which arrow keys are held, driving the camera orbit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
struct CamHeld {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

impl CamHeld {
    fn yaw_dir(self) -> f32 {
        f32::from(self.right) - f32::from(self.left)
    }

    fn pitch_dir(self) -> f32 {
        f32::from(self.up) - f32::from(self.down)
    }
}

/// The controllable body on the painted world.
pub struct WorldMover {
    /// Whether this standalone mover subscribes raw interactive input.
    owns_input: bool,
    /// Body position in octimeters on the world XZ plane.
    pos: (i32, i32),
    /// Held WASD directions.
    held: Held,
    /// The cell the body is gliding toward: an adjacent-cell commit under
    /// WASD, or the clicked cell under click-to-move. `None` when at rest on
    /// a cell center and free to re-commit.
    target: Option<CellPos>,
    /// Whether the active `target` came from a click (so a WASD press knows to
    /// cancel the straight-line walk and hand back to manual control).
    target_from_click: bool,
    /// Cached window size (logical pixels) for the camera aspect and picking.
    window: (u32, u32),
    /// Cached cursor position (logical pixels), updated on mouse move.
    cursor: (f32, f32),
    /// Camera orbit angles around the body: `cam_yaw` is the azimuth (0 puts
    /// the eye behind, `+Z`), `cam_pitch` the elevation above the horizon.
    cam_yaw: f32,
    cam_pitch: f32,
    /// Which arrow keys are held, orbiting the camera each tick.
    cam_held: CamHeld,
}

#[actor(instanced)]
impl WasmActor for WorldMover {
    type Config = MoverConfig;
    const NAMESPACE: &'static str = "aether.kit.mover";

    fn init(config: MoverConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self {
            owns_input: config.owns_input,
            pos: SPAWN_CELL.center_octimeters(),
            held: Held::default(),
            target: None,
            target_from_click: false,
            window: (0, 0),
            cursor: (0.0, 0.0),
            cam_yaw: 0.0,
            cam_pitch: CAMERA_PITCH,
            cam_held: CamHeld::default(),
        })
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        let input = ctx.actor::<InputCapability>();
        if self.owns_input {
            input.subscribe::<Key>();
            input.subscribe::<KeyRelease>();
            input.subscribe::<MouseButton>();
            input.subscribe::<MouseMove>();
        }
        input.subscribe::<WindowSize>();
        let lifecycle = ctx.actor::<LifecycleCapability>();
        lifecycle.subscribe::<Tick>();
        lifecycle.subscribe::<Render>();
    }

    #[handler::single]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _tick: Tick) {
        self.orbit_camera();
        self.advance();
    }

    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        let render = ctx.actor::<RenderCapability>();
        // This actor owns the follow camera: publish the view each frame
        // (latest-wins, so it drives the ground projection too), then the
        // marker geometry over the ground.
        render.send(&ViewProjection { view_proj: self.view_proj() });
        render.send_many(&self.render_triangles());
    }

    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, mail: MouseMove) {
        self.cursor = (mail.x, mail.y);
    }

    #[handler::single]
    fn on_window_size(&mut self, _ctx: &mut WasmCtx<'_>, mail: WindowSize) {
        self.window = (mail.width, mail.height);
    }

    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, mail: MouseButton) {
        // Left-click walks the body; other buttons are ignored.
        if mail.button == mouse_button::LEFT {
            self.click_to_move();
        }
    }

    #[handler::single]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, key: Key) {
        self.set_held(key.code, true);
    }

    #[handler::single]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_>, key: KeyRelease) {
        self.set_held(key.code, false);
    }

    #[handler::single]
    fn on_teleport(&mut self, _ctx: &mut WasmCtx<'_>, mail: MoverTeleport) {
        self.pos = CellPos { x: mail.cell_x, z: mail.cell_z }.center_octimeters();
        self.target = None;
        self.target_from_click = false;
    }
}

impl WorldMover {
    /// WASD moves the body (W is `-Z`, world-forward); the arrow keys orbit
    /// the camera instead of moving.
    fn set_held(&mut self, code: u32, down: bool) {
        match code {
            keycode::KEY_W => self.held.neg_z = down,
            keycode::KEY_S => self.held.pos_z = down,
            keycode::KEY_A => self.held.neg_x = down,
            keycode::KEY_D => self.held.pos_x = down,
            keycode::KEY_LEFT => self.cam_held.left = down,
            keycode::KEY_RIGHT => self.cam_held.right = down,
            keycode::KEY_UP => self.cam_held.up = down,
            keycode::KEY_DOWN => self.cam_held.down = down,
            _ => {}
        }
    }

    /// Orbit the camera one tick from the held arrow keys.
    fn orbit_camera(&mut self) {
        let (yaw, pitch) =
            step_camera(self.cam_yaw, self.cam_pitch, self.cam_held.yaw_dir(), self.cam_held.pitch_dir());
        self.cam_yaw = yaw;
        self.cam_pitch = pitch;
    }

    /// One tick of movement. A held WASD direction cancels an active click
    /// walk and commits the body to the adjacent cell center when it is at
    /// rest; then the body glides toward whichever cell is targeted at the
    /// locked speed, snapping onto the center on arrival so the next tick can
    /// re-commit. The velocity-normalized step keeps a diagonal the same
    /// speed as a cardinal.
    fn advance(&mut self) {
        let (dx, dz) = (self.held.dir_x(), self.held.dir_z());
        if dx != 0 || dz != 0 {
            // Manual control cancels a click walk and re-commits from rest.
            if self.target_from_click {
                self.target = None;
                self.target_from_click = false;
            }
            if self.target.is_none() {
                self.target = Some(commit_target(self.pos, dx, dz));
            }
        }
        let Some(cell) = self.target else {
            return;
        };
        let center = cell.center_octimeters();
        self.pos = step_toward(self.pos, center, SPEED_OCTIMETERS_PER_TICK);
        if self.pos == center {
            self.target = None;
            self.target_from_click = false;
        }
    }

    /// Ray-pick the clicked ground cell and walk the straight line to it —
    /// no pathfinder, since blocking is out of scope this rung.
    fn click_to_move(&mut self) {
        let Some((hit_x, hit_z)) = self.pick_world() else {
            return;
        };
        self.target = Some(CellPos::from_octimeters(hit_x, hit_z));
        self.target_from_click = true;
    }

    /// Window aspect (width / height), falling back before the first
    /// `WindowSize`.
    fn aspect(&self) -> f32 {
        let (w, h) = self.window;
        if w == 0 || h == 0 {
            DEFAULT_ASPECT
        } else {
            w as f32 / h as f32
        }
    }

    /// World-space eye and target for the follow camera: it looks at a point
    /// just above the body and orbits it on a sphere of radius
    /// `CAMERA_DISTANCE` at the current `cam_yaw` / `cam_pitch`, so the view
    /// trails the body as it walks and the arrow keys swing it around.
    fn camera_eye_target(&self) -> (Vec3, Vec3) {
        let to_metres = |oct: i32| oct as f32 / OCTIMETERS_PER_CELL;
        let target = Vec3::new(to_metres(self.pos.0), CAMERA_TARGET_HEIGHT, to_metres(self.pos.1));
        // `yaw = 0` puts the eye behind the body (`+Z`); higher pitch lifts it
        // toward straight overhead.
        let horizontal = CAMERA_DISTANCE * self.cam_pitch.cos();
        let offset = Vec3::new(
            horizontal * self.cam_yaw.sin(),
            CAMERA_DISTANCE * self.cam_pitch.sin(),
            horizontal * self.cam_yaw.cos(),
        );
        (target + offset, target)
    }

    /// Perspective `view_proj` for the follow camera.
    fn view_proj(&self) -> [f32; 16] {
        let (eye, target) = self.camera_eye_target();
        let view = Mat4::look_at_rh(eye, target, Vec3::Y);
        let proj = Mat4::perspective_rh(CAMERA_FOV_Y, self.aspect(), CAMERA_Z_NEAR, CAMERA_Z_FAR);
        (proj * view).to_cols_array()
    }

    /// Cast a ray from the cursor pixel through the follow camera onto the
    /// ground plane (`y = 0`) and return the hit as an octimeter position.
    /// `None` before the first window size or when the ray misses the ground.
    #[allow(clippy::cast_possible_truncation)]
    fn pick_world(&self) -> Option<(i32, i32)> {
        let (w, h) = self.window;
        if w == 0 || h == 0 {
            return None;
        }
        let (px, py) = self.cursor;
        let ndc_x = (px / w as f32).mul_add(2.0, -1.0);
        let ndc_y = (py / h as f32).mul_add(-2.0, 1.0);

        let (eye, target) = self.camera_eye_target();
        // Camera basis, matching `look_at_rh`: `fwd` points into the scene,
        // `right` and `up` span the image plane.
        let fwd = (target - eye).normalize();
        let right = fwd.cross(Vec3::Y).normalize();
        let up = right.cross(fwd);
        let tan = (CAMERA_FOV_Y * 0.5).tan();
        let dir = fwd + right * (ndc_x * tan * self.aspect()) + up * (ndc_y * tan);

        let (hit_x, hit_z) = intersect_ground(eye, dir)?;
        let to_octimeters = |metres: f32| (metres * OCTIMETERS_PER_CELL) as i32;
        Some((to_octimeters(hit_x), to_octimeters(hit_z)))
    }

    /// The marker capsule standing on the ground at the body position. The
    /// only floats in the system live here, at the render boundary; they never
    /// feed back into the sim.
    fn render_triangles(&self) -> Vec<DrawTriangle> {
        let mut out = Vec::with_capacity(512);
        let ax = self.pos.0 as f32 / OCTIMETERS_PER_CELL;
        let az = self.pos.1 as f32 / OCTIMETERS_PER_CELL;
        push_capsule(&mut out, ax, az, MARKER_COLOR);
        out
    }
}

/// The adjacent cell to commit to from `pos` given the held direction signs
/// `(dx, dz)` — the cell rule the WASD walk steps on. The body's current cell
/// is `CellPos::from_octimeters(pos)`; the commit is its neighbor in the held
/// direction, whose center the body then glides onto.
fn commit_target(pos: (i32, i32), dx: i32, dz: i32) -> CellPos {
    let cur = CellPos::from_octimeters(pos.0, pos.1);
    CellPos { x: cur.x + dx, z: cur.z + dz }
}

/// Advance a point `speed` octimeters *along the straight line to* `target` —
/// the same Euclidean distance per tick in every direction (so a diagonal
/// doesn't run √2 faster than a cardinal). Each axis moves its share of the
/// step scaled by the true direction `(dx, dz) / |(dx, dz)|`, rounded to the
/// nearest octimeter, and the move snaps exactly onto `target` once within one
/// step. Integer-only via `isqrt` and recomputed from the live delta each
/// tick, so it stays deterministic and rounding never accumulates.
#[allow(clippy::cast_possible_truncation)]
fn step_toward(cur: (i32, i32), target: (i32, i32), speed: i32) -> (i32, i32) {
    let dx = i64::from(target.0 - cur.0);
    let dz = i64::from(target.1 - cur.1);
    let dist = (dx * dx + dz * dz).isqrt();
    let speed = i64::from(speed);
    if dist <= speed {
        return target;
    }
    // Round speed·d / dist to nearest, away from zero on a tie.
    let round_div = |num: i64| {
        let half = dist / 2;
        if num >= 0 {
            (num + half) / dist
        } else {
            (num - half) / dist
        }
    };
    // |speed·d / dist| ≤ speed, so the result fits an i32 axis step.
    (cur.0 + round_div(speed * dx) as i32, cur.1 + round_div(speed * dz) as i32)
}

/// Advance the camera orbit one tick. `yaw_dir` / `pitch_dir` are the held
/// direction signs (`-1`, `0`, `+1`); yaw wraps freely while pitch stays
/// clamped to a slice above the horizon so the view never dips below it or
/// reaches the degenerate straight-overhead pose.
fn step_camera(yaw: f32, pitch: f32, yaw_dir: f32, pitch_dir: f32) -> (f32, f32) {
    let yaw = yaw_dir.mul_add(CAMERA_YAW_SPEED, yaw).rem_euclid(TAU);
    let pitch = pitch_dir.mul_add(CAMERA_PITCH_SPEED, pitch).clamp(CAMERA_PITCH_MIN, CAMERA_PITCH_MAX);
    (yaw, pitch)
}

/// Intersect the ray `eye + t·dir` (`t ≥ 0`) with the ground plane `y = 0`,
/// returning the world `(x, z)` of the hit, or `None` when the ray points away
/// from the ground (so it never crosses it in front of the eye, which sits
/// above the plane).
fn intersect_ground(eye: Vec3, dir: Vec3) -> Option<(f32, f32)> {
    if dir.y >= 0.0 {
        return None;
    }
    let t = -eye.y / dir.y;
    Some((dir.x.mul_add(t, eye.x), dir.z.mul_add(t, eye.z)))
}

/// Append a shaded capsule (a capped cylinder) standing on the ground at
/// `(cx, cz)`, in meters, tinted `base`. Built as a stack of horizontal rings
/// from the bottom pole to the top pole — two hemisphere caps of
/// [`PLAYER_RADIUS`] joined by a cylinder — each pair of rings bridged by a
/// band of triangles. Per-vertex normals carry a Lambert shade against
/// [`LIGHT_DIR`] baked into the color, so the form reads as solid 3D under a
/// pipeline that has no lighting of its own.
fn push_capsule(out: &mut Vec<DrawTriangle>, cx: f32, cz: f32, base: Rgb) {
    /// Vertices around each ring.
    const RADIAL: usize = 16;
    /// Rings per hemisphere cap (pole to equator inclusive of the equator).
    const CAP_RINGS: usize = 6;

    let radius = PLAYER_RADIUS;
    let cylinder_height = 2.0f32.mul_add(-radius, PLAYER_HEIGHT);
    let bottom_center = radius;
    let top_center = radius + cylinder_height;

    // A ring at latitude `phi` (−π/2 at the bottom pole, +π/2 at the top) sits
    // at height `center + r·sin φ` with horizontal radius `r·cos φ`; the normal
    // is the outward direction `(cos φ·cos θ, sin φ, cos φ·sin θ)`, unit length.
    let ring = |center_y: f32, phi: f32| -> [(Vec3, Vec3); RADIAL] {
        let (s, c) = (phi.sin(), phi.cos());
        let y = radius.mul_add(s, center_y);
        let mut verts = [(Vec3::ZERO, Vec3::ZERO); RADIAL];
        for (j, v) in verts.iter_mut().enumerate() {
            let theta = TAU * j as f32 / RADIAL as f32;
            let (ct, st) = (theta.cos(), theta.sin());
            let normal = Vec3::new(c * ct, s, c * st);
            let rc = radius * c;
            let pos = Vec3::new(rc.mul_add(ct, cx), y, rc.mul_add(st, cz));
            *v = (pos, normal);
        }
        verts
    };

    // Bottom cap (pole → equator) then top cap (equator → pole). The bottom
    // equator and the top equator bound the cylinder body, so the band between
    // them is the cylinder wall.
    let mut rings: Vec<[(Vec3, Vec3); RADIAL]> = Vec::with_capacity(2 * CAP_RINGS);
    for i in 0..CAP_RINGS {
        let phi = -FRAC_PI_2 * (1.0 - i as f32 / (CAP_RINGS - 1) as f32);
        rings.push(ring(bottom_center, phi));
    }
    for i in 0..CAP_RINGS {
        let phi = FRAC_PI_2 * (i as f32 / (CAP_RINGS - 1) as f32);
        rings.push(ring(top_center, phi));
    }

    let to_light = (LIGHT_DIR * -1.0).normalize();
    let shade = |normal: Vec3| {
        let lambert = normal.dot(to_light).max(0.0);
        let f = 0.65f32.mul_add(lambert, 0.35);
        Rgb::new(base.r * f, base.g * f, base.b * f)
    };
    let vert = |p: Vec3, color: Rgb| Vertex { x: p.x, y: p.y, z: p.z, color };
    let tri = |a: (Vec3, Vec3), b: (Vec3, Vec3), c: (Vec3, Vec3)| DrawTriangle {
        verts: [vert(a.0, shade(a.1)), vert(b.0, shade(b.1)), vert(c.0, shade(c.1))],
    };

    for band in 0..rings.len() - 1 {
        let (lo, hi) = (rings[band], rings[band + 1]);
        for j in 0..RADIAL {
            let k = (j + 1) % RADIAL;
            let (l0, hi0, l1, hi1) = (lo[j], hi[j], lo[k], hi[k]);
            out.push(tri(l0, hi0, hi1));
            out.push(tri(l0, hi1, l1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_toward_speed_is_uniform_across_directions() {
        // Tripwire: the velocity-normalization invariant. A diagonal step
        // covers the same ground per tick as a cardinal one — no √2 speed-up.
        // The cardinal move takes the full speed on one axis; the 45° diagonal
        // splits it so the Euclidean distance still ≈ speed.
        let speed = SPEED_OCTIMETERS_PER_TICK;
        let origin = (1000, 1000);

        let cardinal = step_toward(origin, (1000 + 320, 1000), speed);
        assert_eq!(cardinal, (1000 + speed, 1000), "cardinal moves full speed");

        let diagonal = step_toward(origin, (1000 - 320, 1000 + 320), speed);
        let (mx, mz) = (diagonal.0 - 1000, diagonal.1 - 1000);
        assert_eq!(-mx, mz, "the 45° split is symmetric across the axes");
        let moved = f64::from(mx * mx + mz * mz).sqrt();
        assert!(
            (moved - f64::from(speed)).abs() <= 1.0,
            "diagonal distance {moved} should be ≈ {speed}, not {speed}·√2"
        );
    }

    #[test]
    fn cardinal_commit_lands_exactly_on_the_adjacent_cell_center() {
        // Committing from a cell center to an adjacent cell and gliding at the
        // locked speed lands the body exactly on that cell's center — the
        // cell-committed cadence snaps, it doesn't drift.
        let start = CellPos { x: 4, z: 7 };
        let (sx, sz) = start.center_octimeters();
        let target = commit_target((sx, sz), 1, 0);
        assert_eq!(target, CellPos { x: 5, z: 7 }, "commits to the east cell");
        let center = target.center_octimeters();
        let mut p = (sx, sz);
        for _ in 0..1000 {
            if p == center {
                break;
            }
            p = step_toward(p, center, SPEED_OCTIMETERS_PER_TICK);
        }
        assert_eq!(p, center, "the body snaps onto the adjacent cell center");
    }

    #[test]
    fn diagonal_commit_lands_exactly_on_the_diagonal_cell_center() {
        // A diagonal commit (W+A) targets the diagonal neighbor and, via the
        // velocity-normalized step, still lands exactly on its center.
        let start = CellPos { x: 4, z: 7 };
        let (sx, sz) = start.center_octimeters();
        let target = commit_target((sx, sz), -1, -1);
        assert_eq!(target, CellPos { x: 3, z: 6 }, "commits to the north-west cell");
        let center = target.center_octimeters();
        let mut p = (sx, sz);
        for _ in 0..1000 {
            if p == center {
                break;
            }
            p = step_toward(p, center, SPEED_OCTIMETERS_PER_TICK);
        }
        assert_eq!(p, center, "the body snaps onto the diagonal cell center");
    }

    #[test]
    fn camera_orbit_clamps_pitch_above_the_horizon_and_wraps_yaw() {
        // Holding "down" forever floors pitch at the above-horizon minimum;
        // holding "up" ceils it below the straight-overhead maximum. Yaw wraps
        // into [0, τ) rather than growing without bound.
        let (mut yaw, mut pitch) = (0.0, CAMERA_PITCH);
        for _ in 0..10_000 {
            (yaw, pitch) = step_camera(yaw, pitch, 0.0, -1.0);
        }
        assert!((pitch - CAMERA_PITCH_MIN).abs() < 1e-5, "pitch floored: {pitch}");
        for _ in 0..10_000 {
            (yaw, pitch) = step_camera(yaw, pitch, 0.0, 1.0);
        }
        assert!((pitch - CAMERA_PITCH_MAX).abs() < 1e-5, "pitch ceiled: {pitch}");
        for _ in 0..10_000 {
            (yaw, pitch) = step_camera(yaw, pitch, 1.0, 0.0);
        }
        assert!((0.0..TAU).contains(&yaw), "yaw stayed wrapped: {yaw}");
    }

    #[test]
    fn ground_ray_hits_below_and_misses_above() {
        // Straight down from 5 m up lands at the eye's ground footprint; a ray
        // angled upward never reaches the ground ahead of the eye.
        let hit = intersect_ground(Vec3::new(2.0, 5.0, 3.0), Vec3::new(0.0, -1.0, 0.0));
        assert_eq!(hit, Some((2.0, 3.0)));
        assert!(intersect_ground(Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.5, -1.0)).is_none());
    }
}
