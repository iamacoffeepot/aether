//! `warp-figure` — a spike that animates a voxel humanoid by stretching the
//! space it lives in, never by emitting voxels.
//!
//! The figure is built once, at `init`: a chunky lattice humanoid (legs,
//! torso, head, a hanging left arm, and a right arm reaching along `+X`) whose
//! boundary is extracted into a triangle list over a **shared** corner set.
//! From then on the material is frozen. Every frame the actor evaluates a
//! displacement field over those corners and re-emits the same triangles at
//! their new positions, so the arm's reach is a property of the map rather
//! than of the data.
//!
//! Sharing the corners is what makes this safe. Because a corner belongs to
//! every face that meets there, a displacement moves them together — the
//! surface cannot come apart, and the field is free to be as aggressive as it
//! likes. The field's ramp vanishes at the shoulder plane, so the arm
//! elongates out of a joint that never moves and the space between shoulder
//! and hand visibly expands.
//!
//! Split by concept: [`figure`] owns the material (lattice, extraction,
//! shading), [`warp`] owns the map (the field and its anchors), and this
//! module owns the actor that pumps one through the other.
//!
//! # Agent
//! Load the component and give it a camera. It subscribes the frame lifecycle
//! itself and needs no driver mail — there are no `aether.spike.warp.*` kinds
//! to send. Frame it with an orbit camera at roughly `distance = 4.5` on
//! `target = (0.75, 1.15, 0.0)`, the center of the swept bounds. At rest the
//! figure spans `x ∈ [-0.75, 1.35]`, `y ∈ [0.0, 2.30]`, `z ∈ [-0.25, 0.25]`;
//! at full stretch the hand reaches `x = 2.25`. It submits 1684 triangles per
//! frame over 844 shared corners.

pub mod figure;
pub mod warp;

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_kinds::{Render, Tick};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::{TAU, Vec3};
use aether_render::{DrawTriangle, RenderCapability, Vertex};

use crate::figure::Surface;
use crate::warp::{PERIOD_SECONDS, Warp};

/// The spike's actor. `surface` is the material — written once at `init` and
/// read-only afterwards. `warped` and `emitted` are per-frame scratch, sized
/// once so a frame allocates nothing.
pub struct WarpFigure {
    surface: Surface,
    warped: Vec<Vec3>,
    emitted: Vec<DrawTriangle>,
    phase: f32,
}

#[actor]
impl WasmActor for WarpFigure {
    const NAMESPACE: &'static str = "aether.spike.warp";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let surface = figure::build();
        let warped = vec![Vec3::ZERO; surface.rest.len()];
        let emitted = vec![DrawTriangle::default(); surface.triangles.len()];

        Ok(Self { surface, warped, emitted, phase: 0.0 })
    }

    /// Subscribe the two frame stages this actor needs (ADR-0082): `Tick` to
    /// advance the field and re-evaluate it, `Render` to submit. Splitting
    /// them keeps submission after the whole tick chain has settled, matching
    /// the camera's shape. Lives in `wire` — `init` has no send surface.
    ///
    /// On a chassis whose lifecycle graph omits `Render` (headless), the cap
    /// replies `Err(UnsupportedStage)` to this fire-and-forget subscribe; the
    /// reply warn-drops and the figure simply never submits, which is a no-op
    /// where the render cap discards anyway.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        let lifecycle = ctx.actor::<LifecycleCapability>();
        lifecycle.subscribe::<Tick>();
        lifecycle.subscribe::<Render>();
    }

    /// Advance the phase and re-evaluate the displacement field over every
    /// shared corner, then rebuild the emitted triangles from the warped
    /// positions. The lattice is untouched — this is the whole animation.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler::single]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, tick: Tick) {
        self.phase = (self.phase + tick.delta_seconds() * TAU / PERIOD_SECONDS) % TAU;

        let Self { surface, warped, emitted, phase } = self;
        let warp = Warp::at_phase(*phase);
        for (out, rest) in warped.iter_mut().zip(&surface.rest) {
            *out = warp.apply(*rest);
        }

        for ((triangle, indices), color) in emitted.iter_mut().zip(&surface.triangles).zip(&surface.colors) {
            *triangle = DrawTriangle {
                verts: indices.map(|index| {
                    let p = warped[index as usize];
                    Vertex { x: p.x, y: p.y, z: p.z, color: *color }
                }),
            };
        }
    }

    /// Submit the frame's triangles on the `Render` stage, after the tick
    /// chain that recomputed them has settled.
    ///
    /// # Agent
    /// Substrate-driven; do not send manually. If nothing renders, check that
    /// a camera is publishing `aether.view_projection` — the figure sits
    /// around the origin and is invisible under the identity default.
    #[handler::single]
    fn on_render(&mut self, ctx: &mut WasmCtx<'_>, _render: Render) {
        ctx.actor::<RenderCapability>().send_many(&self.emitted);
    }
}

aether_actor::export!(WarpFigure);
