//! The proofs driven through the guest invocation seam: each workspace run answered in place with a canned
//! outcome, and every step output it then reads answered from a local store.

use std::error::Error;

use aether_bloomery_kinds::{
    ClosureArtifact, EncodedArtifact, Invoke, Invoked, ProgramName, ReadArtifactResult, Refusal, Tree,
};
use aether_bloomery_program::{AsyncProgram, Edited, Pending, PollResult, Program, Started, start_async, tooled};
use aether_bloomery_workspace::{
    Outcome, RunError, RunResult, RustToolchain, StepOutcome, ToolName, ToolRecord, TreePath,
};
use aether_bloomery_workspace_programs::proof::{
    ClippyArgs, ClippyProof, ProofBound, ProofVerdict, TestArgs, TestEnv, TestProof,
};
use aether_data::{Cites, Digest, Kind, Ref, Storage, Utf8Text};

type TestResult = Result<(), Box<dyn Error>>;

/// The tree the run reports as `/work` after its last step: the source with fmt's fixes.
fn output_tree() -> Ref<Tree> {
    Ref::from_digest(Digest::from_bytes([4; 32]))
}

/// A canned step that exited `exit_code` with `stdout` and `stderr`, and those two outputs as stored artifacts.
fn step(
    exit_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(StepOutcome, [EncodedArtifact; 2]), Box<dyn Error>> {
    let outcome = StepOutcome {
        exit_code,
        stdout: Ref::of_bytes(stdout),
        stderr: Ref::of_bytes(stderr),
        tool: ToolRecord {
            name: ToolName::new("cargo")?,
            path: TreePath::new("usr/local/rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/bin/cargo")?,
            file: Ref::of_bytes(b"cargo"),
        },
    };
    Ok((outcome, [EncodedArtifact::opaque_bytes(stdout), EncodedArtifact::opaque_bytes(stderr)]))
}

/// `artifact` as the closure member or read reply that stores it.
fn member(artifact: &EncodedArtifact) -> ClosureArtifact {
    let (kind, bytes, _) = artifact.clone().into_parts();
    ClosureArtifact::new(kind, bytes)
}

/// The stored value of `artifact`.
fn value<K: Storage>(artifact: &EncodedArtifact) -> Result<K, Box<dyn Error>> {
    Ok(K::decode_storage(&member(artifact).load(artifact.digest())?)?.value)
}

/// `value` as the artifact that stores it.
fn encoded<K: Storage + Clone + Cites>(value: &K) -> Result<EncodedArtifact, Box<dyn Error>> {
    Ok(EncodedArtifact::new(value)?)
}

/// Run the clippy proof with `reply`, answering every read from `store`.
fn answer(reply: &RunResult, store: &[EncodedArtifact]) -> Result<Invoked, Box<dyn Error>> {
    answer_as::<ClippyProof, _>(&ClippyArgs, reply, store)
}

/// Run `P` over a fixed source with its bound in the closure, answer its run with `reply`, and answer every read
/// from `store`.
fn answer_as<P: AsyncProgram + Program, A: Storage + Clone + Cites>(
    args: &A,
    reply: &RunResult,
    store: &[EncodedArtifact],
) -> Result<Invoked, Box<dyn Error>> {
    let bound = encoded(&ProofBound::new(
        Ref::from_digest(Digest::from_bytes([2; 32])),
        Ref::from_digest(Digest::from_bytes([3; 32])),
        TestEnv::default(),
    ))?;
    let args = encoded(args)?;
    let source = Ref::from_digest(Digest::from_bytes([1; 32]));
    let input = encoded(&tooled(
        source,
        Ref::<A>::from_digest(args.digest()).erase(),
        Ref::<ProofBound>::from_digest(bound.digest()).erase(),
    ))?;
    let closure = [&input, &args, &bound].into_iter().map(member).collect();
    let invoke = Invoke::new(7, ProgramName::new(P::NAME)?, input.digest(), closure);

    let Started::Live { mut session, waiting: Some(Pending::Send(pending)) } = start_async::<P>(invoke) else {
        return Err("expected the first poll to capture the workspace run".into());
    };
    session.fulfill_send(&pending, RunResult::ID, reply.encode_into_bytes());
    loop {
        match session.poll() {
            PollResult::Finished(invoked) => return Ok(invoked),
            PollResult::NeedArtifact(pending) => {
                let stored = store.iter().find(|artifact| artifact.digest() == pending.digest);
                let reply = stored.map_or_else(
                    || ReadArtifactResult::Missing { digest: pending.digest },
                    |artifact| ReadArtifactResult::Found { artifact: member(artifact) },
                );
                session.fulfill(pending, reply);
            }
            other => return Err(format!("expected the proof to finish or read, got {other:?}").into()),
        }
    }
}

/// A completed proof: its result, the verdict the result cites, and every artifact it staged.
struct Proved {
    edited: Edited<ProofVerdict>,
    verdict: ProofVerdict,
    staged: Vec<EncodedArtifact>,
}

/// Run `P` over `steps`, answering reads from their outputs, and read back its result and verdict.
fn proved_as<P: AsyncProgram + Program, A: Storage + Clone + Cites>(
    args: &A,
    steps: Vec<(StepOutcome, [EncodedArtifact; 2])>,
) -> Result<Proved, Box<dyn Error>> {
    let (steps, outputs): (Vec<_>, Vec<_>) = steps.into_iter().unzip();
    let store = outputs.concat();
    let invoked = answer_as::<P, A>(args, &RunResult::Ok(Outcome { steps, tree: output_tree() }), &store)?;
    let Invoked::Completed { seq: 7, result, staged } = invoked else {
        return Err(format!("expected Completed, got {invoked:?}").into());
    };
    let staged_value = |digest: Digest| {
        staged.iter().find(|artifact| artifact.digest() == digest).ok_or("the result cites a staged artifact")
    };
    let edited: Edited<ProofVerdict> = value(staged_value(result)?)?;
    let verdict: ProofVerdict = value(staged_value(edited.detail().digest())?)?;
    Ok(Proved { edited, verdict, staged })
}

/// Run the clippy proof over `steps`, answering reads from their outputs, and read back its result and verdict.
fn proved(steps: Vec<(StepOutcome, [EncodedArtifact; 2])>) -> Result<Proved, Box<dyn Error>> {
    proved_as::<ClippyProof, _>(&ClippyArgs, steps)
}

/// Run the test proof over `steps`, answering reads from their outputs, and read back its result and verdict.
fn proved_test(steps: Vec<(StepOutcome, [EncodedArtifact; 2])>) -> Result<Proved, Box<dyn Error>> {
    proved_as::<TestProof, _>(&TestArgs, steps)
}

/// The staged text `diagnostics` cites.
fn text(staged: &[EncodedArtifact], diagnostics: Ref<Utf8Text>) -> Result<String, Box<dyn Error>> {
    let artifact =
        staged.iter().find(|artifact| artifact.digest() == diagnostics.digest()).ok_or("the diagnostics are staged")?;
    Ok(String::from_utf8(member(artifact).load(artifact.digest())?)?)
}

#[test]
fn a_pass_carries_the_formatted_tree_and_names_what_fmt_rewrote() -> TestResult {
    // Catches a result that keeps the input tree instead of fmt's output, a rewritten file the model is not told
    // to re-read, container paths shown to the model, and an inverted verdict on a clean run.
    let steps = vec![step(Some(0), b"/work/src/lib.rs\n/work/src/main.rs\n", b"")?, step(Some(0), b"", b"")?];
    let Proved { edited, verdict, .. } = proved(steps)?;

    assert_eq!(edited.tree(), output_tree());
    assert_eq!(verdict, ProofVerdict::Passed);
    assert_eq!(
        edited.summary(),
        "`cargo clippy` passed. `cargo fmt` rewrote src/lib.rs, src/main.rs; read a rewritten file \
         again before you edit it."
    );
    Ok(())
}

#[test]
fn a_clippy_failure_reports_each_rendered_diagnostic_once_or_cargos_own_error() -> TestResult {
    // Catches a failure recorded as a pass, raw JSON or artifact lines shown to the model, the duplicate each
    // `--all-targets` warning prints, diagnostics the verdict does not cite, and a cargo error, such as a crate
    // missing from the vendor tree, dropped because it rendered no message.
    let warning = r#"{"reason":"compiler-message","message":{"rendered":"error: unused variable: `x`\n"}}"#;
    let artifact = r#"{"reason":"compiler-artifact","filenames":[]}"#;
    let stdout = [warning, artifact, warning, r#"{"reason":"build-finished","success":false}"#].join("\n");
    let steps = vec![step(Some(0), b"", b"")?, step(Some(101), stdout.as_bytes(), b"error: could not compile\n")?];
    let Proved { edited, verdict, staged } = proved(steps)?;

    let ProofVerdict::Failed { diagnostics } = verdict else {
        return Err(format!("expected a failure, got {verdict:?}").into());
    };
    assert_eq!(text(&staged, diagnostics)?, "error: unused variable: `x`");
    assert_eq!(edited.summary(), "`cargo clippy` failed. `cargo fmt` changed nothing.\n\nerror: unused variable: `x`");

    let missing = "error: no matching package named `rand` found";
    let steps = vec![step(Some(0), b"", b"")?, step(Some(101), b"", format!("{missing}\n").as_bytes())?];
    let Proved { edited, verdict, staged } = proved(steps)?;
    let ProofVerdict::Failed { diagnostics } = verdict else {
        return Err(format!("expected a failure, got {verdict:?}").into());
    };
    assert_eq!(text(&staged, diagnostics)?, missing);
    assert!(edited.summary().ends_with(missing), "{}", edited.summary());
    Ok(())
}

#[test]
fn a_test_step_failure_reports_each_failing_tests_output_and_the_failed_targets() -> TestResult {
    // Catches a test failure recorded as a pass, raw JSON shown to the model, a dropped failure block, the
    // failed-target list missing or after the blocks where the cap could cut it, and a summary that says
    // `cargo clippy` instead of `cargo test`.
    let block = "---- failing stdout ----\nthread 'failing' panicked at src/lib.rs:1\n";
    let passing = "test result: ok. 1 passed; 0 failed;";
    let failing = "test result: FAILED. 0 passed; 1 failed;";
    let json = r#"{"reason":"compiler-artifact","filenames":[]}"#;
    let stdout = format!(
        "     Running unittests src/lib.rs (passing)\n{passing}\n{json}\n     Running unittests src/lib.rs (failing)\n{block}{failing}\n"
    );
    let stderr = "error: 1 target failed:\n    `-p aether-demo --test demo`\n";
    let steps = vec![step(Some(0), b"", b"")?, step(Some(101), stdout.as_bytes(), stderr.as_bytes())?];
    let Proved { edited, verdict, staged } = proved_test(steps)?;

    let ProofVerdict::Failed { diagnostics } = verdict else {
        return Err(format!("expected a failure, got {verdict:?}").into());
    };
    let reported = text(&staged, diagnostics)?;
    assert!(reported.starts_with("error: 1 target failed:"), "{reported}");
    for want in ["`-p aether-demo --test demo`", "---- failing stdout ----", "test result: FAILED"] {
        assert!(reported.contains(want), "the diagnostics hold {want:?}: {reported}");
    }
    assert!(!reported.contains("compiler-artifact"), "{reported}");
    assert_eq!(edited.tree(), output_tree());
    assert!(edited.summary().starts_with("`cargo test` failed. `cargo fmt` changed nothing."), "{}", edited.summary());
    Ok(())
}

#[test]
fn a_fmt_failure_reports_rustfmts_error_and_says_clippy_did_not_run() -> TestResult {
    // Catches a tree rustfmt cannot parse reported as a clippy result, or as a refusal that would end the session.
    let error = "error: expected item, found `}`";
    let Proved { edited, verdict, staged } = proved(vec![step(Some(1), b"", format!("{error}\n").as_bytes())?])?;

    let ProofVerdict::Failed { diagnostics } = verdict else {
        return Err(format!("expected a failure, got {verdict:?}").into());
    };
    assert_eq!(text(&staged, diagnostics)?, error);
    assert_eq!(
        edited.summary(),
        format!("`cargo fmt` failed, so `cargo clippy` did not run. `cargo fmt` changed nothing.\n\n{error}")
    );
    Ok(())
}

#[test]
fn a_workspace_refusal_is_the_programs_refusal_not_a_failed_proof() -> TestResult {
    // Catches a refusal about the environment recorded as a failed proof of the tree.
    let wants = RustToolchain::new("1.97.1", vec!["clippy".to_owned()], Vec::new())?;
    let mismatch =
        aether_bloomery_workspace::Refusal::ToolchainMismatch { tree_wants: wants, environment_provides: None };
    let invoked = answer(&RunResult::Err(RunError::Refused(mismatch)), &[])?;
    let Invoked::Refused { seq: 7, refusal: Refusal::Refused { reason } } = invoked else {
        return Err(format!("expected the program's own refusal, got {invoked:?}").into());
    };
    assert!(reason.as_str().contains("ToolchainMismatch"), "{reason:?}");
    Ok(())
}

#[test]
fn a_run_that_answers_steps_the_two_step_run_cannot_end_with_is_refused() -> TestResult {
    // Catches an executor's malformed outcome recorded as a proof about the tree: no step, or a passing fmt with
    // no clippy after it.
    let (passed, outputs) = step(Some(0), b"", b"")?;
    for steps in [Vec::new(), vec![passed]] {
        let invoked = answer(&RunResult::Ok(Outcome { steps, tree: output_tree() }), &outputs)?;
        assert!(matches!(invoked, Invoked::Refused { seq: 7, refusal: Refusal::Refused { .. } }), "{invoked:?}");
    }
    Ok(())
}
