// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! [`CameraController`] — a keyboard driver for the [`camera`](crate::camera)
//! component.
//!
//! Turns held keys into camera-pose deltas and mails them to a peer
//! [`CameraComponent`], so plain scene
//! navigation ("look around with the keyboard") composes without dragging in
//! a gameplay body the way [`WorldMover`](crate::mover::WorldMover)'s embedded
//! follow-camera does. The camera stays a pure projection state machine; all
//! keyboard policy lives here.
//!
//! # Design
//!
//! The controller keeps a **shadow pose** — its own copy of the camera state
//! it drives — because the camera's `aether.kit.camera.*` deltas are *absolute*
//! (`Some` overwrites, `None` keeps; see [`OrbitParams`]) and the camera
//! exposes no read-back kind. On `wire`
//! it sends one full seed so the shadow is authoritative from the first frame
//! (every field `Some`, including `speed: Some(0.0)` to pin the orbit
//! auto-advance so it never fights the keys), and each tick it emits only the
//! fields that changed. A tick with no mapped key held produces no mail at all.
//!
//! Accepted limit: an out-of-band pose edit (an MCP poke straight to the
//! camera) is snapped back on the next held-key tick, since the shadow, not
//! the camera, is the source of truth.
//!
//! # Config
//!
//! [`ControllerConfig`] (init-config, ADR-0090) selects the target camera
//! name, the mode, and the per-tick rates and clamps — control-scheme
//! variation is config, not code. A bare load boots
//! [`ControllerConfig::default()`].
//!
//! # Mail surface
//!
//! - [`Key`] / [`KeyRelease`] — set / clear a held key. Orbit: WASD pan the
//!   `target` across the ground plane (yaw-relative, diagonals normalized),
//!   ←/→ yaw, ↑/↓ pitch (clamped), Z/X dolly the eye distance. Topdown: WASD
//!   pan the `center`, Z/X scale the `extent`.
//! - [`Tick`] — integrate the held keys and emit the changed-field delta to
//!   the target camera.

mod kinds;
pub use kinds::*;

use core::f32::consts::FRAC_PI_3;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::component::ComponentHostWasmExt;
use aether_capabilities::lifecycle::LifecycleMailboxExt;
use aether_capabilities::{ComponentHostCapability, LifecycleCapability};
use aether_input::{InputCapability, InputMailboxExt};
use aether_kinds::{Key, KeyRelease, Tick, keycode};
use aether_math::{TAU, Vec2, Vec3};

use crate::camera::{CameraComponent, CameraOrbitSet, CameraTopdownSet, OrbitParams, TopdownParams};

/// Load name of the camera component instance the controller drives — the
/// `aether_kit@aether.kit.camera` export's default load name (ADR-0096), the
/// address `.loaded::<CameraComponent>(_)` resolves. Distinct from
/// [`ControllerConfig::camera`], which names a camera *within* that component.
const CAMERA_COMPONENT: &str = "aether.kit.camera";

/// Compiled baseline orbit pose the controller seeds into the target camera:
/// a three-quarter overhead look at the world origin, far enough back to frame
/// a scene. Auto-advance is pinned off at seed time (`speed: Some(0.0)`).
const SEED_TARGET: [f32; 3] = [0.0, 0.0, 0.0];
const SEED_DISTANCE: f32 = 12.0;
/// Negative pitch places the eye above the target looking down (see
/// [`OrbitParams::pitch`]); ~-63° is a legible
/// three-quarter angle well inside the `±π/2` pole.
const SEED_PITCH: f32 = -1.1;
const SEED_YAW: f32 = 0.0;
/// Baseline vertical FOV (radians) — matches the camera component's own orbit
/// default so the seed doesn't visibly change the lens.
const SEED_FOV: f32 = FRAC_PI_3;
/// Compiled baseline topdown pose: origin-centered, matching the eye distance.
const SEED_CENTER: [f32; 2] = [0.0, 0.0];
const SEED_EXTENT: f32 = 12.0;

/// Which mapped keys are currently held. Independent flags so opposite keys
/// (A+D, ←+→) resolve to a zero axis rather than the last one winning.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    yaw_neg: bool,
    yaw_pos: bool,
    pitch_neg: bool,
    pitch_pos: bool,
    zoom_in: bool,
    zoom_out: bool,
}

/// The controller's authoritative copy of the orbit pose it drives.
#[derive(Debug, Clone, Copy)]
struct OrbitShadow {
    target: Vec3,
    yaw: f32,
    pitch: f32,
    distance: f32,
}

/// The controller's authoritative copy of the topdown pose it drives.
#[derive(Debug, Clone, Copy)]
struct TopdownShadow {
    center: Vec2,
    extent: f32,
}

/// The shadow pose, one variant per driven mode.
#[derive(Debug, Clone, Copy)]
enum Shadow {
    Orbit(OrbitShadow),
    Topdown(TopdownShadow),
}

/// Keyboard driver for a peer camera component. Singleton, like the camera it
/// drives; loaded as a non-entry export of `aether_kit.wasm`.
pub struct CameraController {
    config: ControllerConfig,
    held: Held,
    shadow: Shadow,
}

#[actor]
impl WasmActor for CameraController {
    type Config = ControllerConfig;
    const NAMESPACE: &'static str = "aether.kit.camera-controller";

    fn init(config: ControllerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let shadow = match config.mode {
            ControllerMode::Orbit => Shadow::Orbit(OrbitShadow {
                target: Vec3::new(SEED_TARGET[0], SEED_TARGET[1], SEED_TARGET[2]),
                yaw: SEED_YAW,
                pitch: SEED_PITCH,
                distance: SEED_DISTANCE,
            }),
            ControllerMode::Topdown => Shadow::Topdown(TopdownShadow {
                center: Vec2::new(SEED_CENTER[0], SEED_CENTER[1]),
                extent: SEED_EXTENT,
            }),
        };
        Ok(Self { config, held: Held::default(), shadow })
    }

    /// Subscribe the key streams and the tick stage, then seed the target
    /// camera so the shadow is authoritative from frame one. `wire` is the
    /// placement for the seed — `init`'s ctx can't mail.
    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        let input = ctx.actor::<InputCapability>();
        input.subscribe::<Key>();
        input.subscribe::<KeyRelease>();
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();
        self.seed(ctx);
    }

    #[handler::single]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, key: Key) {
        self.set_held(key.code, true);
    }

    #[handler::single]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_>, key: KeyRelease) {
        self.set_held(key.code, false);
    }

    /// Integrate the held keys one tick and, if anything moved, emit the
    /// changed-field delta to the target camera. Nothing held → no mail.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let held = self.held;
        let camera = self.config.camera.clone();
        match &mut self.shadow {
            Shadow::Orbit(orbit) => {
                if let Some(params) = step_orbit(orbit, held, &self.config) {
                    ctx.actor::<ComponentHostCapability>()
                        .loaded::<CameraComponent>(CAMERA_COMPONENT)
                        .send(&CameraOrbitSet { name: camera, params });
                }
            }
            Shadow::Topdown(topdown) => {
                if let Some(params) = step_topdown(topdown, held, &self.config) {
                    ctx.actor::<ComponentHostCapability>()
                        .loaded::<CameraComponent>(CAMERA_COMPONENT)
                        .send(&CameraTopdownSet { name: camera, params });
                }
            }
        }
    }
}

impl CameraController {
    /// Send the full-`Some` seed for the current mode, pinning orbit
    /// auto-advance off so it never fights the keys.
    fn seed(&self, ctx: &mut WasmCtx<'_>) {
        let camera = self.config.camera.clone();
        match &self.shadow {
            Shadow::Orbit(orbit) => {
                ctx.actor::<ComponentHostCapability>().loaded::<CameraComponent>(CAMERA_COMPONENT).send(
                    &CameraOrbitSet {
                        name: camera,
                        params: OrbitParams {
                            distance: Some(orbit.distance),
                            pitch: Some(orbit.pitch),
                            yaw: Some(orbit.yaw),
                            speed: Some(0.0),
                            fov_y_rad: Some(SEED_FOV),
                            target: Some([orbit.target.x, orbit.target.y, orbit.target.z]),
                        },
                    },
                );
            }
            Shadow::Topdown(topdown) => {
                ctx.actor::<ComponentHostCapability>().loaded::<CameraComponent>(CAMERA_COMPONENT).send(
                    &CameraTopdownSet {
                        name: camera,
                        params: TopdownParams {
                            center: Some([topdown.center.x, topdown.center.y]),
                            extent: Some(topdown.extent),
                        },
                    },
                );
            }
        }
    }

    fn set_held(&mut self, code: u32, down: bool) {
        match code {
            keycode::KEY_W => self.held.forward = down,
            keycode::KEY_S => self.held.back = down,
            keycode::KEY_A => self.held.left = down,
            keycode::KEY_D => self.held.right = down,
            keycode::KEY_LEFT => self.held.yaw_neg = down,
            keycode::KEY_RIGHT => self.held.yaw_pos = down,
            keycode::KEY_UP => self.held.pitch_pos = down,
            keycode::KEY_DOWN => self.held.pitch_neg = down,
            keycode::KEY_Z => self.held.zoom_in = down,
            keycode::KEY_X => self.held.zoom_out = down,
            _ => {}
        }
    }
}

/// Per-tick zoom factor from the held Z/X keys, or `None` if neither (or
/// both) is held. Z dollies in (scale by `zoom_rate < 1`), X dollies out
/// (scale by its reciprocal).
fn zoom_factor(held: Held, config: &ControllerConfig) -> Option<f32> {
    match (held.zoom_in, held.zoom_out) {
        (true, false) => Some(config.zoom_rate),
        (false, true) => Some(1.0 / config.zoom_rate),
        _ => None,
    }
}

/// Advance the orbit shadow one tick from the held keys and return the delta
/// to send — `Some` carrying only the fields that changed this tick, or `None`
/// when no mapped key produced motion (the zero-mail-idle invariant).
fn step_orbit(shadow: &mut OrbitShadow, held: Held, config: &ControllerConfig) -> Option<OrbitParams> {
    let mut params = OrbitParams::default();
    let mut changed = false;

    // Pan the target across the ground plane in a yaw-relative basis: at
    // yaw 0, W is world-forward (`-Z`) and D is world-right (`+X`); the basis
    // rotates with yaw so the keys stay screen-relative. Diagonals are
    // velocity-normalized so a diagonal covers the same ground as a cardinal.
    let forward = f32::from(held.forward) - f32::from(held.back);
    let right = f32::from(held.right) - f32::from(held.left);
    if forward != 0.0 || right != 0.0 {
        let (sin_yaw, cos_yaw) = (shadow.yaw.sin(), shadow.yaw.cos());
        let fwd = Vec3::new(-sin_yaw, 0.0, -cos_yaw);
        let rgt = Vec3::new(cos_yaw, 0.0, -sin_yaw);
        let dir = (fwd * forward + rgt * right).normalize();
        shadow.target += dir * config.pan_speed;
        params.target = Some([shadow.target.x, shadow.target.y, shadow.target.z]);
        changed = true;
    }

    let yaw_dir = f32::from(held.yaw_pos) - f32::from(held.yaw_neg);
    if yaw_dir != 0.0 {
        shadow.yaw = yaw_dir.mul_add(config.yaw_speed, shadow.yaw).rem_euclid(TAU);
        params.yaw = Some(shadow.yaw);
        changed = true;
    }

    let pitch_dir = f32::from(held.pitch_pos) - f32::from(held.pitch_neg);
    if pitch_dir != 0.0 {
        shadow.pitch =
            pitch_dir.mul_add(config.pitch_speed, shadow.pitch).clamp(-config.pitch_limit, config.pitch_limit);
        params.pitch = Some(shadow.pitch);
        changed = true;
    }

    if let Some(factor) = zoom_factor(held, config) {
        shadow.distance = (shadow.distance * factor).max(config.distance_floor);
        params.distance = Some(shadow.distance);
        changed = true;
    }

    changed.then_some(params)
}

/// Advance the topdown shadow one tick: WASD pan the ortho center (normalized
/// diagonals), Z/X scale the ortho extent. `None` when idle.
fn step_topdown(shadow: &mut TopdownShadow, held: Held, config: &ControllerConfig) -> Option<TopdownParams> {
    let mut params = TopdownParams::default();
    let mut changed = false;

    let forward = f32::from(held.forward) - f32::from(held.back);
    let right = f32::from(held.right) - f32::from(held.left);
    if forward != 0.0 || right != 0.0 {
        let dir = Vec2::new(right, forward).normalize();
        shadow.center += dir * config.pan_speed;
        params.center = Some([shadow.center.x, shadow.center.y]);
        changed = true;
    }

    if let Some(factor) = zoom_factor(held, config) {
        shadow.extent = (shadow.extent * factor).max(config.distance_floor);
        params.extent = Some(shadow.extent);
        changed = true;
    }

    changed.then_some(params)
}

#[cfg(test)]
mod tests {
    use core::f32::consts::FRAC_PI_2;

    use super::*;

    fn orbit(yaw: f32) -> OrbitShadow {
        OrbitShadow { target: Vec3::ZERO, yaw, pitch: 0.0, distance: SEED_DISTANCE }
    }

    fn held_keys(codes: &[u32]) -> Held {
        let mut c = CameraController {
            config: ControllerConfig::default(),
            held: Held::default(),
            shadow: Shadow::Orbit(orbit(0.0)),
        };
        for &code in codes {
            c.set_held(code, true);
        }
        c.held
    }

    #[test]
    fn idle_emits_no_delta() {
        // Tripwire: the zero-mail-idle invariant. No mapped key held → no
        // delta → the on_tick handler sends nothing.
        let mut s = orbit(0.0);
        assert!(step_orbit(&mut s, Held::default(), &ControllerConfig::default()).is_none());
        let mut t = TopdownShadow { center: Vec2::ZERO, extent: SEED_EXTENT };
        assert!(step_topdown(&mut t, Held::default(), &ControllerConfig::default()).is_none());
    }

    #[test]
    fn diagonal_pan_matches_cardinal_magnitude() {
        // Tripwire: velocity normalization. A W+D diagonal moves the target
        // the same Euclidean distance per tick as a lone W — no √2 speed-up.
        let config = ControllerConfig::default();

        let mut cardinal = orbit(0.0);
        step_orbit(&mut cardinal, held_keys(&[keycode::KEY_W]), &config).expect("W held pans the target");
        let cardinal_mag = cardinal.target.length();

        let mut diagonal = orbit(0.0);
        step_orbit(&mut diagonal, held_keys(&[keycode::KEY_W, keycode::KEY_D]), &config)
            .expect("W+D held pans the target");
        let diagonal_mag = diagonal.target.length();

        assert!(
            (cardinal_mag - config.pan_speed).abs() < 1e-5,
            "cardinal step should equal pan_speed; got {cardinal_mag}"
        );
        assert!(
            (diagonal_mag - config.pan_speed).abs() < 1e-5,
            "diagonal step should equal pan_speed, not pan_speed·√2; got {diagonal_mag}"
        );
    }

    #[test]
    fn pan_basis_rotates_with_yaw() {
        // Tripwire: the pan basis is yaw-relative. W moves the target world
        // `-Z` at yaw 0, but world `-X` after a quarter turn — so the keys
        // stay screen-relative as the camera orbits.
        let config = ControllerConfig::default();

        let mut at_zero = orbit(0.0);
        step_orbit(&mut at_zero, held_keys(&[keycode::KEY_W]), &config).expect("W held pans the target");
        assert!(at_zero.target.x.abs() < 1e-5, "yaw 0: no X drift");
        assert!(at_zero.target.z < 0.0, "yaw 0: W moves -Z");

        let mut at_quarter = orbit(FRAC_PI_2);
        step_orbit(&mut at_quarter, held_keys(&[keycode::KEY_W]), &config).expect("W held pans the target");
        assert!(at_quarter.target.z.abs() < 1e-5, "quarter turn: no Z drift");
        assert!(at_quarter.target.x < 0.0, "quarter turn: W moves -X");
    }

    #[test]
    fn pitch_and_distance_clamp() {
        // Tripwire: the clamps. Holding ↑ forever saturates pitch at
        // +pitch_limit (never reaching the degenerate pole); holding Z forever
        // floors the eye distance rather than collapsing onto the target.
        let config = ControllerConfig::default();

        let mut s = orbit(0.0);
        for _ in 0..100_000 {
            step_orbit(&mut s, held_keys(&[keycode::KEY_UP]), &config);
        }
        assert!((s.pitch - config.pitch_limit).abs() < 1e-4, "pitch saturated at the limit; got {}", s.pitch);

        let mut s = orbit(0.0);
        for _ in 0..100_000 {
            step_orbit(&mut s, held_keys(&[keycode::KEY_Z]), &config);
        }
        assert!((s.distance - config.distance_floor).abs() < 1e-4, "distance floored; got {}", s.distance);
    }

    #[test]
    fn delta_omits_untouched_fields() {
        // Tripwire: the partial-poke contract. A tick that only pans emits a
        // delta with `target` set and every other field `None`, so it rides a
        // single kind without restating (and overwriting) the rest of the pose.
        let config = ControllerConfig::default();
        let mut s = orbit(0.0);
        let params = step_orbit(&mut s, held_keys(&[keycode::KEY_W]), &config).expect("W held pans the target");
        assert!(params.target.is_some(), "pan sets target");
        assert!(params.yaw.is_none(), "yaw untouched");
        assert!(params.pitch.is_none(), "pitch untouched");
        assert!(params.distance.is_none(), "distance untouched");
        assert!(params.speed.is_none(), "speed untouched");
        assert!(params.fov_y_rad.is_none(), "fov untouched");
    }
}
