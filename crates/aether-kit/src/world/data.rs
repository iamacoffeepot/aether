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

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;
use serde::{Deserialize, Serialize};

use super::kinds::{TerrainSurface, WorldPoint};

/// Cells along one edge of a chunk. Chunks are `16 × 16` cells.
pub const CELLS_PER_CHUNK: i32 = 16;

/// Right-shift a cell coordinate by this to derive its chunk
/// (`2^4 = 16` cells per chunk edge). Arithmetic shift — floors on
/// negatives.
pub const CHUNK_BITS: u32 = 4;

/// Cells in one chunk: `16 × 16`. The length of every per-chunk plane.
pub const CELLS_PER_CHUNK_AREA: usize = 256;

/// Subcells along one edge of a cell — the overlay coverage resolution.
/// A cell's [`Chunk::overlay_mask`] stores one scalar coverage byte for
/// each `SUB × SUB` subcell sample. Raising this is a single-constant
/// change; the wire plane length is derived from it
/// ([`OVERLAY_MASK_WIRE_BYTES`]), never hard-coded.
pub const SUBCELLS_PER_CELL_EDGE: u32 = 16;

/// Scalar-coverage samples at or above this value belong to the rendered
/// overlay surface. The contour marcher and terrain sampler share this exact
/// threshold so a picked overlay cannot disagree with the visible field.
pub const SCALAR_COVERAGE_THRESHOLD: u8 = 128;

/// Coverage samples in one cell's overlay plane: `SUB²`.
pub const SUBCELLS_PER_CELL: usize = (SUBCELLS_PER_CELL_EDGE * SUBCELLS_PER_CELL_EDGE) as usize;

/// Wire length in bytes of a chunk's overlay-mask plane:
/// `CELLS_PER_CHUNK_AREA * SUB²` (= 65536 at `SUB = 16`). Version 7 and
/// newer store one coverage byte per subcell, row-major cell order and
/// `z*SUB + x` within each cell. Older save versions stored little-endian
/// bitmasks and expand on decode to `0` / `255`.
pub const OVERLAY_MASK_WIRE_BYTES: usize = CELLS_PER_CHUNK_AREA * SUBCELLS_PER_CELL;

/// Points in one chunk's underlay-point plane: `CELLS_PER_CHUNK_AREA *
/// SUB²` (= 65536 at `SUB = 16`) — one material point per subcell, row-major
/// cell order, and within a cell the same `z*SUB + x` subcell order as the
/// overlay mask. Stride-agnostic by construction: raising
/// [`SUBCELLS_PER_CELL_EDGE`] regrows the plane with no other change.
pub const UNDERLAY_POINTS_PER_CHUNK: usize = CELLS_PER_CHUNK_AREA * SUBCELLS_PER_CELL;

/// The underlay-point sentinel: a point holding this **inherits** its
/// cell's cascade-resolved material ([`World::underlay`]) rather than
/// pinning one. An untouched world stores every point as this, so it
/// meshes exactly as a per-cell underlay. An explicit `0..=5` byte pins the
/// point to a [`Material`] — including `0` = authored [`Material::Void`],
/// which is what cuts a shape or a hole below cell scale.
pub const UNDERLAY_POINT_INHERIT: u8 = 255;

/// Points in one chunk's height-delta plane: `CELLS_PER_CHUNK_AREA * SUB²`
/// (= 65536 at `SUB = 16`), one `i16` octimeter delta per subcell in the same
/// row-major cell order and `z*SUB + x` within-cell order as
/// [`UNDERLAY_POINTS_PER_CHUNK`]. Stride-agnostic by construction: raising
/// [`SUBCELLS_PER_CELL_EDGE`] regrows the plane with no other change.
pub const HEIGHT_POINTS_PER_CHUNK: usize = CELLS_PER_CHUNK_AREA * SUBCELLS_PER_CELL;

/// The height-point default: a point holding this **inherits** its cell's
/// [`Chunk::height`] with no relief. An untouched world stores every point
/// as this, so its surface resolves exactly at cell stride; an explicit
/// non-zero delta lifts (or drops) the point off the cell height by that
/// many octimeters ([`World::point_height`]), the subcell-resolution relief
/// the height pipeline resolves one stride down.
pub const HEIGHT_POINT_INHERIT: i16 = 0;

/// Pre-v7 saves store overlay coverage as one 16-bit cell mask, fixed at the
/// old 4x4 subcell lattice. Decode expands each legacy bit to the current
/// subcell block so old binary masks keep the same world-space shape.
const LEGACY_MASK_SUBCELLS_PER_CELL_EDGE: usize = 4;

/// Octimeters per cell: `1 cell = 1 m = 256 octimeters`.
const OCTIMETERS_PER_CELL: i32 = 256;

/// Right-shift an octimeter coordinate by this to derive its cell.
const OCTIMETER_BITS: u32 = 8;

/// A cell address on the world lattice. Cells are addresses; their
/// properties live in the plane stack.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct CellPos {
    pub x: i32,
    pub z: i32,
}

/// A chunk address — a cell address right-shifted by [`CHUNK_BITS`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

impl CellPos {
    /// The chunk this cell belongs to. Arithmetic right shift, so
    /// negative cells floor toward `-∞`.
    #[must_use]
    pub fn chunk(self) -> ChunkPos {
        ChunkPos { x: self.x >> CHUNK_BITS, z: self.z >> CHUNK_BITS }
    }

    /// The cell's center in octimeters — cell-center-anchored, so a
    /// mover placed here sits in the middle of the cell, not on its
    /// corner. `(x << 8) + 128`.
    #[must_use]
    pub fn center_octimeters(self) -> (i32, i32) {
        ((self.x << OCTIMETER_BITS) + OCTIMETERS_PER_CELL / 2, (self.z << OCTIMETER_BITS) + OCTIMETERS_PER_CELL / 2)
    }

    /// The cell an octimeter position sits in. Arithmetic right shift —
    /// negative positions floor.
    #[must_use]
    pub fn from_octimeters(x: i32, z: i32) -> Self {
        Self { x: x >> OCTIMETER_BITS, z: z >> OCTIMETER_BITS }
    }

    /// Index of this cell within its chunk's row-major planes.
    /// `rem_euclid` so negative cells map into `0..256` correctly.
    pub(super) fn chunk_index(self) -> usize {
        (self.z.rem_euclid(CELLS_PER_CHUNK) * CELLS_PER_CHUNK + self.x.rem_euclid(CELLS_PER_CHUNK)) as usize
    }
}

/// The ground-material vocabulary. Rides the wire as a raw `u8` inside a
/// `Bytes` plane, never as a schema enum, so a plane has one canonical
/// byte-array form. `Void` (`0`) is "nothing here".
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Material {
    #[default]
    Void = 0,
    Grass = 1,
    Dirt = 2,
    Stone = 3,
    Sand = 4,
    Water = 5,
}

impl Material {
    /// The raw wire byte.
    #[must_use]
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte, degrading an unknown value to [`Material::Void`]
    /// rather than erroring — a malformed plane byte becomes empty space,
    /// not a panic.
    #[must_use]
    pub fn from_u8_or_void(byte: u8) -> Self {
        Self::try_from(byte).unwrap_or(Self::Void)
    }
}

impl TryFrom<u8> for Material {
    type Error = u8;

    fn try_from(byte: u8) -> Result<Self, u8> {
        match byte {
            0 => Ok(Self::Void),
            1 => Ok(Self::Grass),
            2 => Ok(Self::Dirt),
            3 => Ok(Self::Stone),
            4 => Ok(Self::Sand),
            5 => Ok(Self::Water),
            other => Err(other),
        }
    }
}

/// A semantic group of cells with a default ground material. Regions are
/// referenced by 1-based id from the per-cell region plane (`0` = no
/// region); the region table is positional, so a region's id is its
/// index in [`World`]'s table plus one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub name: String,
    pub default_material: Material,
    /// The material a cliff face wears where this region's ground breaks
    /// past the step ceiling — the skirt color, and the future hook for
    /// generated rock banding. Defaults to [`Material::Stone`].
    pub cliff_material: Material,
}

/// The step ceiling in octimeters: two edge-adjacent cells whose heights
/// differ by strictly more than this meet at a cliff instead of a
/// continuous slope. The mesher derives cliff faces from it, and movement
/// will read the same constant as its traversability rule, so the drawn
/// break and the walkable break can never disagree.
pub const STEP_MAX_OCTIMETERS: i32 = 64;

/// Ceiling on a smoothing profile's iteration count. The contour pass
/// reads `2 × iterations` subcells outward, so this cap keeps every
/// field-driven read inside the mesher's fixed two-cell apron — the
/// invariant the `R = 1` neighbor remesh relies on.
pub const MAX_SMOOTHING_ITERATIONS: u32 = 4;

/// A contour-smoothing profile — the number and the degrees a cell's
/// smoothing plane points at. Referenced by 1-based id from the per-cell
/// smoothing plane (`0` = no override, the material default applies); the
/// table is positional like the region table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmoothingProfile {
    /// Corner-minimization iteration count (`0` = raw blocky contours).
    /// Clamped to [`MAX_SMOOTHING_ITERATIONS`] at registration.
    pub iterations: u32,
    /// Corner angle in degrees the cellular passes flatten down to (`90`
    /// rounds only true right angles; smaller rounds gentler junctions).
    /// Clamped to `[45, 90]` at registration — the windowed rule's
    /// threshold derivation assumes at least 45.
    pub degrees: u32,
}

/// An authored water surface level. Water cells reference a plane by
/// 1-based id from the per-cell water plane (`0` = the datum-0 level); the
/// table is positional like the region table, so a plane's id is its index
/// in [`World`]'s table plus one. Disconnected water bodies can share a
/// plane (one sea row every coastal cell points at); the level is authored,
/// not derived from the ground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaterPlane {
    /// The surface height in octimeters the referencing water cells lie at,
    /// regardless of their lakebed [`Chunk::height`].
    pub level_octimeters: i32,
}

/// One `16 × 16` block of the world, as a struct-of-arrays: property
/// planes, each row-major (`z * 16 + x`).
#[allow(clippy::large_stack_frames)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Ground fabric — cascade-resolved by [`World::underlay`].
    pub underlay: [Material; CELLS_PER_CHUNK_AREA],
    /// Per-subcell underlay material points — `SUB × SUB` points per cell
    /// (row-major cell order, `z*SUB + x` within a cell). Each byte is a
    /// [`Material`] or the [`UNDERLAY_POINT_INHERIT`] sentinel (inherit the
    /// cell's cascade). All-inherit — the empty default — meshes exactly as
    /// the per-cell [`Chunk::underlay`]; an explicit point shapes the ground
    /// below cell scale ([`World::underlay_point`]).
    pub underlay_points: [u8; UNDERLAY_POINTS_PER_CHUNK],
    /// Per-subcell height deltas in octimeters — `SUB × SUB` points per cell
    /// (same layout as [`Chunk::underlay_points`]). Each `i16` offsets its
    /// subcell off the cell's [`Chunk::height`] ([`World::point_height`]);
    /// [`HEIGHT_POINT_INHERIT`] (`0`) is no relief. An all-zero plane — the
    /// empty default — resolves exactly at cell stride, so a flat or legacy
    /// world's surface and mesh are byte-identical to the per-cell height;
    /// an authored delta shapes standable relief below cell scale (a fused
    /// column, a terrace, a ledge).
    pub height_points: [i16; HEIGHT_POINTS_PER_CHUNK],
    /// Placed surface — raw, never cascade-resolved. `Void` = none.
    pub overlay: [Material; CELLS_PER_CHUNK_AREA],
    /// Overlay subcell coverage bytes — `SUB × SUB` samples per cell
    /// (row-major cell order, `z*SUB + x` within a cell). `255` is full
    /// coverage; `0` is none. Meaningless where `overlay` is `Void`.
    pub overlay_mask: [u8; OVERLAY_MASK_WIRE_BYTES],
    /// Elevation in octimeters (`0` = flat).
    pub height: [i32; CELLS_PER_CHUNK_AREA],
    /// Region id per cell (`0` = no region).
    pub region: [u16; CELLS_PER_CHUNK_AREA],
    /// Water-plane id per cell (`0` = none — the datum-0 level). Meaningful
    /// only where the cascade-resolved underlay is [`Material::Water`];
    /// selects the row of [`World`]'s water-plane table whose level the
    /// cell's water surface lies at.
    pub water_plane: [u16; CELLS_PER_CHUNK_AREA],
    /// Smoothing-profile id per cell (`0` = no override — the material's
    /// own smoothing applies).
    pub smoothing: [u8; CELLS_PER_CHUNK_AREA],
}

impl Chunk {
    /// An empty chunk — all planes `Void` / zero.
    #[must_use]
    #[allow(clippy::large_stack_frames)]
    pub fn empty() -> Self {
        *Self::empty_boxed()
    }

    /// An empty chunk allocated at its final address. The dense subcell
    /// planes are large at `SUB = 16`, so decode and sparse insertion paths
    /// use this form instead of building the chunk by value on a guest stack.
    #[must_use]
    pub fn empty_boxed() -> Box<Self> {
        let mut chunk = Box::<Self>::new_uninit();
        let ptr = chunk.as_mut_ptr();
        // SAFETY: every field is initialized exactly once before
        // `assume_init`, and no read happens until the fully initialized box
        // is returned.
        unsafe {
            ptr::addr_of_mut!((*ptr).underlay).write([Material::Void; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).underlay_points)
                .cast::<u8>()
                .write_bytes(UNDERLAY_POINT_INHERIT, UNDERLAY_POINTS_PER_CHUNK);
            ptr::addr_of_mut!((*ptr).height_points).cast::<i16>().write_bytes(0, HEIGHT_POINTS_PER_CHUNK);
            ptr::addr_of_mut!((*ptr).overlay).write([Material::Void; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).overlay_mask).cast::<u8>().write_bytes(0, OVERLAY_MASK_WIRE_BYTES);
            ptr::addr_of_mut!((*ptr).height).write([0; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).region).write([0; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).water_plane).write([0; CELLS_PER_CHUNK_AREA]);
            ptr::addr_of_mut!((*ptr).smoothing).write([0; CELLS_PER_CHUNK_AREA]);
            chunk.assume_init()
        }
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::empty()
    }
}

/// The world: a sparse set of chunks plus a region table. Cells with no
/// chunk read as `Void` / `0`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct World {
    chunks: BTreeMap<ChunkPos, Box<Chunk>>,
    regions: Vec<Region>,
    smoothing_profiles: Vec<SmoothingProfile>,
    water_planes: Vec<WaterPlane>,
}

impl World {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The cascade-resolved ground material at `cell`: the cell's own
    /// underlay if non-`Void`, else the cell's region's `default_material`,
    /// else `Void`.
    #[must_use]
    pub fn underlay(&self, cell: CellPos) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Void;
        };
        let idx = cell.chunk_index();
        let own = chunk.underlay[idx];
        if own != Material::Void {
            return own;
        }
        let region_id = chunk.region[idx];
        if region_id != 0
            && let Some(region) = self.regions.get(region_id as usize - 1)
        {
            return region.default_material;
        }
        Material::Void
    }

    /// The material at subcell point `(sub_x, sub_z)` of `cell` (each in
    /// `0..SUB`): the point's explicit [`Material`] if it pins one, else —
    /// the [`UNDERLAY_POINT_INHERIT`] sentinel, or a missing chunk — the
    /// cell's cascade-resolved [`World::underlay`]. This is the sample the
    /// mesher expands the ground from, so an all-inherit cell reads its
    /// single cascade material at every point (identical to a per-cell
    /// underlay), while an authored point shapes the fabric below cell
    /// scale. `sub_x` / `sub_z` fold into the cell's point block, so a
    /// caller passing a subcell that has walked into a neighbor still reads
    /// this cell's plane.
    #[must_use]
    pub fn underlay_point(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Void;
        };
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let within = (sub_z.rem_euclid(sub) * sub + sub_x.rem_euclid(sub)) as usize;
        let byte = chunk.underlay_points[cell.chunk_index() * SUBCELLS_PER_CELL + within];
        if byte == UNDERLAY_POINT_INHERIT {
            return self.underlay(cell);
        }
        Material::from_u8_or_void(byte)
    }

    /// Write `cell`'s `SUB × SUB` underlay material points, creating the
    /// cell's chunk if absent. Each provided byte pins a point (a
    /// [`Material`] or the [`UNDERLAY_POINT_INHERIT`] sentinel); a short
    /// slice leaves the cell's remaining points inheriting, so an empty
    /// slice clears the cell back to all-inherit. Bytes past the cell's
    /// point count are ignored.
    pub fn set_cell_points(&mut self, cell: CellPos, points: &[u8]) {
        super::proposal::MutationTarget::set_cell_points(self, cell, points);
    }

    /// Write `cell`'s `SUB × SUB` height deltas (octimeters off the cell's
    /// [`Chunk::height`]), creating the cell's chunk if absent. Mirrors
    /// [`World::set_cell_points`]: a short slice leaves the cell's remaining
    /// points inheriting ([`HEIGHT_POINT_INHERIT`]), so an empty slice clears
    /// the cell back to no relief; deltas past the cell's point count are
    /// ignored.
    pub fn set_cell_heights(&mut self, cell: CellPos, deltas: &[i16]) {
        super::proposal::MutationTarget::set_cell_heights(self, cell, deltas);
    }

    /// The cell's cascade default alone — its region's `default_material`
    /// (`Void` for no region, an unregistered region, or a missing chunk),
    /// ignoring any explicit underlay. The mesher's base/patch split reads
    /// this: a cell whose resolved underlay differs from its own default is
    /// a contoured patch over the default ground.
    #[must_use]
    pub fn cell_default(&self, cell: CellPos) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Void;
        };
        let region_id = chunk.region[cell.chunk_index()];
        if region_id == 0 {
            return Material::Void;
        }
        self.regions.get(region_id as usize - 1).map_or(Material::Void, |region| region.default_material)
    }

    /// The raw overlay material at `cell` — never cascade-resolved.
    #[must_use]
    pub fn overlay(&self, cell: CellPos) -> Material {
        self.chunks.get(&cell.chunk()).map_or(Material::Void, |chunk| chunk.overlay[cell.chunk_index()])
    }

    /// The raw overlay coverage byte at subcell point `(sub_x, sub_z)` of
    /// `cell` — never cascade-resolved. A missing chunk reads `0` (no
    /// coverage), which is the apron read the mesher relies on: a
    /// chunk-border window can sample one subcell into an absent neighbor
    /// and see empty space rather than panicking. The value is meaningless
    /// where [`World::overlay`] is `Void`.
    #[must_use]
    pub fn overlay_coverage(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> u8 {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return 0;
        };
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let within = (sub_z.rem_euclid(sub) * sub + sub_x.rem_euclid(sub)) as usize;
        chunk.overlay_mask[cell.chunk_index() * SUBCELLS_PER_CELL + within]
    }

    fn overlay_material_coverage(&self, global_subcell_x: i32, global_subcell_z: i32, material: Material) -> u8 {
        let subcells = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let cell = CellPos { x: global_subcell_x.div_euclid(subcells), z: global_subcell_z.div_euclid(subcells) };
        if self.overlay(cell) != material {
            return 0;
        }
        self.overlay_coverage(cell, global_subcell_x.rem_euclid(subcells), global_subcell_z.rem_euclid(subcells))
    }

    fn reconstructed_overlay_coverage(&self, global_subcell_x: i32, global_subcell_z: i32, material: Material) -> u8 {
        super::mesher::contour::reconstructed_coverage(
            self.overlay_material_coverage(global_subcell_x, global_subcell_z, material),
            self.overlay_material_coverage(global_subcell_x.saturating_add(1), global_subcell_z, material),
            self.overlay_material_coverage(global_subcell_x, global_subcell_z.saturating_add(1), material),
            self.overlay_material_coverage(
                global_subcell_x.saturating_add(1),
                global_subcell_z.saturating_add(1),
                material,
            ),
            0.5,
            0.5,
        )
    }

    fn continuous_overlay_coverage_at(&self, x_meters: f32, z_meters: f32, material: Material) -> f32 {
        let subcells = SUBCELLS_PER_CELL_EDGE as f32;
        let sample_x = x_meters.mul_add(subcells, -0.5);
        let sample_z = z_meters.mul_add(subcells, -0.5);
        let base_x = floor_to_i32(sample_x);
        let base_z = floor_to_i32(sample_z);
        let fraction_x = sample_x - base_x as f32;
        let fraction_z = sample_z - base_z as f32;
        super::mesher::contour::interpolated_coverage(
            self.reconstructed_overlay_coverage(base_x, base_z, material),
            self.reconstructed_overlay_coverage(base_x.saturating_add(1), base_z, material),
            self.reconstructed_overlay_coverage(base_x, base_z.saturating_add(1), material),
            self.reconstructed_overlay_coverage(base_x.saturating_add(1), base_z.saturating_add(1), material),
            fraction_x,
            fraction_z,
        )
    }

    /// Elevation at `cell` in octimeters — the raw lakebed read. Unset
    /// cells read `0`. Under a water cell this is the ground beneath the
    /// surface, not the water level ([`World::water_level`] resolves that).
    #[must_use]
    pub fn height(&self, cell: CellPos) -> i32 {
        self.chunks.get(&cell.chunk()).map_or(0, |chunk| chunk.height[cell.chunk_index()])
    }

    /// The lakebed elevation in octimeters at subcell point `(sub_x, sub_z)`
    /// of `cell` (each folded into `0..SUB`): the cell's [`World::height`]
    /// plus the point's authored delta, saturating rather than wrapping at
    /// the `i32` extremes. An inherit ([`HEIGHT_POINT_INHERIT`]) point — or a
    /// missing chunk — reads the cell height unchanged, so an all-zero plane
    /// resolves at cell stride. Like [`World::height`] this is the raw ground
    /// read: under a water cell it is the lakebed beneath the surface, not the
    /// water level (`World::point_surface_level` resolves the effective
    /// surface).
    #[must_use]
    pub fn point_height(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> i32 {
        let base = self.height(cell);
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return base;
        };
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let within = (sub_z.rem_euclid(sub) * sub + sub_x.rem_euclid(sub)) as usize;
        let delta = chunk.height_points[cell.chunk_index() * SUBCELLS_PER_CELL + within];
        base.saturating_add(i32::from(delta))
    }

    /// The effective surface level in octimeters at subcell point
    /// `(sub_x, sub_z)` of `cell` — the point-lattice analogue of
    /// [`World::surface_level`]. A water cell's points resolve at the flat
    /// water level (a delta under water is lakebed relief the flat surface
    /// ignores); a land cell's points resolve at [`World::point_height`]. The
    /// corner-plate walk reads this so an authored break inside a cell splits
    /// its plates just as a cell-scale cliff splits the cell lattice.
    pub(crate) fn point_surface_level(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> i32 {
        self.water_level(cell).unwrap_or_else(|| self.point_height(cell, sub_x, sub_z))
    }

    /// The effective surface level of the subcell whose global subcell-lattice
    /// base corner is `(sx, sz)` (`sx = cell.x * SUB + sub_x`). The point
    /// corner plate reads its four incident subcells through this.
    fn subcell_surface_level(&self, sx: i32, sz: i32) -> i32 {
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let cell = CellPos { x: sx.div_euclid(sub), z: sz.div_euclid(sub) };
        self.point_surface_level(cell, sx.rem_euclid(sub), sz.rem_euclid(sub))
    }

    /// Does `cell` or any of its eight neighbors carry an authored height
    /// delta? A `false` here is the shortcut that collapses a flat or legacy
    /// neighborhood to the cell-stride corner-plate math — the corner plate
    /// at any of `cell`'s corners reads at most this 3×3 cell window, so with
    /// no relief anywhere in it the point lattice would resolve identically
    /// and the finer walk is skipped. A `true` engages the per-point patches.
    pub(crate) fn cell_has_height_relief(&self, cell: CellPos) -> bool {
        for dz in -1..=1 {
            for dx in -1..=1 {
                let n = CellPos { x: cell.x + dx, z: cell.z + dz };
                let Some(chunk) = self.chunks.get(&n.chunk()) else {
                    continue;
                };
                let base = n.chunk_index() * SUBCELLS_PER_CELL;
                if chunk.height_points[base..base + SUBCELLS_PER_CELL].iter().any(|&d| d != HEIGHT_POINT_INHERIT) {
                    return true;
                }
            }
        }
        false
    }

    /// The authored water surface level in octimeters at `cell`, or `None`
    /// if the cell is not water. `Some` exactly when the cascade-resolved
    /// underlay is [`Material::Water`]: the level is the cell's water
    /// plane's [`WaterPlane::level_octimeters`], with the datum `0` for
    /// plane id `0` or an unregistered id — the level is authored, never
    /// derived from the lakebed [`World::height`].
    #[must_use]
    pub fn water_level(&self, cell: CellPos) -> Option<i32> {
        if self.underlay(cell) != Material::Water {
            return None;
        }
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Some(0);
        };
        let plane_id = chunk.water_plane[cell.chunk_index()];
        if plane_id == 0 {
            return Some(0);
        }
        Some(self.water_planes.get(plane_id as usize - 1).map_or(0, |plane| plane.level_octimeters))
    }

    /// The effective surface level in octimeters at `cell`: the water
    /// level for a water cell, else the lakebed [`World::height`]. The
    /// surface-resolution machinery — [`World::edge_is_cliff`], the corner
    /// plate walk, the mesher's lift and skirt passes — reads this so a
    /// water cell resolves at its flat authored level instead of the ground
    /// beneath it.
    pub(crate) fn surface_level(&self, cell: CellPos) -> i32 {
        self.water_level(cell).unwrap_or_else(|| self.height(cell))
    }

    /// Do two edge-adjacent cells meet at a cliff — an effective-level step
    /// strictly past [`STEP_MAX_OCTIMETERS`]? The rule is pairwise over the
    /// two cells' effective surface levels (`surface_level`), so any
    /// caller holding two adjacent
    /// cells derives the same answer, and a bank standing past the step
    /// ceiling above a water surface cliffs against it.
    #[must_use]
    pub fn edge_is_cliff(&self, a: CellPos, b: CellPos) -> bool {
        (self.surface_level(a) - self.surface_level(b)).abs() > STEP_MAX_OCTIMETERS
    }

    /// The plate-resolved elevation of lattice corner `(kx, kz)` as seen
    /// from `cell` (one of the corner's four incident cells), in meters.
    /// The four incident cells partition into groups connected by
    /// non-cliff shared edges — a walk around the corner — and the plate
    /// containing `cell` averages its members' effective surface levels
    /// ([`World::surface_level`]). Connected cells share a plate (the
    /// surface blends); a cliff splits the plates (the surface breaks, and
    /// the gap is the skirt's job).
    ///
    /// A plate with any water member pins to the mean of its **water**
    /// members' levels alone, not the mixed mean — so an interior water
    /// corner is exactly flat at the authored level, and a connected shore
    /// corner (land within the step ceiling of the level) meets the water
    /// plane exactly, blending the land down to the waterline like a beach
    /// with no slit and no extra geometry. Past the step ceiling the plates
    /// split as usual and the skirt closes the face.
    fn corner_plate(&self, kx: i32, kz: i32, cell: CellPos) -> f32 {
        // Incident cells in cyclic order, so consecutive entries (mod 4)
        // share an edge and the diagonal pairs do not.
        let cells = [
            CellPos { x: kx - 1, z: kz - 1 },
            CellPos { x: kx, z: kz - 1 },
            CellPos { x: kx, z: kz },
            CellPos { x: kx - 1, z: kz },
        ];
        let levels = cells.map(|c| self.surface_level(c));
        let is_water = cells.map(|c| self.water_level(c).is_some());
        let start = cells.iter().position(|&c| c == cell);
        debug_assert!(start.is_some(), "cell must be incident to the corner");
        let start = start.unwrap_or(2);
        plate_mean_octimeters(levels, is_water, start) / OCTIMETERS_PER_CELL as f32
    }

    /// The plate-resolved elevation in meters of the point-lattice corner at
    /// global subcell-lattice coordinate `(px, pz)` (world meters
    /// `(px / SUB, pz / SUB)`), seen from the incident subcell whose position
    /// in the corner's cyclic incidence is `anchor`. The subcell analogue of
    /// [`World::corner_plate`]: the four incident subcells partition into
    /// plates by the same non-cliff walk over their [`point_surface_level`]s
    /// (with `STEP_MAX_OCTIMETERS` tested between adjacent points), and the
    /// plate containing `anchor` averages its members. Water members pin the
    /// plate to the water level exactly as at cell scale.
    fn point_corner_plate(&self, px: i32, pz: i32, anchor: usize) -> f32 {
        // Incident subcells in the same cyclic order as `corner_plate`, so
        // consecutive entries (mod 4) share a subcell edge.
        let subs = [(px - 1, pz - 1), (px, pz - 1), (px, pz), (px - 1, pz)];
        let levels = subs.map(|(sx, sz)| self.subcell_surface_level(sx, sz));
        let is_water = subs.map(|(sx, sz)| {
            let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
            self.water_level(CellPos { x: sx.div_euclid(sub), z: sz.div_euclid(sub) }).is_some()
        });
        plate_mean_octimeters(levels, is_water, anchor) / OCTIMETERS_PER_CELL as f32
    }

    /// The four point-plate corner heights (meters) of the subcell
    /// `(sub_x, sub_z)` of `cell`, ordered like [`World::cell_corner_heights`]
    /// — `[(low), (x+), (z+), (x+ z+)]`. The subcell spans `1 / SUB` m; the
    /// mesher's per-point cap patches and [`World::surface_height_in`]'s relief
    /// branch bilerp these. Each corner's anchor index selects the plate this
    /// subcell belongs to, so an authored break between adjacent points reads
    /// on the higher-coordinate side exactly as a cell cliff does.
    pub(crate) fn subcell_corner_heights(&self, cell: CellPos, sub_x: i32, sub_z: i32) -> [f32; 4] {
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let sx = cell.x * sub + sub_x;
        let sz = cell.z * sub + sub_z;
        [
            self.point_corner_plate(sx, sz, 2),
            self.point_corner_plate(sx + 1, sz, 3),
            self.point_corner_plate(sx, sz + 1, 1),
            self.point_corner_plate(sx + 1, sz + 1, 0),
        ]
    }

    /// The plate-resolved elevations of `cell`'s four corners, in meters,
    /// ordered `[(x, z), (x+1, z), (x, z+1), (x+1, z+1)]` — the bilinear
    /// patch [`World::surface_height_in`] interpolates and the mesher
    /// emits.
    #[must_use]
    pub fn cell_corner_heights(&self, cell: CellPos) -> [f32; 4] {
        if !self.cell_has_height_relief(cell) {
            return [
                self.corner_plate(cell.x, cell.z, cell),
                self.corner_plate(cell.x + 1, cell.z, cell),
                self.corner_plate(cell.x, cell.z + 1, cell),
                self.corner_plate(cell.x + 1, cell.z + 1, cell),
            ];
        }
        // Relief nearby: the cell's four outer corners resolve through the
        // point lattice, each anchored to the cell's own corner subcell so
        // the corner reads this cell's plate.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        [
            self.subcell_corner_heights(cell, 0, 0)[0],
            self.subcell_corner_heights(cell, sub - 1, 0)[1],
            self.subcell_corner_heights(cell, 0, sub - 1)[2],
            self.subcell_corner_heights(cell, sub - 1, sub - 1)[3],
        ]
    }

    /// The ground elevation in meters at `(wx, wz)` (meters, `1 cell =
    /// 1 m`) as `cell`'s bilinear surface patch reads it — coordinates
    /// clamp to the cell's span. This cell-pinned form is what the mesher
    /// emits vertices from: on a cliff edge the two sides read their own
    /// plates, so the drawn break is exactly the plate break. Two cells
    /// meeting without a cliff share their edge plates and therefore agree
    /// along the whole shared edge.
    #[must_use]
    pub fn surface_height_in(&self, cell: CellPos, wx: f32, wz: f32) -> f32 {
        if !self.cell_has_height_relief(cell) {
            let corners = self.cell_corner_heights(cell);
            let fx = (wx - cell.x as f32).clamp(0.0, 1.0);
            let fz = (wz - cell.z as f32).clamp(0.0, 1.0);
            let bottom = corners[0] + (corners[1] - corners[0]) * fx;
            let top = corners[2] + (corners[3] - corners[2]) * fx;
            return bottom + (top - bottom) * fz;
        }
        // Relief nearby: resolve through the subcell patch containing the
        // point. The coordinates clamp into the cell, then into its subcell,
        // so a caller off the cell span reads the nearest edge subcell.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let sub_f = sub as f32;
        let local_x = ((wx - cell.x as f32) * sub_f).clamp(0.0, sub_f);
        let local_z = ((wz - cell.z as f32) * sub_f).clamp(0.0, sub_f);
        let sub_x = floor_to_i32(local_x).clamp(0, sub - 1);
        let sub_z = floor_to_i32(local_z).clamp(0, sub - 1);
        let corners = self.subcell_corner_heights(cell, sub_x, sub_z);
        let x0 = cell.x as f32 + sub_x as f32 / sub_f;
        let z0 = cell.z as f32 + sub_z as f32 / sub_f;
        let fx = ((wx - x0) * sub_f).clamp(0.0, 1.0);
        let fz = ((wz - z0) * sub_f).clamp(0.0, 1.0);
        let bottom = corners[0] + (corners[1] - corners[0]) * fx;
        let top = corners[2] + (corners[3] - corners[2]) * fx;
        bottom + (top - bottom) * fz
    }

    /// The surface elevation in meters at `(wx, wz)`, resolved through the
    /// owning cell (floor). This is the stood-on height for movers,
    /// ray-picks, and the camera — the same bilinear patch the mesher
    /// draws, so what is drawn is what is stood on. Over a water cell the
    /// patch reads the water surface (the corner plates pin to the water
    /// level), so a mover on water stands at the surface — the swimming
    /// datum, ahead of any blocking rules. A point exactly on a cliff edge
    /// reads the higher-coordinate side (the floor convention).
    #[must_use]
    pub fn surface_height(&self, wx: f32, wz: f32) -> f32 {
        let cell = CellPos { x: floor_to_i32(wx), z: floor_to_i32(wz) };
        self.surface_height_in(cell, wx, wz)
    }

    /// Sample the markable top surface at meter-space XZ coordinates.
    ///
    /// Presence follows the same authored fields the mesher consumes: a
    /// non-Void resolved underlay point or a non-Void overlay sample at the
    /// shared half-coverage threshold. Missing terrain and explicit holes
    /// return None. Height resolves through the existing stood-on surface,
    /// including relief, plate breaks, and water levels.
    #[must_use]
    pub fn terrain_surface_at(&self, x_meters: f32, z_meters: f32) -> Option<TerrainSurface> {
        if !x_meters.is_finite() || !z_meters.is_finite() {
            return None;
        }
        let cell = CellPos { x: floor_to_i32(x_meters), z: floor_to_i32(z_meters) };
        let subcells = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let subcells_f32 = subcells as f32;
        let sub_x = floor_to_i32((x_meters - cell.x as f32) * subcells_f32).clamp(0, subcells - 1);
        let sub_z = floor_to_i32((z_meters - cell.z as f32) * subcells_f32).clamp(0, subcells - 1);
        let underlay_present = self.underlay_point(cell, sub_x, sub_z) != Material::Void;
        let overlay_present = [Material::Grass, Material::Dirt, Material::Stone, Material::Sand, Material::Water]
            .into_iter()
            .any(|material| {
                super::mesher::contour::scalar_coverage_is_inside(
                    self.continuous_overlay_coverage_at(x_meters, z_meters, material),
                )
            });
        if !underlay_present && !overlay_present {
            return None;
        }
        let x_octimeters = i32::try_from((x_meters * OCTIMETERS_PER_CELL as f32).round() as i64).ok()?;
        let z_octimeters = i32::try_from((z_meters * OCTIMETERS_PER_CELL as f32).round() as i64).ok()?;
        Some(TerrainSurface {
            cell,
            mark_point: WorldPoint::new(x_octimeters, z_octimeters),
            height_meters: self.surface_height_in(cell, x_meters, z_meters),
        })
    }

    /// The material `cell`'s cliff faces wear — its region's
    /// `cliff_material`, or [`Material::Stone`] for no region, an
    /// unregistered region, or a missing chunk.
    #[must_use]
    pub fn cliff_material(&self, cell: CellPos) -> Material {
        let Some(chunk) = self.chunks.get(&cell.chunk()) else {
            return Material::Stone;
        };
        let region_id = chunk.region[cell.chunk_index()];
        if region_id != 0
            && let Some(region) = self.regions.get(region_id as usize - 1)
        {
            return region.cliff_material;
        }
        Material::Stone
    }

    /// The chunk at `at`, if present.
    #[must_use]
    pub fn chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.chunks.get(&at).map(Box::as_ref)
    }

    /// Insert (or replace) the chunk at `at`.
    pub fn insert_chunk(&mut self, at: ChunkPos, chunk: impl Into<Box<Chunk>>) {
        self.chunks.insert(at, chunk.into());
    }

    /// The mutable chunk at `at`, creating an empty one when absent. Shape
    /// stamps use this narrow sibling-module seam to write the overlay
    /// material and scalar coverage planes without exposing the world's
    /// chunk map as public API.
    pub(super) fn chunk_mut_or_insert(&mut self, at: ChunkPos) -> &mut Chunk {
        self.chunks.entry(at).or_insert_with(Chunk::empty_boxed).as_mut()
    }

    /// Replace a chunk entry and return its prior box, preserving absence as
    /// `None`. Proposal preview uses this narrow seam to install and restore
    /// staged boxes without exposing the sparse map.
    pub(super) fn replace_chunk(&mut self, at: ChunkPos, replacement: Option<Box<Chunk>>) -> Option<Box<Chunk>> {
        match replacement {
            Some(chunk) => self.chunks.insert(at, chunk),
            None => self.chunks.remove(&at),
        }
    }

    /// Implement the private mutation target through the same sparse chunk
    /// seams used by the public immediate mutations.
    pub(super) fn mutation_chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.chunk(at)
    }

    /// Clone one present chunk directly as a box for proposal copy-on-write.
    pub(super) fn clone_chunk_box(&self, at: ChunkPos) -> Option<Box<Chunk>> {
        self.chunks.get(&at).cloned()
    }

    /// Register a region under a 1-based `id`. The table is positional,
    /// so this grows it (padding intervening slots with empty regions)
    /// and writes `region` at index `id - 1`. `id == 0` is ignored (`0`
    /// is the "no region" sentinel).
    pub fn insert_region(&mut self, id: u32, region: Region) {
        if id == 0 {
            return;
        }
        let index = id as usize - 1;
        if index >= self.regions.len() {
            self.regions.resize(
                index + 1,
                Region { name: String::new(), default_material: Material::Void, cliff_material: Material::Stone },
            );
        }
        self.regions[index] = region;
    }

    /// Register a smoothing profile under a 1-based `id`, clamping
    /// `iterations` to [`MAX_SMOOTHING_ITERATIONS`] and `degrees` to
    /// `[45, 90]`. The table is positional like the region table; `id == 0`
    /// is ignored (`0` is the "no override" sentinel).
    pub fn insert_smoothing_profile(&mut self, id: u32, profile: SmoothingProfile) {
        if id == 0 {
            return;
        }
        let clamped = SmoothingProfile {
            iterations: profile.iterations.min(MAX_SMOOTHING_ITERATIONS),
            degrees: profile.degrees.clamp(45, 90),
        };
        let index = id as usize - 1;
        if index >= self.smoothing_profiles.len() {
            self.smoothing_profiles.resize(index + 1, SmoothingProfile { iterations: 0, degrees: 90 });
        }
        self.smoothing_profiles[index] = clamped;
    }

    /// Register a water plane under a 1-based `id`. The table is positional
    /// like the region table, so this grows it (padding intervening slots
    /// with the datum-0 level) and writes `plane` at index `id - 1`.
    /// `id == 0` is ignored (`0` is the "no plane" sentinel — the datum-0
    /// level).
    pub fn insert_water_plane(&mut self, id: u32, plane: WaterPlane) {
        if id == 0 {
            return;
        }
        let index = id as usize - 1;
        if index >= self.water_planes.len() {
            self.water_planes.resize(index + 1, WaterPlane { level_octimeters: 0 });
        }
        self.water_planes[index] = plane;
    }

    /// The smoothing override at `cell`, if the cell's smoothing plane
    /// points at a registered profile. `None` — plane `0`, missing chunk,
    /// or an unregistered id — means the material default applies.
    #[must_use]
    pub fn smoothing_override(&self, cell: CellPos) -> Option<SmoothingProfile> {
        let chunk = self.chunks.get(&cell.chunk())?;
        let id = chunk.smoothing[cell.chunk_index()];
        if id == 0 {
            return None;
        }
        self.smoothing_profiles.get(id as usize - 1).copied()
    }

    /// Iterate the chunk set in `ChunkPos` order (deterministic — the
    /// `BTreeMap` key order).
    pub fn chunks(&self) -> impl Iterator<Item = (ChunkPos, &Chunk)> {
        self.chunks.iter().map(|(pos, chunk)| (*pos, chunk.as_ref()))
    }
}

impl super::proposal::MutationTarget for World {
    fn chunk(&self, at: ChunkPos) -> Option<&Chunk> {
        self.mutation_chunk(at)
    }

    fn chunk_mut_or_insert(&mut self, at: ChunkPos) -> &mut Chunk {
        Self::chunk_mut_or_insert(self, at)
    }

    fn replace_chunk(&mut self, at: ChunkPos, chunk: Box<Chunk>) {
        Self::replace_chunk(self, at, Some(chunk));
    }
}

/// Decode a cliff-material byte: an unknown value or `Void` reads as
/// [`Material::Stone`] — a cliff face always wears a paintable material.
/// Shared with [`SetRegion::into_region`](super::kinds::SetRegion) in the
/// sibling `kinds` module.
pub(super) fn cliff_material_from_u8(byte: u8) -> Material {
    match Material::try_from(byte) {
        Ok(Material::Void) | Err(_) => Material::Stone,
        Ok(material) => material,
    }
}

/// Floor to `i32` — `as i32` truncates toward zero, which is wrong for
/// negative world coordinates, so step down when it rounded up.
fn floor_to_i32(v: f32) -> i32 {
    #[allow(clippy::cast_possible_truncation)] // world coordinates are far inside i32
    let t = v as i32;
    if (t as f32) > v {
        t - 1
    } else {
        t
    }
}

/// Partition four cyclically-ordered incident members into non-cliff plates
/// and return the mean effective level (octimeters) of the plate containing
/// `start`. Consecutive entries (mod 4) share an edge; a pair within
/// [`STEP_MAX_OCTIMETERS`] joins the same plate, one past it splits. A plate
/// with any water member averages only its water members (the flat-plane
/// pin), else the whole connected plate. Shared by the cell corner plate
/// ([`World::corner_plate`]) and the subcell point corner plate
/// ([`World::point_corner_plate`]) so the two lattices resolve identically.
fn plate_mean_octimeters(levels: [i32; 4], is_water: [bool; 4], start: usize) -> f32 {
    let mut member = [false; 4];
    member[start] = true;
    // Closure over the four cyclic edges to a fixpoint (at most three rounds
    // absorb everything reachable).
    let mut changed = true;
    while changed {
        changed = false;
        for k in 0..4 {
            let a = k;
            let b = (k + 1) % 4;
            let connected = (levels[a] - levels[b]).abs() <= STEP_MAX_OCTIMETERS;
            if connected && member[a] != member[b] {
                member[a] = true;
                member[b] = true;
                changed = true;
            }
        }
    }
    let any_water = (0..4).any(|k| member[k] && is_water[k]);
    let mut sum = 0i32;
    let mut count = 0i32;
    for k in 0..4 {
        if member[k] && (!any_water || is_water[k]) {
            sum += levels[k];
            count += 1;
        }
    }
    sum as f32 / count as f32
}

/// A [`World::from_bytes`] failure — the buffer was truncated or carried
/// an unknown format version.
#[derive(Debug, PartialEq, Eq)]
pub enum WorldDecodeError {
    /// Ran off the end of the buffer mid-record.
    Truncated,
    /// First byte was not a recognized format version.
    BadVersion(u8),
    /// A region name was not valid UTF-8.
    BadName,
    /// A table count exceeded the format's addressable or operational cap.
    LimitExceeded,
}

/// The current write version. Version 7 expands the per-cell packed
/// overlay mask words into one scalar coverage byte per subcell; older
/// packed bits decode as `0` / `255`. Version 6 appends the per-chunk
/// height-delta plane ([`HEIGHT_POINTS_PER_CHUNK`] `i16` octimeter deltas)
/// to the end of each chunk record, after the underlay-point plane.
/// Version 5 appends the per-chunk underlay-point plane
/// ([`UNDERLAY_POINTS_PER_CHUNK`] bytes) to the end of each chunk record.
/// Version 4 adds the water-plane table (after the smoothing-profile table)
/// and the per-chunk water-plane plane (after the height plane); version 3
/// adds a cliff-material byte to each region record; version 2 adds the
/// smoothing-profile table (after the region table) and the per-chunk
/// smoothing plane (after the region plane). Older buffers still decode: a
/// pre-7 buffer expands packed overlay bits to binary coverage, a pre-6
/// buffer reads an all-zero height-delta plane, a pre-5 buffer reads an
/// all-inherit underlay-point plane, a pre-4 buffer reads an empty
/// water-plane table and an all-zero water plane, a pre-3 region reads
/// Stone cliffs, a version-1 buffer reads an empty profile table and an
/// all-zero smoothing plane.
const WORLD_FORMAT_VERSION: u8 = 7;

/// The oldest version [`World::from_bytes`] still decodes.
const WORLD_FORMAT_VERSION_MIN: u8 = 1;

const MAX_DECODED_REGIONS: usize = u16::MAX as usize;
const MAX_DECODED_SMOOTHING_PROFILES: usize = u8::MAX as usize;
const MAX_DECODED_WATER_PLANES: usize = u16::MAX as usize;
const MAX_DECODED_CHUNKS: usize = 65_536;

fn chunk_record_bytes(version: u8) -> usize {
    let overlay_mask_bytes = if version >= 7 {
        OVERLAY_MASK_WIRE_BYTES
    } else {
        2 * CELLS_PER_CHUNK_AREA
    };
    8 + 2 * CELLS_PER_CHUNK_AREA
        + overlay_mask_bytes
        + 4 * CELLS_PER_CHUNK_AREA
        + if version >= 4 {
            2 * CELLS_PER_CHUNK_AREA
        } else {
            0
        }
        + 2 * CELLS_PER_CHUNK_AREA
        + if version >= 2 {
            CELLS_PER_CHUNK_AREA
        } else {
            0
        }
        + if version >= 5 {
            UNDERLAY_POINTS_PER_CHUNK
        } else {
            0
        }
        + if version >= 6 {
            2 * HEIGHT_POINTS_PER_CHUNK
        } else {
            0
        }
}

impl World {
    /// Serialize to the compact `aether.kit.world.load` binary format: a
    /// version byte, the region table, the smoothing-profile table, the
    /// water-plane table, then per-chunk plane records — all little-endian.
    /// Region, profile, and water-plane ids are positional (index + 1), so
    /// the table order is the id order.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(WORLD_FORMAT_VERSION);
        out.extend_from_slice(&(self.regions.len() as u32).to_le_bytes());
        for region in &self.regions {
            let name = region.name.as_bytes();
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(name);
            out.push(region.default_material.to_u8());
            out.push(region.cliff_material.to_u8());
        }
        out.extend_from_slice(&(self.smoothing_profiles.len() as u32).to_le_bytes());
        for profile in &self.smoothing_profiles {
            out.push(profile.iterations as u8);
            out.extend_from_slice(&(profile.degrees as u16).to_le_bytes());
        }
        out.extend_from_slice(&(self.water_planes.len() as u32).to_le_bytes());
        for plane in &self.water_planes {
            out.extend_from_slice(&plane.level_octimeters.to_le_bytes());
        }
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        for (pos, chunk) in &self.chunks {
            out.extend_from_slice(&pos.x.to_le_bytes());
            out.extend_from_slice(&pos.z.to_le_bytes());
            for m in &chunk.underlay {
                out.push(m.to_u8());
            }
            for m in &chunk.overlay {
                out.push(m.to_u8());
            }
            out.extend_from_slice(&chunk.overlay_mask);
            for h in &chunk.height {
                out.extend_from_slice(&h.to_le_bytes());
            }
            for w in &chunk.water_plane {
                out.extend_from_slice(&w.to_le_bytes());
            }
            for r in &chunk.region {
                out.extend_from_slice(&r.to_le_bytes());
            }
            out.extend_from_slice(&chunk.smoothing);
            out.extend_from_slice(&chunk.underlay_points);
            for delta in &chunk.height_points {
                out.extend_from_slice(&delta.to_le_bytes());
            }
        }
        out
    }

    /// Decode the [`World::to_bytes`] format, current or older (a pre-6
    /// buffer carries no height-delta plane — it reads all-zero relief; a
    /// pre-5 buffer carries no underlay-point plane — it reads all-inherit; a
    /// pre-4 buffer carries no water-plane table or plane — both read empty
    /// / zero; a pre-3 region reads Stone cliffs; a version-1 buffer carries
    /// no smoothing table or plane). A truncated buffer or unknown version
    /// returns `Err` rather than panicking; the caller keeps its prior
    /// world on any error.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WorldDecodeError> {
        let mut reader = Reader::new(bytes);
        let version = reader.u8()?;
        if !(WORLD_FORMAT_VERSION_MIN..=WORLD_FORMAT_VERSION).contains(&version) {
            return Err(WorldDecodeError::BadVersion(version));
        }
        let region_count_raw = reader.u32()?;
        let region_count =
            reader.checked_count(region_count_raw, 2 + 1 + usize::from(version >= 3), MAX_DECODED_REGIONS)?;
        let mut regions = Vec::with_capacity(region_count);
        for _ in 0..region_count {
            let name_len = reader.u16()? as usize;
            let name_bytes = reader.take(name_len)?;
            let name = String::from_utf8(name_bytes.to_vec()).map_err(|_| WorldDecodeError::BadName)?;
            let default_material = Material::from_u8_or_void(reader.u8()?);
            let cliff_material = if version >= 3 {
                cliff_material_from_u8(reader.u8()?)
            } else {
                Material::Stone
            };
            regions.push(Region { name, default_material, cliff_material });
        }
        let mut smoothing_profiles = Vec::new();
        if version >= 2 {
            let profile_count_raw = reader.u32()?;
            let profile_count = reader.checked_count(profile_count_raw, 3, MAX_DECODED_SMOOTHING_PROFILES)?;
            smoothing_profiles.reserve(profile_count);
            for _ in 0..profile_count {
                let iterations = u32::from(reader.u8()?);
                let degrees = u32::from(reader.u16()?);
                smoothing_profiles.push(SmoothingProfile { iterations, degrees });
            }
        }
        let mut water_planes = Vec::new();
        if version >= 4 {
            let plane_count_raw = reader.u32()?;
            let plane_count = reader.checked_count(plane_count_raw, 4, MAX_DECODED_WATER_PLANES)?;
            water_planes.reserve(plane_count);
            for _ in 0..plane_count {
                water_planes.push(WaterPlane { level_octimeters: reader.i32()? });
            }
        }
        let chunk_count_raw = reader.u32()?;
        let chunk_count = reader.checked_count(chunk_count_raw, chunk_record_bytes(version), MAX_DECODED_CHUNKS)?;
        let mut chunks = BTreeMap::new();
        for _ in 0..chunk_count {
            let x = reader.i32()?;
            let z = reader.i32()?;
            let mut chunk = Chunk::empty_boxed();
            for slot in &mut chunk.underlay {
                *slot = Material::from_u8_or_void(reader.u8()?);
            }
            for slot in &mut chunk.overlay {
                *slot = Material::from_u8_or_void(reader.u8()?);
            }
            read_overlay_mask(&mut reader, version, &mut chunk)?;
            for slot in &mut chunk.height {
                *slot = reader.i32()?;
            }
            if version >= 4 {
                for slot in &mut chunk.water_plane {
                    *slot = reader.u16()?;
                }
            }
            for slot in &mut chunk.region {
                *slot = reader.u16()?;
            }
            if version >= 2 {
                for slot in &mut chunk.smoothing {
                    *slot = reader.u8()?;
                }
            }
            if version >= 5 {
                for slot in &mut chunk.underlay_points {
                    *slot = reader.u8()?;
                }
            }
            if version >= 6 {
                for slot in &mut chunk.height_points {
                    *slot = reader.i16()?;
                }
            }
            chunks.insert(ChunkPos { x, z }, chunk);
        }
        Ok(Self { chunks, regions, smoothing_profiles, water_planes })
    }
}

fn read_overlay_mask(reader: &mut Reader<'_>, version: u8, chunk: &mut Chunk) -> Result<(), WorldDecodeError> {
    if version >= 7 {
        for slot in &mut chunk.overlay_mask {
            *slot = reader.u8()?;
        }
        return Ok(());
    }
    for cell in 0..CELLS_PER_CHUNK_AREA {
        let mask = reader.u16()?;
        let base = cell * SUBCELLS_PER_CELL;
        let scale = SUBCELLS_PER_CELL_EDGE as usize / LEGACY_MASK_SUBCELLS_PER_CELL_EDGE;
        for legacy_z in 0..LEGACY_MASK_SUBCELLS_PER_CELL_EDGE {
            for legacy_x in 0..LEGACY_MASK_SUBCELLS_PER_CELL_EDGE {
                let bit = legacy_z * LEGACY_MASK_SUBCELLS_PER_CELL_EDGE + legacy_x;
                let coverage = if (mask >> bit) & 1 == 1 {
                    255
                } else {
                    0
                };
                for sz in legacy_z * scale..(legacy_z + 1) * scale {
                    for sx in legacy_x * scale..(legacy_x + 1) * scale {
                        chunk.overlay_mask[base + sz * SUBCELLS_PER_CELL_EDGE as usize + sx] = coverage;
                    }
                }
            }
        }
    }
    Ok(())
}

/// A bounds-checked little-endian byte cursor for [`World::from_bytes`].
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WorldDecodeError> {
        let end = self.pos.checked_add(n).ok_or(WorldDecodeError::Truncated)?;
        let slice = self.bytes.get(self.pos..end).ok_or(WorldDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn checked_count(
        &self,
        count: u32,
        minimum_record_bytes: usize,
        maximum_count: usize,
    ) -> Result<usize, WorldDecodeError> {
        let count = usize::try_from(count).map_err(|_| WorldDecodeError::LimitExceeded)?;
        if count > maximum_count {
            return Err(WorldDecodeError::LimitExceeded);
        }
        let required = count.checked_mul(minimum_record_bytes).ok_or(WorldDecodeError::LimitExceeded)?;
        if required > self.remaining() {
            return Err(WorldDecodeError::Truncated);
        }
        Ok(count)
    }

    fn u8(&mut self) -> Result<u8, WorldDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WorldDecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Result<i16, WorldDecodeError> {
        let b = self.take(2)?;
        Ok(i16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, WorldDecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i32(&mut self) -> Result<i32, WorldDecodeError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{SetChunk, mesher::contour::COVERAGE_CROSSING};

    fn cell(x: i32, z: i32) -> CellPos {
        CellPos { x, z }
    }

    #[test]
    fn chunk_shift_floors_on_negative_cells() {
        // Cell 0 and 15 are both in chunk 0; cell 16 in chunk 1.
        assert_eq!(cell(0, 0).chunk(), ChunkPos { x: 0, z: 0 });
        assert_eq!(cell(15, 15).chunk(), ChunkPos { x: 0, z: 0 });
        assert_eq!(cell(16, 16).chunk(), ChunkPos { x: 1, z: 1 });
        // Arithmetic shift floors: cell -1 is in chunk -1, not 0.
        assert_eq!(cell(-1, -1).chunk(), ChunkPos { x: -1, z: -1 });
        assert_eq!(cell(-16, -16).chunk(), ChunkPos { x: -1, z: -1 });
        assert_eq!(cell(-17, -17).chunk(), ChunkPos { x: -2, z: -2 });
    }

    #[test]
    fn from_octimeters_floors_on_negative_positions() {
        // 256 octimeters per cell; cell 0 spans [0,256), cell -1 spans [-256,0).
        assert_eq!(CellPos::from_octimeters(0, 0), cell(0, 0));
        assert_eq!(CellPos::from_octimeters(255, 255), cell(0, 0));
        assert_eq!(CellPos::from_octimeters(256, 256), cell(1, 1));
        assert_eq!(CellPos::from_octimeters(-1, -1), cell(-1, -1));
        assert_eq!(CellPos::from_octimeters(-256, -256), cell(-1, -1));
        assert_eq!(CellPos::from_octimeters(-257, -257), cell(-2, -2));
    }

    #[test]
    fn center_octimeters_is_cell_center() {
        assert_eq!(cell(0, 0).center_octimeters(), (128, 128));
        assert_eq!(cell(1, 2).center_octimeters(), (384, 640));
        assert_eq!(cell(-1, -1).center_octimeters(), (-128, -128));
    }

    #[test]
    fn negative_cell_indexes_its_chunk_correctly() {
        // Cell -1 sits at local (15,15) of chunk -1 → index 255.
        assert_eq!(cell(-1, -1).chunk_index(), 15 * 16 + 15);
        // Cell -16 sits at local (0,0) of chunk -1 → index 0.
        assert_eq!(cell(-16, -16).chunk_index(), 0);
    }

    #[test]
    fn underlay_cascade_resolves_cell_then_region_then_void() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell (2,3): explicit Stone underlay — cell override wins.
        chunk.underlay[3 * 16 + 2] = Material::Stone;
        // Cell (4,5): Void underlay but in region 1 → region default.
        chunk.region[5 * 16 + 4] = 1;
        // Cell (6,7): Void underlay, no region → Void.
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );

        assert_eq!(world.underlay(cell(2, 3)), Material::Stone, "cell override");
        assert_eq!(world.underlay(cell(4, 5)), Material::Grass, "region default");
        assert_eq!(world.underlay(cell(6, 7)), Material::Void, "no cascade source");
    }

    #[test]
    fn underlay_point_inherits_cascade_or_pins_explicit() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell (2,3): explicit Stone underlay; every point inherits it.
        chunk.underlay[3 * 16 + 2] = Material::Stone;
        // Cell (4,5): Void underlay in region 1 (Grass default); point (0,0)
        // pinned Sand, point (1,0) pinned explicit Void, the rest inherit.
        chunk.region[5 * 16 + 4] = 1;
        let base = (5 * 16 + 4) * SUBCELLS_PER_CELL;
        chunk.underlay_points[base] = Material::Sand.to_u8();
        chunk.underlay_points[base + 1] = Material::Void.to_u8();
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );

        // Cell (2,3): inherit points resolve the cell's own paint.
        assert_eq!(world.underlay_point(cell(2, 3), 0, 0), Material::Stone, "inherit resolves the cell paint");
        assert_eq!(world.underlay_point(cell(2, 3), 3, 3), Material::Stone);
        // Cell (4,5): inherit points resolve the region default; explicit
        // points pin, and an explicit Void reads Void even in a painted cell.
        assert_eq!(world.underlay_point(cell(4, 5), 2, 2), Material::Grass, "inherit resolves the region default");
        assert_eq!(world.underlay_point(cell(4, 5), 0, 0), Material::Sand, "an explicit point overrides the cascade");
        assert_eq!(
            world.underlay_point(cell(4, 5), 1, 0),
            Material::Void,
            "an explicit Void point reads Void in a painted cell",
        );
        // Cell (6,7): no cascade source, so an inherit point reads Void.
        assert_eq!(world.underlay_point(cell(6, 7), 0, 0), Material::Void, "no cascade source");
    }

    #[test]
    fn set_cell_points_writes_a_cell_and_a_short_slice_inherits_the_tail() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay[3 * 16 + 3] = Material::Grass; // cell (3,3)
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        // Stamp two Stone points; the short slice leaves the tail inheriting.
        world.set_cell_points(cell(3, 3), &[Material::Stone.to_u8(), Material::Stone.to_u8()]);
        assert_eq!(world.underlay_point(cell(3, 3), 0, 0), Material::Stone);
        assert_eq!(world.underlay_point(cell(3, 3), 1, 0), Material::Stone);
        assert_eq!(
            world.underlay_point(cell(3, 3), 2, 0),
            Material::Grass,
            "the unwritten tail inherits the cell's Grass",
        );
        // An empty slice clears the cell back to all-inherit.
        world.set_cell_points(cell(3, 3), &[]);
        assert_eq!(
            world.underlay_point(cell(3, 3), 0, 0),
            Material::Grass,
            "an empty stamp clears the cell to inherit",
        );
        // A stamp on an absent chunk creates it and pins the point.
        world.set_cell_points(cell(100, 100), &[Material::Sand.to_u8()]);
        assert_eq!(world.underlay_point(cell(100, 100), 0, 0), Material::Sand);
    }

    #[test]
    fn overlay_never_cascades() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        // Cell in region 1 with a region default, but Void overlay.
        chunk.region[0] = 1;
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_region(
            1,
            Region { name: "r".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );
        // Underlay cascades to Grass; overlay stays raw Void.
        assert_eq!(world.underlay(cell(0, 0)), Material::Grass);
        assert_eq!(world.overlay(cell(0, 0)), Material::Void);
    }

    #[test]
    fn sparse_world_reads_void_and_zero() {
        let world = World::new();
        assert_eq!(world.underlay(cell(100, -50)), Material::Void);
        assert_eq!(world.overlay(cell(100, -50)), Material::Void);
        assert_eq!(world.overlay_coverage(cell(100, -50), 0, 0), 0);
        assert_eq!(world.height(cell(100, -50)), 0);
        assert!(world.chunk(ChunkPos { x: 3, z: 3 }).is_none());
    }

    #[test]
    fn set_chunk_decodes_planes_and_clamps_unknown_material() {
        let mut underlay = vec![0u8; CELLS_PER_CHUNK_AREA];
        underlay[3 * 16 + 2] = Material::Water.to_u8();
        underlay[0] = 99; // unknown byte → Void
        let mut region = vec![0u32; CELLS_PER_CHUNK_AREA];
        region[1] = 7;
        let set = SetChunk {
            chunk_x: 2,
            chunk_z: -1,
            underlay,
            underlay_points: Vec::new(),
            height_points: Vec::new(),
            overlay: vec![0u8; CELLS_PER_CHUNK_AREA],
            overlay_mask: vec![0u8; OVERLAY_MASK_WIRE_BYTES],
            height: vec![0i32; CELLS_PER_CHUNK_AREA],
            region,
            water_plane: vec![0u32; CELLS_PER_CHUNK_AREA],
            smoothing: vec![0u8; CELLS_PER_CHUNK_AREA],
        };
        assert_eq!(set.chunk_pos(), ChunkPos { x: 2, z: -1 });
        let chunk = set.into_chunk();
        assert_eq!(chunk.underlay[3 * 16 + 2], Material::Water);
        assert_eq!(chunk.underlay[0], Material::Void, "unknown byte clamps to Void");
        assert_eq!(chunk.region[1], 7);
    }

    #[test]
    fn set_chunk_copies_overlay_coverage_bytes() {
        // Dense coverage plane; cell 1's first two subcells get direct
        // scalar coverage bytes.
        let mut overlay_mask = vec![0u8; OVERLAY_MASK_WIRE_BYTES];
        overlay_mask[SUBCELLS_PER_CELL] = 17;
        overlay_mask[SUBCELLS_PER_CELL + 1] = 239;
        let set = SetChunk {
            chunk_x: 0,
            chunk_z: 0,
            underlay: Vec::new(),
            underlay_points: Vec::new(),
            height_points: Vec::new(),
            overlay: Vec::new(),
            overlay_mask,
            height: Vec::new(),
            region: Vec::new(),
            water_plane: Vec::new(),
            smoothing: Vec::new(),
        };
        let chunk = set.into_chunk();
        assert_eq!(chunk.overlay_mask[0], 0);
        assert_eq!(chunk.overlay_mask[SUBCELLS_PER_CELL], 17);
        assert_eq!(chunk.overlay_mask[SUBCELLS_PER_CELL + 1], 239);
        assert_eq!(OVERLAY_MASK_WIRE_BYTES, 65_536, "SUB=16 -> 256*256 bytes");
    }

    #[test]
    fn set_chunk_pads_short_planes_and_truncates_long() {
        let set = SetChunk {
            chunk_x: 0,
            chunk_z: 0,
            underlay: vec![Material::Grass.to_u8(); 2], // short → rest Void
            underlay_points: vec![Material::Stone.to_u8(); 2], // short → rest inherit
            height_points: vec![10i16; 2],              // short → rest zero relief
            overlay: vec![0u8; CELLS_PER_CHUNK_AREA + 10], // long → truncated
            overlay_mask: Vec::new(),
            height: vec![5i32; 1],
            region: Vec::new(),
            water_plane: Vec::new(),
            smoothing: vec![3u8; 1], // short → rest no-override
        };
        let chunk = set.into_chunk();
        assert_eq!(chunk.underlay[0], Material::Grass);
        assert_eq!(chunk.underlay[1], Material::Grass);
        assert_eq!(chunk.underlay[2], Material::Void);
        assert_eq!(chunk.height[0], 5);
        assert_eq!(chunk.height[1], 0);
        assert_eq!(chunk.smoothing[0], 3);
        assert_eq!(chunk.smoothing[1], 0);
        // The two written points hold; the tail keeps the inherit sentinel.
        assert_eq!(chunk.underlay_points[0], Material::Stone.to_u8());
        assert_eq!(chunk.underlay_points[1], Material::Stone.to_u8());
        assert_eq!(chunk.underlay_points[2], UNDERLAY_POINT_INHERIT);
        // The two written height deltas hold; the tail keeps zero relief.
        assert_eq!(chunk.height_points[0], 10);
        assert_eq!(chunk.height_points[1], 10);
        assert_eq!(chunk.height_points[2], HEIGHT_POINT_INHERIT);
    }

    #[test]
    fn world_bytes_roundtrip() {
        let mut world = World::new();
        world.insert_region(
            1,
            Region { name: "meadow".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );
        world.insert_region(
            2,
            Region { name: "shore".into(), default_material: Material::Sand, cliff_material: Material::Dirt },
        );
        world.insert_smoothing_profile(1, SmoothingProfile { iterations: 3, degrees: 60 });
        world.insert_water_plane(1, WaterPlane { level_octimeters: -17 });
        world.insert_water_plane(2, WaterPlane { level_octimeters: 320 });
        let mut a = Chunk::empty();
        a.underlay[0] = Material::Stone;
        a.overlay[5] = Material::Water;
        let overlay_base = 5 * SUBCELLS_PER_CELL;
        a.overlay_mask[overlay_base] = 9;
        a.overlay_mask[overlay_base + 1] = 255;
        a.height[10] = -42;
        a.region[20] = 2;
        a.water_plane[40] = 2;
        a.smoothing[30] = 1;
        // An authored underlay-point pattern (a pinned point and an explicit
        // Void hole) rides the v5 chunk record and must survive the trip.
        a.underlay_points[100] = Material::Sand.to_u8();
        a.underlay_points[101] = Material::Void.to_u8();
        world.insert_chunk(ChunkPos { x: 1, z: -3 }, a);
        let mut b = Chunk::empty();
        b.underlay[255] = Material::Dirt;
        world.insert_chunk(ChunkPos { x: -7, z: 4 }, b);

        let bytes = world.to_bytes();
        let decoded = World::from_bytes(&bytes).expect("roundtrip decodes");

        // Structural equality across the whole world.
        assert_eq!(decoded.regions, world.regions);
        assert_eq!(decoded.smoothing_profiles, world.smoothing_profiles);
        assert_eq!(decoded.water_planes, world.water_planes);
        assert_eq!(decoded.chunk(ChunkPos { x: 1, z: -3 }), world.chunk(ChunkPos { x: 1, z: -3 }));
        assert_eq!(decoded.chunk(ChunkPos { x: -7, z: 4 }), world.chunk(ChunkPos { x: -7, z: 4 }));
    }

    #[test]
    fn version_one_buffer_decodes_with_no_smoothing() {
        // Tripwire: the version-1 layout — no profile table, no per-chunk
        // smoothing plane — is pinned here byte-for-byte and must keep
        // decoding as long as WORLD_FORMAT_VERSION_MIN is 1. Build one v1
        // buffer by hand: one region, one chunk with a Stone cell.
        let mut buf = vec![1u8];
        buf.extend_from_slice(&1u32.to_le_bytes()); // one region
        buf.extend_from_slice(&6u16.to_le_bytes());
        buf.extend_from_slice(b"meadow");
        buf.push(Material::Grass.to_u8());
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&2i32.to_le_bytes());
        buf.extend_from_slice(&(-1i32).to_le_bytes());
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[7] = Material::Stone.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions

        let world = World::from_bytes(&buf).expect("a v1 buffer still decodes");
        assert_eq!(world.regions.len(), 1);
        assert!(world.smoothing_profiles.is_empty());
        let chunk = world.chunk(ChunkPos { x: 2, z: -1 }).expect("chunk");
        assert_eq!(chunk.underlay[7], Material::Stone);
        assert_eq!(chunk.smoothing, [0u8; CELLS_PER_CHUNK_AREA]);
    }

    #[test]
    fn smoothing_override_resolves_plane_then_table_then_none() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.smoothing[0] = 1; // registered below
        chunk.smoothing[1] = 9; // never registered
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world.insert_smoothing_profile(
            1,
            SmoothingProfile {
                iterations: 7, // past the cap — clamps to 4
                degrees: 30,   // under the floor — clamps to 45
            },
        );

        assert_eq!(
            world.smoothing_override(cell(0, 0)),
            Some(SmoothingProfile { iterations: 4, degrees: 45 }),
            "registration clamps to the apron-safe range",
        );
        assert_eq!(world.smoothing_override(cell(1, 0)), None, "an unregistered id is no override");
        assert_eq!(world.smoothing_override(cell(2, 0)), None, "plane 0 is no override");
        assert_eq!(world.smoothing_override(cell(100, 100)), None, "a missing chunk is no override");
    }

    #[test]
    fn from_bytes_rejects_truncated_and_bad_version() {
        assert_eq!(World::from_bytes(&[]), Err(WorldDecodeError::Truncated));
        assert_eq!(World::from_bytes(&[9]), Err(WorldDecodeError::BadVersion(9)));
        // Version + a region count claiming one region, but no region bytes.
        let mut buf = vec![WORLD_FORMAT_VERSION];
        buf.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(World::from_bytes(&buf), Err(WorldDecodeError::Truncated));

        let mut oversized_regions = vec![WORLD_FORMAT_VERSION];
        oversized_regions
            .extend_from_slice(&u32::try_from(MAX_DECODED_REGIONS + 1).expect("region cap fits u32").to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_regions), Err(WorldDecodeError::LimitExceeded),);

        let mut oversized_profiles = vec![WORLD_FORMAT_VERSION];
        oversized_profiles.extend_from_slice(&0u32.to_le_bytes());
        oversized_profiles.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_profiles), Err(WorldDecodeError::LimitExceeded),);

        let mut oversized_water = vec![WORLD_FORMAT_VERSION];
        oversized_water.extend_from_slice(&0u32.to_le_bytes());
        oversized_water.extend_from_slice(&0u32.to_le_bytes());
        oversized_water.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_water), Err(WorldDecodeError::LimitExceeded),);

        let mut oversized_chunks = vec![WORLD_FORMAT_VERSION];
        oversized_chunks.extend_from_slice(&0u32.to_le_bytes());
        oversized_chunks.extend_from_slice(&0u32.to_le_bytes());
        oversized_chunks.extend_from_slice(&0u32.to_le_bytes());
        oversized_chunks.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(World::from_bytes(&oversized_chunks), Err(WorldDecodeError::LimitExceeded),);
    }

    /// A world with one chunk whose heights come from `f(x, z)` over the
    /// chunk-local cells.
    fn height_world(f: impl Fn(i32, i32) -> i32) -> World {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        for z in 0..CELLS_PER_CHUNK {
            for x in 0..CELLS_PER_CHUNK {
                chunk.height[(z * CELLS_PER_CHUNK + x) as usize] = f(x, z);
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world
    }

    #[test]
    fn step_ceiling_is_strictly_greater() {
        // Δh == STEP_MAX_OCTIMETERS is a legal step, one past it is a
        // cliff — the strictly-greater semantic movement will share.
        let world = height_world(|x, _| match x {
            0 => 0,
            1 => STEP_MAX_OCTIMETERS,
            _ => 2 * STEP_MAX_OCTIMETERS + 1,
        });
        assert!(!world.edge_is_cliff(cell(0, 0), cell(1, 0)));
        assert!(world.edge_is_cliff(cell(1, 0), cell(2, 0)));
    }

    #[test]
    fn corner_plates_split_by_the_cliff_walk() {
        // Rows z=0 at 0 and z=1 at 200 octimeters: a cliff runs along the
        // whole shared edge, so at corner (1,1) the four incident cells
        // split 2/2 and each side reads its own plate mean.
        let world = height_world(|_, z| {
            if z == 0 {
                0
            } else {
                200
            }
        });
        let low = world.cell_corner_heights(cell(0, 0));
        let high = world.cell_corner_heights(cell(0, 1));
        // Cell (0,0)'s far corners (indices 2, 3 — the z+1 pair) sit on the
        // cliff line and read the low plate; cell (0,1)'s near corners
        // (indices 0, 1) read the high plate at the same lattice points.
        assert_eq!(low[2], 0.0);
        assert_eq!(low[3], 0.0);
        assert_eq!(high[0], 200.0 / 256.0);
        assert_eq!(high[1], 200.0 / 256.0);
    }

    #[test]
    fn connected_corner_is_one_plate() {
        // Heights within the step ceiling all around a corner: every
        // incident cell reads the same mean — the blended-slope case.
        let world = height_world(|x, z| match (x, z) {
            (0, 0) => 0,
            (0, 1) => 64,
            _ => 32,
        });
        let mean = (0.0 + 32.0 + 64.0 + 32.0) / 4.0 / 256.0;
        assert_eq!(world.cell_corner_heights(cell(0, 0))[3], mean);
        assert_eq!(world.cell_corner_heights(cell(1, 0))[2], mean);
        assert_eq!(world.cell_corner_heights(cell(0, 1))[1], mean);
        assert_eq!(world.cell_corner_heights(cell(1, 1))[0], mean);
    }

    #[test]
    fn non_cliff_neighbors_agree_along_the_shared_edge() {
        // The drawn-equals-stood-on contract's continuity half: without a
        // cliff, the two cells' patches read the same height anywhere on
        // the shared edge (shared plates), so the meshes cannot crack.
        let world = height_world(|x, z| 8 * x + 4 * z);
        for step in 0..=4 {
            let wz = 3.0 + step as f32 / 4.0;
            let a = world.surface_height_in(cell(4, 3), 5.0, wz);
            let b = world.surface_height_in(cell(5, 3), 5.0, wz);
            assert!((a - b).abs() < 1e-6, "edge disagreement at wz {wz}");
        }
    }

    #[test]
    fn uniform_height_reads_everywhere() {
        let world = height_world(|_, _| 128);
        assert_eq!(world.surface_height(4.25, 7.75), 0.5);
        assert_eq!(world.surface_height_in(cell(3, 3), 3.5, 3.5), 0.5);
    }

    #[test]
    fn pre_v3_region_decodes_stone_cliffs() {
        // A version-2 buffer's region record has no cliff byte; it must
        // decode with the Stone default. Hand-built like the v1 tripwire:
        // one region, empty profile table, no chunks.
        let mut buf = vec![2u8];
        buf.extend_from_slice(&1u32.to_le_bytes()); // one region
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(b"r");
        buf.push(Material::Grass.to_u8());
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no chunks
        let world = World::from_bytes(&buf).expect("a v2 buffer still decodes");
        assert_eq!(world.regions[0].cliff_material, Material::Stone);
        assert_eq!(world.regions[0].default_material, Material::Grass);
    }

    #[test]
    fn insert_region_ignores_zero_and_grows_table() {
        let mut world = World::new();
        world.insert_region(
            0,
            Region { name: "ignored".into(), default_material: Material::Grass, cliff_material: Material::Stone },
        );
        // id 0 is the no-region sentinel — table stays empty, so a cell
        // pointing at region 1 finds no default and reads Void.
        let mut chunk = Chunk::empty();
        chunk.region[0] = 1;
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        assert_eq!(world.underlay(cell(0, 0)), Material::Void);

        // Inserting id 3 grows the table to length 3 (ids 1,2 padded
        // empty); a cell pointing at region 3 resolves its default.
        world.insert_region(
            3,
            Region { name: "third".into(), default_material: Material::Stone, cliff_material: Material::Stone },
        );
        let mut chunk3 = Chunk::empty();
        chunk3.region[0] = 3;
        world.insert_chunk(ChunkPos { x: 1, z: 0 }, chunk3);
        assert_eq!(world.underlay(cell(16, 0)), Material::Stone);
    }

    #[test]
    fn pre_v4_buffer_decodes_empty_water_table_and_zero_plane() {
        // Tripwire: a version-3 buffer carries no water-plane table and no
        // per-chunk water plane; both must read empty / zero, and a water
        // cell in it resolves at the datum-0 level. Hand-built: no regions
        // or profiles, one chunk with a water cell and the exact v3 plane
        // bytes (no water plane between height and region).
        let mut buf = vec![3u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[0] = Material::Water.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing

        let world = World::from_bytes(&buf).expect("a v3 buffer still decodes");
        assert!(world.water_planes.is_empty());
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert_eq!(chunk.water_plane, [0u16; CELLS_PER_CHUNK_AREA]);
        assert_eq!(world.water_level(cell(0, 0)), Some(0), "datum-0 level");
    }

    #[test]
    fn pre_v5_buffer_decodes_all_inherit_underlay_points() {
        // Tripwire: a version-4 buffer carries no per-chunk underlay-point
        // plane; it must read all-inherit, so every point resolves the cell's
        // cascade material exactly as a per-cell underlay did. Hand-built with
        // the exact v4 chunk-record layout (no underlay-point plane at the
        // tail): no regions / profiles / water planes, one chunk with a Stone
        // cell.
        let mut buf = vec![4u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no water planes
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[0] = Material::Stone.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // water planes
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing

        let world = World::from_bytes(&buf).expect("a v4 buffer still decodes");
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert!(
            chunk.underlay_points.iter().all(|point| *point == UNDERLAY_POINT_INHERIT),
            "a pre-5 buffer reads an all-inherit underlay-point plane",
        );
        assert_eq!(
            world.underlay_point(cell(0, 0), 2, 1),
            Material::Stone,
            "an inherit point resolves the cell's cascade material",
        );
    }

    /// A one-chunk world whose underlay / water-plane / height planes come
    /// from `fill(lx, lz) -> (material, plane id, lakebed height)`, with the
    /// given `(id, level)` water planes registered.
    fn plane_world(planes: &[(u32, i32)], fill: impl Fn(i32, i32) -> (Material, u16, i32)) -> World {
        let mut chunk = Chunk::empty();
        for lz in 0..CELLS_PER_CHUNK {
            for lx in 0..CELLS_PER_CHUNK {
                let (material, plane, height) = fill(lx, lz);
                let i = (lz * CELLS_PER_CHUNK + lx) as usize;
                chunk.underlay[i] = material;
                chunk.water_plane[i] = plane;
                chunk.height[i] = height;
            }
        }
        let mut world = World::new();
        for &(id, level) in planes {
            world.insert_water_plane(id, WaterPlane { level_octimeters: level });
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        world
    }

    #[test]
    fn water_level_resolves_plane_then_datum_and_none_off_water() {
        // A water cell reads its plane's level; plane 0 and an unregistered
        // id both read the datum 0; a non-water cell reads None.
        let world = plane_world(&[(1, 100)], |lx, _| match lx {
            0 => (Material::Water, 1, 0),  // registered plane 1
            1 => (Material::Water, 0, 0),  // plane 0 → datum
            2 => (Material::Water, 9, 0),  // unregistered id → datum
            3 => (Material::Grass, 0, 50), // not water
            _ => (Material::Void, 0, 0),
        });
        assert_eq!(world.water_level(cell(0, 0)), Some(100));
        assert_eq!(world.water_level(cell(1, 0)), Some(0));
        assert_eq!(world.water_level(cell(2, 0)), Some(0));
        assert_eq!(world.water_level(cell(3, 0)), None);
        // The lakebed read is unchanged — height stays the raw ground.
        assert_eq!(world.height(cell(3, 0)), 50);
    }

    #[test]
    fn interior_water_surface_is_flat_at_the_plane_level() {
        // A block of water on one plane renders exactly flat at the authored
        // level regardless of the lakebed heights beneath — every corner of
        // an interior water cell pins to the level.
        let world = plane_world(&[(1, 128)], |lx, lz| {
            // A bumpy lakebed under the water so a non-pinned plate would tilt.
            (Material::Water, 1, 11 * lx - 7 * lz)
        });
        let level_m = 128.0 / 256.0;
        for corner in world.cell_corner_heights(cell(5, 5)) {
            assert!((corner - level_m).abs() < 1e-6, "water corner {corner} not flat at {level_m}");
        }
        // And it is the stood-on surface, while height stays the raw lakebed.
        assert!((world.surface_height(5.5, 5.5) - level_m).abs() < 1e-6);
        assert_eq!(world.height(cell(5, 5)), 11 * 5 - 7 * 5);
    }

    #[test]
    fn connected_shore_land_blends_to_the_water_level() {
        // Land within the step ceiling of the water level shares the corner
        // plate, and the plate pins to the water members — so the land's
        // shared corner meets the water plane exactly (the beach blend), not
        // the mixed mean.
        let world = plane_world(&[(1, 100)], |_, lz| {
            if lz == 0 {
                (Material::Water, 1, 0) // level 100, lakebed 0
            } else {
                (Material::Grass, 0, 140) // within 64 of 100 → connected
            }
        });
        let level_m = 100.0 / 256.0;
        // Corner (1, 1) is shared by the two water cells (0,0),(1,0) and the
        // two land cells (0,1),(1,1); the plate has water members, so every
        // incident cell reads the water level there.
        assert!((world.cell_corner_heights(cell(0, 0))[3] - level_m).abs() < 1e-6);
        assert!(
            (world.cell_corner_heights(cell(1, 1))[0] - level_m).abs() < 1e-6,
            "connected shore land does not blend to the waterline",
        );
    }

    #[test]
    fn past_step_bank_splits_from_the_water_plane() {
        // Land standing past the step ceiling above the water cliffs against
        // it: the corner plates split, the water side stays at its level and
        // the bank reads its own height — the gap the skirt closes.
        let world = plane_world(&[(1, 100)], |_, lz| {
            if lz == 0 {
                (Material::Water, 1, 0) // level 100
            } else {
                (Material::Grass, 0, 200) // |200 - 100| = 100 > 64 → cliff
            }
        });
        assert!(world.edge_is_cliff(cell(0, 0), cell(0, 1)));
        // At corner (1, 1) the water side reads 100 and the bank reads 200.
        assert!((world.cell_corner_heights(cell(0, 0))[3] - 100.0 / 256.0).abs() < 1e-6);
        assert!((world.cell_corner_heights(cell(1, 1))[0] - 200.0 / 256.0).abs() < 1e-6);
    }

    #[test]
    fn a_water_plane_row_rewrite_retunes_the_level() {
        // Retuning a lake is one table write: the same water cell resolves at
        // the new level after re-registering its plane id.
        let mut world = plane_world(&[(1, 100)], |_, _| (Material::Water, 1, 0));
        assert_eq!(world.water_level(cell(3, 3)), Some(100));
        world.insert_water_plane(1, WaterPlane { level_octimeters: 240 });
        assert_eq!(world.water_level(cell(3, 3)), Some(240));
    }

    #[test]
    fn point_height_inherits_applies_and_saturates() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.height[3 * 16 + 3] = 100; // cell (3,3)
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        // An inherit (zero) point reads the cell height unchanged.
        assert_eq!(world.point_height(cell(3, 3), 0, 0), 100, "inherit reads cell");
        // A stamped delta offsets the point off the cell height.
        world.set_cell_heights(cell(3, 3), &[40, -25]);
        assert_eq!(world.point_height(cell(3, 3), 0, 0), 140, "+delta lifts");
        assert_eq!(world.point_height(cell(3, 3), 1, 0), 75, "-delta drops");
        assert_eq!(world.point_height(cell(3, 3), 2, 0), 100, "the untouched tail inherits the cell height");
        // A short stamp leaves the tail inheriting; an empty stamp clears all.
        world.set_cell_heights(cell(3, 3), &[]);
        assert_eq!(world.point_height(cell(3, 3), 0, 0), 100, "an empty stamp clears the cell to inherit");

        // Extremes saturate rather than wrap: a max-magnitude delta on a
        // near-i32-max cell height clamps at the bound, not overflow-wraps.
        let mut extreme = Chunk::empty();
        extreme.height[0] = i32::MAX - 10;
        let mut ex_world = World::new();
        ex_world.insert_chunk(ChunkPos { x: 0, z: 0 }, extreme);
        ex_world.set_cell_heights(cell(0, 0), &[i16::MAX]);
        assert_eq!(ex_world.point_height(cell(0, 0), 0, 0), i32::MAX, "a lift past the range saturates at i32::MAX");
    }

    #[test]
    fn a_missing_chunk_point_height_reads_zero() {
        let world = World::new();
        assert_eq!(world.point_height(cell(50, -20), 2, 1), 0);
    }

    #[test]
    fn pre_v6_buffer_decodes_all_zero_height_points() {
        // Tripwire: a version-5 buffer carries no per-chunk height-delta
        // plane; it must read all-zero relief, so every point resolves the
        // cell's own height exactly as a per-cell height did. Hand-built with
        // the exact v5 chunk-record layout (underlay-point plane at the tail,
        // no height plane after it): no tables, one chunk with a raised cell.
        let mut buf = vec![5u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no water planes
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        let mut underlay = [0u8; CELLS_PER_CHUNK_AREA];
        underlay[0] = Material::Stone.to_u8();
        buf.extend_from_slice(&underlay);
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // masks
        let mut heights = [0u8; 4 * CELLS_PER_CHUNK_AREA];
        heights[0..4].copy_from_slice(&128i32.to_le_bytes()); // cell 0 height
        buf.extend_from_slice(&heights);
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // water planes
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing
        buf.resize(buf.len() + UNDERLAY_POINTS_PER_CHUNK, UNDERLAY_POINT_INHERIT); // points

        let world = World::from_bytes(&buf).expect("a v5 buffer still decodes");
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert!(
            chunk.height_points.iter().all(|point| *point == HEIGHT_POINT_INHERIT),
            "a pre-6 buffer reads an all-zero height-delta plane",
        );
        assert_eq!(world.point_height(cell(0, 0), 2, 1), 128, "a zero-relief point resolves the cell's own height");
    }

    #[test]
    fn pre_v7_buffer_expands_overlay_bits_to_coverage_bytes() {
        // Tripwire: a version-6 buffer stores two packed mask bytes per
        // cell. Decoding expands each bit to the v7 scalar plane: set bits
        // become 255, clear bits become 0, preserving binary midpoint
        // crossings under the scalar mesher.
        let mut buf = vec![6u8];
        buf.extend_from_slice(&0u32.to_le_bytes()); // no regions
        buf.extend_from_slice(&0u32.to_le_bytes()); // no profiles
        buf.extend_from_slice(&0u32.to_le_bytes()); // no water planes
        buf.extend_from_slice(&1u32.to_le_bytes()); // one chunk
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk x
        buf.extend_from_slice(&0i32.to_le_bytes()); // chunk z
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // underlay
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // overlay
        let mut masks = [0u8; 2 * CELLS_PER_CHUNK_AREA];
        masks[0..2].copy_from_slice(&0b0000_0000_0000_0101u16.to_le_bytes());
        buf.extend_from_slice(&masks);
        buf.extend_from_slice(&[0u8; 4 * CELLS_PER_CHUNK_AREA]); // heights
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // water planes
        buf.extend_from_slice(&[0u8; 2 * CELLS_PER_CHUNK_AREA]); // regions
        buf.extend_from_slice(&[0u8; CELLS_PER_CHUNK_AREA]); // smoothing
        buf.resize(buf.len() + UNDERLAY_POINTS_PER_CHUNK, UNDERLAY_POINT_INHERIT); // points
        buf.resize(buf.len() + 2 * HEIGHT_POINTS_PER_CHUNK, 0); // height deltas

        let world = World::from_bytes(&buf).expect("a v6 buffer still decodes");
        let chunk = world.chunk(ChunkPos { x: 0, z: 0 }).expect("chunk");
        assert_eq!(chunk.overlay_mask[0], 255);
        assert_eq!(chunk.overlay_mask[3], 255, "legacy bit 0 expands across its SUB=16 block");
        assert_eq!(chunk.overlay_mask[4], 0);
        assert_eq!(chunk.overlay_mask[8], 255, "legacy bit 2 expands across its SUB=16 block");
    }

    #[test]
    fn world_bytes_roundtrip_preserves_height_deltas() {
        // An authored height-delta pattern rides the v6 chunk record and must
        // survive the trip byte-for-byte alongside the other planes.
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.height[10] = 64;
        world.insert_chunk(ChunkPos { x: 2, z: -1 }, chunk);
        world.set_cell_heights(
            CellPos { x: 32, z: -16 }, // cell (0,0) of chunk (2,-1)
            &[300, -300, 0, 127, -128],
        );

        let bytes = world.to_bytes();
        let decoded = World::from_bytes(&bytes).expect("roundtrip decodes");
        assert_eq!(
            decoded.chunk(ChunkPos { x: 2, z: -1 }),
            world.chunk(ChunkPos { x: 2, z: -1 }),
            "the height-delta plane survives the round trip",
        );
    }

    #[test]
    fn zeroed_deltas_restore_the_cell_stride_surface() {
        // Tripwire: with every height delta zero the surface resolves at cell
        // stride, byte-identical to a world that never carried the plane — the
        // per-cell shortcut must collapse an all-zero neighborhood to the cell
        // math, so authoring then clearing a cell's deltas moves nothing.
        let base = height_world(|x, z| 8 * x - 5 * z); // gentle, no cliffs
        let mut authored = base.clone();
        authored.set_cell_heights(cell(3, 3), &[40; SUBCELLS_PER_CELL]);
        authored.set_cell_heights(cell(3, 3), &[]); // clears back to zero relief
        assert!(
            !authored.cell_has_height_relief(cell(3, 3)),
            "a cleared cell reports no relief, so the shortcut engages",
        );
        for &(wx, wz) in &[(3.5, 3.5), (3.1, 3.9), (4.0, 3.0), (2.5, 4.5), (3.75, 3.25)] {
            assert_eq!(
                base.surface_height(wx, wz),
                authored.surface_height(wx, wz),
                "a net-zero delta plane must not move the surface at ({wx}, {wz})",
            );
        }
    }

    #[test]
    fn a_delta_ramp_reads_continuously() {
        // A per-point height ramp within the step ceiling stays continuous:
        // adjacent points share their plate, so surface_height varies smoothly
        // across subcell boundaries with no break.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let mut world = height_world(|_, _| 0);
        // Cell (5,5) ramps 16 octimeters per subcell in x (0,16,32,48 ≤ step).
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        for sz in 0..sub {
            for sx in 0..sub {
                deltas[(sz * sub + sx) as usize] = (16 * sx) as i16;
            }
        }
        world.set_cell_heights(cell(5, 5), &deltas);
        // March densely across the ramp; no successive step exceeds the
        // per-subcell slope (a break would show a jump far past it).
        let mut prev = world.surface_height(5.02, 5.5);
        for i in 1..48 {
            let wx = 5.02 + i as f32 * 0.02;
            let h = world.surface_height(wx, 5.5);
            assert!((h - prev).abs() < 0.05, "a continuous ramp jumped {} at x {wx}", (h - prev).abs());
            prev = h;
        }
    }

    #[test]
    fn a_delta_plateau_splits_plates_on_its_perimeter() {
        // A 2×2 block of points raised past the step ceiling is a plateau: its
        // interior points stand at the raised level, a point just outside the
        // block stays at the base, and the surface between them breaks (the
        // plate splits on the block's perimeter) rather than blending.
        let sub = SUBCELLS_PER_CELL_EDGE.cast_signed();
        let mut world = height_world(|_, _| 0);
        let mut deltas = [0i16; SUBCELLS_PER_CELL];
        for sz in 1..3 {
            for sx in 1..3 {
                deltas[(sz * sub + sx) as usize] = 200; // > STEP_MAX_OCTIMETERS
            }
        }
        world.set_cell_heights(cell(5, 5), &deltas);
        // Center of the raised block (subcell (1,1)..(2,2)) stands at 200/256.
        let sub_f = sub as f32;
        let inside = world.surface_height(5.0 + 1.5 / sub_f, 5.0 + 1.5 / sub_f);
        assert!((inside - 200.0 / 256.0).abs() < 1e-4, "the plateau interior stands at the raised level, got {inside}");
        // A flat corner subcell stays at the base — the plate did not blend
        // the raise outward across the break.
        let outside = world.surface_height(5.0 + 0.5 / sub_f, 5.0 + 0.5 / sub_f);
        assert!(outside.abs() < 1e-4, "a flat subcell outside the plateau stays at the base, got {outside}");
    }

    #[test]
    fn terrain_surface_sampler_shares_presence_and_height_truth() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay[0] = Material::Stone;
        chunk.height[0] = 256;
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        let surface = world.terrain_surface_at(0.5, 0.5).expect("resolved underlay is markable");
        assert_eq!(surface.cell, cell(0, 0));
        assert_eq!(surface.mark_point, WorldPoint::new(128, 128));
        assert!((surface.height_meters - 1.0).abs() < 1e-4);

        let subcell = (8 * SUBCELLS_PER_CELL_EDGE + 8) as usize;
        world.chunk_mut_or_insert(ChunkPos { x: 0, z: 0 }).underlay_points[subcell] = Material::Void.to_u8();
        assert!(world.terrain_surface_at(0.5, 0.5).is_none(), "an explicit underlay-point hole is not markable");

        let chunk = world.chunk_mut_or_insert(ChunkPos { x: 0, z: 0 });
        chunk.overlay[0] = Material::Sand;
        chunk.overlay_mask[..SUBCELLS_PER_CELL].fill(SCALAR_COVERAGE_THRESHOLD - 1);
        assert!(world.terrain_surface_at(0.5, 0.5).is_none(), "coverage below the contour threshold stays absent");
        world.chunk_mut_or_insert(ChunkPos { x: 0, z: 0 }).overlay_mask[..SUBCELLS_PER_CELL]
            .fill(SCALAR_COVERAGE_THRESHOLD);
        assert!(
            world.terrain_surface_at(0.5, 0.5).is_some(),
            "the exact contour threshold makes an overlay-only sample markable"
        );
    }

    #[test]
    fn terrain_surface_sampler_matches_a_continuous_scalar_overlay_crossing() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.overlay[0] = Material::Stone;
        let subcells = SUBCELLS_PER_CELL_EDGE as usize;
        for subcell_z in 0..subcells {
            for subcell_x in 0..subcells {
                chunk.overlay_mask[subcell_z * subcells + subcell_x] = if subcell_x < subcells / 2 {
                    100
                } else {
                    200
                };
            }
        }
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);

        let low_sample_x = (subcells / 2 - 2) as f32 / subcells as f32 + 0.5 / subcells as f32;
        let high_sample_x = (subcells / 2 - 1) as f32 / subcells as f32 + 0.5 / subcells as f32;
        let high_reconstructed = 150.0;
        let crossing_fraction = (COVERAGE_CROSSING - 100.0) / (high_reconstructed - 100.0);
        let crossing_x = low_sample_x + (high_sample_x - low_sample_x) * crossing_fraction;

        assert!(
            world.terrain_surface_at(crossing_x - 0.001, 0.5).is_none(),
            "the point immediately before the rendered 100→200 crossing stays absent"
        );
        assert!(
            world.terrain_surface_at(crossing_x + 0.001, 0.5).is_some(),
            "the point immediately after the rendered 100→200 crossing is markable"
        );
    }

    #[test]
    fn terrain_surface_sampler_follows_relief_and_rejects_nonfinite_coordinates() {
        let mut world = World::new();
        let mut chunk = Chunk::empty();
        chunk.underlay.fill(Material::Grass);
        world.insert_chunk(ChunkPos { x: 0, z: 0 }, chunk);
        let mut deltas = [0; SUBCELLS_PER_CELL];
        deltas[8 * SUBCELLS_PER_CELL_EDGE as usize + 8] = 128;
        world.set_cell_heights(cell(0, 0), &deltas);

        let surface = world.terrain_surface_at(0.53125, 0.53125).expect("relief remains markable");
        assert!(surface.height_meters > 0.0, "the sampler uses the same relief-aware surface-height path");
        assert!(world.terrain_surface_at(f32::NAN, 0.5).is_none());
        assert!(world.terrain_surface_at(0.5, f32::INFINITY).is_none());
    }

    #[test]
    fn terrain_surface_sampler_resolves_water_and_negative_coordinates_directly() {
        let mut water = World::new();
        water.insert_water_plane(1, WaterPlane { level_octimeters: 512 });
        let mut water_chunk = Chunk::empty();
        water_chunk.underlay[0] = Material::Water;
        water_chunk.height[0] = -256;
        water_chunk.water_plane[0] = 1;
        water.insert_chunk(ChunkPos { x: 0, z: 0 }, water_chunk);
        let water_surface = water.terrain_surface_at(0.5, 0.5).expect("water is a markable top surface");
        assert_eq!(water_surface.cell, cell(0, 0));
        assert_eq!(water_surface.mark_point, WorldPoint::new(128, 128));
        assert!((water_surface.height_meters - 2.0).abs() < 1e-4);

        let negative_cell = cell(-1, -1);
        let mut negative_chunk = Chunk::empty();
        negative_chunk.underlay[negative_cell.chunk_index()] = Material::Grass;
        negative_chunk.height[negative_cell.chunk_index()] = 128;
        let mut negative = World::new();
        negative.insert_chunk(negative_cell.chunk(), negative_chunk);
        let negative_surface =
            negative.terrain_surface_at(-0.5, -0.5).expect("negative lattice coordinates remain markable");
        assert_eq!(negative_surface.cell, negative_cell);
        assert_eq!(negative_surface.mark_point, WorldPoint::new(-128, -128));
        assert!((negative_surface.height_meters - 0.5).abs() < 1e-4);
    }
}
