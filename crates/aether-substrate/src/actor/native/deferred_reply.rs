//! Per-actor table of held reply obligations for deferred HTTP routes
//! (ADR-0154 §3, hardened per iamacoffeepot/aether#3683).
//!
//! A deferred route forwards its request to a peer capability and answers
//! only when that reply lands. Between the forward and the answer, the
//! request's reply obligation — an [`InboundMail`] guard holding a live
//! socket and an open causal chain — is parked here, keyed by the downstream
//! dispatch's correlation id. The paired reply route (or the `504`
//! settlement net) recovers it via that correlation and answers.
//!
//! The table lives per-actor on the [`NativeBinding`](super::NativeBinding),
//! next to the ADR-0139 request-context table, for three reasons the prior
//! process-global static could not give:
//!
//! - **Teardown reclamation.** When the actor drops, this table drops with
//!   it, and each held [`InboundMail`] drops — settling its chain response-
//!   less, which the HTTP server answers `502`. A dead actor's obligations
//!   never orphan.
//! - **A bounded ceiling.** [`DEFERRED_REPLY_CAPACITY`] caps in-flight
//!   deferrals per actor; the route refuses new work (answers `503`) at the
//!   ceiling rather than growing without bound at a slow or dead peer.
//! - **Per-actor locking.** The obligation lock is scoped to one actor's own
//!   traffic, not a chassis-wide contention point in every deferred route's
//!   reply path.
//!
//! Held [`InboundMail`] is a live native socket handle and is not
//! serializable, so — unlike the request-context table — it cannot ride a
//! dehydrate/rehydrate swap. There is deliberately no snapshot: an actor
//! torn down (or replaced) with obligations in flight settles them on drop.

use std::collections::HashMap;

use crate::chassis::inbox::InboundMail;

/// Default per-actor ceiling on concurrently held deferred-reply
/// obligations. Mirrors the ADR-0139
/// [`REQUEST_CONTEXT_CAPACITY`](aether_actor::REQUEST_CONTEXT_CAPACITY)
/// precedent: an SDK default constant, not a chassis config knob (a knob
/// follows only if a real consumer needs to tune it, per
/// iamacoffeepot/aether#3683). Each entry pins a live socket, so the ceiling
/// also bounds the sockets one router can strand at an unresponsive peer.
pub const DEFERRED_REPLY_CAPACITY: usize = 1024;

/// Per-actor held-obligation table for deferred HTTP routes. Bounded by
/// [`DEFERRED_REPLY_CAPACITY`]; refuses at the ceiling rather than evicting a
/// legitimately in-flight request.
pub struct DeferredReplyTable {
    entries: HashMap<u64, InboundMail>,
    capacity: usize,
}

impl DeferredReplyTable {
    pub fn new() -> Self {
        Self { entries: HashMap::new(), capacity: DEFERRED_REPLY_CAPACITY }
    }

    /// Whether another obligation can be held. The deferred route pre-checks
    /// this before taking its inbound so it can answer `503` without
    /// consuming the request when the table is full.
    pub fn has_capacity(&self) -> bool {
        self.entries.len() < self.capacity
    }

    /// Park `inbound` under `correlation`. Correlation ids are monotonic per
    /// actor, so a live collision cannot occur; a displaced entry would mean
    /// the id space wrapped under a still-open obligation — drop it (settling
    /// its chain `502`) and warn rather than silently leak the socket.
    ///
    /// The caller is expected to have gated on [`Self::has_capacity`]; if it
    /// did not and the table is at the ceiling, the insert still proceeds
    /// (the guard is `has_capacity`, not this method) but is a caller bug.
    pub fn hold(&mut self, correlation: u64, inbound: InboundMail) {
        if let Some(displaced) = self.entries.insert(correlation, inbound) {
            tracing::warn!(correlation, "deferred-reply correlation collision; dropped the prior obligation");
            drop(displaced);
        }
    }

    /// Remove and return the obligation held under `correlation`, or `None`
    /// if none is held (already answered, or reclaimed by the `504` net).
    pub fn take(&mut self, correlation: u64) -> Option<InboundMail> {
        self.entries.remove(&correlation)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for DeferredReplyTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
mod tests {
    use std::sync::Arc;

    use aether_data::{KindId, MailId, MailboxId, Source};
    use aether_kinds::descriptors;
    use aether_kinds::trace::Nanos;

    use super::*;
    use crate::chassis::inbox::ReplyLineage;
    use crate::chassis::settlement::SettlementRegistry;
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::mail::{MailRef, Mailer};

    /// A mailer wired to a settlement registry on both seams (as the chassis
    /// builder does at boot), so a test can subscribe to a root's discharge.
    fn env() -> (Arc<Mailer>, Arc<SettlementRegistry>) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let mailer = Arc::new(Mailer::new(registry));
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));
        (mailer, settlement)
    }

    fn held(mailer: &Arc<Mailer>, id: MailboxId, root: MailId) -> InboundMail {
        mailer.record_sent_inflight(root);
        let env = OwnedDispatch::armed(
            KindId(7),
            "test.deferred.kind".to_owned(),
            None,
            Source::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::new(id, 11),
            root,
            None,
            Nanos(0),
            0,
            id,
        );
        InboundMail::from_dispatched(env, Arc::clone(mailer), id, ReplyLineage::new())
    }

    #[test]
    fn hold_then_take_round_trips() {
        let (mailer, _settlement) = env();
        let id = MailboxId(0x301);
        let mut table = DeferredReplyTable::new();

        table.hold(1, held(&mailer, id, MailId::new(id, 1)));
        assert_eq!(table.len(), 1);
        assert!(table.take(1).is_some(), "the held obligation is recovered by its correlation");
        assert_eq!(table.len(), 0);
        assert!(table.take(1).is_none(), "a second take finds nothing");
    }

    #[test]
    fn refuses_at_the_ceiling() {
        let (mailer, _settlement) = env();
        let id = MailboxId(0x302);
        let mut table = DeferredReplyTable { entries: HashMap::new(), capacity: 2 };

        assert!(table.has_capacity());
        table.hold(1, held(&mailer, id, MailId::new(id, 1)));
        assert!(table.has_capacity());
        table.hold(2, held(&mailer, id, MailId::new(id, 2)));
        assert!(!table.has_capacity(), "at the ceiling the route refuses new deferrals (503)");
    }

    /// Dropping the table drops every held obligation, and each drop settles
    /// its chain — the teardown reclamation the process-global static could
    /// not do. Fix #2 of iamacoffeepot/aether#3683.
    #[test]
    fn drop_settles_held_obligations() {
        let (mailer, settlement) = env();
        let id = MailboxId(0x303);
        let root = MailId::new(id, 1);

        let mut table = DeferredReplyTable::new();
        table.hold(1, held(&mailer, id, root));
        let settle = settlement.subscribe_settlement(root);

        drop(table);
        settle.recv().expect("dropping the table settles the held chain");
    }
}
