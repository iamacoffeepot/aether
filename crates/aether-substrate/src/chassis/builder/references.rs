//! The chassis's record of what it composed: one proven reference per root
//! actor, keyed by type (ADR-0230 section 3).
//!
//! Registration is asserted at construction, not looked up afterwards. A
//! singleton capability or pumped actor reaches `Live` inside the chassis
//! boot that composes it, so that boot is where its reference is minted and
//! recorded; an embedder reads it back by type through the chassis handle's
//! `actor_ref`, and nothing is minted at the read. Instanced actors are not
//! recorded — a type can have many instances, and each spawn's `finish`
//! already returns its own reference.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Mutex;

use aether_actor::ActorRef;

/// Type-keyed store of the references a chassis minted for the root actors it
/// composed.
///
/// Written once per composed type — by the native capability boot and by both
/// pumped-actor boots — and read by type. The lock is held only across one
/// insert or one lookup, never across a boot step.
#[derive(Default)]
pub(in crate::chassis) struct ComposedReferences {
    references: Mutex<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

impl ComposedReferences {
    /// Record the reference a boot minted for `A` once its route is `Live`.
    ///
    /// # Panics
    /// Panics if the lock is poisoned — fail-fast per ADR-0063.
    pub(in crate::chassis) fn record<A: 'static>(&self, reference: ActorRef<A>) {
        self.references
            .lock()
            .expect("composed-reference lock poisoned; fail-fast per ADR-0063")
            .insert(TypeId::of::<A>(), Box::new(reference));
    }

    /// The reference recorded for `A`, or `None` when this chassis composed
    /// no `A`.
    ///
    /// # Panics
    /// Panics if the lock is poisoned — fail-fast per ADR-0063.
    pub(in crate::chassis) fn get<A: 'static>(&self) -> Option<ActorRef<A>> {
        self.references
            .lock()
            .expect("composed-reference lock poisoned; fail-fast per ADR-0063")
            .get(&TypeId::of::<A>())
            .and_then(|stored| stored.downcast_ref::<ActorRef<A>>())
            .copied()
    }
}
