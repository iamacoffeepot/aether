//! `aether-kit-terrain` — the terrain-authoring stack.
//!
//! The mark / world / terra / mover actors extracted from `aether-kit`
//! (iamacoffeepot/aether#3951). Each system is one module under the crate
//! root that co-locates the actor with its own `kinds` submodule (the mail
//! shapes peers send it) and support files — guest code all the way down, so
//! there is no data/runtime split, just one module per actor:
//!
//! - [`mark::MarkBook`] — owns revisioned terrain annotations with stable ids,
//!   selected by the `aether_kit_terrain@aether.kit.mark` export. Its CRUD and
//!   snapshot vocabulary lives in [`mark`].
//! - [`terra::TerraEditor`] — owns an ordered terrain-mark selection and
//!   correlated semantic commands, selected by the
//!   `aether_kit_terrain@aether.kit.terra` export.
//! - [`world::WorldView`] — meshes the chunked world plane stack into a
//!   flat-color marching-squares base render, selected by the
//!   `aether_kit_terrain@aether.kit.world` export. The [`world`] module also
//!   holds the `World` / `Chunk` / `Material` data layer and the
//!   `aether.kit.world.*` wire kinds it meshes.
//! - [`mover::WorldMover`] — the input-driven body that walks the painted
//!   world, selected by the `aether_kit_terrain@aether.kit.mover` export. Its
//!   `aether.kit.mover.teleport` placement kind lives in [`mover`].
//!
//! `export!` (below) packs the actors into one cdylib (ADR-0096 multi-actor
//! module) with no default entry — each is selector-only (`module@actor`,
//! ADR-0138) — and the FFI shims it emits are wasm32-only and inert in a host
//! rlib, so the integration tests link the same artifact.
//!
//! # Units
//!
//! Positions are fixed-point integers, so the simulation is bit-exact across
//! machines — the precondition for server authority and deterministic replay.
//! The ground plane is the world XZ plane (Y up); one tile is one real-world
//! meter, subdivided into 256 **octimeters** (the minimum movement quantum,
//! ≈ 3.9 mm).
//!
//! - [`OCTIMETERS_PER_TILE`] = 256 — `1 tile = 1 m = 256 octimeters`.
//! - The **coarse tile** an octimeter position sits on is `pos >>`
//!   [`TILE_BITS`] — a shift, never a divide, because the subdivision is a
//!   power of two. The coarse tile is the unit for occupancy and blocking;
//!   octimeters are the unit for smooth movement.

extern crate alloc;

pub mod mark;
pub mod mover;
pub mod terra;
pub mod world;

pub use mark::{
    Mark, MarkCreate, MarkCreateResult, MarkDelete, MarkDeleteResult, MarkGeometry, MarkGet, MarkGetResult, MarkId,
    MarkList, MarkListResult, MarkMutationError, MarkRef, MarkUpdate, MarkUpdateResult, SavedMarks,
};
pub use mover::{MoverConfig, MoverTeleport};
pub use terra::{
    ClearTerraSelection, CreateTerraMark, DeleteTerraSelection, MoveTerraSelection, RelabelTerraSelection,
    SetTerraSelection, TerraCommandResult, TerraConfig, TerraError, TerraQuery, TerraQueryResult, ToggleTerraSelection,
    WorldDelta,
};
pub use world::{
    ApplyBrush, AutomatonRule, BrushParameters, CELLS_PER_CHUNK, CELLS_PER_CHUNK_AREA, CHUNK_BITS, CellPos, Chunk,
    ChunkPos, CommitProposal, DiscardProposal, HEIGHT_POINT_INHERIT, HEIGHT_POINTS_PER_CHUNK, MARK_OVERLAY_COLOR,
    MARK_OVERLAY_LIFT_METERS, MARK_PATH_HALF_WIDTH_METERS, MARK_POINT_RADIUS_METERS, MARK_SELECTED_COLOR,
    MARK_SELECTED_HALF_WIDTH_METERS, MARK_SELECTED_HANDLE_RADIUS_METERS, MAX_MARK_OVERLAY_TRIANGLES,
    MAX_MARK_OVERLAY_VERTICES, MAX_TERRAIN_PICK_DISTANCE_METERS, Material, OperatorBudget, OperatorCell, OperatorChunk,
    OperatorError, OperatorResult, OperatorStats, PickTerrain, PickTerrainResult, ProposalBounds, ProposalDigest,
    ProposalError, ProposalId, ProposalOperation, ProposalOperationResult, ProposalResult, Propose, Region,
    RunAutomaton, SetCellHeights, SetCellPoints, SetChunk, SetMarkOverlaySelection, SetMarkOverlaySelectionResult,
    SetMarkOverlayVisibility, SetMarkOverlayVisibilityResult, SetProposalPreview, SetRegion, SmoothingProfile,
    StampDisc, StampHexagon, StampPolygon, TERRAIN_PICK_EPSILON_METERS, TERRAIN_PICK_REFINEMENT_STEPS,
    TERRAIN_PICK_STEP_METERS, TerrainPickError, TerrainRay, TerrainSurface, TerrainSurfaceHit, WaterPlane, World,
    WorldDecodeError, WorldDirection, WorldLoad, WorldPoint, WorldPositionMeters,
};

/// Octimeters per tile: `1 tile = 1 meter = 256 octimeters`.
pub const OCTIMETERS_PER_TILE: i32 = 256;

/// Right-shift an octimeter coordinate by this to derive its coarse
/// tile (`2^8 = 256` octimeters per tile).
pub const TILE_BITS: u32 = 8;

// A cdylib carries one `export!` (the shared init/receive FFI entry); the
// macro emits the wasm32 FFI shims and the `aether.kinds` custom section for
// every listed actor, all behind the macro's own `cfg(not(feature =
// "library"))` gate. The terrain crate has no bare-load target — each actor is
// loaded by its `module@actor` selector (ADR-0138 defaultless policy), so the
// `export!` names no default. An embedding cdylib enables this crate's
// `library` feature to strip the entry surface (see the Cargo.toml comment).
aether_actor::export!(world::WorldView, mark::MarkBook, terra::TerraEditor, mover::WorldMover);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coarse_tile_is_a_shift() {
        // A position 1.5 tiles along sits on coarse tile 1.
        let pos = OCTIMETERS_PER_TILE + OCTIMETERS_PER_TILE / 2;
        assert_eq!(pos >> TILE_BITS, 1);
    }
}
