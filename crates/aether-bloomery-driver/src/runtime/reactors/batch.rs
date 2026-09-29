//! Routing batches: planning, compare-and-swap derivation, and committed read-back (ADR-0226 decisions 6 and 8).

use std::collections::BTreeMap;

use aether_bloomery_kinds::{AppendRecords, Detail, Digest, DriverRecord, EncodedArtifact, RecordedHead, Seq};
use aether_bloomery_view::Heads;

use crate::runtime::core::{AppendTicket, Command, PendingWrite, PlannedRecord, ProgramCore};
use crate::runtime::reactors::CommittedRouting;
use crate::runtime::reactors::intents::reaction_failed;

/// Resolve a routing plan against `heads`, the journal view at the fence.
///
/// Each `SetHeads` group compares against the view plus earlier successful
/// groups in the same batch. Every comparison passes before any group move is
/// exposed; a mismatch fails that group once.
fn resolve_plan(heads: &Heads, plan: &[PlannedRecord]) -> (Vec<EncodedArtifact>, Vec<DriverRecord>) {
    let mut moved: BTreeMap<RecordedHead, Digest> = BTreeMap::new();
    let mut artifacts = Vec::new();
    let mut records = Vec::new();
    for planned in plan {
        match planned {
            PlannedRecord::Ready(record) => records.push(record.clone()),
            PlannedRecord::SuppliedCall { input, record } => {
                artifacts.push(input.clone());
                records.push(record.clone());
            }
            PlannedRecord::SetHeads { cause, bundle, reactor, set_heads } => {
                let matches = set_heads.changes().iter().all(|change| {
                    moved.get(change.head()).copied().or_else(|| heads.binding(change.head())) == change.from()
                });
                if !matches {
                    let reason = Detail::new("set_heads compare-and-swap mismatch");
                    records.push(reaction_failed(*cause, *bundle, Some(reactor.clone()), reason));
                    continue;
                }
                for change in set_heads.changes() {
                    moved.insert(change.head().clone(), change.to());
                }
                for change in set_heads.changes() {
                    records.push(DriverRecord::HeadMoved { cause: *cause, record: change.to_move() });
                }
            }
        }
    }
    (artifacts, records)
}

impl ProgramCore {
    /// Take one planning step: check the next `SetHeads` destination, or
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
        let (artifacts, records) = resolve_plan(self.journal.heads(), &plan);
        self.routing.appending = Some(records.clone());
        let ticket = self.mint(AppendTicket::mint);
        let append = AppendRecords::new(artifacts, records, self.journal.cursor());
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
