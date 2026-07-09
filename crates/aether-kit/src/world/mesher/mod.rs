// Chunk-local loop counters and world-cell / octimeter coordinates are
// small integers cast between i32 (coordinate math), usize (plane and
// grid indexing), and f32 (vertex output). The ranges — chunk-bounded
// cells, octimeter positions within a chunk plus a bounded apron — make
// the pedantic precision / sign / truncation lints non-issues here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
// The bilinear-patch and shade arithmetic is written as explicit
// multiply-add chains for readability; a fused mul_add would need a libm
// symbol on the wasm target and does not change the result meaningfully.
#![allow(clippy::suboptimal_flops)]

//! The world-view mesher: a pure function over the plane stack
//! ([`crate::world`]) that turns one chunk into a triangle list, read by
//! the [`WorldView`](super::WorldView) actor and replayed to
//! `"aether.render"` each frame.
//!
//! [`mesh_chunk`] emits a flat-color marching-squares base render, pure and
//! host-testable (no wgpu, no ctx). Every color and geometry decision is a
//! function of world coordinates and the neighbor cells' planes alone, so
//! two chunks agree on their shared border with no shared state.
//!
//! # Underlay pass — one surface, repartitioned
//!
//! The ground is a single surface, world-space in meters (`1 cell = 1 m`),
//! tiled exactly by its material regions. The cascade-resolved material
//! grid runs through [`contour::repartition`] with smoothing disabled (zero
//! iterations — the crisp path, no chamfer), so the partition boundary is
//! the raw marching-squares staircase. Wherever a cell and its one-sample
//! surround are uniformly one material, the cell emits a flat keyed quilt
//! cell in that material's [`style::flat_color`]; everywhere else the partition
//! marches per window, each label's polygon colored by its material keyed
//! at the owning cell, saddles resolved by label order so every window
//! tiles.
//!
//! # Height pass — one lofted level ribbon
//!
//! Every vertex lifts onto the plate-resolved height surface
//! ([`World::surface_height_in`]): cell heights blend into continuous
//! slopes where neighbors sit within the step ceiling
//! ([`crate::world::STEP_MAX_OCTIMETERS`]) and form a cliff when they exceed
//! it. The height pass samples `point_surface_level_at` over the chunk plus
//! apron, discovers each distinct low/high cliff step, and projects that
//! scalar interval to `0..=255`: low floors to zero, high saturates, and any
//! intermediate authored levels retain their fraction. `minimize_corners`
//! bilinearly reconstructs and smooths this separate level plane, then the
//! scalar `127.5` isoline is marched as the cliff contour. Unlike the
//! material partition, the level plane deliberately does not consume the
//! partition's frozen mask — the isoline is the boundary being smoothed.
//!
//! One march emits the high cap and records its exact contour vertices.
//! Those same vertex values become the wall's top ring; the bottom ring
//! copies their `(x, z)` bits and drops only `y` to the low level. Cap and
//! wall therefore share a seam by identity, not by a second edge prediction.
//! High boundary patches in the ordinary material cap are omitted so this
//! smooth cap ribbon owns convex corners; material caps draw afterward and
//! retain the crisp frozen partition over the loft interior. Same-material,
//! material-boundary, and solid/Void cliffs all follow this one level-field
//! path — there are no lattice, contour-closure, or sliver-repair wall
//! classes to reconcile. Relief-free interiors keep the whole-cell fast
//! path; authored point relief resolves through `SubPatch` at subcell stride.

pub mod contour;
pub mod style;

mod constants;
mod coverage;
mod geometry;
mod partition;
mod surface;
mod underlay;
mod voids;
mod walls;
mod windows;

#[cfg(test)]
mod tests;

use alloc::vec::Vec;

use aether_capabilities::render::DrawTriangle;

use crate::world::{ChunkPos, World};
use coverage::mesh_coverage;
use style::StyleTable;
use underlay::mesh_underlay;
use walls::emit_lofts;

/// Mesh one chunk into its flat-color base triangle list. Pure — no wgpu,
/// no ctx — so it is unit-testable host-side. Reads neighbor cells through
/// [`World`] (a bounded apron); a missing neighbor reads as empty. `styles`
/// resolves each material's flat color.
#[must_use]
pub fn mesh_chunk(world: &World, at: ChunkPos, styles: &StyleTable) -> Vec<DrawTriangle> {
    let mut tris = Vec::new();
    // The loft cap lands first. Material caps then overdraw its interior at
    // equal depth while deliberately omitted high-boundary patches leave the
    // smooth contour ribbon exposed.
    emit_lofts(world, at, styles, &mut tris);
    mesh_underlay(world, at, styles, &mut tris);
    mesh_coverage(world, at, styles, &mut tris);
    tris
}
