//! Reply-to-plan mapping: intents to `Requested`, moves, and failures (ADR-0226 decisions 6 and 8).

use std::collections::BTreeMap;

use aether_bloomery_kinds::{
    CallProgram, Detail, Digest, DriverRecord, ProgramRef, ReactionFailed, ReactorIntent, ReactorName,
    ReadArtifactResult, RequestSource, Requested, RuleName, SetHead,
};
use aether_bloomery_view::Heads;
use aether_data::Kind;

use crate::bundles::InstanceState;
use crate::core::{ArtifactRead, ArtifactTicket, Command, PlannedRecord, ProgramCore};
use crate::reactors::{PendingDestination, PlanOrder};

/// One intent's slot in the plan: the bundle that returned it, its cause, and its order key.
struct IntentSlot {
    /// Bundle whose rule returned the intent.
    bundle: Digest,
    /// Trigger or catch-up seq causing the record.
    cause: u64,
    /// Order key for the resolved record.
    order: PlanOrder,
}

impl ProgramCore {
    /// Map one live `Completed` reply's intents into the seq plan.
    pub(crate) fn map_live_completed(
        &mut self,
        digest: Digest,
        trigger: u64,
        intents: Vec<ReactorIntent>,
        out: &mut Vec<Command>,
    ) {
        let Some(work) = self.routing.current.as_ref() else {
            self.abort(format!("completed for {digest} arrived with no seq in progress"), out);
            return;
        };
        if work.n != trigger {
            self.abort(format!("completed for trigger {trigger} arrived during seq {}", work.n), out);
            return;
        }
        let heads = self.routing.heads.clone();
        let mut ordinals: BTreeMap<(ReactorName, RuleName), u32> = BTreeMap::new();
        for (index, intent) in intents.into_iter().enumerate() {
            let slot = IntentSlot { bundle: digest, cause: trigger, order: PlanOrder::Live { digest, index } };
            self.map_one_intent(slot, intent, &heads, &mut ordinals, out);
            if self.aborted {
                return;
            }
        }
    }

    /// Map one live `Failed` reply into the seq plan.
    pub(crate) fn map_live_failed(
        &mut self,
        digest: Digest,
        trigger: u64,
        reactor: ReactorName,
        reason: Detail,
        out: &mut Vec<Command>,
    ) {
        let Some(work) = self.routing.current.as_ref() else {
            self.abort(format!("failed for {digest} arrived with no seq in progress"), out);
            return;
        };
        if work.n != trigger {
            self.abort(format!("failed for trigger {trigger} arrived during seq {}", work.n), out);
            return;
        }
        let record = DriverRecord::ReactionFailed {
            cause: trigger,
            record: ReactionFailed { bundle: digest, reactor: Some(reactor), reason },
        };
        if let Some(work) = self.routing.current.as_mut() {
            work.order.insert(PlanOrder::Live { digest, index: 0 }, PlannedRecord::Ready(record));
        }
    }

    /// Poison one live digest: fail its reaction and reject every head it served.
    pub(crate) fn poison_live_digest(&mut self, digest: Digest, trigger: u64, reason: Detail, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_ref() else {
            self.abort(format!("poison for {digest} arrived with no seq in progress"), out);
            return;
        };
        if work.n != trigger {
            self.abort(format!("poison for trigger {trigger} arrived during seq {}", work.n), out);
            return;
        }
        let Some(served) = work.live.get(&digest).cloned() else {
            self.abort(format!("poison for unevaluated digest {digest} at seq {trigger}"), out);
            return;
        };
        let failed = DriverRecord::ReactionFailed {
            cause: trigger,
            record: ReactionFailed { bundle: digest, reactor: None, reason: reason.clone() },
        };
        if let Some(work) = self.routing.current.as_mut() {
            work.order.insert(PlanOrder::Live { digest, index: 0 }, PlannedRecord::Ready(failed));
            for (position, head) in served.iter().enumerate() {
                let rejected = DriverRecord::ActivationRejected {
                    cause: trigger,
                    record: aether_bloomery_kinds::ActivationRejected {
                        head: head.clone(),
                        bundle: digest,
                        reason: reason.clone(),
                    },
                };
                work.order.insert(PlanOrder::Live { digest, index: 1 + position }, PlannedRecord::Ready(rejected));
            }
        }
        if let Some(instance) = self.bundles.instance_mut(&digest) {
            instance.state = InstanceState::Poisoned { reason };
        } else {
            self.abort(format!("poison for unknown digest {digest}"), out);
            return;
        }
        self.drive_routing(out);
    }

    /// Map one catch-up `Completed` reply's intents into the seq plan.
    pub(crate) fn map_catchup_completed(
        &mut self,
        digest: Digest,
        cause: u64,
        head_index: usize,
        intents: Vec<ReactorIntent>,
        heads: &Heads,
        out: &mut Vec<Command>,
    ) {
        let mut ordinals: BTreeMap<(ReactorName, RuleName), u32> = BTreeMap::new();
        for (index, intent) in intents.into_iter().enumerate() {
            let slot = IntentSlot { bundle: digest, cause, order: PlanOrder::Activation { head_index, cause, index } };
            self.map_one_intent(slot, intent, heads, &mut ordinals, out);
            if self.aborted {
                return;
            }
        }
    }

    /// Map one catch-up `Failed` reply into the seq plan.
    pub(crate) fn map_catchup_failed(
        &mut self,
        digest: Digest,
        cause: u64,
        head_index: usize,
        reactor: ReactorName,
        reason: Detail,
    ) {
        let record = DriverRecord::ReactionFailed {
            cause,
            record: ReactionFailed { bundle: digest, reactor: Some(reactor), reason },
        };
        if let Some(work) = self.routing.current.as_mut() {
            work.order.insert(PlanOrder::Activation { head_index, cause, index: 0 }, PlannedRecord::Ready(record));
        }
    }

    /// Continue one `SetHead` destination check.
    pub(crate) fn continue_destination_artifact(
        &mut self,
        cause: u64,
        bundle: Digest,
        reactor: ReactorName,
        set_head: SetHead,
        result: ReadArtifactResult,
        out: &mut Vec<Command>,
    ) {
        let Some(work) = self.routing.current.as_mut() else {
            self.abort("set_head destination arrived with no seq in progress".to_string(), out);
            return;
        };
        let Some((_, pending)) = work.current_destination.take() else {
            self.abort("set_head destination arrived with none outstanding".to_string(), out);
            return;
        };
        if pending.cause != cause
            || pending.bundle != bundle
            || pending.reactor != reactor
            || pending.set_head.head() != set_head.head()
            || pending.set_head.from() != set_head.from()
            || pending.set_head.to() != set_head.to()
        {
            self.abort("set_head destination arrived for a different intent".to_string(), out);
            return;
        }
        let order = pending.order;
        match result {
            ReadArtifactResult::Found { kind, .. } => {
                if kind == set_head.head().kind() {
                    work.order.insert(
                        order,
                        PlannedRecord::SetHead { cause, bundle, reactor, set_head, destination_ok: true },
                    );
                } else {
                    let failed = DriverRecord::ReactionFailed {
                        cause,
                        record: ReactionFailed {
                            bundle,
                            reactor: Some(reactor),
                            reason: Detail::new("set_head destination has the wrong kind"),
                        },
                    };
                    work.order.insert(order, PlannedRecord::Ready(failed));
                }
                self.drive_routing(out);
            }
            ReadArtifactResult::Missing { .. } => {
                let failed = DriverRecord::ReactionFailed {
                    cause,
                    record: ReactionFailed {
                        bundle,
                        reactor: Some(reactor),
                        reason: Detail::new("set_head destination is missing"),
                    },
                };
                work.order.insert(order, PlannedRecord::Ready(failed));
                self.drive_routing(out);
            }
            ReadArtifactResult::Err { message, .. } => {
                self.abort(format!("set_head destination read failed: {message}"), out);
            }
        }
    }

    /// Map one intent to its ready record or queued destination check.
    fn map_one_intent(
        &mut self,
        slot: IntentSlot,
        intent: ReactorIntent,
        heads: &Heads,
        ordinals: &mut BTreeMap<(ReactorName, RuleName), u32>,
        out: &mut Vec<Command>,
    ) {
        let IntentSlot { bundle, cause, order } = slot;
        let (reactor, rule, kind, bytes) = intent.into_parts();
        let ordinal = ordinals.get(&(reactor.clone(), rule.clone())).copied().unwrap_or(0);
        ordinals.insert((reactor.clone(), rule.clone()), ordinal + 1);
        if kind == CallProgram::ID {
            let Some(call) = CallProgram::decode_from_bytes(&bytes) else {
                self.insert_failed(bundle, cause, reactor, Detail::new("undecodable call_program intent"), order);
                return;
            };
            let Some(bound) = heads.get(&call.program) else {
                self.insert_failed(bundle, cause, reactor, Detail::new("program head unbound"), order);
                return;
            };
            let requested = Requested {
                program: ProgramRef::new(bound.digest(), call.name),
                input: call.input,
                source: RequestSource::Reaction { bundle, reactor, rule, ordinal },
            };
            let record = DriverRecord::Requested { cause: Some(cause), record: requested };
            if let Some(work) = self.routing.current.as_mut() {
                work.order.insert(order, PlannedRecord::Ready(record));
            }
            let _ = out;
        } else if kind == SetHead::ID {
            let Some(set_head) = SetHead::decode_from_bytes(&bytes) else {
                self.insert_failed(bundle, cause, reactor, Detail::new("undecodable set_head intent"), order);
                return;
            };
            if let Some(work) = self.routing.current.as_mut() {
                work.pending_setheads.push_back(PendingDestination { order, cause, bundle, reactor, set_head });
            }
        } else {
            self.insert_failed(
                bundle,
                cause,
                reactor,
                Detail::new(format!("unsupported intent kind {}", kind.0)),
                order,
            );
        }
    }

    /// Insert one `ReactionFailed` for a refused intent.
    fn insert_failed(&mut self, digest: Digest, cause: u64, reactor: ReactorName, reason: Detail, order: PlanOrder) {
        let failed = DriverRecord::ReactionFailed {
            cause,
            record: ReactionFailed { bundle: digest, reactor: Some(reactor), reason },
        };
        if let Some(work) = self.routing.current.as_mut() {
            work.order.insert(order, PlannedRecord::Ready(failed));
        }
    }

    /// Issue one queued destination check, if any outstanding slot is free.
    pub(crate) fn emit_next_destination(&mut self, out: &mut Vec<Command>) {
        let Some(work) = self.routing.current.as_mut() else {
            return;
        };
        if work.current_destination.is_some() {
            return;
        }
        let Some(pending) = work.pending_setheads.pop_front() else {
            return;
        };
        let digest = pending.set_head.to();
        let ticket = self.mint(ArtifactTicket::mint);
        self.artifact_reads.insert(
            ticket,
            ArtifactRead::SetHeadDestination {
                cause: pending.cause,
                bundle: pending.bundle,
                reactor: pending.reactor.clone(),
                set_head: pending.set_head.clone(),
            },
        );
        if let Some(work) = self.routing.current.as_mut() {
            work.current_destination = Some((ticket, pending));
        }
        out.push(Command::ReadArtifact { ticket, request: aether_bloomery_kinds::ReadArtifact { digest } });
    }
}
