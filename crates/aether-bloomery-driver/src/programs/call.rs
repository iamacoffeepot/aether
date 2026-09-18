//! `Call` handling: dedup, refusals, and the `Requested` write (ADR-0226 decision 11).
//!
//! The core handles a `Call` only when it is caught up. The dedup key is
//! `(None, RequestSource::Native { origin, key })`: a recorded request with
//! the same name and input answers from its outcome (or waits on it while in
//! flight), a recorded request with a different name or input is refused
//! `KeyReused`, and a new key resolves the head — `HeadUnbound` when unbound,
//! else one uncaused `Requested` pinning the resolved bundle. A repeat
//! compares name and input only, so a retry after the head moved still gets
//! the first outcome.

use aether_bloomery_kinds::{
    AppendRecords, Call, CallOutcome, CallRefusal, DriverRecord, ProgramRef, RequestSource, Requested, Seq,
};

use super::outcome::answer_command;
use crate::core::{AppendTicket, CallerId, Command, PendingWrite, ProgramCore, RequestedClaim};

impl ProgramCore {
    /// Handle one call against the caught-up folds.
    ///
    /// Answers at once on a dedup hit or refusal; otherwise queues the
    /// `Requested` write. Calls that repeat a queued-but-unrecorded key join
    /// that write instead of queueing a duplicate the fold would reject.
    pub(crate) fn handle_call(&mut self, caller: CallerId, call: Call, out: &mut Vec<Command>) {
        if self.settle_recorded(&[caller], &call, out) {
            return;
        }
        match self.journal.claim_requested(&call, caller) {
            RequestedClaim::Attached => {}
            RequestedClaim::Reused => out.push(Command::Answer {
                caller,
                outcome: CallOutcome::Refused { key: call.key, reason: CallRefusal::KeyReused },
            }),
            RequestedClaim::Absent => {
                self.journal.queue_back(PendingWrite::RequestedCall { callers: vec![caller], call });
            }
        }
    }

    /// Settle `callers` against a request the fold already records under
    /// `call`'s key. Returns `false` when the key is unrecorded.
    ///
    /// A different name or input is refused `KeyReused`; a recorded outcome
    /// answers at once; otherwise the callers wait on the request. A recorded
    /// request without an outcome is always already in this life's pipeline
    /// or has its outcome write queued (prior-life requests are faulted
    /// before any call is handled), so waiting is enough: starting it again
    /// would invoke it twice.
    fn settle_recorded(&mut self, callers: &[CallerId], call: &Call, out: &mut Vec<Command>) -> bool {
        let source = RequestSource::Native { origin: call.origin.clone(), key: call.key };
        let Some(found) = self.journal.requests().find(None, &source) else {
            return false;
        };
        let same = found.requested().program.name() == &call.name && found.requested().input == call.input;
        let seq = found.seq().0;
        let outcome = found.outcome().cloned();
        match outcome {
            _ if !same => out.extend(callers.iter().map(|&caller| Command::Answer {
                caller,
                outcome: CallOutcome::Refused { key: call.key, reason: CallRefusal::KeyReused },
            })),
            Some(recorded) => out.extend(callers.iter().map(|&caller| answer_command(call.key, caller, &recorded))),
            None => self.waiters.entry(seq).or_insert_with(|| (call.key, Vec::new())).1.extend_from_slice(callers),
        }
        true
    }

    /// Derive a queued `Requested` write: repeat the dedup lookup and head
    /// resolution against the current folds, then append under the fence.
    pub(crate) fn derive_requested(&mut self, callers: Vec<CallerId>, call: Call, out: &mut Vec<Command>) {
        if self.settle_recorded(&callers, &call, out) {
            return;
        }
        let Some(binding) = self.journal.heads().get(&call.program) else {
            for caller in callers {
                out.push(Command::Answer {
                    caller,
                    outcome: CallOutcome::Refused { key: call.key, reason: CallRefusal::HeadUnbound },
                });
            }
            return;
        };
        let requested = Requested {
            program: ProgramRef::new(binding.digest(), call.name.clone()),
            input: call.input,
            source: RequestSource::Native { origin: call.origin.clone(), key: call.key },
        };
        let ticket = self.mint(AppendTicket::mint);
        let fence = self.journal.cursor();
        let append =
            AppendRecords::new(Vec::new(), vec![DriverRecord::Requested { cause: None, record: requested }], fence);
        self.journal.set_append(ticket, PendingWrite::RequestedCall { callers, call });
        out.push(Command::Append { ticket, request: append });
    }

    /// Start the pipeline for requests whose `Requested` just read back.
    ///
    /// Each committed request's callers begin waiting on it, and its bundle
    /// queue takes the request. The folded request must equal what was
    /// written; anything else means the journal bytes cannot be trusted.
    pub(crate) fn activate_committed(&mut self, out: &mut Vec<Command>) {
        let seqs: Vec<u64> = self.activations.keys().copied().collect();
        for seq in seqs {
            let Some(found) = self.journal.requests().get(Seq(seq)) else {
                self.abort(format!("committed request {seq} is missing from the journal fold"), out);
                return;
            };
            let bundle = found.requested().program.bundle();
            let recorded = found.requested().clone();
            let activation = self.activations.remove(&seq).expect("activation collected from the map above");
            let source = RequestSource::Native { origin: activation.call.origin.clone(), key: activation.call.key };
            let same = recorded.program.name() == &activation.call.name
                && recorded.input == activation.call.input
                && recorded.source == source;
            if !same {
                self.abort(format!("committed request {seq} folded differently than written"), out);
                return;
            }
            let key = activation.call.key;
            self.waiters.entry(seq).or_insert_with(|| (key, Vec::new())).1.extend(activation.callers);
            self.enqueue_request(bundle, seq, out);
            if self.aborted {
                return;
            }
        }
    }
}
