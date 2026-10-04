//! `proof.clippy` and `proof.test`: format a source tree and prove it in a
//! published environment, as tools a Muse session calls (ADR-0237 decisions
//! 2, 4, 7, and 12; ADR-0234 decision 10).
//!
//! Each proof is a tool: its input is `Tooled<A, ProofBound>`, the
//! session's current tree, the empty arguments the model writes, and the
//! [`ProofBound`] the session binds, the environment, the vendor tree, the
//! cargo config, and the test env. It asks the workspace for one run of two
//! steps over the tree at `/work`, with the network off, the vendor tree at
//! `/vendor`, and the bound's cargo config at `/.cargo`, which replaces
//! crates.io with the vendor tree for every cargo in the run, including the
//! ones a test spawns: `cargo fmt --all` writes its fixes and lists the
//! files it rewrote, then the proof's cargo step. The workspace stops after
//! the first step that exits other than 0.
//!
//! Both programs share one body, differing only in a private description: the cargo step's argv, whether the step takes the
//! bound's test env, the summary name, and the function that turns the
//! failed step's stdout and stderr into diagnostics.
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

mod config;
mod input;
mod report;
mod result;
mod run;

use aether_bloomery_kinds::{Detail, Mode, Ref, Refusal, Tree};
use aether_bloomery_program::{Async, Edited, Env, Program, Tooled, Workspace, program};
use aether_bloomery_workspace::StepOutcome;

pub use config::cargo_config_artifacts;
pub use input::{ClippyArgs, MAX_TEST_ENV, ProofBound, TestArgs, TestEnv, TestEnvError};
pub use report::DIAGNOSTICS_MAX_BYTES;
pub use result::ProofVerdict;

use report::{Ended, clippy_diagnostics, test_diagnostics};

/// What differs between the proofs: the cargo step's argv, whether it takes
/// the bound's test env, the summary name, and how the failed step's outputs
/// become diagnostics.
struct Proof {
    cargo: &'static [&'static str],
    takes_test_env: bool,
    step: &'static str,
    diagnostics: fn(&[u8], &[u8]) -> String,
}

/// The clippy proof's description: CI's lint command, no test env, so its run
/// key and warm layer never vary with the bound's test env.
const CLIPPY: Proof =
    Proof { cargo: &run::CLIPPY_ARGS, takes_test_env: false, step: "cargo clippy", diagnostics: clippy_diagnostics };

/// The test proof's description: the workspace tests with the bound's test
/// env on the test step only.
const TEST: Proof =
    Proof { cargo: &run::TEST_ARGS, takes_test_env: true, step: "cargo test", diagnostics: test_diagnostics };

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

    async fn run(input: Self::Input, env: &mut Env<Async>, workspace: Workspace) -> Result<Self::Result, Refusal> {
        prove(&CLIPPY, input.tree(), input.bound(), &mut env, workspace).await
    }
}

/// The `proof.test` program.
pub struct TestProof;

/// Formats the whole workspace with `cargo fmt` and runs its tests with the
/// session's test env.
///
/// Takes no arguments: pass `{}`. The proof always runs the whole workspace
/// in the session's tree. The result is the tree after fmt, whether the tests
/// passed, and, when they failed, each failing test's output and the targets
/// that failed.
#[program]
impl Program for TestProof {
    const NAME: &'static str = "proof.test";
    const MODE: Mode = Mode::Sampled;
    const INTENT: &'static str = "Format a source tree and run its workspace tests in an environment.";
    type Input = Tooled<TestArgs, ProofBound>;
    type Result = Edited<ProofVerdict>;

    async fn run(input: Self::Input, env: &mut Env<Async>, workspace: Workspace) -> Result<Self::Result, Refusal> {
        prove(&TEST, input.tree(), input.bound(), &mut env, workspace).await
    }
}

/// Run `proof` over `tree` bound to `bound`: fmt first, then the proof's
/// cargo step, answering the formatted tree with its verdict.
async fn prove(
    proof: &Proof,
    tree: Ref<Tree>,
    bound: Ref<ProofBound>,
    env: &mut Env<Async>,
    mut workspace: Workspace,
) -> Result<Edited<ProofVerdict>, Refusal> {
    let bound = env.read(bound).await?;
    let extra = if proof.takes_test_env {
        bound.test_env().as_slice()
    } else {
        &[]
    };
    let outcome = workspace
        .run(run::request(tree, &bound, proof.cargo, extra)?)
        .await?
        .map_err(|refusal| refused(format!("the workspace refused the run: {refusal:?}")))?;

    let (ended, fmt, failed) = ended(&outcome.steps)?;
    let formatted = report::formatted(&env.read_payload(fmt.stdout.erase()).await?);
    let diagnostics = match failed {
        Some(step) => Some((proof.diagnostics)(
            &env.read_payload(step.stdout.erase()).await?,
            &env.read_payload(step.stderr.erase()).await?,
        )),
        None => None,
    };

    let summary = report::summary(&ended, proof.step, &formatted, diagnostics.as_deref());
    let verdict = diagnostics
        .map_or(ProofVerdict::Passed, |diagnostics| ProofVerdict::Failed { diagnostics: env.stage_text(&diagnostics) });
    Ok(Edited::new(outcome.tree, summary, env.stage_encoded(&verdict)?))
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
        ([fmt, cargo], [true, false]) => Ok((Ended::CargoFailed, fmt, Some(cargo))),
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
