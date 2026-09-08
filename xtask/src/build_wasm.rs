//! `cargo xtask build-wasm` — cross-build the component wasm every
//! scenario test's `require_wasm` gate looks for.
//!
//! The gate names this command in the failure text it panics with when
//! the artifact is missing (issue #5724), so the command has to exist
//! and has to build exactly what CI's `Pre-build component wasm for
//! scenario tests` step builds. It is therefore [`crate::dist`]
//! with the chassis binaries dropped, not a second discovery path: one
//! structural sweep (`inventory::discover_components`), one build loop,
//! one freshness stamp. What it adds over spelling `dist --no-bins` out
//! is a name the panic can say and the reader can run without first
//! learning that the wasm arrives as a side effect of assembling an
//! artifact tree called `dist`.

use anyhow::Result;
use clap::Args;

use crate::cargo::Profile;
use crate::dist::{self, DistArgs};

#[derive(Args)]
pub struct BuildWasmArgs {
    /// Cargo profile to cross-build.
    #[arg(long, value_enum, default_value_t = Profile::Debug)]
    profile: Profile,
}

pub fn run(args: &BuildWasmArgs) -> Result<()> {
    dist::run(&DistArgs::wasm_only(args.profile))
}
