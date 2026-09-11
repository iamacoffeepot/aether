//! The executor port as a nonblocking handler may call it (#5564).
//!
//! Every [`ExecutorShell`] call reaches the outside world — a blocking GitHub
//! request, a `git` subprocess, a lane checkout — so a reactor handler that
//! calls one directly pins a scheduler worker for the whole round trip. While
//! it is pinned nothing else on that actor runs: not the next poll, not a
//! withdrawal's cancel, not the deadline sweep. `docs/guide/foundations/invariants.md`
//! states the rule ("Never block in a handler"), ADR-0093 gives the shape that
//! keeps it (hand the call to a worker, answer from a later handler turn).
//!
//! [`ExecutorPort`] is the same calls, with one more answer on the five that
//! reach the outside world and can be handed out:
//! [`Settled::InFlight`] — "a worker holds this call; ask again on a later
//! turn". Every caller that drains, inspects, cancels, or sweeps takes the port
//! rather than the shell, so the same code runs on either side of the offload:
//!
//! - [`ExecutorShell`] answers everything [`Settled::Answered`]. It *is* the
//!   blocking adapter, and the callers that want exactly that — boot-time
//!   reconciliation, the unit suites driving a fake backend — keep it.
//! - The executor reactor mounts an offloading port over the same shell, which
//!   answers from a completed worker or hands the call to one and reports
//!   `InFlight`.
//!
//! `InFlight` is never a fault. A caller treats it as "not asked yet": the
//! outbox entry stays unacked, the handle stays tracked and unobserved, the
//! expired order stays live. Everything re-asks on the next turn, and the
//! offload recognizes the re-ask as the call already running rather than
//! starting a second one.
//!
//! The trait's method names shadow [`ExecutorShell`]'s inherent ones on
//! purpose: a caller holding a `&ExecutorShell` still reaches the inherent
//! blocking method (inherent methods win), and a caller holding a
//! `&dyn ExecutorPort` reaches the settled one. That is what lets the shell
//! keep serving its direct callers unchanged while the reactor's helpers move
//! over wholesale.

use aether_bloomery::{
    BackendId, CandidateRef, Digest, EvidenceRef, ExecutionStatus, ObservedConstructionCheckpoint, ObservedLaneWrites,
    WorkHandle, WorkOrder,
};

use super::{ExecutorPortError, ExecutorShell};

/// One adapter call's answer, or the fact that a worker still holds it.
///
/// Deliberately wraps the *whole* answer rather than sitting inside the
/// `Result`: a call in flight has not succeeded and has not failed, and a
/// caller that pattern-matches it beside `Ok` / `Err` would keep having to
/// decide which of the two an unanswered call resembles.
#[derive(Debug, PartialEq, Eq)]
pub enum Settled<T> {
    /// A worker is running the call. Nothing has happened yet; ask again.
    InFlight,
    /// The call answered.
    Answered(T),
}

/// One observation of a dispatched run: what it is doing, and — once it has
/// finished — what it uploaded.
#[derive(Debug)]
pub struct RunObservation {
    /// The run's current execution state.
    pub status: ExecutionStatus,
    /// The evidence the run uploaded, `Some` exactly when `status` is
    /// [`ExecutionStatus::Completed`].
    ///
    /// Carries the stream's *own* result rather than folding it into the
    /// observation's: a completed run whose artifact surface faulted is still a
    /// completed run, and the caller counts it as one and faults only that arm
    /// — which is what it did when the two calls were separate.
    pub evidence: Option<Result<Vec<EvidenceRef>, ExecutorPortError>>,
}

/// The executor port every reactor helper calls: the [`ExecutorShell`] surface,
/// with the one cheap answer left synchronous and the five that reach the
/// outside world allowed to report [`Settled::InFlight`].
///
/// [`observe_writes`](Self::observe_writes) is settled too, despite being local
/// I/O: it shells `git` once per live lane, which is exactly the multi-hundred-
/// millisecond stall inside a handler this port exists to end.
/// [`backend_for`](Self::backend_for) stays synchronous because it is answered
/// from what the backend already knows about the nonce, never over a wire.
pub trait ExecutorPort {
    /// Which arm of the mounted backend owns `handle` (#5412).
    fn backend_for(&self, handle: &WorkHandle) -> BackendId;

    /// Submit a fully-resolved work order, returning the nonce-carrying handle.
    ///
    /// Settled like the other adapter calls (#5564). The order registry row is
    /// written *before* this runs, as `submitting`: the local lane still
    /// resolves session reuse from that row, but readers that mean "waiting on
    /// a run" ignore it until the worker's completion promotes the row to
    /// `submitted`. [`Settled::InFlight`] is "not asked yet": the outbox entry
    /// stays unacked and the next turn re-asks.
    ///
    /// [`ExecutorShell`]'s inherent [`submit`](ExecutorShell::submit) stays
    /// synchronous — the identity arm, used by boot-time reconciliation and
    /// the unit suites driving a fake backend.
    fn submit(&self, order: &WorkOrder) -> Settled<Result<WorkHandle, ExecutorPortError>>;

    /// Attempt an idle-only submission. `None` is a busy or unsupported backend,
    /// not an accepted order; the durable request remains available to coalesce.
    fn try_submit_idle(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        let _ = order;
        Settled::Answered(Ok(None))
    }

    /// Cheap advisory capacity read; the actual idle submit still reserves.
    fn has_idle_capacity(&self, order: &WorkOrder) -> bool {
        let _ = order;
        false
    }

    /// Settle a previous idle submit or recover an existing process. Must not
    /// start an absent order, including after a restart lost the offload ledger.
    fn settle_idle_submission(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        let _ = order;
        Settled::Answered(Ok(None))
    }

    /// Inspect the run the handle resolves to and, when it has completed,
    /// stream its evidence in the same call.
    ///
    /// One call rather than two, because the intake never wants one without the
    /// other and an offloading port cannot serve them separately: the cycle
    /// learns "completed" by *consuming* the inspect answer, so on the turn the
    /// evidence lands it has no inspect answer left to get past and never
    /// reaches the evidence at all. Fused, one answer carries both — and a
    /// tracked handle costs one round trip a turn instead of two.
    fn observe(&self, handle: &WorkHandle) -> Settled<Result<RunObservation, ExecutorPortError>>;

    /// Cancel the run the handle resolves to. Idempotent (ADR-0177).
    fn cancel(&self, handle: &WorkHandle) -> Settled<Result<(), ExecutorPortError>>;

    /// Release a retained warm lane after cancellation or retirement.
    fn release_physical_run(&self, physical_run: &Digest) -> Settled<Result<(), ExecutorPortError>> {
        let _ = physical_run;
        Settled::Answered(Ok(()))
    }

    /// Retain a captured partial-head repair under its plan-owned Git ref
    /// before the reducer can observe the repaired candidate.
    fn retain_partial_head_repair(
        &self,
        plan: &Digest,
        candidate: &CandidateRef,
        allowed_paths: &[String],
    ) -> Settled<Result<(), ExecutorPortError>> {
        let _ = (plan, candidate, allowed_paths);
        Settled::InFlight
    }

    /// What each live construct lane has written into its working tree so far
    /// (ADR-0204). Infallible once answered: a mount with no readable working
    /// trees observes nothing, which is the honest answer and not a fault.
    fn observe_writes(&self) -> Settled<Vec<ObservedLaneWrites>>;

    /// Capture immutable provisional construction checkpoints without changing
    /// the author checkout's HEAD or index.
    fn observe_construction_checkpoints(&self) -> Settled<Vec<ObservedConstructionCheckpoint>> {
        Settled::Answered(Vec::new())
    }
}

/// The shell answers every call itself, on the calling thread. The identity
/// arm of the port: it is what the reactor's helpers used to call directly, and
/// it is still what a boot-time or unit-test caller wants.
///
/// Written against the mounted backend rather than through the shell's own
/// inherent methods, whose names these shadow — one unambiguous spelling
/// instead of a path that resolves correctly only because inherent methods take
/// precedence.
impl ExecutorPort for ExecutorShell {
    fn backend_for(&self, handle: &WorkHandle) -> BackendId {
        self.backend.backend_for(handle)
    }

    fn submit(&self, order: &WorkOrder) -> Settled<Result<WorkHandle, ExecutorPortError>> {
        Settled::Answered(self.backend.submit(order))
    }

    fn try_submit_idle(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        Settled::Answered(self.backend.try_submit_idle(order))
    }

    fn has_idle_capacity(&self, order: &WorkOrder) -> bool {
        self.backend.has_idle_capacity(order)
    }

    fn settle_idle_submission(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        Settled::Answered(self.backend.settle_idle_submission(order))
    }

    fn observe(&self, handle: &WorkHandle) -> Settled<Result<RunObservation, ExecutorPortError>> {
        Settled::Answered(self.observe_run(handle))
    }

    fn cancel(&self, handle: &WorkHandle) -> Settled<Result<(), ExecutorPortError>> {
        Settled::Answered(self.backend.cancel(handle))
    }

    fn release_physical_run(&self, physical_run: &Digest) -> Settled<Result<(), ExecutorPortError>> {
        Settled::Answered(self.backend.release_physical_run(physical_run))
    }

    fn retain_partial_head_repair(
        &self,
        plan: &Digest,
        candidate: &CandidateRef,
        allowed_paths: &[String],
    ) -> Settled<Result<(), ExecutorPortError>> {
        Settled::Answered(self.backend.retain_partial_head_repair(plan, candidate, allowed_paths))
    }

    fn observe_writes(&self) -> Settled<Vec<ObservedLaneWrites>> {
        Settled::Answered(self.backend.observe_writes())
    }

    fn observe_construction_checkpoints(&self) -> Settled<Vec<ObservedConstructionCheckpoint>> {
        Settled::Answered(self.backend.observe_construction_checkpoints())
    }
}

impl ExecutorShell {
    /// Blocking identity arm for partial-head repair retention. The reactor
    /// offloads this Git write before exposing the completion fact.
    pub fn retain_partial_head_repair(
        &self,
        plan: &Digest,
        candidate: &CandidateRef,
        allowed_paths: &[String],
    ) -> Result<(), ExecutorPortError> {
        self.backend.retain_partial_head_repair(plan, candidate, allowed_paths)
    }

    /// Inspect the run and, when it has completed, stream its evidence — the
    /// blocking body of [`ExecutorPort::observe`].
    ///
    /// Public to the crate rather than folded into the trait impl because the
    /// reactor's offload runs exactly this on a worker thread; both spellings
    /// of the port then perform the same two calls in the same order.
    ///
    /// # Errors
    /// The inspect faulted. A *completed* run whose evidence stream faulted is
    /// still `Ok`, carrying that fault in [`RunObservation::evidence`].
    pub fn observe_run(&self, handle: &WorkHandle) -> Result<RunObservation, ExecutorPortError> {
        let status = self.backend.inspect(handle)?;
        let evidence =
            matches!(status, ExecutionStatus::Completed { .. }).then(|| self.backend.stream_evidence(handle));
        Ok(RunObservation { status, evidence })
    }
}
