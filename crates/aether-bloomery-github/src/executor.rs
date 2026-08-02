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
//! # Two wrappers, one per lane class
//!
//! The lane an order belongs to picks which wrapper the dispatch fires, from
//! [`LaneWorkflows`]: the mechanical lanes fire the zero-secret `transform.yml`,
//! the model lanes fire the credential-bearing `transform-model.yml` and carry
//! the model + effort the order was resolved at. The classification is
//! [`is_model_lane`] over the sealed [`Transformation::command`] — the same
//! question the local backend asks, so the split has one spelling and the
//! zero-secret property rests on sealed content rather than on host
//! configuration.
//!
//! [`Transformation::command`]: aether_bloomery::Transformation::command
//! [#3500]: https://github.com/iamacoffeepot/aether/issues/3500
//! [#3501]: https://github.com/iamacoffeepot/aether/issues/3501

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use aether_bloomery::{
    Conclusion, Digest, EvidenceRef, ExecutionStatus, ExecutorBackend, Nonce, WorkHandle, WorkOrder, is_model_lane,
};

use crate::client::{ActionsApi, GithubError, RunConclusion, RunStatus, WorkflowRun, name_carries_nonce};

/// The `workflow_dispatch` input key carrying the typed lane command — the
/// correlation contract with the external wrapper workflow, whose `inputs:`
/// block names these exact strings (#3668). One constant per key (its siblings
/// below), shared with the fake and the tests, so a drifted key cannot
/// silently dispatch a run the wrapper reads as blank.
pub const INPUT_COMMAND: &str = "command";

/// The input key carrying the evidence-binding subject — see [`INPUT_COMMAND`].
pub const INPUT_SUBJECT: &str = "subject";

/// The input key carrying the correlation nonce — see [`INPUT_COMMAND`].
pub const INPUT_NONCE: &str = "nonce";

/// The input key carrying the displayed digest — see [`INPUT_COMMAND`].
pub const INPUT_DISPLAYED: &str = "displayed";

/// The input key carrying the coordinator-resolved model. Only the model
/// wrapper declares it, so only a model-lane dispatch sends it — see
/// [`INPUT_COMMAND`].
pub const INPUT_MODEL: &str = "model";

/// The input key carrying the resolved reasoning-effort tier — the model
/// wrapper's sibling of [`INPUT_MODEL`].
pub const INPUT_EFFORT: &str = "effort";
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
    /// A model-lane order reached the dispatch carrying no resolved model. The
    /// model wrapper declares `model` as a required input, and the lane's whole
    /// point is running the calibrated profile a receipt attests, so the
    /// executor refuses rather than firing a dispatch the API rejects — or, worse,
    /// one that silently runs at the runner's ambient default.
    UnresolvedModel(Nonce),
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
            Self::UnresolvedModel(nonce) => {
                write!(f, "actions executor backend: model lane order `{}` carries no resolved model", nonce.0)
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
            Self::NoRunForNonce(_) | Self::UnresolvedCheckout(_) | Self::UnresolvedModel(_) => None,
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

/// Which wrapper class an order dispatched at — the recorded form of
/// [`is_model_lane`], kept per submitted nonce so the three handle-only
/// messages can resolve the run against the workflow their dispatch chose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lane {
    /// The zero-secret mechanical wrapper.
    Mechanical,
    /// The credential-bearing model wrapper.
    Model,
}

/// The two wrapper workflows this backend dispatches, one per lane class
/// (ADR-0149 §Execution on Actions).
///
/// Named fields rather than a positional pair because the pairing *is* the
/// security boundary: [`mechanical`](Self::mechanical) carries
/// `permissions: { contents: read }` and no secrets, while
/// [`model`](Self::model) carries a Claude credential. An inverted mapping
/// would put an untrusted mechanical lane on a secret-bearing job, so the two
/// names never reduce to argument order at a call site.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LaneWorkflows {
    /// The zero-secret wrapper every mechanical lane fires
    /// (`.github/workflows/transform.yml`).
    pub mechanical: String,
    /// The credential-bearing wrapper only a model lane fires
    /// (`.github/workflows/transform-model.yml`).
    pub model: String,
}

/// The Actions executor backend over an [`ActionsApi`] client. Holds the
/// dispatch targets: the per-lane wrapper [`LaneWorkflows`] and the protected
/// `git_ref` they are all pinned at.
pub struct ActionsExecutor<C: ActionsApi> {
    client: C,
    correspondence: SharedCorrespondence,
    workflows: LaneWorkflows,
    git_ref: String,
    // Which wrapper each submitted nonce dispatched at. `find_run` is
    // workflow-scoped (the runs endpoint hangs off one workflow file), but
    // `inspect` / `cancel` / `stream_evidence` carry only the nonce handle and
    // never the command that chose the wrapper — so the choice is recorded here
    // at submit and replayed on resolution, the way `RoutingExecutor` records
    // which backend a nonce routed to.
    dispatched: Mutex<HashMap<String, Lane>>,
}

impl<C: ActionsApi> ActionsExecutor<C> {
    /// Build an executor over `client` and the shared `correspondence` (the seam
    /// the `subject` checkout resolves through, ADR-0150), dispatching each
    /// lane's wrapper from `workflows` at the protected `git_ref`.
    pub fn new(
        client: C,
        correspondence: SharedCorrespondence,
        workflows: LaneWorkflows,
        git_ref: impl Into<String>,
    ) -> Self {
        Self { client, correspondence, workflows, git_ref: git_ref.into(), dispatched: Mutex::new(HashMap::new()) }
    }

    /// Borrow the underlying client (test introspection).
    #[must_use]
    pub const fn client(&self) -> &C {
        &self.client
    }

    // The wrapper a lane dispatches at — the only place a lane class becomes a
    // workflow file, so an order reaches the credential-bearing wrapper only by
    // `is_model_lane` answering yes for its command.
    fn workflow_of(&self, lane: Lane) -> &str {
        match lane {
            Lane::Mechanical => &self.workflows.mechanical,
            Lane::Model => &self.workflows.model,
        }
    }

    // The wrappers to resolve a handle's nonce against, most likely first. A
    // nonce this executor submitted resolves against exactly the wrapper it
    // dispatched; one it did not — a coordinator restarted since the dispatch,
    // so the record is gone — probes both, since the run exists under one of
    // them and only the dispatch knew which.
    fn resolution_order(&self, nonce: &str) -> Vec<&str> {
        let recorded = self.lock().get(nonce).copied();

        recorded.map_or_else(
            || vec![self.workflow_of(Lane::Mechanical), self.workflow_of(Lane::Model)],
            |lane| vec![self.workflow_of(lane)],
        )
    }

    // The run for a nonce across its candidate wrappers, or `None` when none of
    // them holds one yet.
    fn find_run(&self, nonce: &str) -> Result<Option<WorkflowRun>, ExecutorError> {
        for workflow in self.resolution_order(nonce) {
            if let Some(run) = self.client.find_run(workflow, nonce)? {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }

    // Resolve the run for a handle's nonce, or the port-specific
    // no-run-for-nonce error — the shared resolution `cancel` and
    // `stream_evidence` both need before they can act on a run.
    fn resolve(&self, handle: &WorkHandle) -> Result<WorkflowRun, ExecutorError> {
        self.find_run(&handle.nonce.0)?.ok_or_else(|| ExecutorError::NoRunForNonce(handle.nonce.clone()))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Lane>> {
        self.dispatched.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// Lowercase-hex a digest's 32 bytes — the evidence-binding form the wrapper's
// `displayed` input carries and the intake's name decode reverses.
fn digest_hex(digest: &Digest) -> String {
    let mut hex = String::with_capacity(64);
    for byte in digest.as_bytes() {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
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
        inputs.insert(INPUT_COMMAND.to_owned(), order.transformation.command.clone());
        inputs.insert(INPUT_SUBJECT.to_owned(), subject);
        inputs.insert(INPUT_NONCE.to_owned(), order.nonce.0.clone());
        // The scope-revision `inputs[0]` binds the returned evidence: the wrapper
        // embeds it in the attempt artifact's name (`attempt.<verdict>.<subject_hex>.
        // <detail_hex>.<nonce>`, #3501), which the pull-side `NameEvidenceClaims`
        // decodes and the broker re-checks against the order's displayed digest.
        // An input-less order (a bare smoke-run shape) omits it and the wrapper
        // falls back to the legacy `evidence-<nonce>` name the intake skips.
        if let Some(displayed) = order.transformation.inputs.first() {
            inputs.insert(INPUT_DISPLAYED.to_owned(), digest_hex(displayed));
        }
        // The lane class picks the wrapper, read off the sealed command through
        // the same `is_model_lane` the local backend asks — not off a routing
        // knob or the presence of a host-filled field, either of which could flip
        // a mechanical lane onto the credential-bearing wrapper. A model lane
        // additionally names the model and effort it runs at, resolved host-side
        // onto the order (ADR-0149 §The line) and carried straight through:
        // the backend re-resolves nothing, so the dispatched profile is the one
        // the bloom sealed and the receipt attests.
        let lane = if is_model_lane(&order.transformation.command) {
            Lane::Model
        } else {
            Lane::Mechanical
        };
        if lane == Lane::Model {
            let resolved = order
                .transformation
                .model
                .as_ref()
                .ok_or_else(|| ExecutorError::UnresolvedModel(order.nonce.clone()))?;
            inputs.insert(INPUT_MODEL.to_owned(), resolved.model.clone());
            inputs.insert(INPUT_EFFORT.to_owned(), resolved.effort.as_str().to_owned());
        }
        self.client.dispatch_workflow(self.workflow_of(lane), &self.git_ref, &inputs)?;
        self.lock().insert(order.nonce.0.clone(), lane);
        Ok(WorkHandle::new(order.nonce.clone()))
    }

    fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error> {
        let run = self.find_run(&handle.nonce.0)?;
        Ok(run.as_ref().map_or(ExecutionStatus::Unknown, map_status))
    }

    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error> {
        let run = self.resolve(handle)?;
        self.client.cancel_run(run.id)?;
        // A cancel is terminal — drop the wrapper record so `dispatched` tracks
        // in-flight orders rather than the lifetime total.
        self.lock().remove(&handle.nonce.0);
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
        //
        // This is the last message the intake cycle sends for a completed order,
        // so the wrapper record is evicted here for the same reason the routing
        // record is: eviction on `inspect` would misroute the stream that follows
        // a `Completed` inspect onto the both-wrappers probe.
        self.lock().remove(&handle.nonce.0);
        Ok(artifacts
            .into_iter()
            .filter(|a| name_carries_nonce(&a.name, &handle.nonce.0))
            .map(|a| EvidenceRef {
                name: a.name,
                nonce: handle.nonce.clone(),
                artifact_id: a.id,
                size_bytes: a.size_bytes,
                // The Actions lane is zero-secret and name-only (ADR-0150): its
                // runner pushes nothing and its artifact bytes are never fetched,
                // so it reports neither a capture nor findings.
                candidate: None,
                findings: None,
            })
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{
        Budget, Conclusion, Digest, ExecutionStatus, ExecutorBackend, NetworkProfile, Nonce, REVIEW_CRITIC_COMMAND,
        ReasoningEffort, ResolvedModel, Transformation, WorkHandle, WorkOrder,
    };

    use std::sync::Arc;

    use super::{ActionsExecutor, ExecutorError, LaneWorkflows};
    use crate::client::{Artifact, RunConclusion, RunStatus};
    use crate::source::to_hex;
    use crate::testing::FakeGithub;

    const WORKFLOW: &str = "bloomery-transform.yml";
    // The credential-bearing sibling — distinct from WORKFLOW so a dispatch that
    // lands on the wrong one is visible by name.
    const MODEL_WORKFLOW: &str = "bloomery-transform-model.yml";
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
                model: None,
            },
            nonce: Nonce(nonce.to_owned()),
        }
    }

    // A model-lane order: the `review.critic` command with a resolved profile,
    // the shape the dispatching host hands the backend (ADR-0149 §The line).
    fn model_order(nonce: &str) -> WorkOrder {
        let mut order = order(nonce);
        order.transformation.command = REVIEW_CRITIC_COMMAND.to_owned();
        order.transformation.model =
            Some(ResolvedModel { model: "claude-opus-5".to_owned(), effort: ReasoningEffort::XHigh });
        order
    }

    fn lanes() -> LaneWorkflows {
        LaneWorkflows { mechanical: WORKFLOW.to_owned(), model: MODEL_WORKFLOW.to_owned() }
    }

    fn executor(fake: FakeGithub) -> ActionsExecutor<FakeGithub> {
        // Every order in these tests checks out CHECKOUT; seed its correspondence
        // to a real sha1 so `submit` resolves the `subject`.
        fake.seed_correspondence(&Digest::from_bytes(CHECKOUT), CHECKOUT_SHA1);
        let correspondence = Arc::new(fake.clone());
        ActionsExecutor::new(fake, correspondence, lanes(), PINNED_REF)
    }

    #[test]
    fn submit_dispatches_the_wrapper_and_returns_the_nonce_handle() {
        let fake = FakeGithub::new();
        let handle = executor(fake.clone()).submit(&order("n-1")).unwrap();

        assert_eq!(handle, WorkHandle::new(Nonce("n-1".to_owned())));
        assert_eq!(fake.dispatched_nonces(), vec!["n-1".to_owned()]);
        // The zero-secret split (ADR-0149 §Execution on Actions): a mechanical
        // lane is untrusted, so it must land on the wrapper that carries
        // `permissions: { contents: read }` and no secrets — never the
        // credential-bearing model wrapper. An inverted lane→workflow mapping
        // would put this order on a job holding a Claude token.
        assert_eq!(fake.dispatched_workflow("n-1").as_deref(), Some(WORKFLOW));
        assert_ne!(fake.dispatched_workflow("n-1").as_deref(), Some(MODEL_WORKFLOW));
        // …and it names no model, so a mechanical dispatch cannot even satisfy
        // the model wrapper's required input.
        assert!(!fake.dispatched_inputs("n-1").unwrap().contains_key("model"));
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
        assert!(
            !inputs.contains_key("displayed"),
            "an input-less order has no evidence binding to hand the wrapper — legacy artifact-name fallback"
        );
    }

    #[test]
    fn submit_hands_the_wrapper_the_evidence_binding_digest_hex() {
        // The scope-revision `inputs[0]` rides the dispatch as `displayed` so the
        // wrapper can compose the `attempt.<verdict>.<subject_hex>.<detail_hex>.<nonce>`
        // artifact name the intake's `NameEvidenceClaims` decodes (#3501).
        let fake = FakeGithub::new();
        let mut bound = order("n-2");
        bound.transformation.inputs = vec![Digest::from_bytes([0x2a; 32])];
        executor(fake.clone()).submit(&bound).unwrap();

        let inputs = fake.dispatched_inputs("n-2").unwrap();
        assert_eq!(
            inputs.get("displayed").map(String::as_str),
            Some(to_hex(&Digest::from_bytes([0x2a; 32])).as_str()),
            "the evidence-binding digest reaches the wrapper as lowercase hex"
        );
    }

    #[test]
    fn submit_dispatches_a_model_lane_at_the_model_wrapper_carrying_its_resolved_profile() {
        // A model lane fires the credential-bearing wrapper and names the model
        // and effort the host resolved onto the order. Before this, every lane
        // fired the one configured wrapper with four fixed inputs, so a model
        // order dispatched the mechanical wrapper and named no model at all —
        // and `transform-model.yml` declares `model` as required, so the lane
        // could not have run even at the right wrapper.
        let fake = FakeGithub::new();
        executor(fake.clone()).submit(&model_order("n-model")).unwrap();

        assert_eq!(fake.dispatched_workflow("n-model").as_deref(), Some(MODEL_WORKFLOW));
        let inputs = fake.dispatched_inputs("n-model").unwrap();
        assert_eq!(inputs.get("model").map(String::as_str), Some("claude-opus-5"));
        assert_eq!(inputs.get("effort").map(String::as_str), Some("xhigh"));
        assert_eq!(inputs.get("command").map(String::as_str), Some(REVIEW_CRITIC_COMMAND));
        assert_eq!(inputs.get("subject").map(String::as_str), Some(CHECKOUT_SHA1));
    }

    #[test]
    fn submit_refuses_a_model_lane_that_carries_no_resolved_model() {
        // The host overlays the resolved profile at dispatch; an order that
        // reached the backend without one would dispatch a `model`-less run at a
        // wrapper that requires it. Refusing names the gap, where forwarding it
        // would surface as an opaque 422 — or, if the wrapper's input were ever
        // relaxed, as a silent run at the runner's ambient model while the
        // receipt attests the sealed profile.
        let fake = FakeGithub::new();
        let mut unresolved = model_order("n-no-model");
        unresolved.transformation.model = None;
        match executor(fake.clone()).submit(&unresolved) {
            Err(ExecutorError::UnresolvedModel(nonce)) => assert_eq!(nonce, Nonce("n-no-model".to_owned())),
            other => panic!("expected UnresolvedModel, got {other:?}"),
        }
        assert!(fake.dispatched_nonces().is_empty(), "a refused model lane dispatches nothing");
    }

    #[test]
    fn submit_errors_cleanly_when_the_checkout_is_unrecorded() {
        // A checkout whose sealed source has no recorded correspondence is the
        // clean `UnresolvedCheckout`, never a dispatched `subject` git cannot
        // check out (ADR-0150 boundary).
        let fake = FakeGithub::new();
        let correspondence = Arc::new(fake.clone());
        let exec = ActionsExecutor::new(fake, correspondence, lanes(), PINNED_REF);
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
