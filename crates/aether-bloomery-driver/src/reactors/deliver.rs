//! Delivery: `Event` sends, live replies, and status resync (ADR-0226 decision 5).

use std::iter::once;

use aether_bloomery_kinds::{ActivationRejected, Detail, Digest, DriverRecord, Evaluated, Event, JournalEntry, Status};

use crate::core::{Command, EvaluateTicket, ProgramCore, StatusTicket};
use crate::reactors::instance::Health;
use crate::reactors::intents::{PlannedIntent, plan_intents, reaction_failed};
use crate::reactors::{Delivery, PlanOrder};

/// The seq an `Evaluated` reply answers.
fn replied_seq(evaluated: &Evaluated) -> u64 {
    match evaluated {
        Evaluated::Completed { seq, .. }
        | Evaluated::OutOfSequence { seq, .. }
        | Evaluated::Poisoned { seq, .. }
        | Evaluated::Failed { seq, .. } => *seq,
    }
}

impl ProgramCore {
    /// Feed one evaluation reply. Unknown tickets return no commands.
    pub fn on_evaluated(&mut self, ticket: EvaluateTicket, evaluated: Evaluated) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(delivery) = self.routing.deliveries.remove(&ticket) else {
            return out;
        };
        let Some(trigger) = self.routing.current.as_ref().map(|work| work.n) else {
            self.abort("evaluation reply arrived with no seq in progress".to_string(), &mut out);
            return out;
        };
        let replied = replied_seq(&evaluated);
        match delivery {
            Delivery::Live { digest } if replied != trigger => {
                let reason = Detail::new(format!("evaluated seq {replied} does not match delivered seq {trigger}"));
                self.poison_live(digest, reason, &mut out);
            }
            Delivery::Live { digest } => self.live_evaluated(digest, trigger, evaluated, &mut out),
            Delivery::CatchUp { cause } => self.catch_up_evaluated(cause, replied, evaluated, &mut out),
        }
        self.drive_routing(&mut out);
        out
    }

    /// Feed one status reply. Unknown tickets return no commands.
    ///
    /// A root still at `N-1` missed `Event(N)` and gets it again; any other
    /// answer means its fold can no longer be trusted, so it is poisoned.
    pub fn on_status(&mut self, ticket: StatusTicket, status: &Status) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(digest) = self.routing.statuses.remove(&ticket) else {
            return out;
        };
        let Some((trigger, entry)) = self.routing.current.as_ref().map(|work| (work.n, work.entry.clone())) else {
            self.abort(format!("status for {digest} arrived with no seq in progress"), &mut out);
            return out;
        };
        if status.poisoned() {
            self.poison_live(digest, Detail::new("reactor reports poisoned"), &mut out);
        } else if status.cursor() + 1 == trigger {
            self.deliver(digest, Delivery::Live { digest }, entry, &mut out);
        } else {
            let reason = format!("status cursor {} after out-of-sequence for {trigger}", status.cursor());
            self.poison_live(digest, Detail::new(reason), &mut out);
        }
        self.drive_routing(&mut out);
        out
    }

    /// Send `entry` to `digest`'s live root.
    pub(crate) fn deliver(&mut self, digest: Digest, delivery: Delivery, entry: JournalEntry, out: &mut Vec<Command>) {
        if !self.live_ready(digest) {
            self.abort(format!("event {} for digest {digest}, which has no ready root", entry.seq), out);
            return;
        }
        let ticket = self.mint(EvaluateTicket::mint);
        self.routing.deliveries.insert(ticket, delivery);
        out.push(Command::Evaluate { ticket, bundle: digest, request: Event::new(entry) });
    }

    /// Plan one live reply to `Event(N)`.
    fn live_evaluated(&mut self, digest: Digest, trigger: u64, evaluated: Evaluated, out: &mut Vec<Command>) {
        let planned = match evaluated {
            Evaluated::Completed { intents, .. } => plan_intents(digest, trigger, intents, &self.routing.heads),
            Evaluated::Failed { reactor, reason, .. } => {
                vec![PlannedIntent::Ready(reaction_failed(trigger, digest, Some(reactor), reason))]
            }
            Evaluated::Poisoned { reason, .. } => {
                self.poison_live(digest, reason, out);
                return;
            }
            Evaluated::OutOfSequence { .. } => {
                if !self.live_ready(digest) {
                    self.abort(format!("out-of-sequence from digest {digest}, which has no ready root"), out);
                    return;
                }
                let ticket = self.mint(StatusTicket::mint);
                self.routing.statuses.insert(ticket, digest);
                out.push(Command::QueryStatus { ticket, bundle: digest });
                return;
            }
        };
        if let Some(work) = self.routing.current.as_mut() {
            work.plan_reply(|index| PlanOrder::Live { digest, index }, planned);
        }
        self.advance_instance(digest, trigger);
    }

    /// Record that `digest`'s root has evaluated `seq`.
    pub(crate) fn advance_instance(&mut self, digest: Digest, seq: u64) {
        if let Some(instance) = self.routing.instances.get_mut(&digest) {
            instance.cursor = seq;
        }
    }

    /// Poison one live digest: fail its reaction and reject every head it served at `N`.
    fn poison_live(&mut self, digest: Digest, reason: Detail, out: &mut Vec<Command>) {
        let Some((cause, served)) = self.routing.current.as_ref().map(|work| (work.n, work.live.get(&digest).cloned()))
        else {
            self.abort(format!("poison for {digest} arrived with no seq in progress"), out);
            return;
        };
        let Some(served) = served else {
            self.abort(format!("poison for digest {digest}, which did not evaluate seq {cause}"), out);
            return;
        };
        let failed = reaction_failed(cause, digest, None, reason.clone());
        let rejected = served.into_iter().map(|head| DriverRecord::ActivationRejected {
            cause,
            record: ActivationRejected { head, bundle: digest, reason: reason.clone() },
        });
        let planned = once(failed).chain(rejected).map(PlannedIntent::Ready).collect();
        if let Some(work) = self.routing.current.as_mut() {
            work.plan_reply(|index| PlanOrder::Live { digest, index }, planned);
        }
        if let Some(instance) = self.routing.instances.get_mut(&digest) {
            instance.health = Health::Poisoned(reason);
        }
    }
}
