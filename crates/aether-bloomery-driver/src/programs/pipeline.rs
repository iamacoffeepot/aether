//! The per-request pipeline: bundle read, section check, name check, closure
//! read, load, invoke, and outcome (ADR-0226 decision 3, steps 3-7).
//!
//! Steps 3 and 5 use the digest's shared read and load, and either can be in
//! flight for the reactor role: a request that finds the other role's read or
//! load waits on it instead of issuing its own. One request per digest is
//! active from its section check until its `Invoked` reply or its pre-`Invoke`
//! fault. Each continuation below runs for a ticket the core issued, so a
//! missing queue, a mismatched active request, or an unexpected state reports
//! an internal inconsistency and aborts rather than deciding from a view the
//! core cannot explain.

use std::mem::replace;

use aether_bloomery_kinds::{
    ClosureArtifact, Detail, Digest, DriverRecord, EncodedArtifact, FaultReason, Invoke, Invoked, Program, ReadClosure,
    ReadClosureResult, Transition,
};
use aether_bloomery_program::unreachable_staged;

use super::queue::{Active, Step};
use crate::bundles::{LoadState, Programs};
use crate::core::{ClosureTicket, Command, InvokeTicket, PendingWrite, ProgramCore};

impl ProgramCore {
    /// Queue one recorded request on its bundle's digest queue.
    ///
    /// A request that finds no active request drives the digest from its
    /// current state; otherwise it waits its turn in the FIFO.
    pub(crate) fn enqueue_request(&mut self, bundle: Digest, seq: u64, out: &mut Vec<Command>) {
        let idle = {
            let queue = self.queues.entry(bundle).or_default();
            queue.waiting.push_back(seq);
            queue.active.is_none()
        };
        if idle {
            self.release(bundle, out);
        }
    }

    /// Release a digest's active request, then start its waiters in FIFO
    /// order until one is left waiting on a reply.
    ///
    /// A waiter that finishes at once (an unavailable digest or an unknown
    /// name) queues its fault and the loop moves to the next, so a long
    /// queue behind a failed bundle drains without recursion.
    pub(crate) fn release(&mut self, bundle: Digest, out: &mut Vec<Command>) {
        while let Some(queue) = self.queues.get_mut(&bundle) {
            queue.active = None;
            let Some(seq) = queue.waiting.pop_front() else {
                break;
            };
            queue.active = Some(Active { seq, step: Step::Declaring });
            if !self.start_active(bundle, seq, out) {
                break;
            }
        }
        if !self.aborted {
            self.pump(out);
        }
    }

    /// Start a newly active request from its digest's shared state.
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

    /// Check the active request's name against the decoded declarations,
    /// then read the input's closure. An unknown name faults without any
    /// closure read or load; the bundle stays usable for its other programs.
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
        let Some(step) = self.queues.get_mut(&bundle).and_then(|queue| queue.step_mut(seq)) else {
            self.abort(format!("request {seq} lost its digest queue during the name check"), out);
            return false;
        };
        *step = Step::ReadingClosure { declaration };
        let ticket = self.mint(ClosureTicket::mint);
        self.closure_reads.insert(ticket, (bundle, seq));
        out.push(Command::ReadClosure { ticket, request: ReadClosure { root: input, limit_bytes: self.limit } });
        false
    }

    /// Fault the active request, then release its digest to the next waiter.
    fn fail_active(&mut self, bundle: Digest, seq: u64, reason: FaultReason, out: &mut Vec<Command>) {
        if self.record_fault(seq, reason, out) {
            self.release(bundle, out);
        }
    }

    /// Wake the digest's program request after its shared read or load finished.
    ///
    /// A request waiting on the read reruns its start; a request waiting on
    /// the load invokes or faults from the outcome. Any other step has its
    /// own reply in flight, so there is nothing to resume.
    pub(crate) fn resume_program(&mut self, bundle: Digest, out: &mut Vec<Command>) {
        let Some(seq) = self.active_seq_for(&bundle) else {
            return;
        };
        if self
            .queues
            .get(&bundle)
            .is_some_and(|queue| queue.active.as_ref().is_some_and(|active| matches!(active.step, Step::Declaring)))
        {
            if self.start_active(bundle, seq, out) {
                self.release(bundle, out);
            }
            return;
        }
        if self.queues.get(&bundle).is_some_and(|queue| {
            queue.active.as_ref().is_some_and(|active| matches!(active.step, Step::Loading { .. }))
        }) {
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

    /// Continue the active request with its input closure.
    pub(crate) fn continue_closure(
        &mut self,
        bundle: Digest,
        seq: u64,
        result: ReadClosureResult,
        out: &mut Vec<Command>,
    ) {
        if self.active_seq_for(&bundle) != Some(seq) {
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

    /// Continue the active request with its invocation reply.
    pub(crate) fn continue_invoked(&mut self, bundle: Digest, seq: u64, invoked: Invoked, out: &mut Vec<Command>) {
        if self.active_seq_for(&bundle) != Some(seq) {
            self.abort(format!("invoked reply for request {seq} arrived with no matching active request"), out);
            return;
        }
        let invoked_seq = invoked.seq();
        if invoked_seq != seq {
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
        }
    }

    /// The active request's seq, if its digest queue exists.
    fn active_seq_for(&self, bundle: &Digest) -> Option<u64> {
        self.queues.get(bundle)?.active.as_ref().map(|active| active.seq)
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

    /// Park the active request on the digest's shared load.
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

    /// Send the active request's `Invoke` to its digest root.
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
        self.release(bundle, out);
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
