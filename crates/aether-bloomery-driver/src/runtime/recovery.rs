//! Startup recovery: fault every prior-life request `Interrupted`, except the
//! clock's, which re-arm (ADR-0226 decision 9, programs; ADR-0245).
//!
//! Once the core first catches up, every request in
//! [`Requests::outstanding`](aether_bloomery_program::Requests::outstanding) is
//! from a prior life, because this core has written nothing yet. Each one is
//! appended a `Fault { Interrupted }` — at most [`EVENTS_PAGE`] records per
//! append — and is never invoked: re-running a program that may trap would
//! crash-loop the engine. Whether to retry is up to the graph. A `clock.until`
//! request is left out of the batch: a timer has no side effects, so recovery
//! re-arms it from the fold, and one whose due time passed while the engine
//! was down fires on the first tick. `Call`s received before the pass commits
//! and reads back wait in FIFO order.

use aether_bloomery_kinds::{AppendRecords, DriverRecord, Fault, FaultReason};

use crate::runtime::clock::is_clock;
use crate::runtime::core::{AppendTicket, Command, EVENTS_PAGE, PendingWrite, ProgramCore};

impl ProgramCore {
    /// Derive one startup batch, or finish recovery when nothing but clock
    /// requests is outstanding.
    pub(crate) fn derive_startup(&mut self, out: &mut Vec<Command>) {
        let batch: Vec<DriverRecord> = self
            .journal
            .requests()
            .outstanding()
            .filter(|request| !is_clock(request.requested().program.bundle()))
            .take(EVENTS_PAGE as usize)
            .map(|request| DriverRecord::Fault {
                cause: request.seq().0,
                record: Fault {
                    program: request.requested().program.clone(),
                    input: request.requested().input,
                    reason: FaultReason::Interrupted,
                },
            })
            .collect();
        if batch.is_empty() {
            self.finish_recovery(out);
            if !self.aborted {
                self.on_recovered_synced(out);
            }
            return;
        }
        let ticket = self.mint(AppendTicket::mint);
        let fence = self.journal.cursor();
        let append = AppendRecords::new(Vec::new(), batch, fence);
        self.journal.set_append(ticket, PendingWrite::Startup);
        out.push(Command::Append { ticket, request: append });
    }

    /// Mark recovery done and re-arm every outstanding clock request.
    pub(crate) fn finish_recovery(&mut self, out: &mut Vec<Command>) {
        self.recovered = true;
        self.rearm_clocks(out);
    }
}
