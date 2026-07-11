//! `aether-kit` — the gameplay-systems layer.
//!
//! Reusable game-building actors that run on the substrate. Each system is
//! one module under the crate root that co-locates the actor with its own
//! `kinds` submodule (the mail shapes peers send it) and whatever support
//! files it needs — the crate is guest code all the way down, so there is
//! no data/runtime split, just one module per actor:
//!
//! - [`camera::CameraComponent`] — the multi-camera driver, selected by the
//!   `aether_kit@aether.kit.camera` export (ADR-0096). Its `aether.kit.camera.*`
//!   driver kinds live in [`camera`].
//! - [`camera::controller::CameraController`] — a keyboard driver that steers a
//!   peer [`camera::CameraComponent`] (WASD / arrows / zoom), selected by the
//!   `aether_kit@aether.kit.camera-controller` export. Its
//!   `aether.kit.camera-controller.config` init-config lives in
//!   [`camera::controller`].
//! - [`client::PlayerClient`] — the outbound player-session and authoritative
//!   presentation actor, selected by the `aether_kit@aether.kit.client`
//!   export. Its `aether.kit.client.config` init-config lives in [`client`].
//! - [`console::ConsoleOverlay`] — a primitive-rendered developer console
//!   overlay, selected by the `aether_kit@aether.kit.console` export. Its
//!   config and extension command vocabulary live in [`console`].
//! - [`mesh::MeshViewer`] — loads a `.dsl` / `.obj` mesh file and replays it
//!   to the render sink, selected by the `aether_kit@aether.kit.mesh`
//!   export. Its `aether.kit.mesh.load` kind lives in [`mesh`].
//! - [`mark::MarkBook`] — owns revisioned terrain annotations with stable ids,
//!   selected by the `aether_kit@aether.kit.mark` export. Its CRUD and
//!   snapshot vocabulary lives in [`mark`].
//! - [`terra::TerraEditor`] — owns an ordered terrain-mark
//!   selection and correlated semantic commands, selected by the
//!   `aether_kit@aether.kit.terra` export.
//! - [`world::WorldView`] — meshes the chunked world plane stack into a
//!   flat-color marching-squares base render, selected by the
//!   `aether_kit@aether.kit.world` export. The [`world`] module also holds
//!   the `World` / `Chunk` / `Material` data layer and the
//!   `aether.kit.world.*` wire kinds it meshes.
//! - [`mover::WorldMover`] — the input-driven body that walks the painted
//!   world, selected by the `aether_kit@aether.kit.mover` export. Its
//!   `aether.kit.mover.teleport` placement kind lives in [`mover`].
//! - [`TurnSim`] — the deterministic fixed-tick reference simulation,
//!   selected by the `aether_kit@aether.kit.sim` export. Its tick-native
//!   intent, trajectory, summary, and catch-up vocabulary lives in [`sim`].
//! - [`widget::Widget`] — the widget-compositing node (ADR-0117), selected
//!   by the `aether_kit@aether.kit.widget` export, with the reference
//!   [`widget::WidgetPanel`] and the concrete [`widget::set`] widgets. The
//!   widget-compositing vocabulary lives in [`widget`] and the visual tokens
//!   in [`widget::theme`].
//! - [`EditorShell`] — the input-only arbiter between independently
//!   rooted editor regions (ADR-0141).
//! - [`TerrainWorkbench`] — the peer-first terrain annotation assembly,
//!   selected by the `aether_kit@aether.kit.workbench` export.
//!
//! `export!` (below) packs the actors into one cdylib (ADR-0096 multi-actor
//! module); the explicit entry type is the bare-load target, and the FFI
//! shims it emits are wasm32-only and inert in a host rlib, so the integration
//! tests link the same artifact.
//!
//! # Units
//!
//! Positions are fixed-point integers, so the simulation is bit-exact
//! across machines — the precondition for server authority and
//! deterministic replay. The ground plane is the world XZ plane (Y up);
//! one tile is one real-world meter, subdivided into 256 **octimeters**
//! (the minimum movement quantum, ≈ 3.9 mm).
//!
//! - [`OCTIMETERS_PER_TILE`] = 256 — `1 tile = 1 m = 256 octimeters`.
//! - The **coarse tile** an octimeter position sits on is `pos >>`
//!   [`TILE_BITS`] — a shift, never a divide, because the subdivision is
//!   a power of two. The coarse tile is the unit for occupancy and
//!   blocking; octimeters are the unit for smooth movement.

extern crate alloc;

pub mod camera;
pub mod client;
pub mod console;
pub mod mark;
pub mod mesh;
pub mod mover;
pub mod sim;
pub mod terra;
pub mod widget;
pub mod workbench;
pub mod world;

pub use client::{PlayerClient, PlayerClientConfig};
pub use console::{
    ConsoleCommandInvoked, ConsoleCommandOutput, ConsoleConfig, ConsoleTheme, RegisterConsoleCommand,
    UnregisterConsoleCommand,
};
pub use mark::{
    Mark, MarkCreate, MarkCreateResult, MarkDelete, MarkDeleteResult, MarkGeometry, MarkGet, MarkGetResult, MarkId,
    MarkList, MarkListResult, MarkMutationError, MarkRef, MarkUpdate, MarkUpdateResult, SavedMarks,
};
pub use mover::{MoverConfig, MoverTeleport};
pub use sim::{
    CellPosition, EntityState, GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn, StateSummary,
    TickBundle, TrajectoryEvent, TrajectoryKind, TurnSim,
};
pub use terra::{
    ClearTerraSelection, CreateTerraMark, DeleteTerraSelection, MoveTerraSelection, RelabelTerraSelection,
    SetTerraSelection, TerraCommandResult, TerraConfig, TerraError, TerraQuery, TerraQueryResult, ToggleTerraSelection,
    WorldDelta,
};
pub use widget::theme::{SetTheme, Theme, ThemeState};
pub use widget::{
    ButtonClicked, ButtonConfig, ChildrenChanged, Collect, EditorConfig, EditorKeyChord, EditorRegionRect, EditorShell,
    FocusGained, FocusLost, HoverGained, HoverLost, ImageConfig, ImageFit, LabelConfig, MembershipEntry,
    NumericChanged, NumericConfig, PanelConfig, RadioConfig, RadioSelected, RegionInputLanes, RegionSpec, ScrollConfig,
    ScrollDelta, ScrollExtent, ScrollOffset, ScrollOutcome, ScrollResidual, SegmentedConfig, SegmentedSelected,
    SetWidgetState, SliderChanged, SliderConfig, TextAreaConfig, TextCommitted, TextFieldConfig, ToggleChanged,
    ToggleConfig, VirtualListConfig, VirtualListSelected, WidgetChildSpec, WidgetClipRect, WidgetConfig,
    WidgetControlState, WidgetDrawItem, WidgetDrawList, WidgetFrame, WidgetKind, WidgetStateChanged, WidgetValidation,
};
pub use workbench::{
    TerrainToolPanel, TerrainViewport, TerrainWorkbench, WorkbenchCamera, WorkbenchConfig, WorkbenchControl,
    WorkbenchDraftState, WorkbenchFailure, WorkbenchInitialSettings, WorkbenchLayout, WorkbenchMarkMode,
    WorkbenchOperator, WorkbenchPanelSettings, WorkbenchProposalState, WorkbenchQuery, WorkbenchQueryResult,
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
// every listed actor. The kit is a subsystem library — a grab-bag of
// unrelated actors (camera, mesh viewer, world mesher, mover, widget set)
// each loaded independently — so ADR-0138's defaultless policy still governs
// every actor except the one explicitly named entry. `console::ConsoleOverlay`
// is the kit's narrow bare-load target; all other actors stay selector-only by
// `module@actor` selector, never by list position. The `behavior` feature
// (ADR-0137, issue 2687) appends `aether-behavior`'s `BehaviorHost` so the
// panel's `WidgetKind::BehaviorHost` arm can spawn it by tag; the two
// invocations are cfg-exclusive, keeping the ordinary kit build's exported set
// (and its `aether.kinds` section) unchanged.
#[cfg(not(feature = "behavior"))]
aether_actor::export!(
    entry = console::ConsoleOverlay,
    camera::CameraComponent,
    camera::controller::CameraController,
    PlayerClient,
    mark::MarkBook,
    mesh::MeshViewer,
    terra::TerraEditor,
    world::WorldView,
    mover::WorldMover,
    TurnSim,
    widget::Widget,
    widget::ScrollWidget,
    widget::set::SliderWidget,
    widget::set::TextFieldWidget,
    widget::set::TextAreaWidget,
    widget::set::RadioGroupWidget,
    widget::set::ButtonWidget,
    widget::set::LabelWidget,
    widget::set::ImageWidget,
    widget::set::VirtualListWidget,
    widget::set::ToggleWidget,
    widget::set::SegmentedWidget,
    widget::set::NumericWidget,
    EditorShell,
    widget::WidgetPanel,
    TerrainToolPanel,
    TerrainViewport,
    TerrainWorkbench
);

#[cfg(feature = "behavior")]
aether_actor::export!(
    entry = console::ConsoleOverlay,
    camera::CameraComponent,
    camera::controller::CameraController,
    PlayerClient,
    mark::MarkBook,
    mesh::MeshViewer,
    terra::TerraEditor,
    world::WorldView,
    mover::WorldMover,
    TurnSim,
    widget::Widget,
    widget::ScrollWidget,
    widget::set::SliderWidget,
    widget::set::TextFieldWidget,
    widget::set::TextAreaWidget,
    widget::set::RadioGroupWidget,
    widget::set::ButtonWidget,
    widget::set::LabelWidget,
    widget::set::ImageWidget,
    widget::set::VirtualListWidget,
    widget::set::ToggleWidget,
    widget::set::SegmentedWidget,
    widget::set::NumericWidget,
    EditorShell,
    widget::WidgetPanel,
    TerrainToolPanel,
    TerrainViewport,
    TerrainWorkbench,
    aether_behavior::BehaviorHost
);

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
