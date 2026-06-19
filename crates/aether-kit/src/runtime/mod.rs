//! The gameplay-systems runtime: the reusable actors `aether-kit`
//! packs into one cdylib (ADR-0096 multi-actor module).
//!
//! - [`Locomotion`] — tile-grid movement on a fixed-point ground plane;
//!   the module **entry**, so a bare `load` of `aether_kit.wasm`
//!   instantiates it.
//! - [`camera::CameraComponent`] — the multi-camera driver, selected by
//!   the `aether_kit@aether.camera` export. Its `aether.camera.*` driver kinds
//!   live in [`crate::camera`].
//!
//! `export!(Locomotion, CameraComponent)` lists the entry first; the
//! macro emits the wasm32 FFI shims and the `aether.kinds` custom
//! section for both actors.

pub mod camera;
pub mod locomotion;

pub use camera::CameraComponent;
pub use locomotion::Locomotion;

// `arena` (the hazard-field builder) keys its fixed-size scratch on the
// locomotion grid dimensions; keep them reachable at the `runtime`
// module root where `arena` imports them.
pub(crate) use locomotion::{GRID_H, GRID_W};

aether_actor::export!(Locomotion, CameraComponent);
