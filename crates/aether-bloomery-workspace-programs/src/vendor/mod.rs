//! `vendor.cargo`: the `cargo vendor` tree for a source tree's `Cargo.lock`,
//! fetched in a published environment (ADR-0237 decisions 2 and 4).
//!
//! The program asks the workspace for one step,
//! `cargo vendor --locked --manifest-path /source/Cargo.toml /work`, with the
//! network on. `Outcome::tree` is `/work` after the last step minus scratch,
//! and a mount is never read back, so the run tree is the empty tree and the
//! source is mounted read-only at `/source`: `/work` ends as exactly the
//! vendor directory, and the result cites it with no reshaping.
//!
//! **Pairing.** [`VendorResult::Vendored`]'s tree is what
//! [`crate::proof::ClippyInput::vendor`] takes when the proof's `source` has
//! the same `Cargo.lock` as this transition's `source`. The proof replaces
//! only `crates-io`, so the pairing covers registry sources only.
//!
//! **Preconditions.**
//! - The empty run tree must be stored. Every journal holding a merged
//!   environment holds it, because `environment.merge` stages an empty `dev`;
//!   when it is missing, the workspace refuses with `InputMissing` and the
//!   program refuses naming it.
//! - The source is a mount, not the run tree, so the workspace's
//!   `rust-toolchain.toml` check does not run here. The vendor layout depends
//!   on cargo, not rustc, and `proof.clippy` still runs that check over the
//!   same source.
//! - Cargo reads config from its working directory, `/work`, so a
//!   `.cargo/config.toml` in the source does not apply, as for the proof.
//!
//! The answer is a [`VendorResult`]. A workspace refusal is the program's own
//! refusal; an exhausted or failed run ends the invocation through the
//! binding, and the program never sees it.

mod input;
mod result;
mod run;

use aether_bloomery_kinds::{Detail, Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, Workspace, program};
use aether_workspace::Outcome;

pub use input::VendorInput;
pub use result::VendorResult;

/// Vendors a source tree's locked crate sources in an environment and records
/// the vendor tree.
///
/// Sampled: the tree depends on registry state and the executor, not only on
/// the cited trees.
pub struct CargoVendor;

#[program]
impl Program for CargoVendor {
    const NAME: &'static str = "vendor.cargo";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str =
        "Vendor a source tree's locked crate sources in an environment and record the vendor tree.";
    type Input = VendorInput;
    type Result = VendorResult;

    async fn run(input: Self::Input, _env: &mut Env<Async>, mut workspace: Workspace) -> Result<Self::Result, Refusal> {
        outcome(
            &workspace
                .run(run::request(&input)?)
                .await?
                .map_err(|refusal| refused(format!("the workspace refused the run: {refusal:?}")))?,
        )
    }
}

/// The answer of the run's one step: `Vendored` citing the output tree on
/// exit 0, `Failed` citing stderr otherwise.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the count when the run answered other than
/// one step: that is the executor's error, not a vendor result.
fn outcome(outcome: &Outcome) -> Result<VendorResult, Refusal> {
    match outcome.steps.as_slice() {
        [step] if step.exit_code == Some(0) => Ok(VendorResult::Vendored { tree: outcome.tree }),
        [step] => Ok(VendorResult::Failed { stderr: step.stderr }),
        other => Err(refused(format!("the one-step run answered {} steps", other.len()))),
    }
}

/// A refusal carrying `reason`.
fn refused(reason: impl AsRef<str>) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}
