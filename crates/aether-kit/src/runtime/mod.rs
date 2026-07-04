//! The gameplay-systems runtime: the reusable actors `aether-kit`
//! packs into one cdylib (ADR-0096 multi-actor module).
//!
//! - [`Locomotion`] — tile-grid movement on a fixed-point ground plane;
//!   the module **entry**, so a bare `load` of `aether_kit.wasm`
//!   instantiates it.
//! - [`camera::CameraComponent`] — the multi-camera driver, selected by
//!   the `aether_kit@aether.camera` export. Its `aether.camera.*` driver kinds
//!   live in [`crate::camera`].
//! - [`mesh_viewer::MeshViewer`] — loads a `.dsl` / `.obj` mesh file
//!   and replays it to the render sink each tick, selected by the
//!   `aether_kit@aether.mesh_viewer` export. Its `aether.mesh.load`
//!   kind lives in [`crate::mesh`].
//!
//! `export!(Locomotion, CameraComponent, MeshViewer)` lists the entry
//! first; the macro emits the wasm32 FFI shims and the `aether.kinds`
//! custom section for all three actors.

pub mod camera;
pub mod locomotion;
pub mod mesh_viewer;

pub use camera::CameraComponent;
pub use locomotion::Locomotion;
pub use mesh_viewer::MeshViewer;

// `arena` (the hazard-field builder) keys its fixed-size scratch on the
// locomotion grid dimensions; keep them reachable at the `runtime`
// module root where `arena` imports them.
pub(crate) use locomotion::{GRID_H, GRID_W};

aether_actor::export!(Locomotion, CameraComponent, MeshViewer);
