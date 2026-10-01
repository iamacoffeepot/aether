//! The per-request pipeline: bundle read, section check, name check, closure
//! read, load, invoke, and outcome (ADR-0226 decision 3, steps 3-7).
//!
//! Steps 3 and 5 use the digest's shared read and load, and either can be in
//! flight for the reactor role: a request that finds the other role's read or
//! load waits on it instead of issuing its own. Up to the invocation limit of
//! requests per digest are active at once, each from its section check until
//! its `Invoked` reply or its pre-`Invoke` fault; they share the digest's one
//! read and one load, and each outcome is recorded against its own request in
//! the order they finish. Each continuation below runs for a ticket the core
//! issued, so a missing queue, a request that is not active, or an unexpected
//! state reports an internal inconsistency and aborts rather than deciding
//! from a view the core cannot explain.

use std::mem::replace;

use aether_bloomery_kinds::{
    ClosureArtifact, Detail, Digest, DriverRecord, EncodedArtifact, FaultReason, Invoke, Invoked, Program, ReadClosure,
    ReadClosureResult, Transition,
};
use aether_bloomery_program::unreachable_staged;

use super::api::provided;
use super::queue::Step;
use crate::runtime::bundles::{LoadState, Programs};
use crate::runtime::clock::is_clock;
use crate::runtime::core::{ClosureTicket, Command, InvokeTicket, PendingWrite, ProgramCore};

impl ProgramCore {
    /// Queue one recorded request on its bundle's digest queue.
    ///
    /// A request that finds the digest below its invocation limit starts from
    /// the digest's current state; otherwise it waits its turn in the FIFO. A
    /// clock request never enters a queue: it is armed on the driver's timer
    /// heap and holds no slot while it waits (ADR-0245).
    pub(crate) fn enqueue_request(&mut self, bundle: Digest, seq: u64, out: &mut Vec<Command>) {
        if is_clock(bundle) {
            self.arm_clock(seq, out);
            if !self.aborted {
                self.pump(out);
            }
            return;
        }
        let room = {
            let queue = self.queues.entry(bundle).or_default();
            queue.waiting.push_back(seq);
            queue.active.len() < self.invocations.get()
        };
        if room {
            self.release(bundle, None, out);
        }
    }

    /// Release a digest's `finished` request, if any, then start its waiters
    /// in FIFO order until the invocation limit is reached or none is left.
    ///
    /// A waiter that finishes at once (an unavailable digest or an unknown
    /// name) queues its fault, frees its place, and the loop moves to the
    /// next, so a long queue behind a failed bundle drains without recursion.
    pub(crate) fn release(&mut self, bundle: Digest, finished: Option<u64>, out: &mut Vec<Command>) {
        let limit = self.invocations.get();
        let mut done = finished;
        while let Some(queue) = self.queues.get_mut(&bundle) {
            if let Some(seq) = done.take() {
                queue.active.remove(&seq);
            }
            if queue.active.len() >= limit {
                break;
            }
            let Some(seq) = queue.waiting.pop_front() else {
                break;
            };
            queue.active.insert(seq, Step::Declaring);
            if self.start_active(bundle, seq, out) {
                done = Some(seq);
            } else if self.aborted {
                break;
            }
        }
        if !self.aborted {
            self.pump(out);
        }
    }

    /// Start an active request from its digest's shared state.
    ///
    /// A digest the reactor role is reading or loading is a normal thing to
    /// find: the request waits on that read or load. A digest that declares
    /// no programs faults without any load.
    ///
    /// Returns `true` when the request finished at once with a queued fault,
    /// `false` when it is waiting on a reply or the core aborted.
    fn start_active(&mut self, bundle: Digest, seq: u64, out: &mut Vec<Command>) -> bool {
        match self.bundles.state(&bundle) {
            None => {
                self.issue_read(bundle, out);
                false
            }
            Some(LoadState::Reading) => false,
            Some(LoadState::Unavailable(reason)) => {
                let reason = reason.clone();
                self.record_fault(seq, FaultReason::BundleUnavailable { reason }, out)
            }
            Some(state) => {
                let declares = state.roles().is_some_and(|roles| roles.programs().is_some());
                if !declares {
                    let reason = Detail::new("bundle declares no programs");
                    return self.record_fault(seq, FaultReason::BundleUnavailable { reason }, out);
                }
                self.check_name(bundle, seq, out)
            }
        }
    }

    /// Check an active request's name against the decoded declarations,
    /// then read the input's closure. An unknown name, or a program that
    /// binds an API with no provider in this unit, faults without any closure
    /// read or load; the bundle stays usable for its other programs.
    ///
    /// Returns `true` when the request finished with a queued fault.
    fn check_name(&mut self, bundle: Digest, seq: u64, out: &mut Vec<Command>) -> bool {
        let Some((program, input)) = self.request_data(seq) else {
            self.abort(format!("active request {seq} is missing from the journal fold"), out);
            return false;
        };
        let Some(programs): Option<&Programs> =
            self.bundles.state(&bundle).and_then(LoadState::roles).and_then(|roles| roles.programs())
        else {
            self.abort(format!("request {seq} reached the name check with no declarations"), out);
            return false;
        };
        let declared = programs.find(program.name()).cloned();
        let Some(declaration) = declared else {
            let reason = Detail::new(format!("bundle has no program named {}", program.name().as_str()));
            return self.record_fault(seq, FaultReason::BundleUnavailable { reason }, out);
        };
        if let Some(api) = declaration.apis.iter().find(|api| !provided(**api)) {
            let reason = Detail::new(format!(
                "program {} binds {api:?}, which has no provider in this unit",
                program.name().as_str()
            ));
            return self.record_fault(seq, FaultReason::BundleUnavailable { reason }, out);
        }
        let Some(step) = self.queues.get_mut(&bundle).and_then(|queue| queue.step_mut(seq)) else {
            self.abort(format!("request {seq} lost its digest queue during the name check"), out);
            return false;
        };
        *step = Step::ReadingClosure { declaration: declaration.program };
        let ticket = self.mint(ClosureTicket::mint);
        self.closure_reads.insert(ticket, (bundle, seq));
        out.push(Command::ReadClosure { ticket, request: ReadClosure { root: input, limit_bytes: self.limit } });
        false
    }

    /// Fault an active request, then release its place to the next waiter.
    fn fail_active(&mut self, bundle: Digest, seq: u64, reason: FaultReason, out: &mut Vec<Command>) {
        if self.record_fault(seq, reason, out) {
            self.release(bundle, Some(seq), out);
        }
    }

    /// Wake the digest's program requests after its shared read or load finished.
    ///
    /// Every request waiting on the read reruns its start; every request
    /// waiting on the load invokes or faults from the outcome, in seq order.
    /// Any other step has its own reply in flight, so there is nothing to
    /// resume. The waiting seqs are taken before any resumes, so a waiter a
    /// release starts here begins from the finished state on its own.
    pub(crate) fn resume_program(&mut self, bundle: Digest, out: &mut Vec<Command>) {
        let Some(queue) = self.queues.get(&bundle) else {
            return;
        };
        let declaring: Vec<u64> =
            queue.active.iter().filter(|(_, step)| matches!(step, Step::Declaring)).map(|(seq, _)| *seq).collect();
        let loading: Vec<u64> =
            queue.active.iter().filter(|(_, step)| matches!(step, Step::Loading { .. })).map(|(seq, _)| *seq).collect();
        for seq in declaring {
            if self.aborted {
                return;
            }
            if self.start_active(bundle, seq, out) {
                self.release(bundle, Some(seq), out);
            }
        }
        for seq in loading {
            if self.aborted {
                return;
            }
            self.resume_loading(bundle, seq, out);
        }
    }

    /// Wake a request that waited on the digest's shared load.
    fn resume_loading(&mut self, bundle: Digest, seq: u64, out: &mut Vec<Command>) {
        match self.bundles.state(&bundle) {
            Some(LoadState::Ready { .. }) => {
                let taken = self
                    .queues
                    .get_mut(&bundle)
                    .and_then(|queue| queue.step_mut(seq))
                    .map(|step| replace(step, Step::Declaring));
                let Some(Step::Loading { declaration, closure }) = taken else {
                    self.abort(format!("request {seq} resumed its load with no loading step"), out);
                    return;
                };
                self.emit_invoke(bundle, seq, declaration, closure, out);
            }
            Some(LoadState::Unavailable(reason)) => {
                let reason = reason.clone();
                self.fail_active(bundle, seq, FaultReason::BundleUnavailable { reason }, out);
            }
            _ => {
                self.abort(format!("request {seq} load finished with the bundle neither ready nor unavailable"), out);
            }
        }
    }

    /// Continue an active request with its input closure.
    pub(crate) fn continue_closure(
        &mut self,
        bundle: Digest,
        seq: u64,
        result: ReadClosureResult,
        out: &mut Vec<Command>,
    ) {
        if !self.is_active(&bundle, seq) {
            self.abort(format!("closure reply for request {seq} arrived with no matching active request"), out);
            return;
        }
        match result {
            ReadClosureResult::Missing { .. } => {
                self.fail_active(bundle, seq, FaultReason::InputMissing, out);
            }
            ReadClosureResult::TooLarge { limit_bytes, .. } => {
                self.fail_active(bundle, seq, FaultReason::ClosureTooLarge { limit_bytes: limit_bytes.get() }, out);
            }
            ReadClosureResult::Err { message, .. } => {
                self.fail_active(bundle, seq, FaultReason::BundleUnavailable { reason: Detail::new(message) }, out);
            }
            ReadClosureResult::Found { artifacts, .. } => {
                self.continue_found_closure(bundle, seq, artifacts, out);
            }
        }
    }

    /// Continue an active request with its invocation reply.
    pub(crate) fn continue_invoked(&mut self, bundle: Digest, seq: u64, invoked: Invoked, out: &mut Vec<Command>) {
        if !self.is_active(&bundle, seq) {
            self.abort(format!("invoked reply for request {seq} arrived with no matching active request"), out);
            return;
        }
        // ADR-0243 §1: a `Closed` reply carries no seq; the host sent it in
        // the closed bundle's place, on this request's reply handle.
        if let Some(invoked_seq) = invoked.seq()
            && invoked_seq != seq
        {
            self.fail_active(
                bundle,
                seq,
                FaultReason::ProtocolViolation {
                    reason: Detail::new(format!("invoked seq {invoked_seq} does not match request {seq}")),
                },
                out,
            );
            return;
        }
        match invoked {
            Invoked::Completed { result, staged, .. } => {
                self.continue_completed(bundle, seq, result, staged, out);
            }
            Invoked::Refused { refusal, .. } => {
                self.fail_active(bundle, seq, FaultReason::from(refusal), out);
            }
            Invoked::Rejected { reason, .. } => {
                self.fail_active(bundle, seq, FaultReason::ProtocolViolation { reason }, out);
            }
            Invoked::Faulted { fault, .. } => {
                self.fail_active(bundle, seq, FaultReason::from(fault), out);
            }
            Invoked::Closed => {
                let reason = Detail::new("the bundle closed before the invocation answered");
                self.fail_active(bundle, seq, FaultReason::BundleUnavailable { reason }, out);
            }
        }
    }

    /// Whether `seq` is one of the digest's active requests.
    fn is_active(&self, bundle: &Digest, seq: u64) -> bool {
        self.queues.get(bundle).is_some_and(|queue| queue.active.contains_key(&seq))
    }

    /// Check a found closure's input kind, then load or invoke.
    fn continue_found_closure(
        &mut self,
        bundle: Digest,
        seq: u64,
        artifacts: Vec<ClosureArtifact>,
        out: &mut Vec<Command>,
    ) {
        let Some((_, input)) = self.request_data(seq) else {
            self.abort(format!("request {seq} is missing from the journal fold"), out);
            return;
        };
        let declaration = if let Some(Step::ReadingClosure { declaration }) =
            self.queues.get(&bundle).and_then(|queue| queue.step(seq))
        {
            declaration.clone()
        } else {
            self.abort(format!("request {seq} reached its closure with no declaration"), out);
            return;
        };
        // The claim and kind are what the journal answered. The program
        // verifies the input's bytes and kind prefix when it reads them.
        let Some(member) = artifacts.iter().find(|artifact| artifact.claimed().unverified() == input) else {
            self.fail_active(bundle, seq, FaultReason::InputMissing, out);
            return;
        };
        if member.kind() != declaration.input {
            self.fail_active(
                bundle,
                seq,
                FaultReason::BundleUnavailable {
                    reason: Detail::new(format!(
                        "input has kind {}, declaration requires {}",
                        member.kind().0,
                        declaration.input.0
                    )),
                },
                out,
            );
            return;
        }
        match self.bundles.state(&bundle) {
            Some(LoadState::Ready { .. }) => {
                self.emit_invoke(bundle, seq, declaration, artifacts, out);
            }
            Some(LoadState::Declared { .. }) => {
                self.issue_load(bundle, out);
                self.wait_on_load(bundle, seq, declaration, artifacts, out);
            }
            Some(LoadState::Loading { .. }) => {
                self.wait_on_load(bundle, seq, declaration, artifacts, out);
            }
            Some(LoadState::Unavailable(reason)) => {
                let reason = reason.clone();
                self.fail_active(bundle, seq, FaultReason::BundleUnavailable { reason }, out);
            }
            Some(LoadState::Reading) | None => {
                self.abort(format!("request {seq} closure arrived with the bundle neither declared nor ready"), out);
            }
        }
    }

    /// Park an active request on the digest's shared load.
    fn wait_on_load(
        &mut self,
        bundle: Digest,
        seq: u64,
        declaration: Program,
        closure: Vec<ClosureArtifact>,
        out: &mut Vec<Command>,
    ) {
        let Some(step) = self.queues.get_mut(&bundle).and_then(|queue| queue.step_mut(seq)) else {
            self.abort(format!("request {seq} lost its digest queue during its closure read"), out);
            return;
        };
        *step = Step::Loading { declaration, closure };
    }

    /// Send an active request's `Invoke` to its digest root.
    fn emit_invoke(
        &mut self,
        bundle: Digest,
        seq: u64,
        declaration: Program,
        closure: Vec<ClosureArtifact>,
        out: &mut Vec<Command>,
    ) {
        let Some((program, input)) = self.request_data(seq) else {
            self.abort(format!("request {seq} is missing from the journal fold"), out);
            return;
        };
        let Some(step) = self.queues.get_mut(&bundle).and_then(|queue| queue.step_mut(seq)) else {
            self.abort(format!("request {seq} lost its digest queue during its invoke"), out);
            return;
        };
        *step = Step::Invoking { declaration };
        let ticket = self.mint(InvokeTicket::mint);
        self.invokes.insert(ticket, (bundle, seq));
        out.push(Command::Invoke { ticket, bundle, request: Invoke::new(seq, program.name().clone(), input, closure) });
    }

    /// Check a completed invocation's staged set against ADR-0224 §3, then
    /// record it: no digest staged twice, `result` present among the staged
    /// artifacts at the declaration's result kind, and every staged blob
    /// reachable from `result` through staged citations. The bundle's own
    /// claim is never trusted (ADR-0224 §7) — the driver walks the same
    /// [`unreachable_staged`] check natively. The first broken rule becomes
    /// the fault; a set that passes all four records one `Transition` whose
    /// append carries the entire staged set.
    fn continue_completed(
        &mut self,
        bundle: Digest,
        seq: u64,
        result: Digest,
        staged: Vec<EncodedArtifact>,
        out: &mut Vec<Command>,
    ) {
        let Some((program, input)) = self.request_data(seq) else {
            self.abort(format!("request {seq} is missing from the journal fold"), out);
            return;
        };
        let declaration =
            if let Some(Step::Invoking { declaration }) = self.queues.get(&bundle).and_then(|queue| queue.step(seq)) {
                declaration.clone()
            } else {
                self.abort(format!("request {seq} completed with no declaration"), out);
                return;
            };
        if let Some(digest) = duplicate_staged_digest(&staged) {
            let reason = Detail::new(format!("staged artifact {digest} appears more than once"));
            self.fail_active(bundle, seq, FaultReason::ProtocolViolation { reason }, out);
            return;
        }
        let Some(result_artifact) = staged.iter().find(|artifact| artifact.digest() == result) else {
            let reason = Detail::new(format!("result {result} is not among the staged artifacts"));
            self.fail_active(bundle, seq, FaultReason::ProtocolViolation { reason }, out);
            return;
        };
        if result_artifact.kind() != declaration.result {
            let reason = Detail::new(format!(
                "staged artifact kind {} does not match declared result kind {}",
                result_artifact.kind().0,
                declaration.result.0
            ));
            self.fail_active(bundle, seq, FaultReason::ProtocolViolation { reason }, out);
            return;
        }
        if let Some(digest) = unreachable_staged(&staged, result) {
            let reason = Detail::new(format!("staged artifact {digest} is not reachable from the result {result}"));
            self.fail_active(bundle, seq, FaultReason::ProtocolViolation { reason }, out);
            return;
        }
        let record = DriverRecord::Transition { cause: seq, record: Transition { program, input, result } };
        self.journal.queue_back(PendingWrite::Outcome { request: seq, artifacts: staged, record });
        self.release(bundle, Some(seq), out);
    }
}

/// The first digest that appears twice in `staged`, walked iteratively.
fn duplicate_staged_digest(staged: &[EncodedArtifact]) -> Option<Digest> {
    let mut seen: Vec<Digest> = Vec::with_capacity(staged.len());
    for artifact in staged {
        let digest = artifact.digest();
        if seen.contains(&digest) {
            return Some(digest);
        }
        seen.push(digest);
    }
    None
}
