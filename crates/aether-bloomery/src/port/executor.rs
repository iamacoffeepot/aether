//! The executor port (ADR-0149 §The boundary): submit, cancel, inspect, and
//! stream-evidence to disposable workers. Exactly four messages, over the
//! typed value vocabulary — there is no arbitrary-command shape.
//!
//! # The handle is the nonce
//!
//! `submit` dispatches a fully-resolved [`WorkOrder`] and returns a
//! [`WorkHandle`]. The handle carries the order's idempotency [`Nonce`], not a
//! worker id: the first backend dispatches via `workflow_dispatch`, which
//! answers `204 No Content` with no run id, so the durable correlation key is
//! the nonce the order carried. The backend embeds the nonce in the dispatched
//! run's name and resolves nonce → run on demand — turning "dispatch tells you
//! nothing" into a non-problem by construction. `cancel` / `inspect` /
//! `stream_evidence` all take the handle and re-resolve the run from its nonce.

use core::fmt;

use alloc::string::String;
use alloc::vec::Vec;

use crate::ids::Nonce;
use crate::values::{
    CandidateRef, StudyCall, StudyCost, SuppressionRequest, SurfaceRequest, Transformation, VerifyFailureSet,
};

/// A fully-resolved unit of work to dispatch. The [`Transformation`] already
/// carries the typed command id, digest-pinned inputs, declared outputs,
/// image, limits, and network posture; the order adds the idempotency
/// [`Nonce`] the backend correlates the dispatched worker by.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkOrder {
    /// The portable transformation to run.
    pub transformation: Transformation,
    /// The idempotency nonce — the durable correlation key `submit` returns as
    /// the handle and the other three messages resolve the run from.
    pub nonce: Nonce,
}

/// What `submit` returns and `cancel` / `inspect` / `stream_evidence` take.
/// **The handle is the nonce**: `workflow_dispatch` returns no run id, so the
/// nonce the order carried is the correlation key the backend resolves the run
/// from on demand.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkHandle {
    /// The order's nonce.
    pub nonce: Nonce,
}

impl WorkHandle {
    /// The handle for `nonce`.
    #[must_use]
    pub const fn new(nonce: Nonce) -> Self {
        Self { nonce }
    }
}

/// The execution state `inspect` reports, mapping the worker's lifecycle onto
/// the port's vocabulary.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExecutionStatus {
    /// The run is dispatched but not yet started.
    Queued,
    /// The run is in progress.
    ///
    /// `last_progress_unix_millis` is a host-observed liveness signal, not a
    /// claim that the worker is idle. `None` means this backend exposes no
    /// trustworthy live progress (the Actions artifact transport, a mechanical
    /// lane that does not stream a transcript, or a local model lane whose
    /// transcript is absent, unreadable, or stamped in the future) — never that
    /// the worker has gone silent. Only a `Some` timestamp is a heartbeat.
    Running {
        /// Unix-millisecond modification time of the backend's live progress
        /// signal, when it has one.
        last_progress_unix_millis: Option<u64>,
    },
    /// The run finished with a concluded result.
    Completed {
        /// The concluded result.
        conclusion: Conclusion,
    },
    /// The run was cancelled (by `cancel` or externally).
    Cancelled,
    /// No run is yet resolvable for the nonce — `submit` dispatched, but the
    /// backend has not yet observed the run appear. Distinct from a hard
    /// error: dispatch is asynchronous, so "not visible yet" is a normal
    /// transient state, not a fault.
    Unknown,
}

/// A completed run's outcome, mapping the backend's richer conclusion set onto
/// the three the port distinguishes. `Copy` like the value vocabulary's other
/// small enums.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conclusion {
    /// The run succeeded.
    Success,
    /// The run failed.
    Failure,
    /// Neither pass nor fail (skipped, action-required).
    Neutral,
}

/// Host-recorded state riding a reference, carried unchanged from the executor
/// backend to intake. A new evidence channel is a field here and nowhere else.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct LaneObservation {
    /// The candidate the run captured (ADR-0152) — reported by a backend that
    /// commits a model-lane run's work itself (the local executor); `None` from
    /// a zero-secret backend (the Actions lane captures nothing) and from every
    /// mechanical or failing run.
    pub candidate: Option<CandidateRef>,
    /// The review critic's findings prose (#3656) — reported by a backend that
    /// reads the run's evidence bytes itself (the local executor, from the
    /// evidence's top-level `findings`); `None` from the name-only Actions lane
    /// and from every lane that stamps none. Host-recorded state riding the
    /// reference, like `candidate` — never part of the artifact-name contract.
    pub findings: Option<String>,
    /// The exact failed members of a `verify.check` result (ADR-0178). The
    /// local backend decodes this from the evidence body; the name-only Actions
    /// backend decodes the equivalent mask from the artifact name. Empty on a
    /// pass and on every non-Verify lane.
    ///
    /// On both transports this is the same value the reference's `name` carries
    /// — the local backend composes that mask from what it reports here, the
    /// Actions backend reads this out of that mask — so it is a second
    /// rendering of the name channel, never an independent one. Intake reads
    /// the set off the name and has nothing here to cross-check it against.
    pub failed_verifiers: VerifyFailureSet,
    /// What the attempt cost (#4679) — reported by a backend that reads the
    /// run's evidence bytes itself (the local executor, from the result
    /// record's token/cost columns); `None` from the name-only Actions lane,
    /// whose artifacts are opaque zips, and from any harness that reported no
    /// usage. Host-recorded state riding the reference, like `candidate` and
    /// `findings` — never part of the artifact-name contract, because the cost
    /// columns are nine integers and a name is not a data channel.
    ///
    /// `None` means *unmeasured*, never *free*: the study lane writes no row
    /// rather than a row of zeroes, so a ledger gap stays legible as a gap.
    pub cost: Option<StudyCost>,
    /// Per-call token columns when the harness reported them, so a long-context
    /// band can charge each call at the rate its own prompt selects. `None` is
    /// the same gap as a missing cost: the ledger bills the dispatch at the
    /// sub-band rate and names the hole, rather than band-selecting from the
    /// aggregate.
    pub calls: Option<Vec<StudyCall>>,
    /// Session-reuse arm from `evidence.json` (`fresh` / `resumed`), when the
    /// backend read the file. Folded at intake — the janitor deletes the file.
    pub session_reuse_arm: Option<String>,
    /// Micro-USD the reuse arm saved against its counterfactual, when both
    /// prices were present.
    pub session_reuse_saved_micro_usd: Option<u64>,
    /// Peak resident bytes from `evidence.json`, when the harness stamped one.
    pub peak_resident_bytes: Option<u64>,
    /// Paths the candidate changed that no declared-surface glob covers
    /// (ADR-0209). Host-recorded state riding the reference, like `findings` —
    /// never part of the artifact-name contract. Empty unless the containment
    /// overlay named a violation.
    pub violating_paths: Vec<String>,
    /// The declining lane's normalized surface request (ADR-0207), when it
    /// returned one and the host could bind it to the order's sealed revision.
    /// Host-recorded state riding the reference like `candidate` and
    /// `findings` — never part of the artifact-name contract, because a
    /// request is a list of paths and prose and a name is not a data channel.
    /// The name-only Actions backend reports `None`.
    pub surface_request: Option<SurfaceRequest>,
    /// The suppressions the candidate states a case for (ADR-0193), read out of
    /// the evidence's own `suppression_requests` channel and normalized.
    ///
    /// Host-recorded state riding the reference like `findings` and
    /// `surface_request`, and never part of the artifact-name contract: a
    /// request is a file, a line, a lint and a sentence, and a name is not a
    /// data channel. Empty from the name-only Actions backend, from every
    /// mechanical run that stated none, and from every model lane.
    pub suppression_requests: Vec<SuppressionRequest>,
}

/// A reference to one piece of evidence a run uploaded — the transport-level
/// list `stream_evidence` returns, filtered to the order's nonce. Decoding the
/// referenced bytes into a reducer attempt-result is the sibling
/// evidence-return path, not this port: the port returns *references*.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EvidenceRef {
    /// The uploaded artifact's name.
    pub name: String,
    /// The nonce this evidence belongs to (the order's).
    pub nonce: Nonce,
    /// The backend-assigned artifact id, for a later fetch.
    pub artifact_id: u64,
    /// The artifact's size in bytes.
    pub size_bytes: u64,
    /// Host-recorded state riding this reference, carried unchanged to intake.
    pub observation: LaneObservation,
}

/// One live construct lane's working tree, as the executor last read it
/// (ADR-0204).
///
/// The nonce rather than the member, because the observation is of a *run* and
/// only the store knows which member and stage a nonce was dispatched for. The
/// caller resolves that from the order row it already holds, exactly as it does
/// for every other nonce-keyed result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ObservedLaneWrites {
    /// The dispatch's idempotency nonce.
    pub nonce: Nonce,
    /// The repository-relative paths the lane has written, raw. The caller
    /// normalizes through
    /// [`normalize_write_paths`](crate::normalize_write_paths) before the fact
    /// is admitted.
    pub paths: Vec<String>,
}

/// Which mounted backend a dispatch went to (#5412) — the unit an intake cycle
/// isolates a fault to.
///
/// A short static name rather than an enum because the port fronts an open set:
/// the host mounts whichever backends it composes, and the core has no business
/// enumerating them. The value is only ever compared and logged, never parsed,
/// so a name a backend states about itself is the whole contract.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BackendId(pub &'static str);

impl BackendId {
    /// What a backend fronting no others answers — a bare mount has one arm and
    /// nothing to distinguish it from.
    pub const SOLE: Self = Self("executor");
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// The disposable-worker boundary (ADR-0149 §The boundary). Exactly the four
/// messages — no arbitrary-command message exists; the command is the typed
/// [`Transformation::command`] id. The implementation does the I/O — this
/// trait is the contract, mirroring [`SourceBackend`](super::SourceBackend)'s
/// `type Error` + method shape.
pub trait ExecutorBackend {
    /// The backend's error type.
    type Error;

    /// Submit a fully-resolved work order to a disposable worker, returning the
    /// nonce-carrying handle the other messages resolve the run from.
    ///
    /// # Errors
    /// Backend-defined — e.g. the dispatch surface is unreachable or refused
    /// the dispatch.
    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, Self::Error>;

    /// Inspect the run's current execution state. A nonce with no yet-resolvable
    /// run is the clean [`ExecutionStatus::Unknown`], not an error.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, distinct from the clean
    /// [`ExecutionStatus::Unknown`] result.
    fn inspect(&self, handle: &WorkHandle) -> Result<ExecutionStatus, Self::Error>;

    /// Cancel the run the handle resolves to.
    ///
    /// **Idempotent for a handle backed by an outstanding order** (ADR-0177):
    /// success means the run is now cancelled, was already terminal, or is
    /// already absent after a prior successful cancel. A nonce this backend
    /// resolves no run for is therefore `Ok(())`, not an error — the deadline
    /// enforcement leaves an expired order durable until its evidence is stored
    /// and its result admitted, so the same cancel is reissued on the next tick
    /// after any fault downstream of it, and a second one must not turn a
    /// recoverable retry into a permanent refusal.
    ///
    /// # Errors
    /// Backend-defined — a transport or backend fault, which stays retryable and
    /// leaves the order live. "No run resolves for the nonce" is not one of them.
    fn cancel(&self, handle: &WorkHandle) -> Result<(), Self::Error>;

    /// Stream the references to the evidence the run uploaded, filtered to the
    /// order's nonce.
    ///
    /// # Errors
    /// Backend-defined — e.g. no run resolves for the nonce, or the artifact
    /// surface is unreachable.
    fn stream_evidence(&self, handle: &WorkHandle) -> Result<Vec<EvidenceRef>, Self::Error>;

    /// Which arm of this backend owns `handle` (#5412).
    ///
    /// Defaulted to [`BackendId::SOLE`] rather than required: a backend fronting
    /// no others has one arm, and the answer carries no information. A composite
    /// — the host's lane router — overrides it so a caller can group the handles
    /// it holds by the arm each will actually be asked on.
    ///
    /// The caller that grouping exists for is the intake cycle, which must not
    /// let one arm's transport fault cost it the other arm's finished results:
    /// a rate-limited shared-runner API blocked every local lane's admission for
    /// twenty-eight minutes because the cycle inspected one flat handle list and
    /// abandoned it on the first fault. Grouped, the faulting arm is skipped for
    /// the rest of the tick and the other arms are still asked — and an arm
    /// holding no outstanding dispatch is never asked at all.
    ///
    /// Infallible and cheap on purpose: it is answered from what the backend
    /// already knows about the nonce, never over the wire.
    fn backend_for(&self, handle: &WorkHandle) -> BackendId {
        let _ = handle;
        BackendId::SOLE
    }

    /// What each live construct lane has written into its working tree so far
    /// (ADR-0204) — the observation per-file write leases are acquired from.
    ///
    /// Defaulted to empty rather than added to the four required messages: a
    /// backend whose lanes run somewhere this process cannot read a working
    /// tree (the Actions arm, whose runs live on GitHub's side of the wire)
    /// honestly observes nothing, and the caller reads an empty observation as
    /// "no new writes", never as an error. Latitude is deliberate about
    /// cadence, not about correctness: ADR-0204 fixes candidate capture as the
    /// *latest* permissible observation point, so a backend that returns
    /// nothing here still cannot let a collision reach the fold undetected —
    /// it merely detects it later.
    ///
    /// Infallible on purpose. A working tree that cannot be read this tick is
    /// an absent observation, and a backend that raised here would make one
    /// unreadable checkout stop the whole sweep.
    fn observe_writes(&self) -> Vec<ObservedLaneWrites> {
        Vec::new()
    }
}
