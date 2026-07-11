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
//! # Height pass — plates and walls
//!
//! Every vertex lifts onto the plate-resolved height surface
//! ([`World::surface_height_in`]): cell heights blend into continuous
//! slopes where neighbors sit within the step ceiling
//! ([`crate::world::STEP_MAX_OCTIMETERS`]) and break where they exceed it. Where a cell
//! carries authored per-point relief (`World::cell_has_height_relief`) the
//! pass resolves one stride down: the interior cap tessellates to
//! `SUB × SUB` subcell quads over point patches (`SubPatch`) so a subcell
//! break shows. A relief-free cell keeps the whole-cell fast path,
//! byte-identical to a world with no height points.
//!
//! One bounded `CliffPlan` classifies every canonical east/north
//! sample adjacency in the fixed `320 × 320` apron exactly once. Only a
//! level difference strictly past the step ceiling enters the plan. Each
//! owned window connects those physical crossings with a finite local case:
//! a consistent two-crossing plate takes a smoothing chord; one/three/four
//! crossings pin at a stable world-coordinate junction. No unique-height or
//! `(low, high)` inventory exists, so unrelated legal ramps cannot acquire a
//! false iso-contour and authored level diversity cannot multiply work.
//!
//! Material polygons are intersected with that local height arrangement.
//! Every contour position carries named high/low sample anchors; high and low
//! cap fragments evaluate through those anchors, and the wall ribbon clones
//! the same positions. Sealed rings are therefore byte-identical at both cap
//! seams. Enclosed Void lows close to their authored floor; only an open
//! silhouette keeps the fixed-depth skirt. The three predictive lattice,
//! material-contour, and clip-gap closure passes no longer exist.

pub mod contour;
pub mod style;

mod cliffs;
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
mod atlas_support;
#[cfg(test)]
mod atlas_tests;
#[cfg(test)]
mod tests;

use alloc::vec::Vec;

use aether_capabilities::render::DrawTriangle;

use crate::world::{ChunkPos, World};
use cliffs::CliffPlan;
use constants::{MAX_APRON_SUBCELLS, OCTIMETERS_PER_SUBCELL, SUBCELLS_PER_CHUNK_EDGE};
use coverage::mesh_coverage;
use style::StyleTable;
use underlay::mesh_underlay;
use walls::emit_walls;

fn normalize_cap_winding(triangles: &mut [DrawTriangle]) {
    for triangle in triangles {
        let [a, b, c] = &triangle.verts;
        let signed_xz_area_doubled = (b.x - a.x) * (c.z - a.z) - (c.x - a.x) * (b.z - a.z);
        if signed_xz_area_doubled > 1e-6 {
            triangle.verts.swap(1, 2);
        }
    }
}

/// Whether meshing `at` and every chunk in its invalidation apron keeps all
/// coordinate arithmetic inside the mesher's `i32` octimeter domain.
///
/// A mesh reads [`MAX_APRON_SUBCELLS`] before and after its chunk. The height
/// break check also reads one sample before the low edge; the high edge's
/// exclusive sample loop plus its one-sample check lands exactly at
/// `SUBCELLS_PER_CHUNK_EDGE + MAX_APRON_SUBCELLS`. Operator invalidation may
/// remesh one neighboring chunk beyond the touched chunk, so validate all
/// three chunk positions on each axis. The proof uses `i64`; the mesher can
/// continue using its compact `i32` arithmetic after this preflight passes.
pub(super) fn chunk_remesh_extent_is_coordinate_safe(at: ChunkPos) -> bool {
    fn mesh_axis_is_coordinate_safe(chunk: i64) -> bool {
        let chunk_subcell = chunk * i64::from(SUBCELLS_PER_CHUNK_EDGE);
        let min_subcell = chunk_subcell - i64::from(MAX_APRON_SUBCELLS) - 1;
        let max_subcell = chunk_subcell + i64::from(SUBCELLS_PER_CHUNK_EDGE) + i64::from(MAX_APRON_SUBCELLS);
        let octimeters = i64::from(OCTIMETERS_PER_SUBCELL);
        i32::try_from(min_subcell * octimeters).is_ok() && i32::try_from(max_subcell * octimeters).is_ok()
    }

    (-1..=1).all(|delta| mesh_axis_is_coordinate_safe(i64::from(at.x) + delta))
        && (-1..=1).all(|delta| mesh_axis_is_coordinate_safe(i64::from(at.z) + delta))
}

/// Mesh one chunk into its flat-color base triangle list. Pure — no wgpu,
/// no ctx — so it is unit-testable host-side. Reads neighbor cells through
/// [`World`] (a bounded apron); a missing neighbor reads as empty. `styles`
/// resolves each material's flat color.
#[must_use]
pub fn mesh_chunk(world: &World, at: ChunkPos, styles: &StyleTable) -> Vec<DrawTriangle> {
    let mut tris = Vec::new();
    let cliffs = CliffPlan::build(world, at);
    mesh_underlay(world, at, &cliffs, styles, &mut tris);
    mesh_coverage(world, at, styles, &mut tris);
    emit_walls(world, styles, &cliffs, &mut tris);
    normalize_cap_winding(&mut tris);

    tris
}
