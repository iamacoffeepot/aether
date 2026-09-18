//! Outcomes: fault construction and answering waiting callers.
//!
//! Every decided outcome — a passing `Transition` or any `Fault` — becomes
//! one queued [`PendingWrite::Outcome`](crate::core::PendingWrite) carrying
//! its staged artifacts and caused record. When the outcome's append reads
//! back, every caller waiting on the request is answered with the outcome
//! entry's own seq and record.

use aether_bloomery_kinds::{
    AppendRecords, CallOutcome, Digest, DriverRecord, EncodedArtifact, Fault, FaultReason, Seq,
};
use aether_bloomery_view::Outcome as RecordedOutcome;

use crate::core::{AppendTicket, CallerId, Command, PendingWrite, ProgramCore};

/// Answer one caller from a recorded outcome, echoing the request's key.
pub fn answer_command(key: u64, caller: CallerId, outcome: &RecordedOutcome) -> Command {
    match outcome {
        RecordedOutcome::Transition { seq, transition } => Command::Answer {
            caller,
            outcome: CallOutcome::Transition { key, seq: seq.0, transition: transition.clone() },
        },
        RecordedOutcome::Fault { seq, fault } => {
            Command::Answer { caller, outcome: CallOutcome::Fault { key, seq: seq.0, fault: fault.clone() } }
        }
    }
}

impl ProgramCore {
    /// Derive a queued outcome write: re-append it unless the refolded state
    /// already shows an outcome for the request, in which case drop it.
    pub(crate) fn derive_outcome(
        &mut self,
        request: u64,
        artifacts: Vec<EncodedArtifact>,
        record: DriverRecord,
        out: &mut Vec<Command>,
    ) {
        let answered = self.journal.requests().get(Seq(request)).and_then(|found| found.outcome()).is_some();
        if answered {
            self.answer_ready(out);
            return;
        }
        let ticket = self.mint(AppendTicket::mint);
        let fence = self.journal.cursor();
        self.journal.set_append(
            ticket,
            PendingWrite::Outcome { request, artifacts: artifacts.clone(), record: record.clone() },
        );
        out.push(Command::Append { ticket, request: AppendRecords::new(artifacts, vec![record], fence) });
    }

    /// Record a fault for one active request, then release its digest queue.
    pub(crate) fn fault_request(&mut self, bundle: Digest, seq: u64, reason: FaultReason, out: &mut Vec<Command>) {
        let Some((program, input)) = self.request_data(seq) else {
            self.abort(format!("cannot fault request {seq}: it is missing from the journal fold"), out);
            return;
        };
        let record = DriverRecord::Fault { cause: seq, record: Fault { program, input, reason } };
        self.journal.queue_back(PendingWrite::Outcome { request: seq, artifacts: Vec::new(), record });
        self.finish_active(bundle, out);
        if self.aborted {
            return;
        }
        self.pump(out);
    }

    /// Answer every waiter whose request now has a recorded outcome.
    pub(crate) fn answer_ready(&mut self, out: &mut Vec<Command>) {
        let mut answered = Vec::new();
        for (seq, (key, _)) in &self.waiters {
            let outcome = self.journal.requests().get(Seq(*seq)).and_then(|found| found.outcome().cloned());
            if let Some(outcome) = outcome {
                answered.push((*seq, *key, outcome));
            }
        }
        for (seq, key, outcome) in answered {
            let (_, callers) = self.waiters.remove(&seq).expect("waiters collected from the map above");
            for caller in callers {
                out.push(answer_command(key, caller, &outcome));
            }
        }
    }
}
