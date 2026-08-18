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

use alloc::string::String;
use alloc::vec::Vec;

use crate::ids::Nonce;
use crate::values::{CandidateRef, StudyCall, StudyCost, Transformation, VerifyFailureSet};

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
}
