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
//! [`ExecutorPort`] is the same calls, with one more answer on the four that
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

use aether_bloomery::{BackendId, EvidenceRef, ExecutionStatus, ObservedLaneWrites, WorkHandle, WorkOrder};

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
/// with the two cheap answers left synchronous and the four that reach the
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
    /// The one call that is *not* settled, and deliberately (#5564): the order
    /// registry row is written before the submit runs — the local lane resolves
    /// session reuse from that very row — so a submit a worker still holds
    /// leaves a row for a dispatch that has not happened. That row is not
    /// private bookkeeping: `outstanding_orders` is what the view, the doctor's
    /// open-dispatch report, and every harness read as "this coordinator is
    /// waiting on a run". Handing the submit out would publish a reservation as
    /// a dispatch. Closing that needs a durable submit-intent row the readers
    /// can tell apart, which is its own change; until then this call keeps the
    /// dispatcher for its round trip.
    ///
    /// # Errors
    /// The dispatch surface is unreachable or refused the dispatch.
    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, ExecutorPortError>;

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

    /// What each live construct lane has written into its working tree so far
    /// (ADR-0204). Infallible once answered: a mount with no readable working
    /// trees observes nothing, which is the honest answer and not a fault.
    fn observe_writes(&self) -> Settled<Vec<ObservedLaneWrites>>;
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

    fn submit(&self, order: &WorkOrder) -> Result<WorkHandle, ExecutorPortError> {
        self.backend.submit(order)
    }

    fn observe(&self, handle: &WorkHandle) -> Settled<Result<RunObservation, ExecutorPortError>> {
        Settled::Answered(self.observe_run(handle))
    }

    fn cancel(&self, handle: &WorkHandle) -> Settled<Result<(), ExecutorPortError>> {
        Settled::Answered(self.backend.cancel(handle))
    }

    fn observe_writes(&self) -> Settled<Vec<ObservedLaneWrites>> {
        Settled::Answered(self.backend.observe_writes())
    }
}

impl ExecutorShell {
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
