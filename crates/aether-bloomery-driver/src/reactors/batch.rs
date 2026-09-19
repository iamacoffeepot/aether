//! Routing batches: planning, compare-and-swap derivation, and committed read-back (ADR-0226 decisions 6 and 8).

use std::collections::BTreeMap;

use aether_bloomery_kinds::{AppendRecords, Detail, Digest, DriverRecord, RecordedHead, Seq};
use aether_bloomery_view::Heads;

use crate::core::{AppendTicket, Command, PendingWrite, PlannedRecord, ProgramCore};
use crate::reactors::CommittedRouting;
use crate::reactors::intents::reaction_failed;

/// Resolve a routing plan against `heads`, the journal view at the fence.
///
/// Each `SetHead` compares against the view plus the earlier moves in the
/// same batch: a pass becomes `HeadMoved`, a mismatch fails that intent alone.
fn resolve_plan(heads: &Heads, plan: &[PlannedRecord]) -> Vec<DriverRecord> {
    let mut moved: BTreeMap<RecordedHead, Digest> = BTreeMap::new();
    let mut records = Vec::with_capacity(plan.len());
    for planned in plan {
        let record = match planned {
            PlannedRecord::Ready(record) => record.clone(),
            PlannedRecord::SetHead { cause, bundle, reactor, set_head } => {
                let current = moved.get(set_head.head()).copied().or_else(|| heads.binding(set_head.head()));
                if current == set_head.from() {
                    moved.insert(set_head.head().clone(), set_head.to());
                    DriverRecord::HeadMoved { cause: *cause, record: set_head.to_move() }
                } else {
                    let reason = Detail::new("set_head compare-and-swap mismatch");
                    reaction_failed(*cause, *bundle, Some(reactor.clone()), reason)
                }
            }
        };
        records.push(record);
    }
    records
}

impl ProgramCore {
    /// Take one planning step: check the next `SetHead` destination, or
    /// queue the seq's batch and finish the seq.
    ///
    /// An empty plan appends nothing.
    pub(crate) fn drive_planning(&mut self, out: &mut Vec<Command>) {
        if self.check_next_destination(out) {
            return;
        }
        let Some(work) = self.routing.current.take() else {
            return;
        };
        let plan: Vec<PlannedRecord> = work.order.into_values().collect();
        if !plan.is_empty() {
            self.journal.queue_back(PendingWrite::Routing { trigger: work.n, plan });
            self.pump(out);
        }
    }

    /// Derive one queued routing batch against the synced view and append it.
    pub(crate) fn derive_routing(&mut self, trigger: u64, plan: Vec<PlannedRecord>, out: &mut Vec<Command>) {
        let records = resolve_plan(self.journal.heads(), &plan);
        self.routing.appending = Some(records.clone());
        let ticket = self.mint(AppendTicket::mint);
        let append = AppendRecords::new(Vec::new(), records, self.journal.cursor());
        self.journal.set_append(ticket, PendingWrite::Routing { trigger, plan });
        out.push(Command::Append { ticket, request: append });
    }

    /// Enqueue the read-back routing batch's requests into the program pipeline.
    ///
    /// Each folded request must equal what was written. A committed restart
    /// batch also ends restart replay; live batches find routing started.
    pub(crate) fn routing_read_back(&mut self, out: &mut Vec<Command>) {
        let Some(CommittedRouting { records, start }) = self.routing.committed.take() else {
            return;
        };
        for (seq, record) in (start..).zip(records) {
            let DriverRecord::Requested { cause, record: written } = record else {
                continue;
            };
            let folded = self.journal.requests().get(Seq(seq));
            if !folded.is_some_and(|found| *found.requested() == written && found.cause().map(|cause| cause.0) == cause)
            {
                self.abort(format!("committed request {seq} folded differently than written"), out);
                return;
            }
            self.enqueue_request(written.program.bundle(), seq, out);
            if self.aborted {
                return;
            }
        }
        self.routing.started = true;
    }
}
