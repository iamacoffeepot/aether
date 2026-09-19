//! Routing batches: compare-and-swap derivation and committed read-back (ADR-0226 decisions 6 and 8).

use std::collections::BTreeMap;

use aether_bloomery_kinds::{AppendRecords, Detail, Digest, DriverRecord, ReactionFailed, RecordedHead};

use crate::bundles::InstanceState;
use crate::core::{AppendTicket, Command, PendingWrite, PlannedRecord, ProgramCore};
use crate::reactors::{PendingDestination, SeqPhase};

impl ProgramCore {
    /// Derive one queued routing batch against the synced view.
    pub(crate) fn derive_routing(
        &mut self,
        trigger: u64,
        plan: Vec<PlannedRecord>,
        live: Vec<Digest>,
        out: &mut Vec<Command>,
    ) {
        let mut batch_moves: BTreeMap<RecordedHead, Digest> = BTreeMap::new();
        let mut final_records: Vec<DriverRecord> = Vec::with_capacity(plan.len());
        for planned in &plan {
            match planned {
                PlannedRecord::Ready(record) => {
                    final_records.push(record.clone());
                }
                PlannedRecord::SetHead { cause, bundle, reactor, set_head, destination_ok } => {
                    if !destination_ok {
                        let failed = DriverRecord::ReactionFailed {
                            cause: *cause,
                            record: ReactionFailed {
                                bundle: *bundle,
                                reactor: Some(reactor.clone()),
                                reason: Detail::new("set_head destination check failed"),
                            },
                        };
                        final_records.push(failed);
                        continue;
                    }
                    let current = batch_moves
                        .get(set_head.head())
                        .copied()
                        .or_else(|| self.journal.heads().binding(set_head.head()));
                    if current == set_head.from() {
                        batch_moves.insert(set_head.head().clone(), set_head.to());
                        final_records.push(DriverRecord::HeadMoved { cause: *cause, record: set_head.to_move() });
                    } else {
                        let failed = DriverRecord::ReactionFailed {
                            cause: *cause,
                            record: ReactionFailed {
                                bundle: *bundle,
                                reactor: Some(reactor.clone()),
                                reason: Detail::new("set_head compare-and-swap mismatch"),
                            },
                        };
                        final_records.push(failed);
                    }
                }
            }
        }
        self.routing.inflight_records = Some(final_records.clone());
        self.routing.inflight_live = Some(live.clone());
        let ticket = self.mint(AppendTicket::mint);
        let fence = self.journal.cursor();
        let append = AppendRecords::new(Vec::new(), final_records, fence);
        self.journal.set_append(ticket, PendingWrite::Routing { trigger, plan, live });
        out.push(Command::Append { ticket, request: append });
    }

    /// Advance `R` over the read-back routing batch and enqueue its requests.
    pub(crate) fn routing_read_back(&mut self, out: &mut Vec<Command>) {
        let Some(committed) = self.routing.committed.take() else {
            return;
        };
        let trigger = committed.trigger;
        let start = committed.start;
        for (index, record) in committed.records.iter().enumerate() {
            let seq = start + u64::try_from(index).unwrap_or(u64::MAX);
            if let DriverRecord::Requested { cause, record: written } = record {
                let Some(found) = self.journal.requests().get(aether_bloomery_kinds::Seq(seq)) else {
                    self.abort(format!("committed request {seq} is missing from the journal fold"), out);
                    return;
                };
                let same = found.requested() == written && found.cause().map(|cause| cause.0) == *cause;
                if !same {
                    self.abort(format!("committed request {seq} folded differently than written"), out);
                    return;
                }
                self.enqueue_request(written.program.bundle(), seq, out);
                if self.aborted {
                    return;
                }
            }
        }
        for digest in &committed.live {
            if let Some(instance) = self.bundles.instance_mut(digest)
                && matches!(instance.state, InstanceState::Ready { .. })
                && instance.cursor == trigger - 1
            {
                instance.cursor = trigger;
            }
        }
        if !self.routing.started {
            self.routing.started = true;
        }
        self.routing.page.retain(|entry| entry.seq != trigger);
    }

    /// Resolve pending destinations serially, then queue the seq's batch.
    pub(crate) fn drive_planning(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_ref() else {
            return;
        };
        if work.phase != SeqPhase::Planning {
            return;
        }
        if work.current_destination.is_some() {
            return;
        }
        if !work.pending_setheads.is_empty() {
            let mut queued: Vec<PendingDestination> =
                self.routing.current.as_mut().expect("seq work").pending_setheads.drain(..).collect();
            queued.sort_by(|left, right| left.order.cmp(&right.order));
            if let Some(work) = self.routing.current.as_mut() {
                work.pending_setheads = queued.into_iter().collect();
            }
            self.emit_next_destination(out);
            return;
        }
        let Some(work) = self.routing.current.take() else {
            return;
        };
        let plan: Vec<PlannedRecord> = work.order.values().cloned().collect();
        let live: Vec<Digest> = work.live.keys().copied().collect();
        let trigger = work.n;
        if plan.is_empty() {
            self.routing.page.retain(|entry| entry.seq != trigger);
            self.check_processed(out);
            return;
        }
        self.journal.queue_back(PendingWrite::Routing { trigger, plan, live });
        self.routing.page.retain(|entry| entry.seq != trigger);
        self.pump(out);
    }
}
