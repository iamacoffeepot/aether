// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! [`CameraController`] — a keyboard driver for one [`camera`](crate::camera)
//! instance, at `aether.kit.camera-controller:<key>`.
//!
//! Turns held keys into poses and mails them to the camera its config names,
//! so plain scene navigation ("look around with the keyboard") composes
//! without dragging in a gameplay body. The camera stays a pose and a lens;
//! all keyboard policy lives here.
//!
//! # Design
//!
//! The controller keeps a **shadow pose**, its own copy of the pose it
//! drives. At `wire` it proves the camera's path and asks the camera
//! [`Where`], and the answer is the shadow's first value, so the keys move the
//! camera from wherever its config put it. Each tick with a mapped key held
//! it steps the shadow and sends the camera the whole [`Pose`]. A tick with
//! no mapped key held produces no mail at all.
//!
//! Accepted limit: a pose sent to the camera from elsewhere (an MCP poke, a
//! `Frame`, a `Glide`) is replaced on the next held-key tick, since the
//! shadow, not the camera, is what the keys step.
//!
//! A controller whose camera does not prove at `wire` fails its birth, so
//! load the camera first. A republish does not run `wire`, so `on_rehydrate`
//! proves the camera and asks again; the shadow is whatever the camera
//! answers.
//!
//! # Config
//!
//! [`ControllerConfig`] (init-config, ADR-0090) names the camera and sets the
//! per-tick rates and clamps: control-scheme variation is config, not code.
//!
//! # Mail surface
//!
//! - [`Key`] / [`KeyRelease`] — set / clear a held key. WASD pan the pose's
//!   `target` across the ground plane (yaw-relative, diagonals normalized),
//!   ←/→ yaw, ↑/↓ pitch (clamped), Z/X dolly the eye distance.
//! - [`Tick`] — step the shadow by the held keys and send the camera the
//!   pose.

mod kinds;
pub use kinds::*;

use aether_actor::{ActorInitError, ActorRef, PriorState, ResolveError, WasmActor, WasmCtx, WasmDropCtx};
use aether_actor::{WasmInitCtx, actor};
use aether_kinds::{Key, KeyRelease, Tick, keycode};
use aether_lifecycle::LifecycleCapability;
use aether_math::{TAU, Vec3};
use aether_window::WindowCapability;

use crate::camera::{CameraComponent, Distance, Pitch, Pose, Where, Yaw};

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

/// The controller's hold on the camera it drives.
#[derive(Debug, Clone, Copy)]
enum Link {
    /// Not wired yet, or a republish found the camera gone; the keys move
    /// nothing.
    Unlinked,
    /// The camera is proven and has been asked where it is.
    Asked(ActorRef<CameraComponent>),
    /// The camera answered: `pose` is the shadow the keys step.
    Driving { camera: ActorRef<CameraComponent>, pose: Pose },
}

/// What a controller carries across a republish: nothing of its own, since
/// the camera holds the pose. Saving it is what makes the replacement's
/// `on_rehydrate` run.
#[aether_data::kind(name = "aether.kit.camera-controller.state", copy, eq, no_serde)]
struct ControllerState;

/// Keyboard driver for one camera instance.
pub struct CameraController {
    config: ControllerConfig,
    held: Held,
    link: Link,
}

#[actor(instanced, root, depends(WindowCapability, LifecycleCapability))]
impl WasmActor for CameraController {
    type Config = ControllerConfig;
    const NAMESPACE: &'static str = "aether.kit.camera-controller";

    fn init(config: ControllerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { config, held: Held::default(), link: Link::Unlinked })
    }

    /// Subscribe the all-window key streams and the tick stage, then prove
    /// the camera and ask where it is. `init`'s ctx can't mail.
    ///
    /// # Errors
    ///
    /// When the config's camera path does not prove: a controller with no
    /// camera would take keys and move nothing, so its birth fails and the
    /// load that asked is told which path.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.subscribe::<WindowCapability, Key>();
        ctx.subscribe::<WindowCapability, KeyRelease>();
        ctx.subscribe::<LifecycleCapability, Tick>();

        self.link_camera(ctx).map_err(|error| ActorInitError::new(format!("the controller's camera: {error}")))
    }

    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        ctx.save_state_kind(0, &ControllerState);
    }

    /// Prove the camera and ask where it is again: a republish does not run
    /// `wire`, and a reference does not outlive the instance that proved it.
    /// A rehydrate cannot refuse, so a camera that no longer proves is
    /// logged and the keys move nothing.
    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_>, _prior: PriorState<'_>) {
        if let Err(error) = self.link_camera(ctx) {
            tracing::error!(target: "aether_kit", %error, "the controller's camera does not prove; the keys move nothing");
        }
    }

    #[handler::event]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, key: Key) {
        self.set_held(key.code, true);
    }

    #[handler::event]
    fn on_key_release(&mut self, _ctx: &mut WasmCtx<'_>, key: KeyRelease) {
        self.set_held(key.code, false);
    }

    /// Step the shadow by the held keys and, if anything moved, send the
    /// camera the pose. Nothing held → no mail.
    #[handler::event]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let Link::Driving { camera, pose } = &mut self.link else {
            return;
        };

        if let Some(stepped) = step(*pose, self.held, &self.config) {
            *pose = stepped;
            ctx.send_to(*camera, &stepped);
        }
    }

    /// The camera's answer to [`Where`]: the pose the keys step from.
    #[handler::response]
    fn on_pose(&mut self, _ctx: &mut WasmCtx<'_>, pose: Pose) {
        self.link = match self.link {
            Link::Unlinked => Link::Unlinked,
            Link::Asked(camera) | Link::Driving { camera, .. } => Link::Driving { camera, pose },
        };
    }
}

impl CameraController {
    /// Prove the config's camera path and ask the camera where it is. A path
    /// that does not prove leaves the controller unlinked.
    fn link_camera(&mut self, ctx: &mut WasmCtx<'_, Self>) -> Result<(), ResolveError> {
        self.link = Link::Unlinked;
        let camera = ctx.resolve(&self.config.camera)?;
        ctx.send_to(camera, &Where);
        self.link = Link::Asked(camera);

        Ok(())
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

/// `pose` stepped one tick by the held keys, or `None` when no mapped key
/// produced motion (the zero-mail-idle invariant). A step whose value a
/// config rate pushed out of range keeps the pose's own.
fn step(pose: Pose, held: Held, config: &ControllerConfig) -> Option<Pose> {
    let mut stepped = pose;
    let mut changed = false;

    // Pan the target across the ground plane in a yaw-relative basis: at
    // yaw 0, W is world-forward (`-Z`) and D is world-right (`+X`); the basis
    // rotates with yaw so the keys stay screen-relative. Diagonals are
    // velocity-normalized so a diagonal covers the same ground as a cardinal.
    let forward = f32::from(held.forward) - f32::from(held.back);
    let right = f32::from(held.right) - f32::from(held.left);
    if forward != 0.0 || right != 0.0 {
        let (sin_yaw, cos_yaw) = pose.yaw.get().sin_cos();
        let ahead = Vec3::new(-sin_yaw, 0.0, -cos_yaw);
        let across = Vec3::new(cos_yaw, 0.0, -sin_yaw);
        stepped.target += (ahead * forward + across * right).normalize() * config.pan_speed;
        changed = true;
    }

    let yaw_dir = f32::from(held.yaw_pos) - f32::from(held.yaw_neg);
    if yaw_dir != 0.0 {
        let yaw = yaw_dir.mul_add(config.yaw_speed, pose.yaw.get()).rem_euclid(TAU);
        stepped.yaw = Yaw::new(yaw).unwrap_or(pose.yaw);
        changed = true;
    }

    let pitch_dir = f32::from(held.pitch_pos) - f32::from(held.pitch_neg);
    if pitch_dir != 0.0 {
        let limit = config.pitch_limit.get().abs();
        let pitch = pitch_dir.mul_add(config.pitch_speed, pose.pitch.get()).clamp(-limit, limit);
        stepped.pitch = Pitch::new(pitch).unwrap_or(pose.pitch);
        changed = true;
    }

    if let Some(factor) = zoom_factor(held, config) {
        let distance = (pose.distance.get() * factor).max(config.distance_floor.get());
        stepped.distance = Distance::new(distance).unwrap_or(pose.distance);
        changed = true;
    }

    changed.then_some(stepped)
}

#[cfg(test)]
mod tests {
    use core::f32::consts::FRAC_PI_2;

    use super::*;

    fn level(yaw: f32) -> Pose {
        Pose { yaw: Yaw::new(yaw).expect("a finite yaw"), pitch: Pitch::new(0.0).expect("level"), ..Pose::BOOT }
    }

    fn held_keys(codes: &[u32]) -> Held {
        let mut controller =
            CameraController { config: ControllerConfig::default(), held: Held::default(), link: Link::Unlinked };
        for &code in codes {
            controller.set_held(code, true);
        }
        controller.held
    }

    #[test]
    fn idle_steps_nothing() {
        // Tripwire: the zero-mail-idle invariant. No mapped key held → no
        // stepped pose → the on_tick handler sends nothing.
        assert_eq!(step(level(0.0), Held::default(), &ControllerConfig::default()), None);
    }

    #[test]
    fn diagonal_pan_matches_cardinal_magnitude() {
        // Tripwire: velocity normalization. A W+D diagonal moves the target
        // the same Euclidean distance per tick as a lone W — no √2 speed-up.
        let config = ControllerConfig::default();

        let cardinal = step(level(0.0), held_keys(&[keycode::KEY_W]), &config).expect("W held pans the target");
        let cardinal_mag = cardinal.target.length();

        let diagonal =
            step(level(0.0), held_keys(&[keycode::KEY_W, keycode::KEY_D]), &config).expect("W+D held pans the target");
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

        let at_zero = step(level(0.0), held_keys(&[keycode::KEY_W]), &config).expect("W held pans the target");
        assert!(at_zero.target.x.abs() < 1e-5, "yaw 0: no X drift");
        assert!(at_zero.target.z < 0.0, "yaw 0: W moves -Z");

        let at_quarter = step(level(FRAC_PI_2), held_keys(&[keycode::KEY_W]), &config).expect("W held pans the target");
        assert!(at_quarter.target.z.abs() < 1e-5, "quarter turn: no Z drift");
        assert!(at_quarter.target.x < 0.0, "quarter turn: W moves -X");
    }

    #[test]
    fn pitch_and_distance_clamp() {
        // Tripwire: the clamps. Holding ↑ forever saturates pitch at
        // +pitch_limit; holding Z forever floors the eye distance rather
        // than collapsing onto the target.
        let config = ControllerConfig::default();

        let mut pose = level(0.0);
        for _ in 0..100_000 {
            pose = step(pose, held_keys(&[keycode::KEY_UP]), &config).expect("↑ held pitches");
        }
        let pitch = pose.pitch.get();
        assert!((pitch - config.pitch_limit.get()).abs() < 1e-4, "pitch saturated at the limit; got {pitch}");

        let mut pose = level(0.0);
        for _ in 0..100_000 {
            pose = step(pose, held_keys(&[keycode::KEY_Z]), &config).expect("Z held zooms");
        }
        let distance = pose.distance.get();
        assert!((distance - config.distance_floor.get()).abs() < 1e-4, "distance floored; got {distance}");
    }
}
