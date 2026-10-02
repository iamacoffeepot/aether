//! `proof.clippy`: format a source tree and check it with clippy in a
//! published environment, as a tool a Muse session calls (ADR-0237 decisions
//! 2, 4, 7, and 12; ADR-0234 decision 10).
//!
//! The proof is a tool: its input is `Tooled<ClippyArgs, ProofBound>`, the
//! session's current tree, the empty arguments the model writes, and the
//! [`ProofBound`] the session binds, the environment and the vendor tree. It
//! asks the workspace for one run of two steps over the tree at `/work`,
//! with the network off and the vendor tree at `/vendor` replacing crates.io:
//! `cargo fmt --all` writes its fixes and lists the files it rewrote, then
//! `cargo clippy --workspace --all-targets --offline` lints with JSON
//! diagnostics. The workspace stops after the first step that exits other
//! than 0.
//!
//! The answer is an `Edited<ProofVerdict>`: the run's output tree, which
//! holds fmt's fixes and any `Cargo.lock` update and becomes the session's
//! tree; a summary naming the verdict, the files fmt rewrote, and the failed
//! step's diagnostics, capped with the cut marked; and the [`ProofVerdict`],
//! which cites those diagnostics. A failed proof is a result, never a
//! refusal: the model reads it and fixes its code. A workspace refusal is the
//! program's own refusal, never a failed proof of the tree; an exhausted or
//! failed run ends the invocation through the binding, and the program never
//! sees it.

mod input;
mod report;
mod result;
mod run;

use aether_bloomery_kinds::{Detail, Mode, Refusal};
use aether_bloomery_program::{Async, Edited, Env, Program, Tooled, Workspace, program};
use aether_bloomery_workspace::StepOutcome;

pub use input::{ClippyArgs, ProofBound};
pub use report::DIAGNOSTICS_MAX_BYTES;
pub use result::ProofVerdict;

use report::Ended;

/// The `proof.clippy` program.
pub struct ClippyProof;

/// Formats the whole workspace with `cargo fmt` and checks it with
/// `cargo clippy --workspace --all-targets -- -D warnings`.
///
/// Takes no arguments: pass `{}`. Formatting never fails the proof: fmt
/// rewrites the files and the result names them, so read a rewritten file
/// again before editing it. The result is the tree after fmt, whether clippy
/// passed, and, when it failed, the diagnostics to fix.
#[program]
impl Program for ClippyProof {
    const NAME: &'static str = "proof.clippy";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Format a source tree and check it with clippy in an environment.";
    type Input = Tooled<ClippyArgs, ProofBound>;
    type Result = Edited<ProofVerdict>;

    async fn run(input: Self::Input, env: &mut Env<Async>, mut workspace: Workspace) -> Result<Self::Result, Refusal> {
        let bound = env.read(input.bound()).await?;
        let outcome = workspace
            .run(run::request(input.tree(), &bound)?)
            .await?
            .map_err(|refusal| refused(format!("the workspace refused the run: {refusal:?}")))?;

        let (ended, fmt, failed) = ended(&outcome.steps)?;
        let formatted = report::formatted(&env.read_payload(fmt.stdout.erase()).await?);
        let diagnostics = match failed {
            Some(step) => {
                let rendered = report::rendered(&env.read_payload(step.stdout.erase()).await?);
                Some(report::diagnostics(&rendered, &env.read_payload(step.stderr.erase()).await?))
            }
            None => None,
        };

        let summary = report::summary(&ended, &formatted, diagnostics.as_deref());
        let verdict = diagnostics.map_or(ProofVerdict::Passed, |diagnostics| ProofVerdict::Failed {
            diagnostics: env.stage_text(&diagnostics),
        });
        Ok(Edited::new(outcome.tree, summary, env.stage_encoded(&verdict)?))
    }
}

/// How the run's steps ended, its fmt step, and the step that failed it, if
/// one did.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the count when the steps are not what the
/// two-step run can answer: no step, a failed fmt followed by another step,
/// or a passing fmt alone. That is the executor's error, not a proof about
/// the tree.
fn ended(steps: &[StepOutcome]) -> Result<(Ended, &StepOutcome, Option<&StepOutcome>), Refusal> {
    let exited_zero: Vec<bool> = steps.iter().map(passed).collect();
    match (steps, exited_zero.as_slice()) {
        ([fmt], [false]) => Ok((Ended::FmtFailed, fmt, Some(fmt))),
        ([fmt, _], [true, true]) => Ok((Ended::Passed, fmt, None)),
        ([fmt, clippy], [true, false]) => Ok((Ended::ClippyFailed, fmt, Some(clippy))),
        _ => Err(refused(format!("the two-step run answered {} steps it cannot end with", steps.len()))),
    }
}

/// Whether `step` exited 0.
fn passed(step: &StepOutcome) -> bool {
    step.exit_code == Some(0)
}

/// A refusal carrying `reason`.
fn refused(reason: impl AsRef<str>) -> Refusal {
    Refusal::Refused { reason: Detail::new(reason) }
}
