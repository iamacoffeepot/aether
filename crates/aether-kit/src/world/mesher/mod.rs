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
//! break shows, the marched wall split reads point (not cell) levels so a
//! silhouette raised by deltas alone closes its wall on the authored line,
//! and same-material breaks close as subcell-lattice walls
//! (`emit_lattice_closure`) standing exactly on the authored break lines. A
//! relief-free cell keeps the whole-cell fast path, byte-identical to a
//! world with no height points.
//!
//! On the cell lattice the corner plates split exactly on cliff edges, and
//! the wall pass closes that gap with a vertical face wearing the high
//! cell's region cliff material as a flat color. The walls stitch from the
//! same repartitioned sample grid the caps march, as the union of two
//! segment classes over one pass: a material or Void boundary standing past
//! the step ceiling lofts its marched contour down as a curtain — the
//! wall's top vertices are the cap contour's own vertices lifted through
//! the same owner-clamped patch, so the seam is watertight by construction
//! — while a same-material cliff, which the material partition leaves no
//! boundary to follow, lofts the cell-edge lattice line the owner-pinned
//! patches already break on. Where the low side is a Void hole with no
//! ground the curtain drops a fixed depth so the hole reads as thick ground
//! rather than a paper lip. Boundary windows lift each vertex through its
//! own (floor) cell — continuous wherever no cliff intervenes.

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
use constants::{MAX_APRON_SUBCELLS, OCTIMETERS_PER_SUBCELL, SUBCELLS_PER_CHUNK_EDGE};
use coverage::mesh_coverage;
use style::StyleTable;
use underlay::mesh_underlay;
use walls::emit_walls;

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
        let max_subcell =
            chunk_subcell + i64::from(SUBCELLS_PER_CHUNK_EDGE) + i64::from(MAX_APRON_SUBCELLS);
        let octimeters = i64::from(OCTIMETERS_PER_SUBCELL);
        i32::try_from(min_subcell * octimeters).is_ok()
            && i32::try_from(max_subcell * octimeters).is_ok()
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
    let partition = mesh_underlay(world, at, styles, &mut tris);
    mesh_coverage(world, at, styles, &mut tris);
    emit_walls(world, at, styles, partition.as_ref(), &mut tris);
    tris
}
