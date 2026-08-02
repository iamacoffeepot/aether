//! The ADR-0149 Actions executor-port demo (#3500 step 5 coverage).
//!
//! Drives a synthetic work order through the executor cap shell against the
//! adapter's in-process `FakeGithub` — the "Done" the issue names, end to end
//! with no token and no network:
//!
//! - `submit` shapes the work order into the wrapper's `command` / `ref` /
//!   `nonce` dispatch inputs and dispatches, returning the nonce-as-handle;
//! - `inspect` reads `Unknown` until a run resolves for the nonce, then maps the
//!   run's lifecycle onto `ExecutionStatus`;
//! - `cancel` resolves the run by nonce and drives it to `Cancelled`;
//! - `stream_evidence` returns only the nonce's uploaded artifacts.
//!
//! The shell mounts a `FakeGithub`-backed `ActionsExecutor` directly (production
//! mounts a `ReqwestGithub`-backed one), then drives the four port ops through
//! the shell — mirroring how `source_demo` builds its `GitSource`.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aether_bloomery::{
    Budget, Conclusion, Digest, ExecutionStatus, NetworkProfile, Nonce, ReasoningEffort, ResolvedModel, Transformation,
    WorkHandle, WorkOrder,
};
use aether_bloomery_github::testing::FakeGithub;
use aether_bloomery_github::{ActionsExecutor, Artifact, LaneWorkflows, RunConclusion, RunStatus};
use aether_chassis_bloomery::bloomery::ExecutorShell;

const WORKFLOW: &str = "bloomery-transform.yml";
const MODEL_WORKFLOW: &str = "bloomery-transform-model.yml";
const PINNED_REF: &str = "refs/heads/main";

fn work_order(nonce: &str) -> WorkOrder {
    WorkOrder {
        transformation: Transformation {
            command: "construct.implement".to_owned(),
            inputs: Vec::new(),
            checkout: Digest::from_bytes([0xC0; 32]),
            outputs: vec!["patch".to_owned()],
            image: "iama/construct:1".to_owned(),
            limits: Budget::default(),
            network: NetworkProfile::None,
            description: None,
            // A model lane carries the profile the host resolved onto it; the
            // backend refuses to dispatch one that does not.
            model: Some(ResolvedModel { model: "claude-opus-5".to_owned(), effort: ReasoningEffort::High }),
        },
        nonce: Nonce(nonce.to_owned()),
    }
}

// Mount a fake-backed executor behind the shell, returning the shell and the
// fake handle to seed/inspect the Actions surface.
fn demo() -> (ExecutorShell, FakeGithub) {
    let fake = FakeGithub::new();
    // Seed the order's checkout correspondence so `submit` resolves the subject.
    fake.seed_git_object(&Digest::from_bytes([0xC0; 32]));
    let lanes = LaneWorkflows { mechanical: WORKFLOW.to_owned(), model: MODEL_WORKFLOW.to_owned() };
    let backend = ActionsExecutor::new(fake.clone(), Arc::new(fake.clone()), lanes, PINNED_REF);
    (ExecutorShell::new(Arc::new(backend)), fake)
}

#[test]
fn a_synthetic_work_order_submits_inspects_and_cancels_through_the_shell() {
    let (shell, fake) = demo();

    // Submit shapes the dispatch inputs and returns the nonce-as-handle.
    let handle = shell.submit(&work_order("n-demo")).unwrap();
    assert_eq!(handle, WorkHandle::new(Nonce("n-demo".to_owned())));
    assert_eq!(fake.dispatched_nonces(), vec!["n-demo".to_owned()]);
    let inputs = fake.dispatched_inputs("n-demo").unwrap();
    assert_eq!(inputs.get("command").map(String::as_str), Some("construct.implement"));
    assert_eq!(inputs.get("nonce").map(String::as_str), Some("n-demo"));

    // No run yet: the clean Unknown, not an error.
    assert_eq!(shell.inspect(&handle).unwrap(), ExecutionStatus::Unknown);

    // A run appears and progresses; inspect maps its lifecycle.
    let _ = fake.seed_run("n-demo", RunStatus::InProgress, None);
    assert_eq!(shell.inspect(&handle).unwrap(), ExecutionStatus::Running);

    // Cancel resolves the run by nonce and drives it to Cancelled.
    shell.cancel(&handle).unwrap();
    assert_eq!(shell.inspect(&handle).unwrap(), ExecutionStatus::Cancelled);
}

#[test]
fn stream_evidence_returns_the_completed_runs_nonce_scoped_artifacts() {
    let (shell, fake) = demo();
    let handle = shell.submit(&work_order("n-ev")).unwrap();
    let run_id = fake.seed_run("n-ev", RunStatus::Completed, Some(RunConclusion::Success));
    fake.seed_run_artifacts(
        run_id,
        vec![
            Artifact { id: 11, name: "evidence-n-ev-report".to_owned(), size_bytes: 42 },
            Artifact { id: 12, name: "leftover-n-elsewhere".to_owned(), size_bytes: 7 },
        ],
    );

    assert_eq!(shell.inspect(&handle).unwrap(), ExecutionStatus::Completed { conclusion: Conclusion::Success });

    let evidence = shell.stream_evidence(&handle).unwrap();
    assert_eq!(evidence.len(), 1, "only the nonce's artifact, the unrelated one filtered out");
    assert_eq!(evidence[0].artifact_id, 11);
    assert_eq!(evidence[0].name, "evidence-n-ev-report");
    assert_eq!(evidence[0].nonce, Nonce("n-ev".to_owned()));
}
