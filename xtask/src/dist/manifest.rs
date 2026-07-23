use std::collections::BTreeMap;

use serde::Serialize;

/// `dist/manifest.json` schema. Paths are relative to `dist/` and use
/// forward slashes so the manifest is stable across host OSes.
#[derive(Serialize)]
pub(super) struct Manifest {
    /// Triple the component wasm is built for (`wasm32-unknown-unknown`).
    pub(super) target: String,
    /// Cargo profile the tree was built under (`debug` / `release`).
    pub(super) profile: String,
    /// Wasm stem → `components/<stem>.wasm`.
    pub(super) components: BTreeMap<String, String>,
    /// Chassis bin name → `bin/<name>`. Empty under `--no-bins`.
    pub(super) chassis: BTreeMap<String, String>,
}
