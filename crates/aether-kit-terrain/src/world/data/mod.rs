// Serialization casts here are bounded by the fixed plane geometry: a
// chunk has 256 cells, region ids and plane lengths are small by
// construction, and `region: Vec<u32>` narrows to the `[u16; 256]`
// region plane by design (ids past u16 are not addressable). The
// truncation these lints warn about cannot occur in this domain.
#![allow(clippy::cast_possible_truncation)]
// `chunk_index` casts a `rem_euclid` result (always `0..16`) to `usize`;
// the sign-loss the lint warns about cannot occur.
#![allow(clippy::cast_sign_loss)]
// The surface-height math casts octimeter heights and cell coordinates
// (small bounded integers) to f32; the precision loss the lint warns
// about cannot occur in this range.
#![allow(clippy::cast_precision_loss)]
// The bilinear surface interpolation is written as explicit multiply-add
// chains for readability; a fused mul_add would need a libm symbol on
// the wasm target and does not change the result meaningfully.
#![allow(clippy::suboptimal_flops)]

//! The chunked world plane stack.
//!
//! The world is a **cell lattice**: cells are addresses, not objects.
//! What a cell *is* — its ground fabric, elevation, region membership —
//! lives in a stack of property planes over that lattice, chunked
//! `16 × 16` cells (a [`Chunk`]). A [`World`] holds a sparse set of
//! chunks keyed by [`ChunkPos`] plus a region table.
//!
//! # Units
//!
//! `1 cell = 1 m = 256 octimeters` (the fixed-point movement quantum).
//! The chunk a cell sits in is the cell right-shifted by [`CHUNK_BITS`]
//! — an arithmetic shift, so a negative cell floors toward `-∞` (cell
//! `-1` is in chunk
//! `-1`, not `0`), which is the intended lattice tiling. A cell's index
//! within its chunk is `z.rem_euclid(16) * 16 + x.rem_euclid(16)`
//! (row-major), so negative cells index their chunk correctly.
//!
//! # Ground fabric — underlay / overlay
//!
//! Two material planes:
//!
//! - [`Chunk::underlay`] — the ground fabric. This is what the region
//!   cascade resolves ([`World::underlay`]): the cell's own underlay if
//!   non-[`Material::Void`], else the cell's region's `default_material`,
//!   else `Void`.
//! - [`Chunk::overlay`] — an optional crisp placed surface (path, floor).
//!   `Void` means no overlay. Never cascade-resolved
//!   ([`World::overlay`] is a raw plane read).
//! - [`Chunk::overlay_mask`] — a scalar overlay coverage byte per subcell
//!   (`z*SUB + x` inside each cell, row-major cells). `255` is full
//!   coverage and `0` is none; meshing thresholds at half coverage, so
//!   legacy binary data keeps its exact midpoint crossings while soft
//!   authored edges can place crossings between subcells. The subcell is
//!   the finest semantic resolution — paint must not out-resolve movement
//!   / blocking, which resolve at the subcell. Binary 0/255 samples
//!   OR-compose within the overlay layer; scalar blends are authored
//!   coverage values. Painter's order applies across layers.
//! - [`Chunk::underlay_points`] — an optional per-subcell material plane
//!   that shapes the ground fabric below cell scale. Each point is a
//!   [`Material`] byte or the [`UNDERLAY_POINT_INHERIT`] sentinel; an
//!   inherit point resolves to its cell's cascade
//!   ([`World::underlay_point`]), so an all-inherit plane meshes exactly as
//!   the per-cell [`Chunk::underlay`]. An explicit point — including a
//!   `Void` one that cuts a hole — moves sub-cell shape (a hexagonal
//!   column, a two-material seam inside a cell) into authored data; the
//!   mesher samples it in place of expanding the cell material. The cell
//!   byte stays the gameplay and cascade truth — points are presentation-
//!   layer ground shaping bounded inside the cell.
//!
//! # Water planes
//!
//! [`Material::Water`] is underlay ground fabric, not an overlay: a cell
//! painted Water in the underlay reads as water, tiling and smoothing
//! through the same partition as every other material. What makes its
//! surface flat is a **water-plane table** — [`WaterPlane`] rows holding a
//! [`WaterPlane::level_octimeters`], referenced by 1-based id from the
//! per-cell [`Chunk::water_plane`] plane (`0` = the datum-0 level). A water
//! cell resolves its surface at its plane's level ([`World::water_level`])
//! rather than its own lakebed [`Chunk::height`], so the surface lies flat
//! regardless of the ground beneath — disconnected areas sharing a plane
//! share a level (one sea row every coastal cell references).
//!
//! # Height — cell stride and per-point relief
//!
//! Elevation lives in two layers. [`Chunk::height`] is one octimeter value
//! per cell (the cascade carrier and semantic anchor movement, water, and the
//! corner cascade hang off), and [`Chunk::height_points`] adds an optional
//! `i16` delta per subcell over it: [`World::point_height`] resolves
//! `height(cell) + delta`, so an all-zero plane resolves at cell stride and a
//! flat or legacy world is byte-identical to the per-cell height. The height
//! pipeline — the corner-plate walk, the cliff test, and the bilinear patches
//! [`World::cell_corner_heights`] / [`World::surface_height_in`] — generalizes
//! one stride down to the point lattice **only where deltas exist**
//! (`World::cell_has_height_relief` gates it); an authored delta is real,
//! standable relief the mesher caps and walls draw and a mover stands on
//! (drawn≡stood-on holds at subcell scale). Water pins per cell: a water
//! cell's surface is its plane level regardless of its points, so a delta
//! under water is lakebed relief the flat surface ignores.

mod chunk;
mod codec;
mod coords;
#[cfg(test)]
mod fixture;
mod layout;
mod material;
mod plane;
mod surface;
mod table;
mod world;

pub use chunk::Chunk;
pub use codec::WorldDecodeError;
pub use coords::{CellPos, ChunkPos};
pub use layout::{
    CELLS_PER_CHUNK, CELLS_PER_CHUNK_AREA, CHUNK_BITS, HEIGHT_POINT_INHERIT, HEIGHT_POINTS_PER_CHUNK,
    OVERLAY_MASK_WIRE_BYTES, SCALAR_COVERAGE_THRESHOLD, SUBCELLS_PER_CELL, SUBCELLS_PER_CELL_EDGE,
    UNDERLAY_POINT_INHERIT, UNDERLAY_POINTS_PER_CHUNK,
};
pub use material::Material;
pub use surface::STEP_MAX_OCTIMETERS;
pub use table::{MAX_SMOOTHING_ITERATIONS, Region, SmoothingProfile, WaterPlane};
pub use world::World;

pub(super) use material::cliff_material_from_u8;
