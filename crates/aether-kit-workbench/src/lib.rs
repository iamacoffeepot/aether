//! `aether-kit-workbench` — the terrain-annotation workbench assembly.
//!
//! The one module extracted from `aether-kit` (iamacoffeepot/aether#3953) that
//! genuinely spans widget + terrain + console, so it becomes the top-of-stack
//! assembly crate. [`TerrainWorkbench`] arbitrates its editor regions with
//! `EditorShell` and populates them with the `set::*` widgets from
//! [`aether-kit-widget`](aether_kit_widget), inline-spawns [`ConsoleOverlay`]
//! from [`aether-kit-commons`](aether_kit_commons), and drives the mark / terra / world
//! vocabulary from [`aether-kit-terrain`](aether_kit_terrain). Its two
//! sub-actors — [`TerrainToolPanel`] and [`TerrainViewport`] — are inline
//! children of the workbench root.
//!
//! `export!` (below) packs the three workbench actors into one cdylib (ADR-0096
//! multi-actor module) with no default entry — each is selector-only
//! (`aether_kit_workbench@aether.kit.workbench`, ADR-0138) — and the FFI shims
//! it emits are wasm32-only and inert in a host rlib, so the integration test
//! links the same artifact. Nothing embeds this crate, so it declares no
//! `library` feature: its `export!` owns the sole entry surface in the
//! assembled wasm module.
//!
//! [`ConsoleOverlay`]: aether_kit_commons::console::ConsoleOverlay

extern crate alloc;

mod workbench;

pub use workbench::{
    TerrainToolPanel, TerrainViewport, TerrainWorkbench, WorkbenchCamera, WorkbenchConfig, WorkbenchControl,
    WorkbenchDraftState, WorkbenchFailure, WorkbenchInitialSettings, WorkbenchLayout, WorkbenchMarkMode,
    WorkbenchOperator, WorkbenchPanelSettings, WorkbenchProposalState, WorkbenchQuery, WorkbenchQueryResult,
};

// A cdylib carries one `export!` (the shared init/receive FFI entry); the macro
// emits the wasm32 FFI shims and the `aether.kinds` custom section for every
// listed actor, all behind the macro's own `cfg(not(feature = "library"))`
// gate. The workbench has no bare-load target — each actor is loaded by its
// `aether_kit_workbench@actor` selector (ADR-0138 defaultless policy), so
// `export!` names no default. Nothing embeds this crate, so no `library`
// feature is declared: this is the sole live `export!` in the assembled wasm
// module.
aether_actor::export!(TerrainWorkbench, TerrainToolPanel, TerrainViewport);
