//! Actor-lifecycle registry (ADR-0079, issue 607). Keyed by full-name
//! `MailboxId`, tracks live actor entries plus tombstones (retired
//! full names) and namespace ownership.
//!
//! Phase 2 of issue 607 lands the storage shape; nothing populates it
//! yet. Phase 3 wires `NativeCtx::spawn_child` as the first writer
//! (`Live` slot insertion); Phase 4 adds the close path
//! (`Live` → `Dead`, plus `tombstones` insertion) and the monitor
//! forward/reverse indices.
//!
//! Distinct from [`crate::registry::Registry`] — that one owns
//! mailbox-name → handler routing and kind descriptors. This one owns
//! actor-state lifecycle. Future PRs may collapse the two; today they
//! sit side-by-side so the lifecycle work doesn't perturb the routing
//! path.

use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

use crate::capability::Envelope;
use crate::mail::MailboxId;

/// One actor slot in the registry. `Live` carries the inbox sender
/// (for direct mail routing into the dispatcher), the actor's
/// type-erased `Arc` (for `resolve_actor`-style downcasts), and the
/// actor's `TypeId` (gates type-keyed lookups). `Dead` is a sentinel
/// for entries whose dispatcher has joined and whose actor has
/// dropped — mail addressed to the slot warn-drops, and `spawn_child`
/// rejects the name for reuse. ADR-0079 §Drop / lifecycle.
#[derive(Clone)]
pub enum ActorEntry {
    Live {
        sender: Sender<Envelope>,
        actor: Arc<dyn std::any::Any + Send + Sync>,
        type_id: TypeId,
    },
    Dead,
}

/// Storage for actor-lifecycle state. All fields are private; the
/// public surface is read-only lookups (Phase 2). Phase 3 adds
/// internal mutators reachable through `NativeCtx::spawn_child`;
/// Phase 4 adds the close path and monitor indices.
#[derive(Default)]
pub struct ActorRegistry {
    /// Sparse, keyed on full-name `MailboxId`. `Live` while the
    /// dispatcher thread is running; `Dead` once it joined and the
    /// actor dropped. Mail-routing readers ignore `Dead` (warn-drop).
    actors: RwLock<HashMap<MailboxId, ActorEntry>>,

    /// Retired full names. `spawn_child` rejects reuse; lookups
    /// distinguish "never existed" from "previously existed and
    /// closed." Single static membership — no per-tombstone allocation
    /// beyond the HashSet entry itself.
    tombstones: RwLock<HashSet<MailboxId>>,

    /// One owner per `NAMESPACE`. Populated at chassis-build for
    /// singletons (Phase 3+) and at first `spawn_child` for instanced
    /// types. Insertion conflicts when a different `TypeId` already
    /// owns the namespace — the ADR-0079 guard against
    /// Singleton/Instanced or Instanced/Instanced name collisions.
    name_owners: RwLock<HashMap<&'static str, TypeId>>,
}

impl ActorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Some(actor)` only if the slot at `id` is `Live`. `Dead` and
    /// missing both return `None` — callers can't distinguish via this
    /// path, by design (ADR-0079: `Dead` is opaque to lookup; spawn-time
    /// retirement check goes through [`Self::is_tombstoned`]).
    pub fn live_actor(&self, id: MailboxId) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        let actors = self.actors.read().unwrap();
        match actors.get(&id) {
            Some(ActorEntry::Live { actor, .. }) => Some(Arc::clone(actor)),
            _ => None,
        }
    }

    /// `Some(sender)` only if the slot at `id` is `Live`. `Sender` is
    /// `Clone`, so the returned handle can be used to push mail
    /// directly into the actor's inbox without holding the registry
    /// lock across the send.
    pub fn live_sender(&self, id: MailboxId) -> Option<Sender<Envelope>> {
        let actors = self.actors.read().unwrap();
        match actors.get(&id) {
            Some(ActorEntry::Live { sender, .. }) => Some(sender.clone()),
            _ => None,
        }
    }

    /// `TypeId` of the actor occupying the slot at `id`, or `None` if
    /// the slot is `Dead` or missing. The downcast-safety counterpart
    /// to [`Self::live_actor`].
    pub fn type_id_at(&self, id: MailboxId) -> Option<TypeId> {
        let actors = self.actors.read().unwrap();
        match actors.get(&id) {
            Some(ActorEntry::Live { type_id, .. }) => Some(*type_id),
            _ => None,
        }
    }

    /// Has this id been tombstoned (its actor closed)? `spawn_child`
    /// uses this in Phase 3 to reject reuse of retired full names.
    pub fn is_tombstoned(&self, id: MailboxId) -> bool {
        self.tombstones.read().unwrap().contains(&id)
    }

    /// `TypeId` that owns the given namespace, if any. Populated at
    /// chassis-build (singletons) and first spawn (instanced).
    pub fn namespace_owner(&self, namespace: &'static str) -> Option<TypeId> {
        self.name_owners.read().unwrap().get(namespace).copied()
    }

    /// Claim ownership of `namespace` for `type_id`. Returns `Ok(())` on
    /// fresh claim or when the same `TypeId` re-claims the same
    /// namespace (idempotent — multiple instanced spawns of the same
    /// type share one namespace). Returns `Err(other_type_id)` when a
    /// different type already owns the namespace — the ADR-0079 guard
    /// against Singleton/Instanced or Instanced/Instanced collisions.
    pub(crate) fn try_claim_namespace(
        &self,
        namespace: &'static str,
        type_id: TypeId,
    ) -> Result<(), TypeId> {
        let mut owners = self.name_owners.write().unwrap();
        match owners.get(namespace) {
            Some(&existing) if existing == type_id => Ok(()),
            Some(&existing) => Err(existing),
            None => {
                owners.insert(namespace, type_id);
                Ok(())
            }
        }
    }

    /// Insert a `Live` actor entry under `id`. Returns `Err(())` if a
    /// `Live` entry already exists at `id` (caller must check
    /// `is_tombstoned` separately for the retired-name case). Used by
    /// the spawn primitive after init succeeds.
    pub(crate) fn insert_live(
        &self,
        id: MailboxId,
        sender: Sender<Envelope>,
        actor: Arc<dyn std::any::Any + Send + Sync>,
        type_id: TypeId,
    ) -> Result<(), ()> {
        let mut actors = self.actors.write().unwrap();
        match actors.get(&id) {
            Some(ActorEntry::Live { .. }) => Err(()),
            // `Dead` slot or empty: install the live entry. Phase 4
            // populates `Dead` on close, but Phase 3 only ever sees
            // empty slots.
            _ => {
                actors.insert(
                    id,
                    ActorEntry::Live {
                        sender,
                        actor,
                        type_id,
                    },
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_registry_is_empty() {
        let r = ActorRegistry::new();
        assert!(r.live_actor(MailboxId(1)).is_none());
        assert!(r.live_sender(MailboxId(1)).is_none());
        assert!(r.type_id_at(MailboxId(1)).is_none());
        assert!(!r.is_tombstoned(MailboxId(1)));
        assert!(r.namespace_owner("aether.example").is_none());
    }
}
