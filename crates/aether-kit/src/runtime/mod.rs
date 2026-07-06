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
//! - [`world_view::WorldView`] — meshes the chunked world plane stack
//!   ([`crate::world`]) into corner-blended ground quads and
//!   marching-squares overlay contours and replays them to the render
//!   sink each frame, selected by the `aether_kit@aether.world` export.
//!   Its `aether.kit.world.*` kinds live in [`crate::world`]; the pure
//!   mesher it drives lives in [`mesher`].
//!
//! `export!(Locomotion, CameraComponent, MeshViewer, WorldView)` lists
//! the entry first; the macro emits the wasm32 FFI shims and the
//! `aether.kinds` custom section for all four actors.

pub mod camera;
pub mod locomotion;
pub mod mesh_viewer;
pub mod mesher;
pub mod world_view;

pub use camera::CameraComponent;
pub use locomotion::Locomotion;
pub use mesh_viewer::MeshViewer;
pub use mesher::mesh_chunk;
pub use world_view::WorldView;

// `arena` (the hazard-field builder) keys its fixed-size scratch on the
// locomotion grid dimensions; keep them reachable at the `runtime`
// module root where `arena` imports them.
pub(crate) use locomotion::{GRID_H, GRID_W};

aether_actor::export!(Locomotion, CameraComponent, MeshViewer, WorldView);
