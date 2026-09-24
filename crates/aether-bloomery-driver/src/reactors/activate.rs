//! Activation: loads, `Warm` paging, reuse, rejections, and owed catch-up (ADR-0226 decision 5).

use std::collections::VecDeque;

use aether_bloomery_kinds::{
    Activated, Detail, Digest, DriverRecord, Evaluated, Head, JournalEntry, OpaqueBytes, Seq, Warm, WarmEntries, Warmed,
};
use aether_bloomery_view::HeadActivation;

use crate::core::{Command, ProgramCore, WarmTicket};
use crate::reactors::claim::Claim;
use crate::reactors::instance::Health;
use crate::reactors::intents::{PlannedIntent, plan_intents, reaction_failed};
use crate::reactors::{
    ActivationPhase, ActivationStep, ActivationWork, CatchUp, Delivery, PlanOrder, RoutingRead, SeqPhase, WarmBatch,
};

/// Next step of an owed catch-up.
enum CatchUpStep {
    /// Every owed seq is evaluated.
    Done,
    /// Read the page after the scratch cursor.
    Read(u64),
    /// Deliver the owed entry.
    Deliver(Digest, JournalEntry),
}

impl ProgramCore {
    /// Feed one warmup reply. Unknown tickets return no commands.
    ///
    /// A poisoned or out-of-sequence warm fails the digest, which rejects the
    /// activation or restart it served.
    pub fn on_warmed(&mut self, ticket: WarmTicket, warmed: Warmed) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(WarmBatch { digest, first, last }) = self.routing.warms.remove(&ticket) else {
            return out;
        };
        match warmed {
            Warmed::Folded { through } => {
                let cursor = self.routing.instances.get(&digest).map(|instance| instance.cursor);
                if through != last || cursor.map(|cursor| cursor + 1) != Some(first) {
                    let reason =
                        format!("warm of {digest} over {first}..={last} from {cursor:?} folded through {through}");
                    self.abort(reason, &mut out);
                    return out;
                }
                self.advance_instance(digest, through);
            }
            Warmed::Poisoned { reason, .. } => self.fail_reactor(digest, Health::Poisoned, reason),
            Warmed::OutOfSequence { first, expected } => {
                let reason = Detail::new(format!("warm out-of-sequence: first {first}, expected {expected}"));
                self.fail_reactor(digest, Health::Untrusted, reason);
            }
        }
        self.drive_routing(&mut out);
        out
    }

    /// Compute the heads `N` changed and start activating them.
    pub(crate) fn begin_activations(&mut self, out: &mut Vec<Command>) {
        if !self.ensure_set_cached(out) {
            return;
        }
        let curr = self.selection();
        if let Some(work) = self.routing.current.as_mut() {
            work.to_activate = Self::changed_heads(&work.prev, &curr);
            work.phase = SeqPhase::Activating;
        }
    }

    /// Take one step on the serial activations: start the next head, page its warm, or move to planning.
    pub(crate) fn drive_activations(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_ref() else {
            return;
        };
        match work.activation.as_ref().map(|activation| &activation.phase) {
            Some(ActivationPhase::Warming) => self.drive_warming(out),
            Some(ActivationPhase::CatchingUp(_)) => self.drive_catch_up(out),
            Some(ActivationPhase::Loading) => {
                let Some(activation) = self.routing.activation() else {
                    return;
                };
                let (bundle, live_from) = (activation.bundle, activation.live_from);
                match self.claim_activation(bundle, live_from, out) {
                    Err(reason) => self.reject_activation(reason),
                    Ok(phase) => {
                        if let Some(activation) =
                            self.routing.current.as_mut().and_then(|work| work.activation.as_mut())
                        {
                            activation.phase = phase;
                        }
                    }
                }
            }
            None => match work.to_activate.get(work.activate_index).cloned() {
                Some((head, bundle)) => self.start_activation(head, bundle, out),
                None => {
                    if let Some(work) = self.routing.current.as_mut() {
                        work.phase = SeqPhase::Planning;
                    }
                }
            },
        }
    }

    /// Start activating one changed head: reject it, wait on its shared read
    /// or load, or warm its live root.
    ///
    /// The live-from is the head's owed start, or `N+1`. A root already past
    /// that start cannot evaluate the owed interval, so the head is rejected.
    fn start_activation(&mut self, head: Head<OpaqueBytes>, bundle: Digest, out: &mut Vec<Command>) {
        let Some(trigger) = self.routing.current.as_ref().map(|work| work.n) else {
            return;
        };
        let live_from = match self.journal.activations().get(&head) {
            Some(HeadActivation::Owed { from }) => from.0,
            _ => trigger + 1,
        };
        let phase = match self.claim_activation(bundle, live_from, out) {
            Err(reason) => {
                if let Some(work) = self.routing.current.as_mut() {
                    work.reject_head(head, bundle, reason);
                }
                return;
            }
            Ok(phase) => phase,
        };
        if let Some(work) = self.routing.current.as_mut() {
            work.activation = Some(ActivationWork { head, bundle, live_from, phase });
        }
    }

    /// Claim the activation's digest for the reactor role: refuse it, wait on
    /// its shared read or load, or warm its live root from its cursor.
    fn claim_activation(
        &mut self,
        bundle: Digest,
        live_from: u64,
        out: &mut Vec<Command>,
    ) -> Result<ActivationPhase, Detail> {
        match self.claim_reactor(bundle, out) {
            Claim::Refuse(reason) => Err(reason),
            Claim::Pending => Ok(ActivationPhase::Loading),
            Claim::Ready { cursor } if cursor >= live_from => {
                Err(Detail::new("shared instance is past the owed start"))
            }
            Claim::Ready { .. } => Ok(ActivationPhase::Warming),
        }
    }

    /// Page the next warm batch through `live_from - 1`, then catch up or succeed.
    fn drive_warming(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_ref() else {
            return;
        };
        let Some(activation) = work.activation.as_ref() else {
            return;
        };
        let (trigger, digest, live_from) = (work.n, activation.bundle, activation.live_from);
        let cursor = self.routing.instances.get(&digest).map_or(0, |instance| instance.cursor);
        if cursor + 1 < live_from {
            self.emit_routing_read(cursor, RoutingRead::Warm, out);
        } else if live_from > trigger {
            self.activate_head(out);
        } else {
            let before = live_from - 1;
            let Some(scratch) = self.journal.history().heads_at(Seq(before)) else {
                let cursor = self.journal.cursor();
                self.abort(format!("catch-up start {before} is past the journal-view cursor {cursor}"), out);
                return;
            };
            if let Some(activation) = self.routing.current.as_mut().and_then(|work| work.activation.as_mut()) {
                let catch_up = CatchUp { scratch, next: live_from, page: VecDeque::new() };
                activation.phase = ActivationPhase::CatchingUp(catch_up);
            }
            self.emit_routing_read(before, RoutingRead::CatchUp, out);
        }
    }

    /// Warm the current activation's root with one page, trimmed below its live-from.
    pub(crate) fn continue_warm_page(&mut self, entries: Vec<JournalEntry>, out: &mut Vec<Command>) {
        let Some((digest, live_from)) =
            self.routing.activation().map(|activation| (activation.bundle, activation.live_from))
        else {
            self.abort("warm page arrived with no activation".to_string(), out);
            return;
        };
        self.send_warm(digest, entries.into_iter().filter(|entry| entry.seq < live_from).collect(), out);
    }

    /// Send one `Warm` batch of `entries` to `digest`'s ready root.
    pub(crate) fn send_warm(&mut self, digest: Digest, entries: Vec<JournalEntry>, out: &mut Vec<Command>) {
        let batch = match WarmEntries::new(entries) {
            Ok(batch) => batch,
            Err(error) => {
                self.abort(format!("warm batch for {digest} is {error}"), out);
                return;
            }
        };
        if !self.live_ready(digest) {
            self.abort(format!("warm for digest {digest}, which has no ready root"), out);
            return;
        }
        let ticket = self.mint(WarmTicket::mint);
        self.routing.warms.insert(ticket, WarmBatch { digest, first: batch.first(), last: batch.last() });
        out.push(Command::Warm { ticket, bundle: digest, request: Warm::new(batch) });
    }

    /// Buffer one catch-up page, trimmed to `N`, and deliver the next owed seq.
    pub(crate) fn continue_catch_up_page(&mut self, entries: Vec<JournalEntry>, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_mut() else {
            self.abort("catch-up page arrived with no seq in progress".to_string(), out);
            return;
        };
        let trigger = work.n;
        let Some(ActivationWork { phase: ActivationPhase::CatchingUp(catch_up), .. }) = work.activation.as_mut() else {
            self.abort("catch-up page arrived with no catch-up in progress".to_string(), out);
            return;
        };
        catch_up.page = entries.into_iter().filter(|entry| entry.seq <= trigger).collect();
        self.drive_catch_up(out);
    }

    /// Fold buffered entries into the scratch `Heads` and deliver the next owed `Event`.
    fn drive_catch_up(&mut self, out: &mut Vec<Command>) {
        let step = loop {
            let Some(work) = self.routing.current.as_mut() else {
                return;
            };
            let trigger = work.n;
            let Some(ActivationWork { bundle, phase: ActivationPhase::CatchingUp(catch_up), .. }) =
                work.activation.as_mut()
            else {
                return;
            };
            if catch_up.next > trigger {
                break CatchUpStep::Done;
            }
            let Some(entry) = catch_up.page.pop_front() else {
                break CatchUpStep::Read(catch_up.scratch.cursor().0);
            };
            if let Err(error) = catch_up.scratch.apply(&entry.to_entry()) {
                let reason = format!("catch-up scratch rejected entry {}: {error}", entry.seq);
                self.abort(reason, out);
                return;
            }
            if entry.seq == catch_up.next {
                break CatchUpStep::Deliver(*bundle, entry);
            }
        };
        match step {
            CatchUpStep::Done => self.activate_head(out),
            CatchUpStep::Read(after) => self.emit_routing_read(after, RoutingRead::CatchUp, out),
            CatchUpStep::Deliver(digest, entry) => {
                let cause = entry.seq;
                self.deliver(digest, Delivery::CatchUp { cause }, entry, out);
            }
        }
    }

    /// Plan one catch-up reply to the owed `Event(cause)`.
    ///
    /// `CallProgram` heads resolve through `cause` in the scratch `Heads`.
    /// A poisoned or out-of-sequence reply fails the digest, which drops the
    /// activation's catch-up records and rejects the head.
    pub(crate) fn catch_up_evaluated(
        &mut self,
        cause: u64,
        replied: u64,
        evaluated: Evaluated,
        out: &mut Vec<Command>,
    ) {
        let Some((digest, next)) = self.routing.catch_up().map(|(digest, catch_up)| (digest, catch_up.next)) else {
            self.abort(format!("catch-up reply for {cause} arrived with no catch-up in progress"), out);
            return;
        };
        if next != cause {
            self.abort(format!("catch-up reply for {cause} arrived while owing {next}"), out);
            return;
        }
        let planned = match evaluated {
            _ if replied != cause => {
                let reason = Detail::new(format!("catch-up evaluated seq {replied} does not match {cause}"));
                self.fail_reactor(digest, Health::Untrusted, reason);
                return;
            }
            Evaluated::Completed { intents, .. } => {
                let Some((_, catch_up)) = self.routing.catch_up() else {
                    return;
                };
                plan_intents(digest, cause, intents, &catch_up.scratch)
            }
            Evaluated::Failed { reactor, reason, .. } => {
                vec![PlannedIntent::Ready(reaction_failed(cause, digest, Some(reactor), reason))]
            }
            Evaluated::Poisoned { reason, .. } => {
                self.fail_reactor(digest, Health::Poisoned, reason);
                return;
            }
            Evaluated::OutOfSequence { .. } => {
                let reason = Detail::new(format!("catch-up out-of-sequence at {cause}"));
                self.fail_reactor(digest, Health::Untrusted, reason);
                return;
            }
        };
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        let head_index = work.activate_index;
        work.plan_reply(
            |index| PlanOrder::Activation { head_index, step: ActivationStep::CatchUp { cause, index } },
            planned,
        );
        if let Some(ActivationWork { phase: ActivationPhase::CatchingUp(catch_up), .. }) = work.activation.as_mut() {
            catch_up.next = cause + 1;
        }
        self.advance_instance(digest, cause);
        self.drive_catch_up(out);
    }

    /// Record the current head's `Activated` and move on to the next head.
    fn activate_head(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        let Some(ActivationWork { head, bundle, live_from, .. }) = work.activation.take() else {
            return;
        };
        match Activated::new(head, bundle, Seq(live_from)) {
            Ok(record) => work.finish_head(DriverRecord::Activated { cause: work.n, record }),
            Err(error) => self.abort(format!("activation live_from {live_from} refused: {error:?}"), out),
        }
    }

    /// Reject the activation in progress with `reason`.
    pub(crate) fn reject_activation(&mut self, reason: Detail) {
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        if let Some(ActivationWork { head, bundle, .. }) = work.activation.take() {
            work.reject_head(head, bundle, reason);
        }
    }
}
