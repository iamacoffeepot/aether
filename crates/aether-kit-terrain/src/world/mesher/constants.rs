use crate::world::{CELLS_PER_CHUNK, SUBCELLS_PER_CELL_EDGE};

/// Cells along one chunk edge, as a plain `i32` for loop bounds.
pub(super) const EDGE: i32 = CELLS_PER_CHUNK;

/// Subcells along one cell edge, as `i32`.
pub(super) const SUB: i32 = SUBCELLS_PER_CELL_EDGE as i32;

/// Subcells along one chunk edge (`16 * 16 = 256`).
pub(super) const SUBCELLS_PER_CHUNK_EDGE: i32 = EDGE * SUB;

/// Octimeters per subcell (`256 / SUB = 16`).
pub(super) const OCTIMETERS_PER_SUBCELL: i32 = 256 / SUB;

/// Octimeters per meter, for the octimeter-to-meter conversion at vertex
/// emit.
pub(super) const OCTIMETERS_PER_METER: f32 = 256.0;

/// How far in octimeters an unbounded-void wall drops below its top edge —
/// the border-skirt fallback for the one void case with no far rim within
/// the fill-over march bound (a void that reaches the world border). A
/// bounded void joint closes instead as a real groove: wall down to the void
/// floor, floor across, wall back up (see [`super::voids::emit_void_floors`]).
/// The skirt
/// reads as thick ground rather than a paper-thin lip.
pub(super) const WALL_VOID_SKIRT_OCTIMETERS: i32 = 512;

/// Upsample factor for the partition contour grid. At `SUB = 16`, the base
/// subcell lattice is already finer than the old upsampled grid, and keeping
/// the extra pass makes dense remesh tests miss CI time budgets.
pub(super) const CONTOUR_UPSAMPLE: usize = 1;

/// Apron cap in subcells (two cells) so a chunk's reads stay within the
/// eight-neighbor remesh the `R = 1` invalidation covers.
pub(super) const MAX_APRON_SUBCELLS: i32 = 2 * SUB;

/// The coverage layer lift: two octimeters over the underlay datum, matching
/// the retired overlay rim lift without restoring the old rim/body stack.
pub(super) const COVERAGE_LIFT: f32 = 2.0 / OCTIMETERS_PER_METER;
