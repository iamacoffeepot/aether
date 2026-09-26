//! The guest invocation seam for a program that asks the workspace for one run: start it, answer the captured run in
//! place, and poll to the invocation's end.

use std::error::Error;

use aether_bloomery_kinds::{ClosureArtifact, Digest, EncodedArtifact, Invoke, Invoked, ProgramName, Ref, Tree};
use aether_bloomery_program::{AsyncProgram, Pending, PollResult, Started, start_async};
use aether_data::{Cites, Kind, Storage};
use aether_workspace::{Outcome, RunResult, StepOutcome, ToolName, ToolRecord, TreePath};

/// Start `P` over `input`, answer its one captured run with `reply`, and poll to the invocation's end.
///
/// The closure carries only the encoded input: the program reads nothing but its input.
pub fn answer<P: AsyncProgram>(input: &P::Input, reply: &RunResult) -> Result<Invoked, Box<dyn Error>> {
    let encoded = EncodedArtifact::new(input)?;
    let closure = ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec());
    let invoke = Invoke::new(7, ProgramName::new(P::NAME)?, encoded.digest(), vec![closure]);

    let Started::Live { mut session, waiting: Some(Pending::Send(pending)) } = start_async::<P>(invoke) else {
        return Err("expected the first poll to capture the workspace run".into());
    };
    session.fulfill_send(&pending, RunResult::ID, reply.encode_into_bytes());
    match session.poll() {
        PollResult::Finished(invoked) => Ok(invoked),
        other => Err(format!("expected the invocation to finish, got {other:?}").into()),
    }
}

/// The fixed tree every answered run reports as `/work` after its last step.
pub fn output_tree() -> Ref<Tree> {
    Ref::from_digest(Digest::from_bytes([4; 32]))
}

/// An outcome of one cargo step that exited `exit_code`, with distinct stdout and stderr, over [`output_tree`].
pub fn one_step(exit_code: Option<i32>) -> Result<RunResult, Box<dyn Error>> {
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
    Ok(RunResult::Ok(Outcome { steps: vec![step], tree: output_tree() }))
}

/// Assert that `invoked` completed with `expected` as its result and its one staged artifact.
pub fn completed_with<R: Storage + Clone + Cites>(invoked: Invoked, expected: &R) -> Result<(), Box<dyn Error>> {
    let expected = EncodedArtifact::new(expected)?;
    let Invoked::Completed { seq: 7, result, staged } = invoked else {
        return Err(format!("expected Completed, got {invoked:?}").into());
    };
    assert_eq!((result, staged), (expected.digest(), vec![expected]), "the result cites without staging");
    Ok(())
}
