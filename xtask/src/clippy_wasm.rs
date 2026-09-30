//! `cargo xtask clippy-wasm` — clippy-lint the component wasm set on
//! `wasm32-unknown-unknown` (issue #7185).
//!
//! The workspace `Clippy gate` (`verify.clippy`) compiles on the host only,
//! so any `cfg(target_family = "wasm")` code in `aether-actor` or a guest
//! component crate is never clippy-linted. This command reuses
//! `build-wasm`'s structural discovery (`inventory::discover_components` +
//! `build_plans`) and runs one `cargo clippy` per plan through
//! `cargo::clippy_component`, the clippy twin of `build_component`'s
//! warning-denying verdict.

use anyhow::{Context, Result, bail};
use cargo_metadata::MetadataCommand;
use clap::Args;

use crate::cargo::clippy_component;
use crate::inventory::{build_plans, discover_components};

#[derive(Args)]
pub struct ClippyWasmArgs {}

pub fn run(_args: &ClippyWasmArgs) -> Result<()> {
    let metadata = MetadataCommand::new().no_deps().exec().context("run cargo metadata")?;

    let components = discover_components(&metadata);
    if components.is_empty() {
        bail!("no wasm component crates discovered (cdylib target + aether-actor dep)");
    }

    for plan in build_plans(&components) {
        clippy_component(&plan)?;
    }

    Ok(())
}
