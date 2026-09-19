//! Activation: loads, `Warm` paging, reuse, rejections, and owed catch-up (ADR-0226 decision 5).

use std::collections::VecDeque;

use aether_bloomery_kinds::{
    Activated, Detail, Digest, DriverRecord, Evaluated, Head, JournalEntry, OpaqueBytes, ReadArtifact,
    ReadArtifactResult, Seq, Warm, WarmEntries, Warmed,
};
use aether_bloomery_view::{HeadActivation, Heads};
use aether_data::Kind;

use crate::bundles::InstanceState;
use crate::core::{ArtifactRead, ArtifactTicket, Command, LoadOutcome, LoadTicket, ProgramCore, WarmTicket};
use crate::reactors::intents::{PlannedIntent, plan_intents, reaction_failed};
use crate::reactors::{
    ActivationPhase, ActivationStep, ActivationWork, CatchUp, Delivery, PlanOrder, RoutingRead, SeqPhase, WarmBatch,
};

/// How one changed head's activation starts.
enum Start {
    /// Reject the head without touching its digest.
    Reject(Detail),
    /// Read and load the digest first.
    Load,
    /// Warm the digest's ready root from its cursor.
    Warm,
    /// The digest's load is already in flight, which serial activation never leaves behind.
    Busy,
}

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
                let cursor = self.bundles.instance(&digest).map(|instance| instance.cursor);
                if through != last || cursor.map(|cursor| cursor + 1) != Some(first) {
                    let reason =
                        format!("warm of {digest} over {first}..={last} from {cursor:?} folded through {through}");
                    self.abort(reason, &mut out);
                    return out;
                }
                self.advance_instance(digest, through);
            }
            Warmed::Poisoned { reason, .. } => self.fail_instance(digest, InstanceState::Poisoned, reason),
            Warmed::OutOfSequence { first, expected } => {
                let reason = Detail::new(format!("warm out-of-sequence: first {first}, expected {expected}"));
                self.fail_instance(digest, InstanceState::Unavailable, reason);
            }
        }
        self.drive_routing(&mut out);
        out
    }

    /// Continue one reactor bundle artifact read: load it, or mark the digest unavailable.
    pub(crate) fn continue_reactor_artifact(
        &mut self,
        digest: Digest,
        result: ReadArtifactResult,
        out: &mut Vec<Command>,
    ) {
        let reason = match result {
            ReadArtifactResult::Found { kind, bytes, .. } if kind == OpaqueBytes::ID => {
                let ticket = self.mint(LoadTicket::mint);
                self.loads.insert(ticket, digest);
                self.bundles.set_reactor_state(&digest, InstanceState::Loading);
                out.push(Command::Load { ticket, bundle: digest, wasm: bytes });
                return;
            }
            ReadArtifactResult::Found { .. } => Detail::new("bundle artifact has the wrong kind"),
            ReadArtifactResult::Missing { .. } => Detail::new("bundle artifact is missing"),
            ReadArtifactResult::Err { message, .. } => Detail::new(message),
        };
        self.fail_instance(digest, InstanceState::Unavailable, reason);
        self.drive_routing(out);
    }

    /// Continue one reactor load.
    pub(crate) fn continue_reactor_loaded(&mut self, digest: Digest, outcome: LoadOutcome, out: &mut Vec<Command>) {
        match outcome {
            LoadOutcome::Loaded { root } => {
                self.bundles.set_reactor_state(&digest, InstanceState::Ready { root });
                if let Some(activation) = self.routing.current.as_mut().and_then(|work| work.activation.as_mut()) {
                    activation.phase = ActivationPhase::Warming;
                }
            }
            LoadOutcome::Failed { error } => self.fail_instance(digest, InstanceState::Unavailable, Detail::new(error)),
        }
        self.drive_routing(out);
    }

    /// Mark `digest` failed with `failed` and reject what waited on it: the
    /// activation in progress while routing, or the restart's warming digest
    /// before it.
    fn fail_instance(&mut self, digest: Digest, failed: fn(Detail) -> InstanceState, reason: Detail) {
        self.bundles.set_reactor_state(&digest, failed(reason.clone()));
        if self.routing.started {
            self.reject_activation(reason);
        } else {
            self.fail_restart_digest(&reason);
        }
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
                self.abort("activation is loading with no read or load outstanding".to_string(), out);
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

    /// Start activating one changed head: reject it, reuse its ready root, or read its bundle.
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
        let start = self.bundles.claim_reactor(bundle).map_or_else(
            || Start::Reject(Detail::new("digest is loaded as a program bundle")),
            |instance| match &instance.state {
                InstanceState::Poisoned(reason) | InstanceState::Unavailable(reason) => Start::Reject(reason.clone()),
                InstanceState::Ready { .. } if instance.cursor >= live_from => {
                    Start::Reject(Detail::new("shared instance is past the owed start"))
                }
                InstanceState::Ready { .. } => Start::Warm,
                InstanceState::Reading => Start::Load,
                InstanceState::Loading => Start::Busy,
            },
        );
        let load = matches!(start, Start::Load);
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        let phase = match start {
            Start::Reject(reason) => {
                work.reject_head(head, bundle, reason);
                return;
            }
            Start::Busy => {
                self.abort(format!("activation for {bundle} found its load already in flight"), out);
                return;
            }
            Start::Warm => ActivationPhase::Warming,
            Start::Load => ActivationPhase::Loading,
        };
        work.activation = Some(ActivationWork { head, bundle, live_from, phase });
        if load {
            let ticket = self.mint(ArtifactTicket::mint);
            self.artifact_reads.insert(ticket, ArtifactRead::ReactorBundle(bundle));
            out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest: bundle } });
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
        let cursor = self.bundles.instance(&digest).map_or(0, |instance| instance.cursor);
        if cursor + 1 < live_from {
            self.emit_routing_read(cursor, RoutingRead::Warm, out);
        } else if live_from > trigger {
            self.activate_head(out);
        } else {
            if let Some(activation) = self.routing.current.as_mut().and_then(|work| work.activation.as_mut()) {
                let catch_up = CatchUp { scratch: Heads::new(), next: live_from, page: VecDeque::new() };
                activation.phase = ActivationPhase::CatchingUp(catch_up);
            }
            self.emit_routing_read(0, RoutingRead::CatchUp, out);
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
        let Some(&InstanceState::Ready { root }) = self.bundles.instance(&digest).map(|instance| &instance.state)
        else {
            self.abort(format!("warm for digest {digest}, which has no ready root"), out);
            return;
        };
        let ticket = self.mint(WarmTicket::mint);
        self.routing.warms.insert(ticket, WarmBatch { digest, first: batch.first(), last: batch.last() });
        out.push(Command::Warm { ticket, root, request: Warm::new(batch) });
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
                self.fail_instance(digest, InstanceState::Unavailable, reason);
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
                self.fail_instance(digest, InstanceState::Poisoned, reason);
                return;
            }
            Evaluated::OutOfSequence { .. } => {
                let reason = Detail::new(format!("catch-up out-of-sequence at {cause}"));
                self.fail_instance(digest, InstanceState::Unavailable, reason);
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
    fn reject_activation(&mut self, reason: Detail) {
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        if let Some(ActivationWork { head, bundle, .. }) = work.activation.take() {
            work.reject_head(head, bundle, reason);
        }
    }
}
