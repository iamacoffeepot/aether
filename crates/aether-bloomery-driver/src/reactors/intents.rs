//! Reply-to-plan mapping: intents to `Requested`, moves, and failures (ADR-0226 decisions 6 and 8).

use std::collections::BTreeMap;

use aether_bloomery_kinds::{
    CallProgram, Detail, Digest, DriverRecord, ProgramRef, ReactionFailed, ReactorIntent, ReactorName, ReadArtifact,
    ReadArtifactResult, RequestSource, Requested, RuleName, SetHead,
};
use aether_bloomery_view::Heads;
use aether_data::{Kind, KindId};

use crate::core::{ArtifactRead, ArtifactTicket, Command, PlannedRecord, ProgramCore};
use crate::reactors::PendingDestination;

/// One intent's plan: a ready record, or a `SetHead` awaiting its destination check.
pub enum PlannedIntent {
    /// A record appended as decided.
    Ready(DriverRecord),
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
            if kind == SetHead::ID {
                return SetHead::decode_from_bytes(&bytes).map_or_else(
                    || failed(Detail::new("undecodable set_head intent")),
                    |set_head| {
                        PlannedIntent::Destination(PendingDestination {
                            cause,
                            bundle,
                            reactor: reactor.clone(),
                            set_head,
                        })
                    },
                );
            }
            if kind != CallProgram::ID {
                return failed(Detail::new(format!("unsupported intent kind {}", kind.0)));
            }
            let Some(call) = CallProgram::decode_from_bytes(&bytes) else {
                return failed(Detail::new("undecodable call_program intent"));
            };
            let Some(bound) = heads.get(&call.program) else {
                return failed(Detail::new("program head unbound"));
            };
            PlannedIntent::Ready(DriverRecord::Requested {
                cause: Some(cause),
                record: Requested {
                    program: ProgramRef::new(bound.digest(), call.name),
                    input: call.input,
                    source: RequestSource::Reaction { bundle, reactor, rule, ordinal },
                },
            })
        })
        .collect()
}

/// Plan one checked `SetHead` from its destination's stored kind.
///
/// A destination stored under the head's kind leaves the move for
/// derivation; a missing (`None`) or wrong-kind destination fails the intent.
fn plan_destination(pending: PendingDestination, stored: Option<KindId>) -> PlannedRecord {
    let PendingDestination { cause, bundle, reactor, set_head } = pending;
    let reason = match stored {
        Some(kind) if kind == set_head.head().kind() => {
            return PlannedRecord::SetHead { cause, bundle, reactor, set_head };
        }
        Some(_) => "set_head destination has the wrong kind",
        None => "set_head destination is missing",
    };
    PlannedRecord::Ready(reaction_failed(cause, bundle, Some(reactor), Detail::new(reason)))
}

impl ProgramCore {
    /// Check the next queued `SetHead` destination, in plan order.
    ///
    /// A destination in the artifact cache is planned at once, with no read;
    /// any other is read from the journal. Returns `false` when none is
    /// queued.
    pub(crate) fn check_next_destination(&mut self, out: &mut Vec<Command>) -> bool {
        let Some((order, pending)) = self.routing.current.as_mut().and_then(|work| work.destinations.pop_first())
        else {
            return false;
        };
        let digest = pending.set_head.to();
        if let Some(kind) = self.artifacts.kind(digest) {
            let planned = plan_destination(pending, Some(kind));
            if let Some(work) = self.routing.current.as_mut() {
                work.order.insert(order, planned);
            }
            return true;
        }
        let ticket = self.mint(ArtifactTicket::mint);
        self.artifact_reads.insert(ticket, ArtifactRead::SetHeadDestination);
        if let Some(work) = self.routing.current.as_mut() {
            work.checking = Some((order, pending));
        }
        out.push(Command::ReadArtifact { ticket, request: ReadArtifact { digest } });
        true
    }

    /// Continue the checked `SetHead`'s destination read.
    ///
    /// A found destination enters the artifact cache before it is planned,
    /// so a later check of the same digest reads nothing.
    pub(crate) fn continue_destination_artifact(&mut self, result: ReadArtifactResult, out: &mut Vec<Command>) {
        let Some((order, pending)) = self.routing.current.as_mut().and_then(|work| work.checking.take()) else {
            self.abort("set_head destination arrived with none being checked".to_string(), out);
            return;
        };
        let stored = match result {
            ReadArtifactResult::Found { kind, bytes, .. } => {
                self.artifacts.insert(pending.set_head.to(), kind, bytes);
                Some(kind)
            }
            ReadArtifactResult::Missing { .. } => None,
            ReadArtifactResult::Err { message, .. } => {
                self.abort(format!("set_head destination read failed: {message}"), out);
                return;
            }
        };
        let planned = plan_destination(pending, stored);
        if let Some(work) = self.routing.current.as_mut() {
            work.order.insert(order, planned);
        }
        self.drive_routing(out);
    }
}
