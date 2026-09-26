//! `environment.merge`: the toolchain directory placed in the base userland,
//! and what the result declares (ADR-0237 decision 3).
//!
//! The merge takes the base tree and the whole toolchain image as
//! [`MergeInput`]. It selects the image's one directory under
//! `usr/local/rustup/toolchains/`, cites it at the same path in the base, drops
//! Docker's init-layer placeholders, and returns an [`Environment`] whose
//! `platform`, `provides`, `tools` and `env` are derived from entry names
//! alone. It reads only `Tree` members, never a file blob, and it walks only
//! the few directories on the paths it names: every subtree it does not touch
//! keeps its digest.

mod clean;
mod graft;
mod input;
mod toolchain;

use aether_bloomery_kinds::{Detail, Mode, Name, Node, Refusal};
use aether_bloomery_program::{Env, Program, Sync, program};
use aether_workspace::{Environment, Provides};

pub use input::MergeInput;

/// Places the toolchain directory in the base userland and declares the
/// environment it makes.
///
/// Pure: the result depends only on the two cited trees. Every refusal names
/// the in-tree path it refuses.
pub struct EnvironmentMerge;

#[program]
impl Program for EnvironmentMerge {
    const NAME: &'static str = "environment.merge";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str =
        "Place the toolchain directory in the base userland and declare the environment it makes.";
    type Input = MergeInput;
    type Result = Environment;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let toolchain = toolchain::select(*env, input.toolchain)?;
        let base = env.injected(input.base)?;
        let cleaned = clean::clean(env, &base)?;
        let root = graft::graft(
            env,
            cleaned,
            &names(toolchain::TOOLCHAINS)?,
            toolchain.name,
            Node::Directory(toolchain.tree),
        )?;

        Ok(Environment {
            root: env.stage_encoded(&root)?,
            platform: toolchain.platform,
            provides: Provides { rust: Some(toolchain.rust) },
            tools: toolchain.tools,
            env: toolchain.env,
        })
    }
}

/// A refusal carrying `reason`, which names the path and the rule.
fn refused(reason: impl AsRef<str>) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}

/// One entry name the merge spells out itself.
fn name(entry: &str) -> Result<Name, Refusal> {
    Name::new(entry).map_err(|error| refused(format!("entry name {entry:?}: {error}")))
}

/// The entry names of a `/`-separated in-tree path.
fn names(path: &str) -> Result<Vec<Name>, Refusal> {
    path.split('/').map(name).collect()
}
