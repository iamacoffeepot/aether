//! `aether-kit` — the gameplay-systems layer.
//!
//! Reusable game-building actors that run on the substrate. Each system is
//! one module under the crate root that co-locates the actor with its own
//! `kinds` submodule (the mail shapes peers send it) and whatever support
//! files it needs — the crate is guest code all the way down, so there is
//! no data/runtime split, just one module per actor:
//!
//! - [`locomotion::Locomotion`] — tile-grid movement on a fixed-point
//!   ground plane; the module **entry**, so a bare `load` of `aether_kit.wasm`
//!   instantiates it. Its wire kinds live in [`locomotion`].
//! - [`camera::CameraComponent`] — the multi-camera driver, selected by the
//!   `aether_kit@aether.camera` export (ADR-0096). Its `aether.camera.*`
//!   driver kinds live in [`camera`].
//! - [`mesh::MeshViewer`] — loads a `.dsl` / `.obj` mesh file and replays it
//!   to the render sink, selected by the `aether_kit@aether.mesh_viewer`
//!   export. Its `aether.mesh.load` kind lives in [`mesh`].
//! - [`world::WorldView`] — meshes the chunked world plane stack into the
//!   keyed-quilt gouache grammar, selected by the `aether_kit@aether.world`
//!   export. The [`world`] module also holds the `World` / `Chunk` /
//!   `Material` data layer and the `aether.kit.world.*` wire kinds it meshes.
//! - [`mover::WorldMover`] — the input-driven body that walks the painted
//!   world, selected by the `aether_kit@aether.kit.mover` export. Its
//!   `aether.kit.mover.teleport` placement kind lives in [`mover`].
//! - [`widget::Widget`] — the widget-compositing node (ADR-0117), selected
//!   by the `aether_kit@aether.kit.widget` export, with the reference
//!   [`widget::WidgetPanel`] and the concrete [`widget::set`] widgets. The
//!   widget-compositing vocabulary lives in [`widget`] and the visual tokens
//!   in [`widget::theme`].
//!
//! `export!` (below) packs the actors into one cdylib (ADR-0096 multi-actor
//! module); the entry type is listed first, and the FFI shims it emits are
//! wasm32-only and inert in a host rlib, so the integration tests link the
//! same artifact.
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
pub mod locomotion;
pub mod mesh;
pub mod mover;
pub mod widget;
pub mod world;

pub use locomotion::{Preview, SetGranularity, SetWalkable, Teleport};
pub use mover::MoverTeleport;
pub use widget::theme::{SetTheme, Theme, WidgetState};
pub use widget::{
    ButtonClicked, ButtonConfig, ChildrenChanged, Collect, FocusGained, FocusLost, LabelConfig,
    MembershipEntry, PanelConfig, RadioConfig, RadioSelected, SliderChanged, SliderConfig,
    TextCommitted, TextFieldConfig, WidgetChildSpec, WidgetConfig, WidgetDrawItem, WidgetDrawList,
    WidgetFrame, WidgetKind,
};
pub use world::{
    CELLS_PER_CHUNK, CELLS_PER_CHUNK_AREA, CHUNK_BITS, CellPos, Chunk, ChunkPos,
    HEIGHT_POINT_INHERIT, HEIGHT_POINTS_PER_CHUNK, Material, Region, SetCellHeights, SetCellPoints,
    SetChunk, SetMaterialStyle, SetRegion, SetSmoothingProfile, SetViewMode, SetWaterPlane,
    SmoothingProfile, ViewMode, WaterPlane, World, WorldDecodeError, WorldLoad,
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
// each loaded independently — so it is deliberately DEFAULTLESS per ADR-0138:
// the bare `export!(…)` (no `entry =`) designates no bare-load entry, and
// every consumer loads a specific actor by its `module@actor` selector. A
// `load` with no export selector against the kit is a hard error naming the
// exports, not an instantiation of whichever actor happens to sit first. The
// `behavior` feature (ADR-0137, issue 2687) appends `aether-behavior`'s
// `BehaviorHost` so the panel's `WidgetKind::BehaviorHost` arm can spawn it
// by tag; the two invocations are cfg-exclusive, keeping the ordinary kit
// build's exported set (and its `aether.kinds` section) unchanged.
#[cfg(not(feature = "behavior"))]
aether_actor::export!(
    locomotion::Locomotion,
    camera::CameraComponent,
    mesh::MeshViewer,
    world::WorldView,
    mover::WorldMover,
    widget::Widget,
    widget::set::SliderWidget,
    widget::set::TextFieldWidget,
    widget::set::RadioGroupWidget,
    widget::set::ButtonWidget,
    widget::set::LabelWidget,
    widget::WidgetPanel
);

#[cfg(feature = "behavior")]
aether_actor::export!(
    locomotion::Locomotion,
    camera::CameraComponent,
    mesh::MeshViewer,
    world::WorldView,
    mover::WorldMover,
    widget::Widget,
    widget::set::SliderWidget,
    widget::set::TextFieldWidget,
    widget::set::RadioGroupWidget,
    widget::set::ButtonWidget,
    widget::set::LabelWidget,
    widget::WidgetPanel,
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
