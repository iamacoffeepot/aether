//! `proof.clippy` driven through the guest invocation seam, with the captured workspace run answered in place.
//!
//! The closure carries only the encoded input over fixed digests: the program reads nothing but its input.

use std::error::Error;

use aether_bloomery_kinds::{ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, ProgramName, Ref, Refusal};
use aether_bloomery_program::{Pending, PollResult, Program, Started, start_async};
use aether_bloomery_workspace_programs::proof::{ClippyInput, ClippyProof, ClippyResult};
use aether_data::Kind;
use aether_workspace::{Outcome, RunResult, RustToolchain, StepOutcome, ToolName, ToolRecord, TreePath};

/// Start the proof over fixed digests, answer its one captured run with `reply`, and poll to the invocation's end.
fn answer(reply: &RunResult) -> Result<Invoked, Box<dyn Error>> {
    let input = ClippyInput {
        source: Ref::from_digest(Digest::from_bytes([1; 32])),
        environment: Ref::from_digest(Digest::from_bytes([2; 32])),
        vendor: Ref::from_digest(Digest::from_bytes([3; 32])),
    };
    let encoded = EncodedArtifact::new(&input)?;
    let closure = ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec());
    let invoke = Invoke::new(7, ProgramName::new(ClippyProof::NAME)?, encoded.digest(), vec![closure]);

    let Started::Live { mut session, waiting: Some(Pending::Send(pending)) } = start_async::<ClippyProof>(invoke)
    else {
        return Err("expected the first poll to capture the workspace run".into());
    };
    session.fulfill_send(&pending, RunResult::ID, reply.encode_into_bytes());
    match session.poll() {
        PollResult::Finished(invoked) => Ok(invoked),
        other => Err(format!("expected the invocation to finish, got {other:?}").into()),
    }
}

/// An outcome of one clippy step that exited `exit_code`, with distinct stdout and stderr.
fn one_step(exit_code: Option<i32>) -> Result<RunResult, Box<dyn Error>> {
    let step = StepOutcome {
        exit_code,
        stdout: Ref::of_bytes(b"stdout"),
        stderr: Ref::of_bytes(b"stderr"),
        tool: ToolRecord {
            name: ToolName::new("cargo")?,
            path: TreePath::new("usr/local/rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/bin/cargo")?,
            file: Ref::of_bytes(b"cargo"),
        },
    };
    Ok(RunResult::Ok(Outcome { steps: vec![step], tree: Ref::from_digest(Digest::from_bytes([4; 32])) }))
}

/// Assert that `invoked` completed with `expected` as its result and its one staged artifact.
fn completed_with(invoked: Invoked, expected: &ClippyResult) -> Result<(), Box<dyn Error>> {
    let expected = EncodedArtifact::new(expected)?;
    let Invoked::Completed { seq: 7, result, staged } = invoked else {
        return Err(format!("expected Completed, got {invoked:?}").into());
    };
    assert_eq!((result, staged), (expected.digest(), vec![expected]), "the result cites stderr without staging it");
    Ok(())
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
    let empty = RunResult::Ok(Outcome { steps: Vec::new(), tree: Ref::from_digest(Digest::from_bytes([4; 32])) });
    let invoked = answer(&empty)?;
    assert!(matches!(invoked, Invoked::Refused { seq: 7, refusal: Refusal::Refused { .. } }), "{invoked:?}");
    Ok(())
}
