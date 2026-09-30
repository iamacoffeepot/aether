//! Reply-to-plan mapping: intents to `Requested`, moves, and failures (ADR-0226 decisions 6 and 8).

use std::collections::{BTreeMap, BTreeSet};

use aether_bloomery_kinds::{
    CallInput, CallProgram, Detail, Digest, DriverRecord, EncodedArtifact, LEGACY_CALL_PROGRAM_ID, LEGACY_SET_HEAD_ID,
    ProgramRef, ReactionFailed, ReactorIntent, ReactorName, ReadArtifact, ReadArtifactResult, RequestSource, Requested,
    RuleName, SetHeads, decode_call_program, decode_set_heads,
};
use aether_bloomery_program::Heads;
use aether_data::{Kind, KindId};

use crate::runtime::core::{ArtifactRead, ArtifactTicket, Command, PlannedRecord, ProgramCore};
use crate::runtime::reactors::PendingDestination;

/// One intent's plan: a ready record, a supplied-input call, or a `SetHeads`
/// awaiting its destination check.
pub enum PlannedIntent {
    /// A record appended as decided.
    Ready(DriverRecord),
    /// A call whose captured input is staged with its `Requested` record.
    SuppliedCall { input: EncodedArtifact, record: DriverRecord },
    /// A move whose destination must be read before derivation.
    Destination(PendingDestination),
}

/// A `ReactionFailed` for one reply of `bundle`, caused by `cause`.
///
/// `reactor` is `None` when the whole bundle failed rather than one reactor.
pub fn reaction_failed(cause: u64, bundle: Digest, reactor: Option<ReactorName>, reason: Detail) -> DriverRecord {
    DriverRecord::ReactionFailed { cause, record: ReactionFailed { bundle, reactor, reason } }
}

/// Plan one `Completed` reply's intents in reply order.
///
/// `CallProgram` resolves its program head through `heads`, the prefix
/// through `cause`. Ordinals count each `(reactor, rule)`'s intents within
/// the reply. An unbound head, an undecodable payload, or an unsupported
/// kind fails that intent alone.
pub fn plan_intents(bundle: Digest, cause: u64, intents: Vec<ReactorIntent>, heads: &Heads) -> Vec<PlannedIntent> {
    let mut ordinals: BTreeMap<(ReactorName, RuleName), u32> = BTreeMap::new();
    intents
        .into_iter()
        .map(|intent| {
            let (reactor, rule, kind, bytes) = intent.into_parts();
            let next = ordinals.entry((reactor.clone(), rule.clone())).or_insert(0);
            let ordinal = *next;
            *next += 1;
            let failed =
                |reason: Detail| PlannedIntent::Ready(reaction_failed(cause, bundle, Some(reactor.clone()), reason));
            if kind == SetHeads::ID || kind == LEGACY_SET_HEAD_ID {
                let Some(set_heads) = decode_set_heads(kind, &bytes) else {
                    return failed(Detail::new("undecodable set_heads intent"));
                };
                if set_heads.changes().is_empty() {
                    return failed(Detail::new("set_heads group is empty"));
                }
                let mut heads = BTreeSet::new();
                if !set_heads.changes().iter().all(|change| heads.insert(change.head().clone())) {
                    return failed(Detail::new("set_heads group contains a duplicate head"));
                }
                return PlannedIntent::Destination(PendingDestination {
                    cause,
                    bundle,
                    reactor: reactor.clone(),
                    set_heads,
                    next_change: 0,
                });
            }
            if kind != CallProgram::ID && kind != LEGACY_CALL_PROGRAM_ID {
                return failed(Detail::new(format!("unsupported intent kind {}", kind.0)));
            }
            let Some(call) = decode_call_program(kind, &bytes) else {
                return failed(Detail::new("undecodable call_program intent"));
            };
            let Some(bound) = heads.get(&call.program) else {
                return failed(Detail::new("program head unbound"));
            };
            let program = ProgramRef::new(bound.digest(), call.name);
            let source = RequestSource::Reaction { bundle, reactor, rule, ordinal };
            match call.input {
                CallInput::Stored(input) => PlannedIntent::Ready(DriverRecord::Requested {
                    cause: Some(cause),
                    record: Requested { program, input, source },
                }),
                CallInput::Value(input) => {
                    let digest = input.digest();
                    PlannedIntent::SuppliedCall {
                        input,
                        record: DriverRecord::Requested {
                            cause: Some(cause),
                            record: Requested { program, input: digest, source },
                        },
                    }
                }
            }
        })
        .collect()
}

/// Result of checking one destination in an atomic group.
enum DestinationStep {
    /// More destinations remain in the group.
    Continue(PendingDestination),
    /// The group is accepted for derivation or refused once.
    Complete(PlannedRecord),
}

/// Check one `SetHeads` destination against its stored kind.
///
/// Every destination stored under its target head's kind leaves the whole
/// group for derivation. A missing or wrong-kind destination refuses it once.
fn plan_destination(mut pending: PendingDestination, stored: Option<KindId>) -> DestinationStep {
    let change = &pending.set_heads.changes()[pending.next_change];
    let reason = match stored {
        Some(kind) if kind == change.head().kind() => {
            pending.next_change += 1;
            if pending.next_change < pending.set_heads.changes().len() {
                return DestinationStep::Continue(pending);
            }
            return DestinationStep::Complete(PlannedRecord::SetHeads {
                cause: pending.cause,
                bundle: pending.bundle,
                reactor: pending.reactor,
                set_heads: pending.set_heads,
            });
        }
        Some(_) => "set_heads destination has the wrong kind",
        None => "set_heads destination is missing",
    };
    DestinationStep::Complete(PlannedRecord::Ready(reaction_failed(
        pending.cause,
        pending.bundle,
        Some(pending.reactor),
        Detail::new(reason),
    )))
}

impl ProgramCore {
    /// Check the next queued `SetHeads` destination, in plan order.
    ///
    /// A destination in the artifact cache is planned at once, with no read;
    /// any other is read from the journal. Returns `false` when none is
    /// queued.
    pub(crate) fn check_next_destination(&mut self, out: &mut Vec<Command>) -> bool {
        let Some((order, pending)) = self.routing.current.as_mut().and_then(|work| work.destinations.pop_first())
        else {
            return false;
        };
        let digest = pending.set_heads.changes()[pending.next_change].to();
        if let Some(kind) = self.artifacts.kind(digest) {
            if let Some(work) = self.routing.current.as_mut() {
                match plan_destination(pending, Some(kind)) {
                    DestinationStep::Continue(pending) => {
                        work.destinations.insert(order, pending);
                    }
                    DestinationStep::Complete(planned) => {
                        work.order.insert(order, planned);
                    }
                }
            }
            return true;
        }
        let ticket = self.mint(ArtifactTicket::mint);
        self.artifact_reads.insert(ticket, ArtifactRead::SetHeadsDestination);
        if let Some(work) = self.routing.current.as_mut() {
            work.checking = Some((order, pending));
        }
        out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest } });
        true
    }

    /// Continue the checked `SetHeads` destination read.
    ///
    /// A found destination enters the artifact cache before it is planned,
    /// so a later check of the same digest reads nothing. Only its existence
    /// and kind are used, so its bytes are not read: the kind is what the
    /// journal answered, as it always was.
    pub(crate) fn continue_destination_artifact(&mut self, result: ReadArtifactResult, out: &mut Vec<Command>) {
        let Some((order, pending)) = self.routing.current.as_mut().and_then(|work| work.checking.take()) else {
            self.abort("set_heads destination arrived with none being checked".to_string(), out);
            return;
        };
        let stored = match result {
            ReadArtifactResult::Found { artifact } => {
                let kind = artifact.kind();
                self.artifacts.insert(pending.set_heads.changes()[pending.next_change].to(), artifact);
                Some(kind)
            }
            ReadArtifactResult::Missing { .. } => None,
            ReadArtifactResult::Err { message, .. } => {
                self.abort(format!("set_heads destination read failed: {message}"), out);
                return;
            }
        };
        if let Some(work) = self.routing.current.as_mut() {
            match plan_destination(pending, stored) {
                DestinationStep::Continue(pending) => {
                    work.destinations.insert(order, pending);
                }
                DestinationStep::Complete(planned) => {
                    work.order.insert(order, planned);
                }
            }
        }
        self.drive_routing(out);
    }
}
