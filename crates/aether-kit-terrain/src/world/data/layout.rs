//! The chunk / cell / subcell dimensions — the format's dimensional
//! contract.
//!
//! The dense plane lengths are *derived* from the cell and subcell counts
//! rather than written out, so raising [`SUBCELLS_PER_CELL_EDGE`] regrows
//! every per-subcell plane with no other change. The chain lives in one
//! file so that derivation reads at a glance.

/// Cells along one edge of a chunk. Chunks are `16 × 16` cells.
pub const CELLS_PER_CHUNK: i32 = 16;

/// Right-shift a cell coordinate by this to derive its chunk
/// (`2^4 = 16` cells per chunk edge). Arithmetic shift — floors on
/// negatives.
pub const CHUNK_BITS: u32 = 4;

/// Cells in one chunk: `16 × 16`. The length of every per-chunk plane.
pub const CELLS_PER_CHUNK_AREA: usize = 256;

/// Subcells along one edge of a cell — the overlay coverage resolution.
/// A cell's [`Chunk::overlay_mask`](crate::world::Chunk::overlay_mask) stores one scalar coverage byte for
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
/// cell's cascade-resolved material ([`World::underlay`](crate::world::World::underlay)) rather than
/// pinning one. An untouched world stores every point as this, so it
/// meshes exactly as a per-cell underlay. An explicit `0..=5` byte pins the
/// point to a [`Material`](crate::world::Material) — including `0` = authored [`Material::Void`](crate::world::Material::Void),
/// which is what cuts a shape or a hole below cell scale.
pub const UNDERLAY_POINT_INHERIT: u8 = 255;

/// Points in one chunk's height-delta plane: `CELLS_PER_CHUNK_AREA * SUB²`
/// (= 65536 at `SUB = 16`), one `i16` octimeter delta per subcell in the same
/// row-major cell order and `z*SUB + x` within-cell order as
/// [`UNDERLAY_POINTS_PER_CHUNK`]. Stride-agnostic by construction: raising
/// [`SUBCELLS_PER_CELL_EDGE`] regrows the plane with no other change.
pub const HEIGHT_POINTS_PER_CHUNK: usize = CELLS_PER_CHUNK_AREA * SUBCELLS_PER_CELL;

/// The height-point default: a point holding this **inherits** its cell's
/// [`Chunk::height`](crate::world::Chunk::height) with no relief. An untouched world stores every point
/// as this, so its surface resolves exactly at cell stride; an explicit
/// non-zero delta lifts (or drops) the point off the cell height by that
/// many octimeters ([`World::point_height`](crate::world::World::point_height)), the subcell-resolution relief
/// the height pipeline resolves one stride down.
pub const HEIGHT_POINT_INHERIT: i16 = 0;

/// Pre-v7 saves store overlay coverage as one 16-bit cell mask, fixed at the
/// old 4x4 subcell lattice. Decode expands each legacy bit to the current
/// subcell block so old binary masks keep the same world-space shape.
pub(super) const LEGACY_MASK_SUBCELLS_PER_CELL_EDGE: usize = 4;

/// Octimeters per cell: `1 cell = 1 m = 256 octimeters`.
pub(super) const OCTIMETERS_PER_CELL: i32 = 256;

/// Right-shift an octimeter coordinate by this to derive its cell.
pub(super) const OCTIMETER_BITS: u32 = 8;
