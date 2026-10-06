//! `aether-kit` — the common standalone actors (camera, mesh viewer, asset bundle).
//!
//! Reusable game-building actors that run on the substrate. Each system is
//! one module under the crate root that co-locates the actor with its own
//! `kinds` submodule (the mail shapes peers send it) and whatever support
//! files it needs — the crate is guest code all the way down, so there is
//! no data/runtime split, just one module per actor:
//!
//! - [`camera::CameraComponent`] — one camera per instance, at
//!   `aether.kit.camera:<key>`, selected by the `aether_kit@aether.kit.camera`
//!   export (ADR-0096). It publishes its view to whoever subscribed, the
//!   renderer included; its `aether.kit.camera.*` kinds live in [`camera`].
//! - [`camera::controller::CameraController`] — a keyboard driver that steers
//!   one [`camera::CameraComponent`] instance (WASD / arrows / zoom), selected
//!   by the `aether_kit@aether.kit.camera-controller` export. Its
//!   `aether.kit.camera-controller.config` init-config lives in
//!   [`camera::controller`].
//! - [`mesh::MeshViewer`] — loads a `.dsl` / `.obj` mesh file and replays it
//!   to the render sink, selected by the `aether_kit@aether.kit.mesh`
//!   export. Its `aether.kit.mesh.load` kind and its config, which names the
//!   camera whose eye its outlines face, live in [`mesh`].
//! - [`bundle::BundleComponent`] — the reference asset bundle (ADR-0163 §4):
//!   carries a tile in a wasm custom section, makes it an engine resident in
//!   the load window, draws it every frame, and destroys it symmetrically on
//!   teardown. Selected by the `aether_kit@aether.kit.bundle` export;
//!   it has no driver kinds, so no `kinds` submodule.
//!
//! The terrain-authoring stack — the mark / world / terra / mover actors and
//! their `CellPos` / `WorldPoint` / octimeter position vocabulary — was
//! extracted to the sibling `aether-kit-terrain` crate
//! (iamacoffeepot/aether#3951), the reference game-loop pair (`TurnSim` /
//! `PlayerClient`) to `aether-kit-sim` (iamacoffeepot/aether#3952), and the
//! terrain-annotation workbench that composed the widget / console / terrain
//! layers to `aether-kit-workbench` (iamacoffeepot/aether#3953). All three
//! sibling crates have since been shelved out of the workspace (dormant, not
//! in active use; git history holds them), so kit depends on none of them.
//!
//! `export!` (below) packs the actors into one cdylib (ADR-0096 multi-actor
//! module); it exports several actors, so every load names its export, and the FFI
//! shims it emits are wasm32-only and inert in a host rlib, so the integration
//! tests link the same artifact.

#![forbid(unsafe_code)]

extern crate alloc;

pub mod bundle;
pub mod camera;
pub mod mesh;

// A cdylib carries one `export!` (the shared init/receive FFI entry); the
// macro emits the wasm32 FFI shims and the `aether.kinds` custom section for
// every listed actor. The kit is a subsystem library — a grab-bag of
// independently loaded actors (camera, camera controller, mesh viewer, asset
// bundle) with no unselected entry (a load or spawn names its namespace, ADR-0241
// §9), so this holds for every export here: each is reached by `module@actor` selector, never by
// list position. The widget set and its `EditorShell` arbiter live in
// `aether-widget`, exported from its own cdylib, not here (the shelved
// terrain / sim / workbench siblings likewise owned their own cdylibs while
// they were in the workspace).
aether_actor::export!(
    public =
        [camera::CameraComponent, camera::controller::CameraController, mesh::MeshViewer, bundle::BundleComponent,]
);
