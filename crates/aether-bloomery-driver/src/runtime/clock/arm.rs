//! Arming one clock request: the name check, the input read, the 7-day
//! refusal, and the push onto the heap.
//!
//! A request that fails a check records its `Fault` like any other program
//! request. One that passes holds no slot of any kind: no digest queue, no
//! invocation, and no call in flight, only its heap entry.

use aether_bloomery_kinds::{
    ClosureArtifact, Detail, Digest, FaultReason, MAX_DUE_AHEAD_MILLIS, ReadArtifact, ReadArtifactResult, Seq, Until,
};
use aether_bloomery_program::{ClockUntil, Program};
use aether_data::{Kind, Storage};

use super::{Due, is_clock};
use crate::runtime::core::{ArtifactRead, ArtifactTicket, Command, ProgramCore};

impl ProgramCore {
    /// Arm the recorded clock request at `seq`.
    ///
    /// A name other than `clock.until` faults `BundleUnavailable`. The input
    /// comes from the artifact cache, or else from one journal read that
    /// [`Self::continue_clock_input`] continues.
    pub(crate) fn arm_clock(&mut self, seq: u64, out: &mut Vec<Command>) {
        let Some((program, input)) = self.request_data(seq) else {
            self.abort(format!("clock request {seq} is missing from the journal fold"), out);
            return;
        };
        if program.name().as_str() != ClockUntil::NAME {
            let reason = Detail::new(format!("the clock has no program named {}", program.name().as_str()));
            self.record_fault(seq, FaultReason::BundleUnavailable { reason }, out);
            return;
        }
        if let Some(artifact) = self.artifacts.get(input) {
            let result = ReadArtifactResult::Found { artifact: artifact.clone() };
            self.continue_clock_input(seq, result, out);
            return;
        }
        let ticket = self.mint(ArtifactTicket::mint);
        self.artifact_reads.insert(ticket, ArtifactRead::Until(seq));
        out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest: input } });
    }

    /// Arm the clock request at `seq` from its input read.
    ///
    /// A missing input faults `InputMissing`, one that is not an [`Until`]
    /// faults `InputDecode`, and a failed read faults `BundleUnavailable`, as
    /// a failed closure read does. A due time more than
    /// [`MAX_DUE_AHEAD_MILLIS`] after the request's recorded time is refused.
    /// Anything else is armed, and the first armed timer asks for a tick.
    pub(crate) fn continue_clock_input(&mut self, seq: u64, result: ReadArtifactResult, out: &mut Vec<Command>) {
        let Some(found) = self.journal.requests().get(Seq(seq)) else {
            self.abort(format!("clock request {seq} is missing from the journal fold"), out);
            return;
        };
        let input = found.requested().input;
        let requested_at_millis = found.recorded_at_millis();
        let until = match result {
            ReadArtifactResult::Found { artifact } => {
                let until = decode_until(&artifact, input);
                self.artifacts.insert(input, artifact);
                until
            }
            ReadArtifactResult::Missing { .. } => {
                self.record_fault(seq, FaultReason::InputMissing, out);
                return;
            }
            ReadArtifactResult::Err { message, .. } => {
                self.record_fault(seq, FaultReason::BundleUnavailable { reason: Detail::new(message) }, out);
                return;
            }
        };
        let Some(Until { due_millis }) = until else {
            self.record_fault(seq, FaultReason::InputDecode, out);
            return;
        };
        let latest_millis = requested_at_millis.saturating_add(MAX_DUE_AHEAD_MILLIS);
        if due_millis > latest_millis {
            let reason = Detail::new(format!(
                "due time {due_millis} is more than seven days after the request at {requested_at_millis}"
            ));
            self.record_fault(seq, FaultReason::Refused { reason }, out);
            return;
        }
        if self.timers.arm(Due { due_millis, seq }) {
            out.push(Command::ArmTick);
        }
    }

    /// Re-arm every outstanding clock request once startup recovery is done.
    ///
    /// A timer has no side effects, so the restart re-arms it rather than
    /// faulting it `Interrupted`: one whose due time passed while the engine
    /// was down fires on the first tick.
    pub(crate) fn rearm_clocks(&mut self, out: &mut Vec<Command>) {
        let outstanding: Vec<u64> = self
            .journal
            .requests()
            .outstanding()
            .filter(|request| is_clock(request.requested().program.bundle()))
            .map(|request| request.seq().0)
            .collect();
        for seq in outstanding {
            self.arm_clock(seq, out);
            if self.aborted {
                return;
            }
        }
    }
}

/// The [`Until`] a found input holds, or `None` when it is another kind or
/// its bytes do not verify or decode.
fn decode_until(artifact: &ClosureArtifact, input: Digest) -> Option<Until> {
    if artifact.kind() != Until::ID {
        return None;
    }
    let bytes = artifact.load(input).ok()?;
    Until::decode_storage(&bytes).ok().map(|data| data.value)
}
