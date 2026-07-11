// Plane bytes narrow u32 ids down to u16 at the wire boundary; the truncation
// is the documented format contract, not a bug.
#![allow(clippy::cast_possible_truncation)]
// The kinds pull the world module's shared data vocabulary (the `Chunk` /
// `Material` / `Region` data layer plus its geometry consts, many referenced
// only from doc links) from the parent module.
#![allow(clippy::wildcard_imports)]

//! Wire kinds for the world plane stack — the `aether.kit.world.*` mail a
//! peer sends [`WorldView`] to write chunks, paint cells or compact vector
//! stamps, register regions, or load a serialized world.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::data::cliff_material_from_u8;
use super::*;
use crate::mark::MarkRef;

/// `aether.kit.world.set_chunk` — write one chunk's planes. The ground
/// planes ride as raw `Bytes` (256 material/shape bytes each);
/// `overlay_mask` and `underlay_points` ride as up to one byte per subcell,
/// and `height_points` as up to [`HEIGHT_POINTS_PER_CHUNK`] `i16` deltas;
/// `height` / `region` / `water_plane` ride as length-256 vectors. Shorter
/// vectors pad with `Void` / `0` (and a short `underlay_points` /
/// `height_points` leaves the tail inheriting); longer ones truncate.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_chunk")]
pub struct SetChunk {
    pub chunk_x: i32,
    pub chunk_z: i32,
    /// Underlay plane — 256 raw material bytes (`Material as u8`).
    pub underlay: Vec<u8>,
    /// Underlay material-point plane — up to [`UNDERLAY_POINTS_PER_CHUNK`]
    /// bytes (`256 * SUB²`; 65536 at `SUB = 16`), one point per subcell in
    /// row-major cell order (`z*SUB + x` within a cell). Each byte is a
    /// [`Material`] or the [`UNDERLAY_POINT_INHERIT`] sentinel; a short
    /// vector leaves the remaining points inheriting, so an empty vector is
    /// the all-inherit default.
    pub underlay_points: Vec<u8>,
    /// Height-delta plane — up to [`HEIGHT_POINTS_PER_CHUNK`] `i16` octimeter
    /// deltas, one per subcell in the same layout as `underlay_points`. Each
    /// offsets its subcell off the cell's `height` ([`World::point_height`]);
    /// a short vector leaves the remaining points at [`HEIGHT_POINT_INHERIT`]
    /// (`0`, no relief), so an empty vector is the flat default.
    pub height_points: Vec<i16>,
    /// Overlay plane — 256 raw material bytes. `0` = no overlay.
    pub overlay: Vec<u8>,
    /// Overlay subcell coverage plane — [`OVERLAY_MASK_WIRE_BYTES`] bytes
    /// (`256 * SUB²`; 65536 at `SUB = 16`), one coverage byte per subcell in
    /// row-major cell order (`z*SUB + x` within a cell). A short vector
    /// leaves the remaining samples uncovered, so an empty vector is the
    /// no-coverage default.
    pub overlay_mask: Vec<u8>,
    /// Elevation plane — 256 octimeter values.
    pub height: Vec<i32>,
    /// Region-id plane — 256 values. `0` = no region.
    pub region: Vec<u32>,
    /// Water-plane-id plane — 256 values, narrowing to `u16` like `region`.
    /// `0` = the datum-0 level; meaningful only under a water cell.
    pub water_plane: Vec<u32>,
    /// Smoothing-profile-id plane — 256 raw bytes. `0` = no override.
    pub smoothing: Vec<u8>,
}

impl SetChunk {
    /// The chunk address this write targets.
    #[must_use]
    pub fn chunk_pos(&self) -> ChunkPos {
        ChunkPos { x: self.chunk_x, z: self.chunk_z }
    }

    /// Decode the wire planes into a [`Chunk`]. Material bytes degrade to
    /// `Void` on an unknown value; every plane pads to 256 / truncates
    /// past it; `region` casts `u32` down to `u16`.
    #[must_use]
    pub fn into_chunk(self) -> Box<Chunk> {
        let mut chunk = Chunk::empty_boxed();
        for (dst, byte) in chunk.underlay.iter_mut().zip(&self.underlay) {
            *dst = Material::from_u8_or_void(*byte);
        }
        // Underlay-point plane: a short vector leaves the tail inheriting
        // (Chunk::empty seeds every point to the inherit sentinel).
        for (dst, byte) in chunk.underlay_points.iter_mut().zip(&self.underlay_points) {
            *dst = *byte;
        }
        // Height-delta plane: a short vector leaves the tail at zero relief
        // (Chunk::empty seeds every delta to the inherit sentinel).
        for (dst, delta) in chunk.height_points.iter_mut().zip(&self.height_points) {
            *dst = *delta;
        }
        for (dst, byte) in chunk.overlay.iter_mut().zip(&self.overlay) {
            *dst = Material::from_u8_or_void(*byte);
        }
        // Coverage plane: a short vector leaves the tail uncovered
        // (Chunk::empty seeds every sample to 0); trailing bytes truncate.
        for (dst, byte) in chunk.overlay_mask.iter_mut().zip(&self.overlay_mask) {
            *dst = *byte;
        }
        for (dst, value) in chunk.height.iter_mut().zip(&self.height) {
            *dst = *value;
        }
        for (dst, value) in chunk.region.iter_mut().zip(&self.region) {
            *dst = *value as u16;
        }
        for (dst, value) in chunk.water_plane.iter_mut().zip(&self.water_plane) {
            *dst = *value as u16;
        }
        for (dst, byte) in chunk.smoothing.iter_mut().zip(&self.smoothing) {
            *dst = *byte;
        }
        chunk
    }
}

/// `aether.kit.world.set_cell_points` — stamp one cell's `SUB × SUB`
/// underlay material points, the single-cell live-paint counterpart to
/// `set_chunk`'s whole-plane write. `points` carries up to
/// [`SUBCELLS_PER_CELL`] bytes in `z*SUB + x` subcell order; a short vector
/// leaves the cell's remaining points inheriting (an empty vector clears
/// the cell back to all-inherit). Each byte is a [`Material`] or the
/// [`UNDERLAY_POINT_INHERIT`] sentinel — including `0` = authored `Void`,
/// which cuts a hole.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_cell_points")]
pub struct SetCellPoints {
    pub x: i32,
    pub z: i32,
    pub points: Vec<u8>,
}

impl SetCellPoints {
    /// The cell this stamp targets.
    #[must_use]
    pub fn cell(&self) -> CellPos {
        CellPos { x: self.x, z: self.z }
    }
}

/// A point on the world's XZ plane, expressed in octimeters.
///
/// Keeping the axes and unit named in the wire vocabulary prevents callers
/// from having to infer whether a positional pair is `[x, z]`, `[z, x]`, or
/// measured in cells rather than octimeters.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldPoint {
    pub x_octimeters: i32,
    pub z_octimeters: i32,
}

impl WorldPoint {
    #[must_use]
    pub const fn new(x_octimeters: i32, z_octimeters: i32) -> Self {
        Self { x_octimeters, z_octimeters }
    }
}

/// A position in the rendered world's meter-space coordinate system.
///
/// Axes and units stay named so callers never have to infer the ordering or
/// scale of a positional vector.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct WorldPositionMeters {
    pub x_meters: f32,
    pub y_meters: f32,
    pub z_meters: f32,
}

/// A ray direction. Components need not normalize it before sending.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct WorldDirection {
    pub x_unitless: f32,
    pub y_unitless: f32,
    pub z_unitless: f32,
}

/// A bounded world-space ray used for terrain-surface picking.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct TerrainRay {
    pub origin: WorldPositionMeters,
    pub direction: WorldDirection,
    pub max_distance_meters: f32,
}

/// The markable terrain sample underneath a picked world position.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct TerrainSurface {
    pub cell: CellPos,
    pub mark_point: WorldPoint,
    pub height_meters: f32,
}

/// First top-surface intersection of a validated terrain ray.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct TerrainSurfaceHit {
    pub position: WorldPositionMeters,
    pub surface: TerrainSurface,
    pub ray_distance_meters: f32,
}

/// Why a terrain ray cannot be evaluated.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerrainPickError {
    NonFiniteRay,
    ZeroDirection,
    InvalidMaxDistance,
}

/// `aether.kit.world.pick_terrain` — intersect a bounded world ray with the
/// first markable terrain top surface.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.world.pick_terrain")]
pub struct PickTerrain {
    pub ray: TerrainRay,
}

/// Reply to [`PickTerrain`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[kind(name = "aether.kit.world.pick_terrain_result")]
pub enum PickTerrainResult {
    Hit { hit: TerrainSurfaceHit },
    Miss,
    Rejected { error: TerrainPickError },
}

/// `aether.kit.world.set_mark_overlay_visibility` — show or hide the
/// read-only `MarkBook` projection rendered by [`WorldView`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.world.set_mark_overlay_visibility")]
pub struct SetMarkOverlayVisibility {
    pub visible: bool,
}

/// Visibility state after [`SetMarkOverlayVisibility`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.world.set_mark_overlay_visibility_result")]
pub struct SetMarkOverlayVisibilityResult {
    pub visible: bool,
    pub synchronized: bool,
}

/// `aether.kit.world.set_mark_overlay_selection` — select an exact cached
/// mark revision for highlighted overlay rendering, or clear the selection.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.world.set_mark_overlay_selection")]
pub struct SetMarkOverlaySelection {
    pub selected: Option<MarkRef>,
}

/// Result of applying an exact-revision overlay selection.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[kind(name = "aether.kit.world.set_mark_overlay_selection_result")]
pub enum SetMarkOverlaySelectionResult {
    Selected { reference: MarkRef },
    Cleared,
    Stale { requested: MarkRef, current: MarkRef },
    Unsynchronized { requested: MarkRef, cached: Option<MarkRef> },
}

/// Maximum number of vertices accepted by one polygon stamp.
pub const MAX_STAMP_VERTICES: usize = 1024;
/// Maximum raster extent of one stamp along either axis, in subcells.
pub const MAX_STAMP_EDGE_SUBCELLS: usize = 4096;
/// Maximum total raster area allocated for one stamp, in subcells.
pub const MAX_STAMP_SUBCELLS: usize = 1_048_576;
/// Maximum estimated scanline work accepted by one stamp. The estimate
/// includes edge tests, intersection sorting, and interval-to-subcell visits.
pub const MAX_STAMP_RASTER_WORK: usize = 33_554_432;

/// `aether.kit.world.stamp_polygon` — rasterize a polygon directly into the
/// scalar overlay coverage plane. `points` is a [`WorldPoint`] vertex ring;
/// concave rings use even-odd fill. `material` is a raw [`Material`] byte.
/// Fewer than three points, more than [`MAX_STAMP_VERTICES`] points, a
/// degenerate or oversized ring, or `Material::Void` paints nothing. Across
/// all stamp kinds, equal materials max-compose coverage; a different
/// material replaces the mask of each cell it reaches in painter order.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.stamp_polygon")]
pub struct StampPolygon {
    pub points: Vec<WorldPoint>,
    pub material: u8,
}

/// `aether.kit.world.stamp_disc` — rasterize a compact disc description into
/// the scalar overlay coverage plane. `center` is a [`WorldPoint`],
/// `radius_octimeters` is center-to-edge distance, and `material` is a raw
/// [`Material`] byte. A zero radius, oversized raster, or `Material::Void`
/// paints nothing.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.stamp_disc")]
pub struct StampDisc {
    pub center: WorldPoint,
    pub radius_octimeters: u32,
    pub material: u8,
}

/// `aether.kit.world.stamp_hexagon` — rasterize a flat-top regular hexagon
/// into the scalar overlay coverage plane. `center` is a [`WorldPoint`],
/// `radius_octimeters` is center-to-vertex distance, and `material` is a raw
/// [`Material`] byte. A zero radius, oversized raster, or `Material::Void`
/// paints nothing.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.stamp_hexagon")]
pub struct StampHexagon {
    pub center: WorldPoint,
    pub radius_octimeters: u32,
    pub material: u8,
}

/// A cell address used by terrain operators.
///
/// The axes stay named on the wire so this cannot be confused with a point
/// measured in octimeters or with a positional `[x, z]` pair.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OperatorCell {
    pub cell_x: i32,
    pub cell_z: i32,
}

impl OperatorCell {
    /// Convert the wire address to the world's internal lattice address.
    #[must_use]
    pub const fn cell_pos(self) -> CellPos {
        CellPos { x: self.cell_x, z: self.cell_z }
    }
}

/// A chunk address reported by terrain-operator statistics.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OperatorChunk {
    pub chunk_x: i32,
    pub chunk_z: i32,
}

impl From<ChunkPos> for OperatorChunk {
    fn from(value: ChunkPos) -> Self {
        Self { chunk_x: value.x, chunk_z: value.z }
    }
}

/// Hard execution limits for one terrain operator.
///
/// Operators charge before every mutation. Reaching either limit returns a
/// typed failure with the consistent partial result and never performs the
/// over-cap write.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorBudget {
    pub max_steps: u32,
    pub max_subcells: u32,
}

/// Parameters shared by every disc placement along a brush path.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrushParameters {
    pub radius_octimeters: u32,
    pub spacing_octimeters: u32,
    /// Raw [`Material`] byte written to the overlay plane.
    pub material: u8,
}

/// Deterministic reference automata supported by [`RunAutomaton`].
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomatonRule {
    /// Paint every cell within `generations` four-neighbor expansions of the
    /// seed. Generation zero contains only the seed.
    Grow {
        /// Raw [`Material`] byte written to every accepted cell's point plane.
        material: u8,
        generations: u32,
    },
}

/// Why a terrain operator stopped without completing its requested work.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OperatorError {
    InvalidParameters { reason: String },
    StepBudgetExhausted,
    SubcellBudgetExhausted,
}

/// Exact accounting for a complete or partial terrain-operator execution.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OperatorStats {
    pub steps_run: u32,
    pub subcells_written: u32,
    pub touched_chunks: Vec<OperatorChunk>,
}

/// `aether.kit.world.apply_brush` — place bounded disc stamps at a stable
/// spacing along a named world-space path.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.apply_brush")]
pub struct ApplyBrush {
    pub source: MarkRef,
    pub path: Vec<WorldPoint>,
    pub brush: BrushParameters,
    pub budget: OperatorBudget,
}

/// `aether.kit.world.run_automaton` — execute one bounded reference automaton
/// from a named cell seed.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.run_automaton")]
pub struct RunAutomaton {
    pub source: MarkRef,
    pub seed: OperatorCell,
    pub rule: AutomatonRule,
    pub budget: OperatorBudget,
}

/// Reply shared by [`ApplyBrush`] and [`RunAutomaton`].
///
/// A failure reports the writes accepted before exhaustion. Transaction state
/// belongs to ADR-0143's proposal surface and is deliberately absent here.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.kit.world.operator_result")]
pub enum OperatorResult {
    Applied { source: MarkRef, stats: OperatorStats },
    Failed { source: MarkRef, error: OperatorError, stats: OperatorStats },
}

/// Session-scoped identifier for a staged terrain proposal.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProposalId {
    pub value: u64,
}

/// Six-axis render-world bounds of geometry changed by a proposal.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct ProposalBounds {
    pub min_x_meters: f32,
    pub min_y_meters: f32,
    pub min_z_meters: f32,
    pub max_x_meters: f32,
    pub max_y_meters: f32,
    pub max_z_meters: f32,
}

/// Deterministic geometry summary for a staged terrain proposal.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProposalDigest {
    pub touched_chunks: Vec<OperatorChunk>,
    pub triangle_count: u64,
    pub changed_geometry_bounds: Option<ProposalBounds>,
}

/// Bounded terrain mutation that can be staged without changing committed terrain.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
pub enum ProposalOperation {
    SetChunk { request: SetChunk },
    SetCellPoints { request: SetCellPoints },
    SetCellHeights { request: SetCellHeights },
    StampPolygon { request: StampPolygon },
    StampDisc { request: StampDisc },
    StampHexagon { request: StampHexagon },
    ApplyBrush { request: ApplyBrush },
    RunAutomaton { request: RunAutomaton },
}

/// Operation-specific result retained alongside a staged proposal.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ProposalOperationResult {
    Mutation,
    Operator { result: OperatorResult },
}

/// `aether.kit.world.propose` — stage one bounded terrain mutation.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.propose")]
pub struct Propose {
    pub operation: ProposalOperation,
}

/// `aether.kit.world.commit_proposal` — atomically install a fresh proposal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.world.commit_proposal")]
pub struct CommitProposal {
    pub proposal_id: ProposalId,
}

/// `aether.kit.world.discard_proposal` — drop a fresh or stale proposal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.world.discard_proposal")]
pub struct DiscardProposal {
    pub proposal_id: ProposalId,
}

/// `aether.kit.world.set_proposal_preview` — select or clear the rendered proposal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.kit.world.set_proposal_preview")]
pub struct SetProposalPreview {
    pub proposal_id: Option<ProposalId>,
}

/// Observable rejection from the proposal lifecycle.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ProposalError {
    ProposalIdExhausted,
    /// The component session already retains its maximum proposal count.
    StagedProposalLimitReached,
    NoTouchedChunks {
        operation_result: ProposalOperationResult,
    },
    UnknownProposal {
        proposal_id: ProposalId,
    },
    StaleProposal {
        proposal_id: ProposalId,
        proposed_at_revision: u64,
        committed_revision: u64,
    },
}

/// Reply shared by every proposal lifecycle request.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[kind(name = "aether.kit.world.proposal_result")]
pub enum ProposalResult {
    Staged { proposal_id: ProposalId, operation_result: ProposalOperationResult, digest: ProposalDigest },
    Committed { proposal_id: ProposalId, digest: ProposalDigest },
    Discarded { proposal_id: ProposalId },
    PreviewSet { active_proposal_id: Option<ProposalId>, digest: Option<ProposalDigest> },
    Rejected { error: ProposalError },
}

/// `aether.kit.world.set_cell_heights` — stamp one cell's `SUB × SUB` height
/// deltas, the single-cell live-paint counterpart to `set_chunk`'s
/// whole-plane `height_points` write and the height sibling of
/// `set_cell_points`. `deltas` carries up to [`SUBCELLS_PER_CELL`] `i16`
/// octimeter offsets in `z*SUB + x` subcell order; a short vector leaves the
/// cell's remaining points at [`HEIGHT_POINT_INHERIT`] (an empty vector
/// clears the cell back to no relief). Each delta lifts (or drops) its
/// subcell off the cell's `height` — real, standable relief the height
/// pipeline resolves one stride down.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_cell_heights")]
pub struct SetCellHeights {
    pub x: i32,
    pub z: i32,
    pub deltas: Vec<i16>,
}

impl SetCellHeights {
    /// The cell this stamp targets.
    #[must_use]
    pub fn cell(&self) -> CellPos {
        CellPos { x: self.x, z: self.z }
    }
}

/// `aether.kit.world.set_region` — register a region in the table under a
/// 1-based `region_id`, giving the underlay cascade a default material to
/// fall through to. `default_material` and `cliff_material` are raw
/// `Material` bytes; a `cliff_material` byte that decodes to `Void` or
/// nothing falls back to Stone (a cliff face always wears something).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.set_region")]
pub struct SetRegion {
    pub region_id: u32,
    pub name: String,
    pub default_material: u8,
    pub cliff_material: u8,
}

impl SetRegion {
    /// The [`Region`] this registers, decoding `default_material`
    /// (unknown → `Void`) and `cliff_material` (unknown or `Void` →
    /// Stone).
    #[must_use]
    pub fn into_region(self) -> Region {
        Region {
            name: self.name,
            default_material: Material::from_u8_or_void(self.default_material),
            cliff_material: cliff_material_from_u8(self.cliff_material),
        }
    }
}

/// `aether.kit.world.load` — load a serialized world from the substrate's
/// I/O surface (ADR-0041 namespace + path). The bytes are the
/// [`World::to_bytes`] format; a decode failure keeps the prior world.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.world.load")]
pub struct WorldLoad {
    pub namespace: String,
    pub path: String,
}
