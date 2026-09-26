//! `proof.clippy`: whether a source tree passes clippy in a published
//! environment (ADR-0237 decisions 2, 4, and 7).
//!
//! The proof asks the workspace for one step,
//! `cargo clippy --workspace --all-targets --frozen -- -D warnings`, over the
//! source at `/work`, with the network off. The root is read-only and the
//! network is off, so crate sources are an input: [`ClippyInput::vendor`] is a
//! `cargo vendor` tree mounted at `/vendor`, and crates.io is replaced by it.
//! `vendor.cargo` produces that tree: its `Vendored.tree` is the tree the
//! field takes.
//! Without it, clippy on any tree with registry dependencies would fail at
//! resolution, and the journal would record a failed proof that says nothing
//! about the code.
//!
//! The answer is a [`ClippyResult`] citing the step's stored stderr, never a
//! copy of it. A workspace refusal is the program's own refusal, never a
//! failed proof of the tree; an exhausted or failed run ends the invocation
//! through the binding, and the program never sees it.

mod input;
mod result;
mod run;

use aether_bloomery_kinds::{Detail, Mode, Refusal};
use aether_bloomery_program::{Async, Env, Program, Workspace, program};
use aether_workspace::StepOutcome;

pub use input::ClippyInput;
pub use result::ClippyResult;

/// Runs clippy over a source tree in an environment and records the verdict.
///
/// Sampled: the verdict depends on the executor's run, not only on the cited
/// trees.
pub struct ClippyProof;

#[program]
impl Program for ClippyProof {
    const NAME: &'static str = "proof.clippy";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Run clippy over a source tree in an environment and record whether it passed.";
    type Input = ClippyInput;
    type Result = ClippyResult;

    async fn run(input: Self::Input, _env: &mut Env<Async>, mut workspace: Workspace) -> Result<Self::Result, Refusal> {
        let outcome = workspace
            .run(run::request(&input)?)
            .await?
            .map_err(|refusal| refused(format!("the workspace refused the run: {refusal:?}")))?;
        verdict(&outcome.steps)
    }
}

/// The verdict of the run's one step: `Passed` on exit 0, `Failed` otherwise.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the count when the run answered other than
/// one step: that is the executor's error, not a proof about the tree.
fn verdict(steps: &[StepOutcome]) -> Result<ClippyResult, Refusal> {
    match steps {
        [step] if step.exit_code == Some(0) => Ok(ClippyResult::Passed { stderr: step.stderr }),
        [step] => Ok(ClippyResult::Failed { stderr: step.stderr }),
        other => Err(refused(format!("the one-step run answered {} steps", other.len()))),
    }
}

/// A refusal carrying `reason`.
fn refused(reason: impl AsRef<str>) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}
