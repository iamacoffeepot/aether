//! Blocking adapter calls, moved off the dispatch tick (#5564).
//!
//! Every executor-port call and every candidate push reaches the outside world
//! — a blocking GitHub request, a `git` subprocess, a lane checkout — and until
//! this module the tick made them inline. A single slow submit therefore pinned
//! the scheduler worker for its whole round trip, and while it was pinned the
//! reactor ran nothing else: no drain, no withdrawal cancel, no deadline sweep.
//! `docs/guide/foundations/invariants.md` forbids exactly that ("Never block in
//! a handler"), and ADR-0093 gives the shape that keeps it — hand the call to a
//! worker, answer from a later handler turn.
//!
//! # A ledger, not a queue
//!
//! The tick derives everything it wants from durable state: the outbox rows it
//! drained, the orders the registry still holds, the handles it tracks. So the
//! offload keeps no backlog of deferred work — a buffered call would be a
//! *stale* request replayed after the world moved. It keeps a ledger instead:
//!
//! `submit` is settled too (#5564): the order row is written as `submitting`
//! before the call is handed out, so readers that mean "waiting on a run" do
//! not see a reservation as a dispatch. See [`ExecutorPort::submit`].
//!
//! - `in_flight` — the calls a worker holds right now, keyed by [`AdapterCall`]
//!   so a re-ask on the next turn recognizes its own call rather than starting
//!   a second one. That key is also the reactor's **cancellation intent**: a
//!   cancel re-asked by every sweep is one worker call, not one per poll tick.
//! - `answers` — what workers finished and no turn has consumed yet. Taken once.
//! - `wanted` — what *this* turn asked for and nobody holds. Drained at the end
//!   of the turn into at most [`MAX_IN_FLIGHT`] concurrent workers; whatever
//!   does not fit is dropped, because the next turn re-derives it.
//!
//! A turn that finds no answer reports [`Settled::InFlight`], which every
//! caller reads as "not asked yet": the outbox entry stays unacked, the handle
//! stays tracked and unobserved, the expired order stays live. Nothing is lost,
//! and nothing is decided on a call that never happened.
//!
//! # Getting back to the actor
//!
//! [`AdapterOffload::start_wanted`] dispatches through
//! [`NativeCtx::dispatch_blocking_with`] — the ADR-0093 primitive `aether-http`'s
//! egress and `aether-process`'s subprocess runs already use. The worker writes
//! its answer into the shared ledger and returns; the primitive pushes the
//! completion wake to this actor's own mailbox, and the reactor's
//! `#[handler(task)]` runs the next dispatch cycle right there. So a finished
//! call is consumed at completion rather than at the next poll interval, and
//! the poll timer keeps running underneath either way.
//!
//! `TaskQueue` is the fleet's usual bound over that primitive and is
//! deliberately not used here: it re-replies its worker's output to a caller
//! (so the output must be a `Kind`) and it *queues* what it cannot run. This
//! reactor has no caller to reply to — its turn starts on a timer wake — and
//! wants a ceiling rather than a queue, for the staleness reason above.
//!
//! The ledger is behind a `Mutex` because it is the one thing the actor and its
//! workers share; the rest of the offload is plain actor state on the
//! single-threaded dispatcher, which is its own mutual exclusion.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aether_bloomery::{
    BackendId, BloomId, CandidateRef, Digest, Nonce, ObservedConstructionCheckpoint, ObservedLaneWrites,
    SourceSnapshot, WorkHandle, WorkOrder, WorkpieceId,
};
use aether_substrate::actor::native::{DEFAULT_MAX_IN_FLIGHT, NativeCtx};

use super::CandidatePush;
use crate::bloomery::SourceShell;
use crate::bloomery::executor::{ExecutorPort, ExecutorPortError, ExecutorShell, RunObservation, Settled};

/// How many blocking adapter calls this reactor may have in flight at once.
///
/// Matched to `aether_substrate::actor::native::DEFAULT_MAX_IN_FLIGHT`, the
/// calibration every other bounded offload in the fleet uses, rather than
/// invented here: the ceiling protects the host's worker-thread budget, which
/// is a fleet-wide property and not an executor one. Deliberately not a config
/// knob — nothing in the deployment story wants to tune it, and an unset knob
/// is one more way to mount a reactor that cannot dispatch.
pub const MAX_IN_FLIGHT: usize = DEFAULT_MAX_IN_FLIGHT;

/// One blocking call, named by what makes it *the same call* on a later turn.
///
/// The nonce is the whole identity of an order's calls: a re-drained outbox row
/// mints the same nonce ([`dispatch_nonce`](crate::bloomery::intake::dispatch_nonce)),
/// and a re-swept expired order names the same one. A publish is keyed by the
/// pair it actually performs, so a re-push of the same commit onto the same ref
/// is recognized while a different ref is its own call.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum AdapterCall {
    /// `ExecutorPort::submit` for an order's nonce.
    Submit(Nonce),
    /// Idle-only admission has a different answer and never queues the order.
    SubmitIdle(Nonce),
    /// `ExecutorPort::observe` for a tracked handle's nonce.
    Observe(Nonce),
    /// `ExecutorPort::cancel` for a nonce — the reactor's cancellation intent.
    Cancel(Nonce),
    /// Release one retained physical lane.
    ReleasePhysicalRun(Digest),
    RetainPartialHeadRepair(Digest),
    /// `ExecutorPort::observe_writes`, which takes no argument and so is one
    /// call at a time for the whole reactor.
    ObserveWrites,
    /// `ExecutorPort::observe_construction_checkpoints`, one whole-reactor read.
    ObserveConstructionCheckpoints,
    /// `SourceShell::snapshot` for an immutable sealed base checkout.
    SnapshotBase(Digest),
    /// `CandidatePush::push` of one capture onto one ref (ADR-0152).
    Publish {
        commit_hex: String,
        target_ref: String,
    },
}

/// A call plus everything the worker needs that the key does not carry — the
/// submitted order's whole transformation, the handle a backend resolves its
/// run from. Separate from [`AdapterCall`] because identity is the nonce: a
/// re-drain that overlays a fresher advisory is still the same submit, and
/// keying on the payload would start a second run for it.
#[derive(Clone, Debug)]
enum AdapterWork {
    /// Boxed: `WorkOrder` is the transformation plus nonce, far larger than
    /// the other variants, and identity is the nonce on [`AdapterCall`].
    Submit(Box<WorkOrder>),
    SubmitIdle {
        order: Box<WorkOrder>,
        allow_new: bool,
    },
    Observe(WorkHandle),
    Cancel(WorkHandle),
    ReleasePhysicalRun(Digest),
    RetainPartialHeadRepair {
        plan: Digest,
        candidate: CandidateRef,
        allowed_paths: Vec<String>,
    },
    ObserveWrites,
    ObserveConstructionCheckpoints,
    SnapshotBase(Digest),
    Publish {
        commit_hex: String,
        target_ref: String,
    },
}

impl AdapterWork {
    fn call(&self) -> AdapterCall {
        match self {
            Self::Submit(order) => AdapterCall::Submit(order.nonce.clone()),
            Self::SubmitIdle { order, .. } => AdapterCall::SubmitIdle(order.nonce.clone()),
            Self::Observe(handle) => AdapterCall::Observe(handle.nonce.clone()),
            Self::Cancel(handle) => AdapterCall::Cancel(handle.nonce.clone()),
            Self::ReleasePhysicalRun(run) => AdapterCall::ReleasePhysicalRun(*run),
            Self::RetainPartialHeadRepair { plan, candidate, allowed_paths } => {
                let _ = (candidate, allowed_paths);
                AdapterCall::RetainPartialHeadRepair(*plan)
            }
            Self::ObserveWrites => AdapterCall::ObserveWrites,
            Self::ObserveConstructionCheckpoints => AdapterCall::ObserveConstructionCheckpoints,
            Self::SnapshotBase(base) => AdapterCall::SnapshotBase(*base),
            Self::Publish { commit_hex, target_ref } => {
                AdapterCall::Publish { commit_hex: commit_hex.clone(), target_ref: target_ref.clone() }
            }
        }
    }

    /// Run the call. The whole blocking surface of this reactor is these arms,
    /// and they only ever execute on a worker thread. Submit uses the shell's
    /// inherent synchronous method — the identity arm of the port.
    fn run(self, shell: &ExecutorShell, source: Option<&SourceShell>, pusher: &dyn CandidatePush) -> AdapterAnswer {
        match self {
            Self::Submit(order) => AdapterAnswer::Submit(shell.submit(&order)),
            Self::SubmitIdle { order, allow_new } => AdapterAnswer::SubmitIdle(if allow_new {
                shell.try_submit_idle(&order)
            } else {
                shell.settle_idle_submission(&order)
            }),
            Self::Observe(handle) => AdapterAnswer::Observe(shell.observe_run(&handle)),
            Self::Cancel(handle) => AdapterAnswer::Cancel(shell.cancel(&handle)),
            Self::ReleasePhysicalRun(run) => AdapterAnswer::ReleasePhysicalRun(shell.release_physical_run(&run)),
            Self::RetainPartialHeadRepair { plan, candidate, allowed_paths } => AdapterAnswer::RetainPartialHeadRepair(
                shell.retain_partial_head_repair(&plan, &candidate, &allowed_paths),
            ),
            Self::ObserveWrites => AdapterAnswer::ObserveWrites(shell.observe_writes()),
            Self::ObserveConstructionCheckpoints => {
                AdapterAnswer::ObserveConstructionCheckpoints(shell.observe_construction_checkpoints())
            }
            Self::SnapshotBase(base) => AdapterAnswer::SnapshotBase(
                source
                    .ok_or_else(|| "source is not configured for base verification".to_owned())
                    .and_then(|source| source.snapshot(&base).map_err(|error| error.to_string())),
            ),
            Self::Publish { commit_hex, target_ref } => AdapterAnswer::Publish(pusher.push(&commit_hex, &target_ref)),
        }
    }
}

/// What a worker finished. One variant per call shape, because the port's
/// answers have five different types and none of them is worth erasing.
#[derive(Debug)]
enum AdapterAnswer {
    Submit(Result<WorkHandle, ExecutorPortError>),
    SubmitIdle(Result<Option<WorkHandle>, ExecutorPortError>),
    Observe(Result<RunObservation, ExecutorPortError>),
    Cancel(Result<(), ExecutorPortError>),
    ReleasePhysicalRun(Result<(), ExecutorPortError>),
    RetainPartialHeadRepair(Result<(), ExecutorPortError>),
    ObserveWrites(Vec<ObservedLaneWrites>),
    ObserveConstructionCheckpoints(Vec<ObservedConstructionCheckpoint>),
    SnapshotBase(Result<SourceSnapshot, String>),
    Publish(Result<(), String>),
}

/// A worker's hold on one of [`MAX_IN_FLIGHT`] slots, released on drop.
struct Slot {
    ledger: Arc<Mutex<Ledger>>,
    call: AdapterCall,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut ledger) = self.ledger.lock() {
            ledger.in_flight.remove(&self.call);
        }
    }
}

/// The state the actor and its workers share: what is running, what has
/// answered, and what this round still wants run.
#[derive(Default)]
struct Ledger {
    in_flight: BTreeSet<AdapterCall>,
    answers: BTreeMap<AdapterCall, AdapterAnswer>,
    /// This round's backlog, in the order the turn asked for it. Drained a
    /// slot at a time as workers free them, so a round wider than the ceiling
    /// finishes across its completion turns rather than losing its tail.
    wanted: VecDeque<AdapterWork>,
    /// Every call this round has already started. A completion turn re-derives
    /// the same wants from the same durable state, and without this it would
    /// re-run each one the moment its answer landed — turning the reactor's
    /// configured poll interval into "as fast as the network answers".
    asked_this_round: BTreeSet<AdapterCall>,
}

/// A capture waiting to be published (ADR-0152), held across turns because the
/// admission that produced it is consumed the moment its cycle ends.
///
/// In-memory and best-effort, exactly as the inline push it replaces was: a
/// crash between the admit and the push leaves the capture local-only, a
/// downstream checkout of it fails visibly, and the stage machinery retries.
#[derive(Clone, Debug)]
pub struct PendingPublish {
    pub bloom: BloomId,
    pub workpiece: WorkpieceId,
    pub target_ref: String,
    pub commit_hex: String,
    /// What the log calls this push — `"candidate"` or `"member checkpoint"`.
    pub kind: &'static str,
}

/// The reactor's blocking-adapter offload.
pub struct AdapterOffload {
    ledger: Arc<Mutex<Ledger>>,
    /// Captures admitted on an earlier turn whose push has not answered yet.
    publishing: Vec<PendingPublish>,
}

impl Default for AdapterOffload {
    fn default() -> Self {
        Self::new()
    }
}

impl AdapterOffload {
    #[must_use]
    pub fn new() -> Self {
        Self { ledger: Arc::new(Mutex::new(Ledger::default())), publishing: Vec::new() }
    }

    /// How many workers are running right now — the ceiling's accounting, and
    /// what a test asserts the bound against.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.lock().in_flight.len()
    }

    /// The port this turn calls through. Borrows the shell for the one answer
    /// that stays synchronous ([`ExecutorPort::backend_for`], answered from
    /// what the backend already knows about the nonce rather than over a wire).
    #[must_use]
    pub fn port<'a>(&'a self, shell: &'a ExecutorShell) -> OffloadedPort<'a> {
        OffloadedPort { offload: self, shell }
    }

    /// Queue `capture` for publication if this turn is the first to name it.
    /// Idempotent on the (ref, commit) pair, so a re-admitted attempt does not
    /// enqueue a second push of the same capture.
    pub fn publish(&mut self, capture: PendingPublish) {
        let already = self
            .publishing
            .iter()
            .any(|held| held.target_ref == capture.target_ref && held.commit_hex == capture.commit_hex);
        if !already {
            self.publishing.push(capture);
        }
    }

    /// Take every publication that has answered, leaving the rest queued and
    /// re-asking for each so a worker picks it up. The caller journals each
    /// answered push against the store it owns.
    pub fn drain_publications(&mut self) -> Vec<(PendingPublish, Result<(), String>)> {
        let mut answered = Vec::new();
        let mut waiting = Vec::new();
        for capture in mem::take(&mut self.publishing) {
            let work =
                AdapterWork::Publish { commit_hex: capture.commit_hex.clone(), target_ref: capture.target_ref.clone() };
            match self.take_or_want(work) {
                Some(AdapterAnswer::Publish(result)) => answered.push((capture, result)),
                _ => waiting.push(capture),
            }
        }
        self.publishing = waiting;
        answered
    }

    /// Open a new round: forget what the last one asked and what it never got
    /// to. Called by the poll turn, and only by the poll turn — the round is
    /// what holds the reactor to its configured cadence.
    ///
    /// A leftover want is dropped rather than carried, because the turn that
    /// opens the round re-derives every want it still has from the store, the
    /// outbox, and the tracked handles. Carrying one forward would replay a
    /// request the world has since moved past.
    pub fn open_round(&mut self) {
        let mut ledger = self.lock();
        ledger.asked_this_round.clear();
        ledger.wanted.clear();
    }

    /// Hand this round's backlog to workers, up to [`MAX_IN_FLIGHT`] at once.
    /// What does not fit stays queued and starts as slots free, so a round
    /// wider than the ceiling still finishes inside its own round.
    pub fn start_wanted(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        shell: &ExecutorShell,
        source: Option<&SourceShell>,
        pusher: &Arc<dyn CandidatePush>,
    ) {
        // Every ledger borrow here is bound to its own block: the guard is not
        // reentrant, and this loop takes it three times per pass.
        while self.in_flight() < MAX_IN_FLIGHT {
            let next = {
                let mut ledger = self.lock();
                ledger.wanted.pop_front()
            };
            let Some(work) = next else {
                break;
            };

            let call = work.call();
            let claimed = {
                let mut ledger = self.lock();
                let claimed = ledger.in_flight.insert(call.clone());
                if claimed {
                    ledger.asked_this_round.insert(call.clone());
                }
                claimed
            };
            // Already out on a worker from an earlier round — the re-derived
            // want is redundant, not a second run.
            if claimed {
                self.spawn(ctx, shell, source, pusher, call, work);
            }
        }
    }

    fn spawn(
        &self,
        ctx: &mut NativeCtx<'_>,
        shell: &ExecutorShell,
        source: Option<&SourceShell>,
        pusher: &Arc<dyn CandidatePush>,
        call: AdapterCall,
        work: AdapterWork,
    ) {
        let ledger = Arc::clone(&self.ledger);
        let shell = shell.clone();
        let source = source.cloned();
        let pusher = Arc::clone(pusher);
        let key = call.clone();
        ctx.dispatch_blocking_with(call, move || {
            // The worker frees its own slot, and frees it last: the guard is
            // declared first so it drops after the answer is filed, and it
            // drops on a panicking call as surely as on an answering one. The
            // ceiling therefore cannot be leaked by a worker that dies, and a
            // turn can never see a call that is neither in flight nor answered.
            let _slot = Slot { ledger: Arc::clone(&ledger), call: key.clone() };
            let answer = work.run(&shell, source.as_ref(), pusher.as_ref());
            if let Ok(mut ledger) = ledger.lock() {
                ledger.answers.insert(key, answer);
            }
        });
    }

    /// Take a landed answer for `work`'s call, or record that this round wants
    /// it run.
    ///
    /// A call this round has already started is never wanted again, whether it
    /// is still out or has already answered: one run per round is what keeps
    /// the adapter surface on the reactor's poll cadence rather than on the
    /// completion wakes' own.
    fn take_or_want(&self, work: AdapterWork) -> Option<AdapterAnswer> {
        let call = work.call();
        let mut ledger = self.lock();
        if let Some(answer) = ledger.answers.remove(&call) {
            return Some(answer);
        }
        let held = ledger.in_flight.contains(&call)
            || ledger.asked_this_round.contains(&call)
            || ledger.wanted.iter().any(|held| held.call() == call);
        if !held {
            ledger.wanted.push_back(work);
        }
        None
    }

    /// The ledger, recovering a poisoned lock rather than propagating the
    /// panic. A worker that panicked mid-call left the maps structurally intact
    /// (it only ever inserts one finished answer), and a reactor that stopped
    /// dispatching because one adapter call panicked would be a far worse
    /// outcome than the missing answer — which the next turn re-asks for.
    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The [`ExecutorPort`] the dispatch tick calls through: an answer a worker
/// already landed, or [`Settled::InFlight`] plus a note that this turn wants
/// the call.
pub struct OffloadedPort<'a> {
    offload: &'a AdapterOffload,
    shell: &'a ExecutorShell,
}

impl OffloadedPort<'_> {
    /// Resolve the exact immutable tree beneath a sealed base checkout without
    /// blocking the reactor turn.
    pub fn snapshot_base(&self, base: &Digest) -> Settled<Result<SourceSnapshot, String>> {
        match self.offload.take_or_want(AdapterWork::SnapshotBase(*base)) {
            Some(AdapterAnswer::SnapshotBase(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }
}

impl ExecutorPort for OffloadedPort<'_> {
    fn backend_for(&self, handle: &WorkHandle) -> BackendId {
        ExecutorPort::backend_for(self.shell, handle)
    }

    fn submit(&self, order: &WorkOrder) -> Settled<Result<WorkHandle, ExecutorPortError>> {
        match self.offload.take_or_want(AdapterWork::Submit(Box::new(order.clone()))) {
            Some(AdapterAnswer::Submit(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }

    fn try_submit_idle(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        {
            let mut ledger = self.offload.lock();
            let call = AdapterCall::SubmitIdle(order.nonce.clone());
            let cancel = AdapterCall::Cancel(order.nonce.clone());
            if ledger.in_flight.contains(&cancel)
                || ledger.answers.contains_key(&cancel)
                || ledger.asked_this_round.contains(&cancel)
                || ledger.wanted.iter().any(|work| work.call() == cancel)
            {
                return Settled::InFlight;
            }
            let required_pending = ledger.in_flight.iter().any(|call| matches!(call, AdapterCall::Submit(_)))
                || ledger.wanted.iter().any(|work| matches!(work, AdapterWork::Submit(_)));
            if required_pending && !ledger.in_flight.contains(&call) && !ledger.answers.contains_key(&call) {
                ledger.wanted.retain(|work| work.call() != call);
                drop(ledger);
                return Settled::Answered(Ok(None));
            }
        }
        match self.offload.take_or_want(AdapterWork::SubmitIdle { order: Box::new(order.clone()), allow_new: true }) {
            Some(AdapterAnswer::SubmitIdle(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }

    fn has_idle_capacity(&self, order: &WorkOrder) -> bool {
        let required_pending = {
            let ledger = self.offload.lock();
            ledger.in_flight.iter().any(|call| matches!(call, AdapterCall::Submit(_)))
                || ledger.wanted.iter().any(|work| matches!(work, AdapterWork::Submit(_)))
        };
        !required_pending && self.shell.has_idle_capacity(order)
    }

    fn settle_idle_submission(&self, order: &WorkOrder) -> Settled<Result<Option<WorkHandle>, ExecutorPortError>> {
        // A wanted call has not started yet; turn it into a probe before the
        // worker takes it. An in-flight call keeps its immutable input and its
        // answer is consumed through the same key as the original submission.
        {
            let mut ledger = self.offload.lock();
            // A joined pre-check may already be preparing through the required
            // submit path. Its own answer is authoritative: an inspect probe
            // could otherwise mistake that in-progress preparation for a run.
            let required = AdapterCall::Submit(order.nonce.clone());
            if let Some(AdapterAnswer::Submit(answer)) = ledger.answers.remove(&required) {
                return Settled::Answered(answer.map(Some));
            }
            if ledger.in_flight.contains(&required)
                || ledger.asked_this_round.contains(&required)
                || ledger.wanted.iter().any(|work| work.call() == required)
            {
                return Settled::InFlight;
            }
            for work in &mut ledger.wanted {
                if let AdapterWork::SubmitIdle { order: wanted, allow_new } = work
                    && wanted.nonce == order.nonce
                {
                    *allow_new = false;
                }
            }
        }
        match self.offload.take_or_want(AdapterWork::SubmitIdle { order: Box::new(order.clone()), allow_new: false }) {
            Some(AdapterAnswer::SubmitIdle(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }

    fn observe(&self, handle: &WorkHandle) -> Settled<Result<RunObservation, ExecutorPortError>> {
        match self.offload.take_or_want(AdapterWork::Observe(handle.clone())) {
            Some(AdapterAnswer::Observe(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }

    fn cancel(&self, handle: &WorkHandle) -> Settled<Result<(), ExecutorPortError>> {
        {
            let mut ledger = self.offload.lock();
            let idle = AdapterCall::SubmitIdle(handle.nonce.clone());
            ledger.wanted.retain(|work| work.call() != idle);
            // A submit already on a worker can still create the process.
            // Cancel only after that call settles, including expiry cancels.
            if ledger.in_flight.contains(&idle) {
                return Settled::InFlight;
            }
        }
        match self.offload.take_or_want(AdapterWork::Cancel(handle.clone())) {
            Some(AdapterAnswer::Cancel(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }

    fn release_physical_run(&self, physical_run: &Digest) -> Settled<Result<(), ExecutorPortError>> {
        match self.offload.take_or_want(AdapterWork::ReleasePhysicalRun(*physical_run)) {
            Some(AdapterAnswer::ReleasePhysicalRun(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }

    fn retain_partial_head_repair(
        &self,
        plan: &Digest,
        candidate: &CandidateRef,
        allowed_paths: &[String],
    ) -> Settled<Result<(), ExecutorPortError>> {
        match self.offload.take_or_want(AdapterWork::RetainPartialHeadRepair {
            plan: *plan,
            candidate: *candidate,
            allowed_paths: allowed_paths.to_vec(),
        }) {
            Some(AdapterAnswer::RetainPartialHeadRepair(answer)) => Settled::Answered(answer),
            _ => Settled::InFlight,
        }
    }

    fn observe_writes(&self) -> Settled<Vec<ObservedLaneWrites>> {
        match self.offload.take_or_want(AdapterWork::ObserveWrites) {
            Some(AdapterAnswer::ObserveWrites(observed)) => Settled::Answered(observed),
            _ => Settled::InFlight,
        }
    }

    fn observe_construction_checkpoints(&self) -> Settled<Vec<ObservedConstructionCheckpoint>> {
        match self.offload.take_or_want(AdapterWork::ObserveConstructionCheckpoints) {
            Some(AdapterAnswer::ObserveConstructionCheckpoints(observed)) => Settled::Answered(observed),
            _ => Settled::InFlight,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::bloomery::UnconfiguredActionsBackend;
    use crate::bloomery::executor::LocalExecutorError;
    use aether_bloomery::testing::digest;
    use aether_bloomery::{StageCatalog, StageId, Transformation};
    use std::io;

    fn shell() -> ExecutorShell {
        ExecutorShell::new(Arc::new(UnconfiguredActionsBackend::new("test backend".to_owned())))
    }

    fn order(nonce: &str) -> WorkOrder {
        WorkOrder {
            transformation: Transformation::for_aggregate_verify(
                &StageCatalog::binding_of(StageId::AggregateVerify),
                digest(1),
                digest(2),
                digest(3),
            ),
            nonce: Nonce(nonce.to_owned()),
            instruction_bundle: None,
            prompt_manifest: None,
            physical_run: None,
            release_physical_run: true,
        }
    }

    #[test]
    fn base_snapshot_is_queued_for_a_blocking_worker() {
        let offload = AdapterOffload::new();
        let shell = shell();
        let base = digest(9);

        assert!(matches!(offload.port(&shell).snapshot_base(&base), Settled::InFlight));
        assert!(matches!(offload.lock().wanted.front(), Some(AdapterWork::SnapshotBase(held)) if *held == base));
        assert!(offload.lock().answers.is_empty(), "the reactor turn does not execute source I/O inline");
    }

    #[test]
    fn cancellation_downgrades_only_an_unstarted_idle_call_and_restart_only_probes() {
        let offload = AdapterOffload::new();
        let shell = shell();
        let port = offload.port(&shell);
        let order = order("idle");
        assert!(matches!(port.try_submit_idle(&order), Settled::InFlight));
        assert!(matches!(port.settle_idle_submission(&order), Settled::InFlight));
        assert!(matches!(offload.lock().wanted.front(), Some(AdapterWork::SubmitIdle { allow_new: false, .. })));
        let restarted = AdapterOffload::new();
        assert!(matches!(restarted.port(&shell).settle_idle_submission(&order), Settled::InFlight));
        assert!(matches!(restarted.lock().wanted.front(), Some(AdapterWork::SubmitIdle { allow_new: false, .. })));
    }

    #[test]
    fn a_started_idle_answer_survives_promotion_without_another_submission() {
        let offload = AdapterOffload::new();
        let shell = shell();
        let order = order("started");
        let handle = WorkHandle::new(order.nonce.clone());
        offload
            .lock()
            .answers
            .insert(AdapterCall::SubmitIdle(order.nonce.clone()), AdapterAnswer::SubmitIdle(Ok(Some(handle.clone()))));
        assert!(
            matches!(offload.port(&shell).settle_idle_submission(&order), Settled::Answered(Ok(Some(got))) if got == handle)
        );
        assert!(offload.lock().wanted.is_empty());
    }

    #[test]
    fn a_promoted_required_submit_settles_before_any_idle_probe() {
        let offload = AdapterOffload::new();
        let shell = shell();
        let order = order("promoted");
        let call = AdapterCall::Submit(order.nonce.clone());
        offload.lock().in_flight.insert(call.clone());
        assert!(matches!(offload.port(&shell).settle_idle_submission(&order), Settled::InFlight));
        assert!(offload.lock().wanted.is_empty(), "no probe can mistake preparing for a successful run");
        offload.lock().in_flight.remove(&call);
        offload.lock().answers.insert(
            call.clone(),
            AdapterAnswer::Submit(Err(ExecutorPortError::Local(LocalExecutorError::Spawn(io::Error::from(
                io::ErrorKind::ArgumentListTooLong,
            ))))),
        );
        assert!(matches!(offload.port(&shell).settle_idle_submission(&order), Settled::Answered(Err(_))));
        let ledger = offload.lock();
        assert!(!ledger.answers.contains_key(&call));
        assert!(ledger.wanted.is_empty(), "the required failure is returned, not hidden by inspect");
        drop(ledger);
    }

    #[test]
    fn expiry_cancel_removes_an_unstarted_idle_call_and_prevents_requeue() {
        let offload = AdapterOffload::new();
        let shell = shell();
        let port = offload.port(&shell);
        let order = order("expiring");
        let handle = WorkHandle::new(order.nonce.clone());
        assert!(matches!(port.try_submit_idle(&order), Settled::InFlight));
        assert!(matches!(port.cancel(&handle), Settled::InFlight));
        assert!(matches!(port.try_submit_idle(&order), Settled::InFlight));
        let ledger = offload.lock();
        assert_eq!(ledger.wanted.len(), 1);
        assert!(matches!(ledger.wanted.front(), Some(AdapterWork::Cancel(_))));
        drop(ledger);
    }

    #[test]
    fn expiry_cancel_waits_for_a_started_idle_submission_to_settle() {
        let offload = AdapterOffload::new();
        let shell = shell();
        let order = order("started-expiry");
        let handle = WorkHandle::new(order.nonce.clone());
        let call = AdapterCall::SubmitIdle(order.nonce);
        offload.lock().in_flight.insert(call.clone());
        assert!(matches!(offload.port(&shell).cancel(&handle), Settled::InFlight));
        assert!(offload.lock().wanted.is_empty());
        offload.lock().in_flight.remove(&call);
        offload.lock().answers.insert(call, AdapterAnswer::SubmitIdle(Ok(Some(handle.clone()))));
        assert!(matches!(offload.port(&shell).cancel(&handle), Settled::InFlight));
        assert!(matches!(offload.lock().wanted.front(), Some(AdapterWork::Cancel(_))));
    }

    #[test]
    fn required_adapter_work_overtakes_idle_work_that_has_not_started() {
        let offload = AdapterOffload::new();
        let shell = shell();
        let port = offload.port(&shell);
        let idle = order("idle");
        assert!(matches!(port.try_submit_idle(&idle), Settled::InFlight));
        assert!(matches!(port.submit(&order("required")), Settled::InFlight));
        assert!(matches!(port.try_submit_idle(&idle), Settled::Answered(Ok(None))));
        let ledger = offload.lock();
        assert_eq!(ledger.wanted.len(), 1);
        assert!(matches!(ledger.wanted.front(), Some(AdapterWork::Submit(_))));
        drop(ledger);
    }
}
