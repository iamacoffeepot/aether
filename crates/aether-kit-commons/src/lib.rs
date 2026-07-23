//! `aether-kit-commons` — the common standalone actors (camera, console, mesh viewer).
//!
//! Reusable game-building actors that run on the substrate. Each system is
//! one module under the crate root that co-locates the actor with its own
//! `kinds` submodule (the mail shapes peers send it) and whatever support
//! files it needs — the crate is guest code all the way down, so there is
//! no data/runtime split, just one module per actor:
//!
//! - [`camera::CameraComponent`] — the multi-camera driver, selected by the
//!   `aether_kit_commons@aether.kit.camera` export (ADR-0096). Its `aether.kit.camera.*`
//!   driver kinds live in [`camera`].
//! - [`camera::controller::CameraController`] — a keyboard driver that steers a
//!   peer [`camera::CameraComponent`] (WASD / arrows / zoom), selected by the
//!   `aether_kit_commons@aether.kit.camera-controller` export. Its
//!   `aether.kit.camera-controller.config` init-config lives in
//!   [`camera::controller`].
//! - [`console::ConsoleOverlay`] — a primitive-rendered developer console
//!   overlay, selected by the `aether_kit_commons@aether.kit.console` export. Its
//!   config and extension command vocabulary live in [`console`].
//! - [`mesh::MeshViewer`] — loads a `.dsl` / `.obj` mesh file and replays it
//!   to the render sink, selected by the `aether_kit_commons@aether.kit.mesh`
//!   export. Its `aether.kit.mesh.load` kind lives in [`mesh`].
//! - [`bundle::BundleComponent`] — the reference asset bundle (ADR-0163 §4):
//!   carries a tile in a wasm custom section, makes it an engine resident in
//!   the load window, draws it every frame, and destroys it symmetrically on
//!   teardown. Selected by the `aether_kit_commons@aether.kit.bundle` export;
//!   it has no driver kinds, so no `kinds` submodule.
//!
//! The terrain-authoring stack — the mark / world / terra / mover actors and
//! their `CellPos` / `WorldPoint` / octimeter position vocabulary — was
//! extracted to the sibling `aether-kit-terrain` crate
//! (iamacoffeepot/aether#3951), the reference game-loop pair (`TurnSim` /
//! `PlayerClient`) to `aether-kit-sim` (iamacoffeepot/aether#3952), and the
//! terrain-annotation workbench that composed the widget / console / terrain
//! layers to `aether-kit-workbench` (iamacoffeepot/aether#3953). None of the
//! remaining kit systems consume those crates, so kit no longer depends on any
//! of them at the crate level.
//!
//! `export!` (below) packs the actors into one cdylib (ADR-0096 multi-actor
//! module); the explicit entry type is the bare-load target, and the FFI
//! shims it emits are wasm32-only and inert in a host rlib, so the integration
//! tests link the same artifact.

extern crate alloc;

pub mod bundle;
pub mod camera;
pub mod console;
pub mod mesh;

pub use console::{
    ConsoleCommandInvoked, ConsoleCommandOutput, ConsoleConfig, ConsoleTheme, RegisterConsoleCommand,
    UnregisterConsoleCommand,
};

// A cdylib carries one `export!` (the shared init/receive FFI entry); the
// macro emits the wasm32 FFI shims and the `aether.kinds` custom section for
// every listed actor. The kit is a subsystem library — a grab-bag of
// unrelated actors (camera, mesh viewer, console) each loaded independently
// — so ADR-0138's defaultless policy still governs every actor except the one
// explicitly named as the default. `console::ConsoleOverlay` is the kit's
// narrow bare-load target; all other actors stay selector-only by
// `module@actor` selector, never by list position. The widget set and its
// `EditorShell` arbiter live in `aether-kit-widget`, the terrain stack (mark /
// world / terra / mover) in `aether-kit-terrain`, the reference game-loop pair
// (`TurnSim` / `PlayerClient`) in `aether-kit-sim`, and the workbench that
// composed the widget / terrain / console layers in `aether-kit-workbench`;
// each is exported from its own cdylib, not here.
aether_actor::export!(
    default = console::ConsoleOverlay,
    camera::CameraComponent,
    camera::controller::CameraController,
    mesh::MeshViewer,
    bundle::BundleComponent
);
