//! Live delivery: `Event` fan-out, reply collection, and status resync (ADR-0226 decision 5).

use std::collections::BTreeMap;

use aether_bloomery_kinds::{Detail, Digest, Evaluated, Event, Head, JournalEntry, OpaqueBytes, Status};

use crate::bundles::InstanceState;
use crate::core::{Command, EvaluateTicket, ProgramCore, StatusTicket};
use crate::reactors::{EvaluateContext, StatusContext};

impl ProgramCore {
    /// Feed one evaluation reply. Unknown tickets return no commands.
    pub fn on_evaluated(&mut self, ticket: EvaluateTicket, evaluated: Evaluated) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(context) = self.routing.evaluates.remove(&ticket) else {
            return out;
        };
        if context.head.is_some() {
            self.handle_catchup_evaluated(context, evaluated, &mut out);
        } else {
            self.handle_live_evaluated(&context, evaluated, &mut out);
        }
        out
    }

    /// Feed one status reply. Unknown tickets return no commands.
    pub fn on_status(&mut self, ticket: StatusTicket, status: &Status) -> Vec<Command> {
        let mut out = Vec::new();
        if self.aborted {
            return out;
        }
        let Some(context) = self.routing.statuses.remove(&ticket) else {
            return out;
        };
        let digest = context.digest;
        let trigger = context.trigger;
        let current = self.routing.current.as_ref().map(|work| work.n);
        let Some(n) = current else {
            self.abort(format!("status for {digest} arrived with no seq in progress"), &mut out);
            return out;
        };
        if n != trigger {
            self.abort(format!("status for trigger {trigger} arrived during seq {n}"), &mut out);
            return out;
        }
        if status.poisoned() {
            let reason = Detail::new("reactor reports poisoned");
            self.poison_live_digest(digest, trigger, reason, &mut out);
            return out;
        }
        if status.cursor() == trigger - 1 {
            let Some(entry) = self.routing.current.as_ref().map(|work| work.entry.clone()) else {
                self.abort("status resync found no seq work".to_string(), &mut out);
                return out;
            };
            let root = self.bundles.instance(&digest).and_then(|instance| match instance.state {
                InstanceState::Ready { root } => Some(root),
                _ => None,
            });
            let Some(root) = root else {
                self.abort(format!("status resync for unready digest {digest}"), &mut out);
                return out;
            };
            let ticket = self.mint(EvaluateTicket::mint);
            self.routing.evaluates.insert(ticket, EvaluateContext { digest, trigger, cause: trigger, head: None });
            out.push(Command::Evaluate { ticket, root, request: Event::new(entry) });
            return out;
        }
        let reason = Detail::new(format!(
            "status cursor {} after out-of-sequence for {trigger}, expected {}",
            status.cursor(),
            trigger - 1
        ));
        self.poison_live_digest(digest, trigger, reason, &mut out);
        out
    }

    /// Emit one live `Event(N)` per distinct digest, all in flight together.
    pub(crate) fn emit_live_events(
        &mut self,
        trigger: u64,
        entry: &JournalEntry,
        live: &BTreeMap<Digest, Vec<Head<OpaqueBytes>>>,
        out: &mut Vec<Command>,
    ) -> Result<(), String> {
        for digest in live.keys() {
            let Some(instance) = self.bundles.instance(digest) else {
                return Err(format!("live digest {digest} has no instance for seq {trigger}"));
            };
            let InstanceState::Ready { root } = instance.state else {
                return Err(format!("live digest {digest} is not ready for seq {trigger}"));
            };
            if instance.cursor != trigger - 1 {
                return Err(format!(
                    "live instance {digest} cursor {} is not {} for seq {trigger}",
                    instance.cursor,
                    trigger - 1
                ));
            }
            let ticket = self.mint(EvaluateTicket::mint);
            self.routing
                .evaluates
                .insert(ticket, EvaluateContext { digest: *digest, trigger, cause: trigger, head: None });
            out.push(Command::Evaluate { ticket, root, request: Event::new(entry.clone()) });
        }
        Ok(())
    }

    /// Handle one live `Evaluated` reply by digest role and seq.
    fn handle_live_evaluated(&mut self, context: &EvaluateContext, evaluated: Evaluated, out: &mut Vec<Command>) {
        let digest = context.digest;
        let trigger = context.trigger;
        let reply_seq = match &evaluated {
            Evaluated::Completed { seq, .. }
            | Evaluated::OutOfSequence { seq, .. }
            | Evaluated::Poisoned { seq, .. }
            | Evaluated::Failed { seq, .. } => *seq,
        };
        if reply_seq != trigger {
            let reason = Detail::new(format!("evaluated seq {reply_seq} does not match trigger {trigger}"));
            self.poison_live_digest(digest, trigger, reason, out);
            return;
        }
        match evaluated {
            Evaluated::Completed { intents, .. } => {
                self.map_live_completed(digest, trigger, intents, out);
                if self.aborted {
                    return;
                }
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.cursor = trigger;
                }
                self.drive_routing(out);
            }
            Evaluated::Failed { reactor, reason, .. } => {
                self.map_live_failed(digest, trigger, reactor, reason, out);
                if self.aborted {
                    return;
                }
                if let Some(instance) = self.bundles.instance_mut(&digest) {
                    instance.cursor = trigger;
                }
                self.drive_routing(out);
            }
            Evaluated::Poisoned { reason, .. } => {
                self.poison_live_digest(digest, trigger, reason, out);
            }
            Evaluated::OutOfSequence { .. } => {
                let Some(instance) = self.bundles.instance(&digest) else {
                    self.abort(format!("out-of-sequence for unknown digest {digest}"), out);
                    return;
                };
                let InstanceState::Ready { root } = instance.state else {
                    self.abort(format!("out-of-sequence for unready digest {digest}"), out);
                    return;
                };
                let ticket = self.mint(StatusTicket::mint);
                self.routing.statuses.insert(ticket, StatusContext { digest, trigger });
                out.push(Command::QueryStatus { ticket, root });
            }
        }
    }
}
