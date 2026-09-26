//! `proof.clippy` driven through the guest invocation seam, with the captured workspace run answered in place.
//!
//! The closure carries only the encoded input over fixed digests: the program reads nothing but its input.

mod support;

use std::error::Error;

use aether_bloomery_kinds::{Digest, Invoked, Ref, Refusal};
use aether_bloomery_workspace_programs::proof::{ClippyInput, ClippyProof, ClippyResult};
use aether_workspace::{Outcome, RunResult, RustToolchain};

use support::{completed_with, one_step, output_tree};

/// Start the proof over fixed digests, answer its one captured run with `reply`, and poll to the invocation's end.
fn answer(reply: &RunResult) -> Result<Invoked, Box<dyn Error>> {
    let input = ClippyInput {
        source: Ref::from_digest(Digest::from_bytes([1; 32])),
        environment: Ref::from_digest(Digest::from_bytes([2; 32])),
        vendor: Ref::from_digest(Digest::from_bytes([3; 32])),
    };
    support::answer::<ClippyProof>(&input, reply)
}

#[test]
fn exit_zero_passes_citing_stderr() -> Result<(), Box<dyn Error>> {
    // Catches an inverted verdict on a clean run.
    completed_with(answer(&one_step(Some(0))?)?, &ClippyResult::Passed { stderr: Ref::of_bytes(b"stderr") })
}

#[test]
fn a_non_zero_exit_fails_citing_stderr_not_stdout() -> Result<(), Box<dyn Error>> {
    // Catches a denied lint recorded as a pass, and the wrong log cited.
    completed_with(answer(&one_step(Some(101))?)?, &ClippyResult::Failed { stderr: Ref::of_bytes(b"stderr") })
}

#[test]
fn a_workspace_refusal_is_the_programs_refusal_not_a_failed_proof() -> Result<(), Box<dyn Error>> {
    // Catches a refusal about the environment recorded as a failed proof of the tree.
    let wants = RustToolchain::new("1.97.1", vec!["clippy".to_owned()], Vec::new())?;
    let mismatch = aether_workspace::Refusal::ToolchainMismatch { tree_wants: wants, environment_provides: None };
    let invoked = answer(&RunResult::Refused(mismatch))?;
    let Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason } } = invoked else {
        return Err(format!("expected the program's own refusal, got {invoked:?}").into());
    };
    assert!(reason.as_str().contains("ToolchainMismatch"), "{reason:?}");
    Ok(())
}

#[test]
fn a_run_that_answers_no_step_is_refused() -> Result<(), Box<dyn Error>> {
    // Catches an executor's malformed outcome recorded as a proof about the tree.
    let invoked = answer(&RunResult::Ok(Outcome { steps: Vec::new(), tree: output_tree() }))?;
    assert!(matches!(invoked, Invoked::Refused { seq: 7, refusal: Refusal::Refused { .. } }), "{invoked:?}");
    Ok(())
}
