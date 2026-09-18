//! The per-request pipeline: bundle read, section check, name check, closure
//! read, load, invoke, and outcome (ADR-0226 decision 3, steps 3-7).
//!
//! One request per digest is active from its section check until its
//! `Invoked` reply or its pre-`Invoke` fault. Each continuation below runs
//! for a ticket the core issued, so a missing queue, a mismatched active
//! request, or an unexpected state reports an internal inconsistency and
//! aborts rather than deciding from a view the core cannot explain.

use std::collections::VecDeque;
use std::mem::replace;

use aether_bloomery_kinds::{
    ClosureArtifact, Detail, Digest, DriverRecord, EncodedArtifact, FaultReason, Invoke, Invoked, OpaqueBytes, Program,
    ReadArtifact, ReadArtifactResult, ReadClosure, ReadClosureResult, Transition,
};
use aether_data::{Kind, MailboxId};

use crate::bundles::{Active, DigestQueue, DigestState, programs};
use crate::core::{
    ArtifactTicket, ClosureTicket, Command, InvokeTicket, LoadOutcome, LoadTicket, PendingWrite, ProgramCore,
};

/// Next step for a request whose closure checked out.
enum Next {
    Load(Vec<u8>),
    Invoke(MailboxId),
}

impl ProgramCore {
    /// Queue one recorded request on its bundle's digest queue.
    ///
    /// The first request for a digest reads the bundle artifact; a request
    /// that finds no active request drives the digest from its current
    /// state; otherwise it waits its turn in the FIFO.
    pub(crate) fn enqueue_request(&mut self, bundle: Digest, seq: u64, out: &mut Vec<Command>) {
        let Some(queue) = self.bundles.queue_mut(&bundle) else {
            let ticket = self.mint(ArtifactTicket::mint);
            self.artifact_reads.insert(ticket, bundle);
            self.bundles.insert(
                bundle,
                DigestQueue { state: DigestState::Reading, active: Some(Active::new(seq)), waiting: VecDeque::new() },
            );
            out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest: bundle } });
            return;
        };
        queue.waiting.push_back(seq);
        if queue.active.is_none() {
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
        while let Some(queue) = self.bundles.queue_mut(&bundle) {
            queue.active = None;
            let Some(seq) = queue.waiting.pop_front() else {
                break;
            };
            queue.active = Some(Active::new(seq));
            if !self.start_active(bundle, seq, out) {
                break;
            }
        }
        if !self.aborted {
            self.pump(out);
        }
    }

    /// Start a newly active request from its digest's state.
    ///
    /// Returns `true` when the request finished at once with a queued fault,
    /// `false` when it is waiting on a reply or the core aborted.
    fn start_active(&mut self, bundle: Digest, seq: u64, out: &mut Vec<Command>) -> bool {
        match self.bundles.queue(&bundle).map(|queue| &queue.state) {
            Some(DigestState::Declared { .. } | DigestState::Ready { .. }) => self.check_name(bundle, seq, out),
            Some(DigestState::Unavailable { reason }) => {
                let reason = reason.clone();
                self.record_fault(seq, FaultReason::BundleUnavailable { reason }, out)
            }
            Some(DigestState::Reading | DigestState::Loading { .. }) | None => {
                self.abort(format!("request {seq} became active while its bundle was mid-read or mid-load"), out);
                false
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
        let Some(DigestState::Declared { programs, .. } | DigestState::Ready { programs, .. }) =
            self.bundles.queue(&bundle).map(|queue| &queue.state)
        else {
            self.abort(format!("request {seq} reached the name check with no declarations"), out);
            return false;
        };
        let declared = programs.iter().find(|declared| declared.name == *program.name()).cloned();
        let Some(declaration) = declared else {
            let reason = Detail::new(format!("bundle has no program named {}", program.name().as_str()));
            return self.record_fault(seq, FaultReason::BundleUnavailable { reason }, out);
        };
        let Some(active) = self.active_mut(&bundle, seq) else {
            self.abort(format!("request {seq} lost its digest queue during the name check"), out);
            return false;
        };
        active.declaration = Some(declaration);
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

    /// Continue the active request with its bundle artifact bytes.
    pub(crate) fn continue_artifact(&mut self, bundle: Digest, result: ReadArtifactResult, out: &mut Vec<Command>) {
        let Some(seq) = self.active_seq_for(&bundle) else {
            self.abort("bundle artifact reply arrived with no active request".to_string(), out);
            return;
        };
        let declared = match result {
            ReadArtifactResult::Found { kind, bytes, .. } if kind == OpaqueBytes::ID => {
                programs(&bytes).map(|programs| (programs, bytes))
            }
            ReadArtifactResult::Found { kind, .. } => {
                Err(Detail::new(format!("bundle artifact has kind {}, expected opaque bytes", kind.0)))
            }
            ReadArtifactResult::Missing { .. } => Err(Detail::new("bundle artifact is missing")),
            ReadArtifactResult::Err { message, .. } => Err(Detail::new(message)),
        };
        match declared {
            Ok((programs, wasm)) => {
                if self.set_digest_state(bundle, DigestState::Declared { programs, wasm }, out)
                    && self.check_name(bundle, seq, out)
                {
                    self.release(bundle, out);
                }
            }
            Err(reason) => {
                if self.set_digest_state(bundle, DigestState::Unavailable { reason: reason.clone() }, out) {
                    self.fail_active(bundle, seq, FaultReason::BundleUnavailable { reason }, out);
                }
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

    /// Continue the active request with its load outcome.
    pub(crate) fn continue_loaded(&mut self, bundle: Digest, outcome: LoadOutcome, out: &mut Vec<Command>) {
        let Some(seq) = self.active_seq_for(&bundle) else {
            self.abort("load reply arrived with no active request".to_string(), out);
            return;
        };
        match outcome {
            LoadOutcome::Loaded { root } => {
                let transitioned = match self.bundles.queue_mut(&bundle) {
                    Some(queue) if matches!(queue.state, DigestState::Loading { .. }) => {
                        if let DigestState::Loading { programs } = replace(&mut queue.state, DigestState::Reading) {
                            queue.state = DigestState::Ready { root, programs };
                        }
                        true
                    }
                    _ => false,
                };
                if !transitioned {
                    self.abort(format!("load reply for request {seq} arrived with no load outstanding"), out);
                    return;
                }
                self.emit_invoke(bundle, seq, root, out);
            }
            LoadOutcome::Failed { error } => {
                let reason = Detail::new(error);
                if self.set_digest_state(bundle, DigestState::Unavailable { reason: reason.clone() }, out) {
                    self.fail_active(bundle, seq, FaultReason::BundleUnavailable { reason }, out);
                }
            }
        }
    }

    /// Continue the active request with its invocation reply.
    pub(crate) fn continue_invoked(&mut self, bundle: Digest, seq: u64, invoked: Invoked, out: &mut Vec<Command>) {
        if self.active_seq_for(&bundle) != Some(seq) {
            self.abort(format!("invoked reply for request {seq} arrived with no matching active request"), out);
            return;
        }
        let invoked_seq = match &invoked {
            Invoked::Completed { seq, .. } | Invoked::Refused { seq, .. } | Invoked::Rejected { seq, .. } => *seq,
        };
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
        }
    }

    /// The active request's seq, if its digest queue exists.
    fn active_seq_for(&self, bundle: &Digest) -> Option<u64> {
        self.bundles.queue(bundle).and_then(DigestQueue::active_seq)
    }

    /// The active request's declaration, resolved by its name check.
    fn active_declaration(&self, bundle: &Digest) -> Option<Program> {
        self.bundles.queue(bundle)?.active.as_ref()?.declaration.clone()
    }

    /// The active request's slot, when `seq` is the one driving the digest.
    fn active_mut(&mut self, bundle: &Digest, seq: u64) -> Option<&mut Active> {
        self.bundles.queue_mut(bundle)?.active.as_mut().filter(|active| active.seq == seq)
    }

    /// Replace one digest's state, aborting when its queue is missing.
    fn set_digest_state(&mut self, bundle: Digest, state: DigestState, out: &mut Vec<Command>) -> bool {
        let Some(queue) = self.bundles.queue_mut(&bundle) else {
            self.abort("digest queue missing for an active request".to_string(), out);
            return false;
        };
        queue.state = state;
        true
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
        let Some(declaration) = self.active_declaration(&bundle) else {
            self.abort(format!("request {seq} reached its closure with no declaration"), out);
            return;
        };
        let Some(member) = artifacts.iter().find(|artifact| artifact.digest() == input) else {
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
        let Some(active) = self.active_mut(&bundle, seq) else {
            self.abort(format!("request {seq} lost its digest queue during its closure read"), out);
            return;
        };
        active.closure = Some(artifacts);
        let next = match self.bundles.queue_mut(&bundle) {
            None => None,
            Some(queue) => match replace(&mut queue.state, DigestState::Reading) {
                DigestState::Ready { root, programs } => {
                    queue.state = DigestState::Ready { root, programs };
                    Some(Next::Invoke(root))
                }
                DigestState::Declared { programs, wasm } => {
                    queue.state = DigestState::Loading { programs };
                    Some(Next::Load(wasm))
                }
                other => {
                    queue.state = other;
                    None
                }
            },
        };
        let Some(next) = next else {
            self.abort(format!("request {seq} closure arrived with the bundle neither declared nor ready"), out);
            return;
        };
        match next {
            Next::Invoke(root) => self.emit_invoke(bundle, seq, root, out),
            Next::Load(wasm) => {
                let ticket = self.mint(LoadTicket::mint);
                self.loads.insert(ticket, bundle);
                out.push(Command::Load { ticket, bundle, wasm });
            }
        }
    }

    /// Send the active request's `Invoke` to its digest root.
    fn emit_invoke(&mut self, bundle: Digest, seq: u64, root: MailboxId, out: &mut Vec<Command>) {
        let Some((program, input)) = self.request_data(seq) else {
            self.abort(format!("request {seq} is missing from the journal fold"), out);
            return;
        };
        let closure = self.active_mut(&bundle, seq).and_then(|active| active.closure.take());
        let Some(closure) = closure else {
            self.abort(format!("request {seq} reached invoke with no closure"), out);
            return;
        };
        let ticket = self.mint(InvokeTicket::mint);
        self.invokes.insert(ticket, (bundle, seq));
        out.push(Command::Invoke { ticket, root, request: Invoke::new(seq, program.name().clone(), input, closure) });
    }

    /// Check a completed invocation's staged result, then record it.
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
        let Some(declaration) = self.active_declaration(&bundle) else {
            self.abort(format!("request {seq} completed with no declaration"), out);
            return;
        };
        let valid = staged.len() == 1
            && staged.first().is_some_and(|staged| staged.digest() == result && staged.kind() == declaration.result);
        if !valid {
            let reason = if staged.len() != 1 {
                format!("completed invocation staged {} artifacts, expected exactly one", staged.len())
            } else if staged[0].digest() != result {
                format!("staged artifact digest {} does not match result {result}", staged[0].digest())
            } else {
                format!(
                    "staged artifact kind {} does not match declared result kind {}",
                    staged[0].kind().0,
                    declaration.result.0
                )
            };
            self.fail_active(bundle, seq, FaultReason::ProtocolViolation { reason: Detail::new(reason) }, out);
            return;
        }
        let record = DriverRecord::Transition { cause: seq, record: Transition { program, input, result } };
        self.journal.queue_back(PendingWrite::Outcome { request: seq, artifacts: staged, record });
        self.release(bundle, out);
    }
}
