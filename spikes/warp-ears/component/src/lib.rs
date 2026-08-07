//! `warp-ears` — one kitsune ear, rendered twice, posed two ways.
//!
//! The left instance is linear-blend skinning. The right instance is a warp
//! displacement field carrying a det-J fold guard. They are driven from the
//! **same** bone transforms and the **same** volumetrized weights over the
//! **same** corner lattice, so they agree by construction wherever the pose is
//! benign, and every difference on screen is a difference between the two
//! representations rather than between two implementations. A test holds them
//! to that ([`warp::tests::the_two_paths_agree_at_a_benign_pose`]); without it
//! the whole comparison would be unfalsifiable.
//!
//! The program walks the ear through a natural flick, a half-turn twist, and a
//! fold onto a contact slab. Three things come out of it:
//!
//! - At the twist the skinned instance's mid-ear cross-sections collapse —
//!   linear matrix blending has nothing to interpolate through when two bone
//!   rotations approach antipodal. The warp instance is handed the identical
//!   collapse and *refuses* it: the guard bisects a uniform scale on the
//!   displacement until the worst occupied cell's Jacobian determinant clears a
//!   floor, and draws the largest fraction of the pose that stays non-inverted.
//! - The warp instance tints each cell by that determinant, warm as it
//!   compresses toward the floor and cool as it expands. The skinned instance
//!   cannot draw the equivalent, because its output is a position rather than a
//!   map and there is nothing left to measure.
//! - At the fold both ears drive through the contact slab identically. Neither
//!   representation has anything to say about two surfaces overlapping — the
//!   guard is a statement about a cell inverting, not about contact — so the
//!   pose that a fold guard is powerless against is drawn as a demonstration
//!   rather than argued about.
//!
//! Split by concept: [`data`] is the baked dataset, [`ear`] the material and
//! its one-shot extraction, [`rig`] the bones and weights both paths share,
//! [`program`] the scripted timeline, [`lbs`] and [`warp`] the two paths,
//! [`slab`] the contact plate, [`kinds`] the driver mails, and this module the
//! actor that pumps one through the other.
//!
//! # Agent
//! Load the component and give it a camera. It subscribes the frame lifecycle
//! itself. Pin a pose with `aether.spike.warp-ears.set_phase { phase }` before
//! capturing — the free-running loop takes 12 seconds and a capture without a
//! pin lands wherever it lands. Resume with
//! `aether.spike.warp-ears.set_auto { auto: true }`.
//!
//! Phases worth capturing: `0.05` rest, `0.136` the flick's peak, `0.45` the
//! twist at 90°, `0.60` the full half-turn (the guard is engaged and the tint
//! is hot), `0.90` the deepest fold. Frame it with an orbit camera on
//! `target = (0.0, 0.75, 0.0)` at `distance ≈ 5.0`, looking from the front with
//! a little yaw so the twist reads as depth rather than as a silhouette.

pub mod curve;
pub mod data;
pub mod ear;
pub mod kinds;
pub mod lbs;
pub mod program;
pub mod rig;
pub mod slab;
pub mod warp;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_kinds::{Render, Tick};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::{Rgb, Vec3};
use aether_render::{DrawTriangle, RenderCapability, Vertex};

use crate::ear::Surface;
use crate::kinds::{SetAuto, SetPhase};
use crate::program::{PERIOD_SECONDS, Program};
use crate::rig::Rig;
use crate::warp::Guard;

/// World offset of the skinned instance. Negative `x` puts it on the left of a
/// camera looking down `−Z` from in front.
///
/// The separation is set by the fold, not by the rest pose: both ears swing
/// through 2.4 world units of `x` on their way down against the contact plate
/// (they fold toward `−x`, since that is the direction the skull is in), and at
/// a tighter spacing the left instance's tip would arrive inside the right
/// instance's plate. Two silhouettes that overlap for part of the program are
/// worse than a slightly wider frame.
const SKINNED_OFFSET: Vec3 = Vec3::new(-1.5, 0.0, 0.0);

/// World offset of the warp instance, the same distance to the right.
const WARPED_OFFSET: Vec3 = Vec3::new(1.5, 0.0, 0.0);

/// Direction *towards* the key light, normalized at use. Grazes the front and
/// the character's-left side so both instances keep a lit face toward a camera
/// in front of them.
const LIGHT_DIRECTION: Vec3 = Vec3::new(0.42, 0.78, 0.62);

/// Fraction of a face's color that survives with the light fully behind it.
const AMBIENT: f32 = 0.36;

/// One instance's per-frame scratch: the posed corner lattice and the
/// triangles built from it. Sized once at `init`, so a frame allocates nothing.
struct Instance {
    corners: Vec<Vec3>,
    offset: Vec3,
}

/// The spike's actor. `surface`, `rig`, and `slab` are the material — written
/// once at `init` and read-only afterwards. Everything else is per-frame
/// scratch or the two pieces of animation state.
pub struct WarpEars {
    surface: Surface,
    rig: Rig,
    slab: Vec<[Vec3; 3]>,
    skinned: Instance,
    warped: Instance,
    field: Vec<Vec3>,
    determinants: Vec<f32>,
    emitted: Vec<DrawTriangle>,
    guard: Guard,
    phase: f32,
    auto: bool,
}

#[actor]
impl WasmActor for WarpEars {
    // A genuine dash-sibling of `aether.spike.warp`, per the repo's naming
    // rule: this is the ears-flavoured adjacent spike, not a nesting under the
    // figure spike and not a generic multi-word segment. The dash carries no
    // addressing semantics — the full namespace determines identity.
    const NAMESPACE: &'static str = "aether.spike.warp-ears";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let surface = ear::build();
        let rig = Rig::build(&surface.rest);
        let slab = slab::build(rig.contact_point, rig.contact_normal);

        let corners = surface.rest.len();
        let triangles = surface.triangles.len() + slab.len();
        let mut actor = Self {
            skinned: Instance { corners: vec![Vec3::ZERO; corners], offset: SKINNED_OFFSET },
            warped: Instance { corners: vec![Vec3::ZERO; corners], offset: WARPED_OFFSET },
            field: vec![Vec3::ZERO; corners],
            determinants: vec![1.0; surface.cells.len()],
            emitted: vec![DrawTriangle::default(); triangles * 2],
            guard: Guard { applied: 1.0, min_determinant: 1.0 },
            phase: 0.0,
            auto: true,
            surface,
            rig,
            slab,
        };
        actor.rebuild();

        Ok(actor)
    }

    /// Subscribe the two frame stages this actor needs (ADR-0082): `Tick` to
    /// advance the program and re-pose both instances, `Render` to submit.
    /// Splitting them keeps submission after the whole tick chain has settled,
    /// matching the camera's shape. Lives in `wire` — `init` has no send
    /// surface.
    ///
    /// On a chassis whose lifecycle graph omits `Render` (headless), the cap
    /// replies `Err(UnsupportedStage)` to this fire-and-forget subscribe; the
    /// reply warn-drops and the ears simply never submit, which is a no-op
    /// where the render cap discards anyway.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        let lifecycle = ctx.actor::<LifecycleCapability>();
        lifecycle.subscribe::<Tick>();
        lifecycle.subscribe::<Render>();
    }

    /// Advance the phase when auto-advance is on, then re-pose both instances.
    /// The lattice is untouched — the pose is entirely a property of the map.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually. Pin a phase with
    /// `set_phase` instead.
    #[handler::single]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, tick: Tick) {
        if self.auto {
            self.phase = (self.phase + tick.delta_seconds() / PERIOD_SECONDS).fract();
        }
        self.rebuild();
    }

    /// Pin the program at one phase and stop auto-advance, re-posing
    /// immediately so a capture bundled behind this mail is phase-exact rather
    /// than one tick stale.
    ///
    /// # Agent
    /// The mail to send before every capture. `phase` is clamped to `[0, 1]`.
    #[handler::single]
    fn on_set_phase(&mut self, _ctx: &mut WasmCtx<'_>, mail: SetPhase) {
        self.phase = mail.phase.clamp(0.0, 1.0);
        self.auto = false;
        self.rebuild();
    }

    /// Resume or re-stop the free-running loop from wherever the phase
    /// currently sits.
    ///
    /// # Agent
    /// Send `{ auto: true }` to hand the program back to the clock after a
    /// series of pinned captures.
    #[handler::single]
    fn on_set_auto(&mut self, _ctx: &mut WasmCtx<'_>, mail: SetAuto) {
        self.auto = mail.auto;
    }

    /// Submit the frame's triangles on the `Render` stage, after the tick chain
    /// that recomputed them has settled.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If nothing renders, check that a
    /// camera is publishing `aether.view_projection` — both instances sit
    /// around the origin and are invisible under the identity default.
    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        ctx.actor::<RenderCapability>().send_many(&self.emitted);
    }
}

impl WarpEars {
    /// Pose both instances at the current phase and rebuild every emitted
    /// triangle. The one place the two paths are run, so they are always run
    /// against the same pose.
    fn rebuild(&mut self) {
        let pose = self.rig.pose(&Program::at_phase(self.phase));

        lbs::pose_corners(&self.surface.rest, &self.rig.weights, &pose, &mut self.skinned.corners);

        warp::displacement(&self.surface.rest, &self.rig.weights, &pose, &mut self.field);
        self.guard = warp::guard(
            &self.surface.cells,
            &self.surface.rest,
            &self.field,
            &mut self.warped.corners,
            &mut self.determinants,
        );

        let ear_triangles = self.surface.triangles.len();
        let slab_triangles = self.slab.len();
        let (skinned_half, warped_half) = self.emitted.split_at_mut(ear_triangles + slab_triangles);

        let (skinned_ear, skinned_slab) = skinned_half.split_at_mut(ear_triangles);
        emit_ear(skinned_ear, &self.surface, &self.skinned, None);
        emit_slab(skinned_slab, &self.slab, self.skinned.offset);

        let (warped_ear, warped_slab) = warped_half.split_at_mut(ear_triangles);
        emit_ear(warped_ear, &self.surface, &self.warped, Some(&self.determinants));
        emit_slab(warped_slab, &self.slab, self.warped.offset);
    }

    /// The guard's verdict for the frame currently emitted. `applied < 1.0`
    /// means the warp instance is drawing less than the pose the skinned
    /// instance drew in full.
    #[must_use]
    pub const fn guard(&self) -> Guard {
        self.guard
    }
}

/// Build one instance's ear triangles from its posed corners. `determinants`
/// present tints each cell by its local volume ratio; absent leaves the class
/// colors alone, which is exactly the difference between the two sides.
fn emit_ear(out: &mut [DrawTriangle], surface: &Surface, instance: &Instance, determinants: Option<&[f32]>) {
    for (((slot, indices), &color), &cell) in
        out.iter_mut().zip(&surface.triangles).zip(&surface.colors).zip(&surface.tri_cell)
    {
        let verts = indices.map(|index| instance.corners[index as usize] + instance.offset);
        let color = determinants.map_or(color, |dets| warp::tint(color, dets[cell as usize]));
        *slot = triangle(verts, shade(color, verts));
    }
}

/// Build one instance's contact plate. The plate does not move, so only the
/// instance offset separates the two copies.
fn emit_slab(out: &mut [DrawTriangle], slab: &[[Vec3; 3]], offset: Vec3) {
    for (slot, &verts) in out.iter_mut().zip(slab) {
        let verts = verts.map(|vertex| vertex + offset);
        *slot = triangle(verts, shade(slab::COLOR, verts));
    }
}

fn triangle(verts: [Vec3; 3], color: Rgb) -> DrawTriangle {
    DrawTriangle { verts: verts.map(|p| Vertex { x: p.x, y: p.y, z: p.z, color }) }
}

/// Lambert-shade a triangle from its *posed* geometry.
///
/// The rest-pose normal is useless here — the twist and the fold rotate faces
/// through most of a hemisphere, so a baked shading term would light the ear as
/// if it had never moved. At this triangle count the cross product per frame is
/// cheaper than the bookkeeping any alternative would need.
fn shade(color: Rgb, verts: [Vec3; 3]) -> Rgb {
    let normal = (verts[1] - verts[0]).cross(verts[2] - verts[0]).normalize_or(Vec3::Y);
    let lambert = (1.0 - AMBIENT).mul_add(normal.dot(LIGHT_DIRECTION.normalize()).max(0.0), AMBIENT);

    Rgb::new(color.r * lambert, color.g * lambert, color.b * lambert)
}

aether_actor::export!(WarpEars);
