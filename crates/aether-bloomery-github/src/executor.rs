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
//! `command` / `subject` / `nonce` dispatch inputs (the `subject` carrying the
//! order's checkout target, #3572), the run-name nonce embedding, and the
//! workflow filename; either issue can land first with the other conforming.
//!
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500
//! [#3501]: https://github.com/iamacoffeepot/aether/issues/3501

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_bloomery::{Conclusion, EvidenceRef, ExecutionStatus, ExecutorBackend, Nonce, WorkHandle, WorkOrder};

use crate::client::{ActionsApi, GithubError, RunConclusion, RunStatus, WorkflowRun};
use crate::correspondence::CorrespondenceError;
use crate::source::SharedCorrespondence;

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
    /// The order's checkout digest resolved no real git object through the
    /// [`Correspondence`](crate::Correspondence) store (ADR-0150) — the sealed
    /// source was never materialized or its correspondence never seeded, so the
    /// executor refuses cleanly rather than dispatching a `subject` git cannot
    /// check out.
    UnresolvedCheckout(Nonce),
    /// The correspondence store itself faulted while resolving the checkout.
    Correspondence(CorrespondenceError),
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Github(error) => write!(f, "actions executor backend: {error}"),
            Self::NoRunForNonce(nonce) => {
                write!(f, "actions executor backend: no run resolves for nonce `{}`", nonce.0)
            }
            Self::UnresolvedCheckout(nonce) => {
                write!(
                    f,
                    "actions executor backend: no git-object correspondence for the checkout of nonce `{}`",
                    nonce.0
                )
            }
            Self::Correspondence(error) => write!(f, "actions executor backend: {error}"),
        }
    }
}

impl Error for ExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Github(error) => Some(error),
            Self::Correspondence(error) => Some(error),
            Self::NoRunForNonce(_) | Self::UnresolvedCheckout(_) => None,
        }
    }
}

impl From<CorrespondenceError> for ExecutorError {
    fn from(error: CorrespondenceError) -> Self {
        Self::Correspondence(error)
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
    correspondence: SharedCorrespondence,
    workflow_file: String,
    git_ref: String,
}

impl<C: ActionsApi> ActionsExecutor<C> {
    /// Build an executor over `client` and the shared `correspondence` (the seam
    /// the `subject` checkout resolves through, ADR-0150), dispatching
    /// `workflow_file` at the protected `git_ref`.
    pub fn new(
        client: C,
        correspondence: SharedCorrespondence,
        workflow_file: impl Into<String>,
        git_ref: impl Into<String>,
    ) -> Self {
        Self { client, correspondence, workflow_file: workflow_file.into(), git_ref: git_ref.into() }
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

// Does `name` carry `nonce` as a delimiter-bounded segment? The wrapper embeds
// the nonce in an artifact's name between non-alphanumeric delimiters (or at a
// name edge, e.g. `evidence-{nonce}-log`). A raw `contains` would let a nonce
// that is a prefix of a longer one (`n-1` inside `n-12`) pull an unrelated
// concern's evidence into this order's set, so a match counts only when the
// character on each side of the occurrence is a boundary — a non-alphanumeric
// character or the string's edge. The nonce itself may contain `-`, so a
// split-on-delimiter test would over-segment it; bounding each occurrence is
// the delimiter-safe form.
fn name_carries_nonce(name: &str, nonce: &str) -> bool {
    if nonce.is_empty() {
        return false;
    }
    name.match_indices(nonce).any(|(start, matched)| {
        let before_is_boundary = name[..start].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_is_boundary = name[start + matched.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric());
        before_is_boundary && after_is_boundary
    })
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
        //
        // Two refs, held structurally apart (ADR-0149 §Execution, #3572):
        //
        // - The `workflow_dispatch` fires at `self.git_ref` — the protected,
        //   pinned ref — passed as `dispatch_workflow`'s own positional argument,
        //   so the wrapper *definition* that runs is always the reviewed one
        //   (invariant 1: the workflow definition stays pinned).
        // - The `subject` input carries the order's checkout target — the git
        //   commit the wrapper feeds `actions/checkout`, resolved from the
        //   transformation's `checkout` digest to a **real git object sha**
        //   through the correspondence store (ADR-0150), never hex-punned. This is
        //   the tree the work runs *on*, distinct from the scope-revision
        //   `inputs[0]` that binds the returned evidence.
        //
        // The two are separate keys precisely so a caller can never again put the
        // pinned ref where the checkout target belongs (the stub bug this fixes).
        let subject = self
            .correspondence
            .resolve_git(&order.transformation.checkout)?
            .ok_or_else(|| ExecutorError::UnresolvedCheckout(order.nonce.clone()))?
            .to_hex();
        let mut inputs = BTreeMap::new();
        inputs.insert("command".to_owned(), order.transformation.command.clone());
        inputs.insert("subject".to_owned(), subject);
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
        // The match is delimiter-bounded (see `name_carries_nonce`) so a nonce
        // that is a prefix of a longer one does not pull the wrong artifacts.
        Ok(artifacts
            .into_iter()
            .filter(|a| name_carries_nonce(&a.name, &handle.nonce.0))
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
        Budget, Conclusion, Digest, ExecutionStatus, ExecutorBackend, NetworkProfile, Nonce, Transformation,
        WorkHandle, WorkOrder,
    };

    use std::sync::Arc;

    use super::{ActionsExecutor, ExecutorError};
    use crate::client::{Artifact, RunConclusion, RunStatus};
    use crate::source::to_hex;
    use crate::testing::FakeGithub;

    const WORKFLOW: &str = "bloomery-transform.yml";
    const PINNED_REF: &str = "refs/heads/main";
    // A distinctive checkout target so the dispatched `subject` input is
    // recognizable — hex-rendered, this is `"22"` repeated 32 times.
    const CHECKOUT: [u8; 32] = [0x22; 32];
    // The real git object sha the checkout resolves to through the correspondence
    // — a sha1 (40-hex), deliberately *not* the checkout digest's own 64-hex, so
    // the test proves `subject` is the resolved git sha and never the digest hex.
    const CHECKOUT_SHA1: &str = "abcdef0123456789abcdef0123456789abcdef01";

    fn order(nonce: &str) -> WorkOrder {
        WorkOrder {
            transformation: Transformation {
                command: "verify.clippy".to_owned(),
                inputs: Vec::new(),
                checkout: Digest::from_bytes(CHECKOUT),
                outputs: Vec::new(),
                image: "iama/verify:1".to_owned(),
                limits: Budget::default(),
                network: NetworkProfile::None,
                description: None,
            },
            nonce: Nonce(nonce.to_owned()),
        }
    }

    fn executor(fake: FakeGithub) -> ActionsExecutor<FakeGithub> {
        // Every order in these tests checks out CHECKOUT; seed its correspondence
        // to a real sha1 so `submit` resolves the `subject`.
        fake.seed_correspondence(&Digest::from_bytes(CHECKOUT), CHECKOUT_SHA1);
        let correspondence = Arc::new(fake.clone());
        ActionsExecutor::new(fake, correspondence, WORKFLOW, PINNED_REF)
    }

    #[test]
    fn submit_dispatches_the_wrapper_and_returns_the_nonce_handle() {
        let fake = FakeGithub::new();
        let handle = executor(fake.clone()).submit(&order("n-1")).unwrap();

        assert_eq!(handle, WorkHandle::new(Nonce("n-1".to_owned())));
        assert_eq!(fake.dispatched_nonces(), vec!["n-1".to_owned()]);
        assert_eq!(fake.dispatched_workflow("n-1").as_deref(), Some(WORKFLOW));
        // Invariant 1 (#3572): the dispatch itself fires at the protected pinned
        // ref — the workflow *definition* that runs is always the reviewed one.
        assert_eq!(fake.dispatched_ref("n-1").as_deref(), Some(PINNED_REF));
        // The command / subject / nonce shaping is the contract with the wrapper.
        // `subject` is the order's checkout target (the sealed source the worker
        // checks out), never the pinned ref — the two are structurally separate.
        let inputs = fake.dispatched_inputs("n-1").unwrap();
        assert_eq!(inputs.get("command").map(String::as_str), Some("verify.clippy"));
        // `subject` is the checkout's *resolved git sha* (a real sha1), never the
        // pinned ref and never the digest's own hex (ADR-0150 — no hex-punning).
        assert_eq!(inputs.get("subject").map(String::as_str), Some(CHECKOUT_SHA1));
        assert_ne!(
            inputs.get("subject").map(String::as_str),
            Some(to_hex(&Digest::from_bytes(CHECKOUT)).as_str()),
            "the subject is the resolved git sha, not the digest hex"
        );
        assert_ne!(
            inputs.get("subject").map(String::as_str),
            Some(PINNED_REF),
            "the checkout target is not the pinned ref"
        );
        assert!(!inputs.contains_key("ref"), "the ambiguous `ref` input is retired in favor of `subject`");
        assert_eq!(inputs.get("nonce").map(String::as_str), Some("n-1"));
    }

    #[test]
    fn submit_errors_cleanly_when_the_checkout_is_unrecorded() {
        // A checkout whose sealed source has no recorded correspondence is the
        // clean `UnresolvedCheckout`, never a dispatched `subject` git cannot
        // check out (ADR-0150 boundary).
        let fake = FakeGithub::new();
        let correspondence = Arc::new(fake.clone());
        let exec = ActionsExecutor::new(fake, correspondence, WORKFLOW, PINNED_REF);
        match exec.submit(&order("n-unrecorded")) {
            Err(ExecutorError::UnresolvedCheckout(nonce)) => assert_eq!(nonce, Nonce("n-unrecorded".to_owned())),
            other => panic!("expected UnresolvedCheckout, got {other:?}"),
        }
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
    fn stream_evidence_does_not_leak_a_superstring_nonce() {
        // A run hosts this order's `n-4` artifacts alongside a sibling concern
        // whose nonce `n-42` embeds `n-4` as a prefix. A raw substring filter
        // would leak the `n-42` artifact into `n-4`'s evidence; the
        // delimiter-bounded match must not, since the character after the `n-4`
        // occurrence in `evidence-n-42-log` is the alphanumeric `2`.
        let fake = FakeGithub::new();
        let exec = executor(fake.clone());
        let handle = exec.submit(&order("n-4")).unwrap();
        let run_id = fake.seed_run("n-4", RunStatus::Completed, Some(RunConclusion::Success));
        fake.seed_run_artifacts(
            run_id,
            vec![
                Artifact { id: 1, name: "evidence-n-4-log".to_owned(), size_bytes: 10 },
                Artifact { id: 2, name: "evidence-n-42-log".to_owned(), size_bytes: 20 },
            ],
        );

        let evidence = exec.stream_evidence(&handle).unwrap();
        let ids: Vec<u64> = evidence.iter().map(|e| e.artifact_id).collect();
        assert_eq!(ids, vec![1], "the n-42 artifact must not leak into n-4's evidence set");
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
