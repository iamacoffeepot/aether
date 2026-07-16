//! The Actions executor-port backend (ADR-0149 §The boundary / §Execution on
//! Actions, [#3500]).
//!
//! Implements [`ExecutorBackend`] over GitHub Actions ([`ActionsApi`]): a
//! [`submit`](ExecutorBackend::submit) shapes a fully-resolved [`WorkOrder`]
//! into the wrapper workflow's dispatch inputs and fires a `workflow_dispatch`
//! at a protected pinned ref; `inspect` / `cancel` / `stream_evidence` resolve
//! the dispatched run by the order's nonce and map onto the run + artifacts
//! API. No GitHub type crosses into a core `aether_bloomery` module — the port
//! values ([`WorkHandle`] / [`ExecutionStatus`] / [`EvidenceRef`]) are the
//! boundary.
//!
//! # The nonce is the handle
//!
//! `workflow_dispatch` answers `204 No Content` with no run id, so the durable
//! correlation key is the order's [`Nonce`]. The wrapper embeds it in the run's
//! name (`run-name:`), and this backend resolves nonce → run on demand through
//! [`ActionsApi::find_run`]. That resolution is the shared correlation contract
//! with the wrapper workflow ([#3501], a sibling): the wrapper fixes the
//! `command` / `ref` / `nonce` dispatch inputs, the run-name nonce embedding,
//! and the workflow filename; either issue can land first with the other
//! conforming.
//!
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500
//! [#3501]: https://github.com/iamacoffeepot/aether/issues/3501

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_bloomery::{Conclusion, EvidenceRef, ExecutionStatus, ExecutorBackend, Nonce, WorkHandle, WorkOrder};

use crate::client::{ActionsApi, GithubError, RunConclusion, RunStatus, WorkflowRun};

/// An executor-port fault. Its own type because the port needs an arm the value
/// vocabulary does not carry — a message asked to act on a run that does not
/// resolve for its nonce — alongside the underlying Actions transport faults.
#[derive(Debug)]
pub enum ExecutorError {
    /// The underlying Actions call failed (transport or non-2xx status).
    Github(GithubError),
    /// A `cancel` / `stream_evidence` resolved no run for the nonce — the
    /// worker was never dispatched, or has not yet appeared. (`inspect`
    /// reports the same condition as the clean [`ExecutionStatus::Unknown`],
    /// not this error, because reporting "no run yet" *is* its job.)
    NoRunForNonce(Nonce),
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Github(error) => write!(f, "actions executor backend: {error}"),
            Self::NoRunForNonce(nonce) => {
                write!(f, "actions executor backend: no run resolves for nonce `{}`", nonce.0)
            }
        }
    }
}

impl Error for ExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Github(error) => Some(error),
            Self::NoRunForNonce(_) => None,
        }
    }
}

impl From<GithubError> for ExecutorError {
    fn from(error: GithubError) -> Self {
        Self::Github(error)
    }
}

/// The Actions executor backend over an [`ActionsApi`] client. Holds the
/// dispatch target: the wrapper's `workflow_file` and the protected `git_ref`
/// it is pinned at.
pub struct ActionsExecutor<C: ActionsApi> {
    client: C,
    workflow_file: String,
    git_ref: String,
}

impl<C: ActionsApi> ActionsExecutor<C> {
    /// Build an executor over `client`, dispatching `workflow_file` at the
    /// protected `git_ref`.
    pub fn new(client: C, workflow_file: impl Into<String>, git_ref: impl Into<String>) -> Self {
        Self { client, workflow_file: workflow_file.into(), git_ref: git_ref.into() }
    }

    /// Borrow the underlying client (test introspection).
    #[must_use]
    pub const fn client(&self) -> &C {
        &self.client
    }

    // Resolve the run for a handle's nonce, or the port-specific
    // no-run-for-nonce error — the shared resolution `cancel` and
    // `stream_evidence` both need before they can act on a run.
    fn resolve(&self, handle: &WorkHandle) -> Result<WorkflowRun, ExecutorError> {
        self.client
            .find_run(&self.workflow_file, &handle.nonce.0)?
            .ok_or_else(|| ExecutorError::NoRunForNonce(handle.nonce.clone()))
    }
}

// Map a resolved run's folded lifecycle onto the port's `ExecutionStatus`.
fn map_status(run: &WorkflowRun) -> ExecutionStatus {
    match run.status {
        RunStatus::Queued => ExecutionStatus::Queued,
        RunStatus::InProgress => ExecutionStatus::Running,
        RunStatus::Completed => match run.conclusion {
            Some(RunConclusion::Cancelled) => ExecutionStatus::Cancelled,
            Some(RunConclusion::Success) => ExecutionStatus::Completed { conclusion: Conclusion::Success },
            Some(RunConclusion::Failure) => ExecutionStatus::Completed { conclusion: Conclusion::Failure },
            // A completed run always carries a conclusion in practice; a
            // missing one is anomalous, mapped to the neither-verdict Neutral
            // rather than fabricating pass/fail or conflating it with the
            // no-run-yet `Unknown`.
            Some(RunConclusion::Neutral) | None => ExecutionStatus::Completed { conclusion: Conclusion::Neutral },
        },
    }
}

impl<C: ActionsApi> ExecutorBackend for ActionsExecutor<C> {
    type Error = ExecutorError;

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error> {
        // Shape the transformation + order into the wrapper's dispatch inputs.
        // The `ref` input mirrors the protected pinned ref the dispatch runs at
        // (the wrapper checks out what it was pinned to) — the exact input set
        // is the shared contract with #3501.
        let mut inputs = BTreeMap::new();
        inputs.insert("command".to_owned(), order.transformation.command.clone());
        inputs.insert("ref".to_owned(), self.git_ref.clone());
        inputs.insert("nonce".to_owned(), order.nonce.0.clone());
        self.client.dispatch_workflow(&self.workflow_file, &self.git_ref, &inputs)?;
        Ok(WorkHandle::new(order.nonce.clone()))
    }

    fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        let run = self.client.find_run(&self.workflow_file, &handle.nonce.0)?;
        Ok(run.as_ref().map_or(ExecutionStatus::Unknown, map_status))
    }

    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error> {
        let run = self.resolve(handle)?;
        self.client.cancel_run(run.id)?;
        Ok(())
    }

    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error> {
        let run = self.resolve(handle)?;
        let artifacts = self.client.list_run_artifacts(run.id)?;
        // Filter to the order's nonce: a run's uploaded artifacts embed the
        // nonce in their name (the wrapper's convention), so evidence from an
        // unrelated concern sharing the run does not leak into this order's set.
        Ok(artifacts
            .into_iter()
            .filter(|a| a.name.contains(&handle.nonce.0))
            .map(|a| EvidenceRef {
                name: a.name,
                nonce: handle.nonce.clone(),
                artifact_id: a.id,
                size_bytes: a.size_bytes,
            })
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{
        Budget, Conclusion, ExecutionStatus, ExecutorBackend, NetworkProfile, Nonce, Transformation, WorkHandle,
        WorkOrder,
    };

    use super::{ActionsExecutor, ExecutorError};
    use crate::client::{Artifact, RunConclusion, RunStatus};
    use crate::testing::FakeGithub;

    const WORKFLOW: &str = "bloomery-transform.yml";
    const PINNED_REF: &str = "refs/heads/main";

    fn order(nonce: &str) -> WorkOrder {
        WorkOrder {
            transformation: Transformation {
                command: "verify.clippy".to_owned(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                image: "iama/verify:1".to_owned(),
                limits: Budget::default(),
                network: NetworkProfile::None,
            },
            nonce: Nonce(nonce.to_owned()),
        }
    }

    fn executor(fake: FakeGithub) -> ActionsExecutor<FakeGithub> {
        ActionsExecutor::new(fake, WORKFLOW, PINNED_REF)
    }

    #[test]
    fn submit_dispatches_the_wrapper_and_returns_the_nonce_handle() {
        let fake = FakeGithub::new();
        let handle = executor(fake.clone()).submit(&order("n-1")).unwrap();

        assert_eq!(handle, WorkHandle::new(Nonce("n-1".to_owned())));
        assert_eq!(fake.dispatched_nonces(), vec!["n-1".to_owned()]);
        assert_eq!(fake.dispatched_workflow("n-1").as_deref(), Some(WORKFLOW));
        assert_eq!(fake.dispatched_ref("n-1").as_deref(), Some(PINNED_REF));
        // The command / ref / nonce shaping is the contract with the wrapper.
        let inputs = fake.dispatched_inputs("n-1").unwrap();
        assert_eq!(inputs.get("command").map(String::as_str), Some("verify.clippy"));
        assert_eq!(inputs.get("ref").map(String::as_str), Some(PINNED_REF));
        assert_eq!(inputs.get("nonce").map(String::as_str), Some("n-1"));
    }

    #[test]
    fn inspect_is_unknown_until_a_run_resolves_then_maps_the_lifecycle() {
        let fake = FakeGithub::new();
        let exec = executor(fake.clone());
        let handle = exec.submit(&order("n-2")).unwrap();

        // Dispatched but no run seeded yet: the clean Unknown, not an error.
        assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Unknown);

        let _ = fake.seed_run("n-2", RunStatus::Queued, None);
        assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Queued);

        let _ = fake.seed_run("n-2", RunStatus::InProgress, None);
        assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Running);

        let _ = fake.seed_run("n-2", RunStatus::Completed, Some(RunConclusion::Success));
        assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Completed { conclusion: Conclusion::Success });

        let _ = fake.seed_run("n-2", RunStatus::Completed, Some(RunConclusion::Failure));
        assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Completed { conclusion: Conclusion::Failure });
    }

    #[test]
    fn cancel_resolves_the_run_and_drives_it_to_cancelled() {
        let fake = FakeGithub::new();
        let exec = executor(fake.clone());
        let handle = exec.submit(&order("n-3")).unwrap();
        let _ = fake.seed_run("n-3", RunStatus::InProgress, None);

        exec.cancel(&handle).unwrap();
        assert_eq!(exec.inspect(&handle).unwrap(), ExecutionStatus::Cancelled);
    }

    #[test]
    fn cancel_with_no_resolvable_run_is_the_no_run_error() {
        let exec = executor(FakeGithub::new());
        let handle = WorkHandle::new(Nonce("n-missing".to_owned()));
        match exec.cancel(&handle) {
            Err(ExecutorError::NoRunForNonce(nonce)) => assert_eq!(nonce, Nonce("n-missing".to_owned())),
            other => panic!("expected NoRunForNonce, got {other:?}"),
        }
    }

    #[test]
    fn stream_evidence_returns_only_the_nonces_artifacts() {
        let fake = FakeGithub::new();
        let exec = executor(fake.clone());
        let handle = exec.submit(&order("n-4")).unwrap();
        let run_id = fake.seed_run("n-4", RunStatus::Completed, Some(RunConclusion::Success));
        // The run carries this order's evidence plus an unrelated artifact; only
        // the nonce-embedding names are this order's.
        fake.seed_run_artifacts(
            run_id,
            vec![
                Artifact { id: 1, name: "evidence-n-4-log".to_owned(), size_bytes: 10 },
                Artifact { id: 2, name: "evidence-n-4-diff".to_owned(), size_bytes: 20 },
                Artifact { id: 3, name: "unrelated-n-other".to_owned(), size_bytes: 30 },
            ],
        );

        let evidence = exec.stream_evidence(&handle).unwrap();
        let ids: Vec<u64> = evidence.iter().map(|e| e.artifact_id).collect();
        assert_eq!(ids, vec![1, 2], "only the nonce's artifacts, the unrelated one filtered out");
        assert!(evidence.iter().all(|e| e.nonce == Nonce("n-4".to_owned())));
    }

    #[test]
    fn stream_evidence_with_no_resolvable_run_is_the_no_run_error() {
        let exec = executor(FakeGithub::new());
        let handle = WorkHandle::new(Nonce("n-gone".to_owned()));
        match exec.stream_evidence(&handle) {
            Err(ExecutorError::NoRunForNonce(_)) => {}
            other => panic!("expected NoRunForNonce, got {other:?}"),
        }
    }
}
