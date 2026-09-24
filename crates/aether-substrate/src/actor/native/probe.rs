//! The read-only probe an off-thread helper holds to decide about a peer.
//!
//! A sidecar thread that must decide about a peer before it wakes its actor
//! asks two questions of the proof it holds: is that actor `Live` now, and
//! does it accept a kind. [`ActorProbe`] answers those and nothing else. It
//! sends nothing, resolves no name or position, enumerates nothing, and hands
//! out neither registry (ADR-0230).

use std::sync::Arc;

use aether_actor::ErasedActorRef;
use aether_data::KindId;

use crate::mail::capability::CapabilityRegistry;
use crate::mail::registry::Registry;

/// Answers two questions about an actor a proven reference names, from any
/// thread: is it `Live` now, and does it accept a kind.
///
/// Minted by `ctx.actor_probe()` on
/// [`NativeInitCtx`](crate::actor::native::NativeInitCtx). Cloning it is
/// cheap, and every clone reads the same substrate.
#[derive(Clone)]
pub struct ActorProbe {
    routes: Arc<Registry>,
    capabilities: Arc<CapabilityRegistry>,
}

impl ActorProbe {
    pub(crate) fn new(routes: Arc<Registry>, capabilities: Arc<CapabilityRegistry>) -> Self {
        Self { routes, capabilities }
    }

    /// Whether the actor `actor` proves is `Live` in the published route view
    /// now. A racy snapshot: the answer can be stale by the time the caller
    /// acts on it. An actor that holds the reference observes departure
    /// through `ctx.monitor`.
    #[must_use]
    pub fn is_live(&self, actor: ErasedActorRef) -> bool {
        self.routes.is_live(actor)
    }

    /// Whether the actor `actor` proves accepts `kind`: it has a `#[handler]`
    /// for it or a `#[fallback]`. A departed actor accepts nothing.
    ///
    /// # Panics
    /// Panics if the capability registry's lock is poisoned (see
    /// [`CapabilityRegistry::accepts_actor`]).
    #[must_use]
    pub fn accepts(&self, actor: ErasedActorRef, kind: KindId) -> bool {
        self.capabilities.accepts_actor(actor, kind)
    }
}

#[cfg(test)]
mod tests {
    use crate::mail::registry::noop_handler;
    use crate::testing::{drop_ref, fresh_substrate, registered_ref};

    #[test]
    fn is_live_reads_the_routing_registry() {
        let (registry, mailer) = fresh_substrate();
        let reference = registered_ref(&registry, "test.probe.peer", noop_handler());
        let probe = mailer.actor_probe();

        assert!(probe.is_live(reference));

        drop_ref(&registry, reference);

        assert!(!probe.is_live(reference));
    }
}
