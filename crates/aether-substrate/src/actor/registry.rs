//! Actor-lifecycle registry (ADR-0079, issue 607). Keyed by full-name
//! `MailboxId`, tracks live actor entries plus tombstones (retired
//! full names), namespace ownership, and the bidirectional monitor
//! indices (Phase 4b).
//!
//! Phase 2 of issue 607 lands the storage shape; Phase 3 wires
//! `NativeCtx::spawn_child` as the first writer (`Live` slot
//! insertion); Phase 4a flips `Live` → `Dead` and inserts tombstones
//! at close; Phase 4b adds the crate-internal `monitors_of` /
//! `monitoring` indices plus the `register_monitor` /
//! `deregister_monitor` / `close_actor` surface.
//!
//! Distinct from [`crate::mail::registry::Registry`] — that one owns
//! mailbox-name → handler routing and kind descriptors. This one owns
//! actor-state lifecycle. Future PRs may collapse the two; today they
//! sit side-by-side so the lifecycle work doesn't perturb the routing
//! path.

// Registry RwLock guards are intentionally held across the full
// read-then-update or match-then-mutate sequence — releasing the
// guard mid-sequence would open a TOCTOU window where another writer
// could mutate the map between the `get` and the dependent action.
#![allow(clippy::significant_drop_tightening)]

use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

use crate::actor::native::envelope::Envelope;
use crate::mail::MailboxId;
use std::any::Any;

/// One actor slot in the registry. `Live` carries the inbox sender
/// (for direct mail routing into the dispatcher), the actor's
/// `TypeId` (gates type-keyed `resolve_actors` enumeration), and its
/// subname (Phase 5 — surfaced through
/// [`crate::chassis::builder::PassiveChassis::resolve_actors`] so callers
/// can iterate `(subname, MailboxId)` pairs). `Dead` is a sentinel for entries
/// whose dispatcher has joined and whose actor has dropped — mail
/// addressed to the slot warn-drops, and `spawn_child` rejects the
/// name for reuse. ADR-0079 §Drop / lifecycle.
///
/// Issue 629 / Phase A: the pre-629 `actor: Arc<dyn Any + Send + Sync>`
/// field retired. The actor itself is owned exclusively by its
/// dispatcher thread as `Box<A>`; the registry no longer holds a
/// cross-thread share. Type-keyed lookups (`resolve_actor` /
/// `resolve_actors`) return [`MailboxId`] addresses, not `Arc<A>`.
///
/// `sender` is `Arc<Sender<Envelope>>` so the registry's sink
/// handler can hold a `Weak<Sender>` and upgrade only while the
/// actor is `Live`; on `mark_dead` the Arc drops and the weak
/// upgrade fails, making mail addressed to a dead instanced
/// mailbox warn-drop.
///
/// `subname` is empty (`String::new()`) for slot inserts that don't
/// originate from the spawn path (singletons today never `insert_live`;
/// future Phase 7 alignment may revisit).
#[derive(Clone)]
pub enum ActorEntry {
    Live {
        sender: Arc<Sender<Envelope>>,
        type_id: TypeId,
        subname: String,
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
    /// beyond the `HashSet` entry itself.
    tombstones: RwLock<HashSet<MailboxId>>,

    /// One owner per `NAMESPACE`. Populated at chassis-build for
    /// singletons (Phase 3+) and at first `spawn_child` for instanced
    /// types. Insertion conflicts when a different `TypeId` already
    /// owns the namespace — the ADR-0079 guard against
    /// Singleton/Instanced or Instanced/Instanced name collisions.
    name_owners: RwLock<HashMap<&'static str, TypeId>>,

    /// Forward monitor index: `monitors_of[target]` is the list of
    /// watchers that registered a monitor against `target`. Drained at
    /// `target`'s close to fan out [`aether_kinds::MonitorNotice`].
    /// ADR-0079 §Discovery and monitoring.
    monitors_of: RwLock<HashMap<MailboxId, Vec<MonitorEntry>>>,

    /// Reverse monitor index: `monitoring[watcher]` is the list of
    /// targets `watcher` is watching. Walked at `watcher`'s close to
    /// remove `watcher` from each target's `monitors_of` (so a dead
    /// watcher doesn't accumulate as a stale entry on every target it
    /// was monitoring).
    monitoring: RwLock<HashMap<MailboxId, Vec<MailboxId>>>,
}

/// One entry in the registry's internal `monitors_of` index. Today
/// only carries the watcher's id; the struct shape leaves room for a
/// future per-monitor option (monitor reason, reply-target override)
/// without rewriting the vec storage.
#[derive(Debug, Clone, Copy)]
pub struct MonitorEntry {
    pub watcher: MailboxId,
}

/// Failure modes for the registry's internal `register_monitor` entry
/// point (reached from [`crate::actor::native::ctx::NativeCtx::monitor`]).
/// ADR-0079 v1: monitors must address an actor that is currently `Live`.
/// Tombstoned targets (retired-and-closed full names) can't be monitored
/// — the mail wouldn't fire anyway, since the close fan-out already
/// ran when the slot flipped `Live` → `Dead`.
#[derive(Debug, PartialEq, Eq)]
pub enum MonitorError {
    /// No `Live` entry at the target id. Either the actor never existed
    /// or it was removed without going through the close path
    /// (impossible in production today; future replacements may flow
    /// through here instead of `mark_dead`).
    TargetNotFound,
    /// The target's full name is in `tombstones` — it lived and
    /// closed, and `MonitorNotice` already fired (or would have, if any
    /// monitor were registered). Registering now is meaningless.
    TargetTombstoned,
}

impl ActorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue 629 / Phase A: `true` only if the slot at `id` is `Live`.
    /// Replaces the pre-629 `live_actor(id) -> Option<Arc<dyn Any +
    /// Send + Sync>>` accessor; the actor itself no longer escapes its
    /// dispatcher thread. Callers that needed the actor reference now
    /// read a cap-exported handle (drivers) or send mail (peers).
    /// `Dead` and missing both return `false` — callers can't
    /// distinguish via this path, by design (ADR-0079: `Dead` is opaque
    /// to lookup; spawn-time retirement check goes through
    /// [`Self::is_tombstoned`]).
    ///
    /// # Panics
    /// Panics if the `actors` `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior writer panicked under
    /// the guard, a substrate-level invariant violation.
    pub fn is_live(&self, id: MailboxId) -> bool {
        let actors = self
            .actors
            .read()
            .expect("actors lock poisoned; fail-fast per ADR-0063");
        matches!(actors.get(&id), Some(ActorEntry::Live { .. }))
    }

    /// `Some(sender)` only if the slot at `id` is `Live`. The returned
    /// `Sender` is cloned out of the registry's `Arc<Sender>` so the
    /// caller can push mail directly into the actor's inbox without
    /// holding the registry lock or affecting the Arc's strong count.
    ///
    /// # Panics
    /// Panics if the `actors` `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior writer panicked under
    /// the guard, a substrate-level invariant violation.
    pub fn live_sender(&self, id: MailboxId) -> Option<Sender<Envelope>> {
        let actors = self
            .actors
            .read()
            .expect("actors lock poisoned; fail-fast per ADR-0063");
        match actors.get(&id) {
            Some(ActorEntry::Live { sender, .. }) => Some((**sender).clone()),
            _ => None,
        }
    }

    /// `TypeId` of the actor occupying the slot at `id`, or `None` if
    /// the slot is `Dead` or missing. The downcast-safety counterpart
    /// to [`Self::is_live`].
    ///
    /// # Panics
    /// Panics if the `actors` `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior writer panicked under
    /// the guard, a substrate-level invariant violation.
    pub fn type_id_at(&self, id: MailboxId) -> Option<TypeId> {
        let actors = self
            .actors
            .read()
            .expect("actors lock poisoned; fail-fast per ADR-0063");
        match actors.get(&id) {
            Some(ActorEntry::Live { type_id, .. }) => Some(*type_id),
            _ => None,
        }
    }

    /// Has this id been tombstoned (its actor closed)? `spawn_child`
    /// uses this in Phase 3 to reject reuse of retired full names.
    ///
    /// # Panics
    /// Panics if the `tombstones` `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior writer panicked under
    /// the guard, a substrate-level invariant violation.
    pub fn is_tombstoned(&self, id: MailboxId) -> bool {
        self.tombstones
            .read()
            .expect("tombstones lock poisoned; fail-fast per ADR-0063")
            .contains(&id)
    }

    /// `TypeId` that owns the given namespace, if any. Populated at
    /// chassis-build (singletons) and first spawn (instanced).
    ///
    /// # Panics
    /// Panics if the `name_owners` `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior writer panicked under
    /// the guard, a substrate-level invariant violation.
    pub fn namespace_owner(&self, namespace: &'static str) -> Option<TypeId> {
        self.name_owners
            .read()
            .expect("name_owners lock poisoned; fail-fast per ADR-0063")
            .get(namespace)
            .copied()
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
        let mut owners = self
            .name_owners
            .write()
            .expect("name_owners lock poisoned; fail-fast per ADR-0063");
        match owners.get(namespace) {
            Some(&existing) if existing == type_id => Ok(()),
            Some(&existing) => Err(existing),
            None => {
                owners.insert(namespace, type_id);
                Ok(())
            }
        }
    }

    /// Issue 607 Phase 7: release ownership of `namespace` iff
    /// `type_id` currently owns it. Used in the chassis-boot unwind
    /// path when a singleton's `init` fails — without this release,
    /// the failed cap's namespace stays claimed and a later cap with
    /// a different `TypeId` legitimately claiming the same namespace
    /// (after the failed cap is gone) collides. Returns `true` if the
    /// entry was released, `false` if absent or owned by a different
    /// type (typically a caller bug, but we don't panic — the boot
    /// failure path runs even on weird states).
    ///
    /// Crate-private — only the boot-failure paths in
    /// [`crate::chassis::ctx`] / [`crate::chassis::builder`] call this.
    pub(crate) fn release_namespace(&self, namespace: &'static str, type_id: TypeId) -> bool {
        let mut owners = self
            .name_owners
            .write()
            .expect("name_owners lock poisoned; fail-fast per ADR-0063");
        match owners.get(namespace) {
            Some(&existing) if existing == type_id => {
                owners.remove(namespace);
                true
            }
            _ => false,
        }
    }

    /// Insert a `Live` actor entry under `id`. Returns `Err(())` if a
    /// `Live` entry already exists at `id` (caller must check
    /// `is_tombstoned` separately for the retired-name case). Used by
    /// the spawn primitive after init succeeds.
    ///
    /// `subname` is the per-instance segment Phase 5
    /// [`super::PassiveChassis::resolve_actors`] iterates over;
    /// callers that don't originate from the spawn path (today: none;
    /// tests use empty strings) pass `""` and accept they won't show
    /// up in the `resolve_actors` iterator.
    pub(crate) fn insert_live(
        &self,
        id: MailboxId,
        sender: Arc<Sender<Envelope>>,
        type_id: TypeId,
        subname: String,
    ) -> Result<(), ()> {
        let mut actors = self
            .actors
            .write()
            .expect("actors lock poisoned; fail-fast per ADR-0063");
        if let Some(ActorEntry::Live { .. }) = actors.get(&id) {
            Err(())
        } else {
            // `Dead` slot or empty: install the live entry. Phase 4
            // populates `Dead` on close, but Phase 3 only ever sees
            // empty slots.
            actors.insert(
                id,
                ActorEntry::Live {
                    sender,
                    type_id,
                    subname,
                },
            );
            Ok(())
        }
    }

    /// Issue 607 Phase 5 (ADR-0079): walk every `Live` slot whose
    /// `TypeId` matches `T` and hand the caller `(subname, MailboxId)`.
    /// Used by [`super::PassiveChassis::resolve_actors`] /
    /// [`super::BuiltChassis::resolve_actors`] for chassis-level
    /// enumeration of instanced actors.
    ///
    /// **Crate-private on purpose.** Cap handlers should not introspect
    /// the registry at runtime — caps that supervise a fleet of
    /// instances (e.g. `TcpCapability` over `TcpListenerActor`) hold
    /// their own cap-local map of children and update it on
    /// `MonitorNotice`. The chassis-level surface is for
    /// embedder/test diagnostics, not in-handler state. ADR-0079
    /// supervisor-as-cap pattern.
    ///
    /// Issue 629 / Phase A: returns `(subname, MailboxId)` instead of
    /// `(subname, Arc<T>)`. The actor itself no longer escapes its
    /// dispatcher thread — callers that need to reach into instance
    /// state mail the address; the registry only owns addressing data.
    ///
    /// Both Vec slot allocations land while the read lock is held; the
    /// lock drops before the caller iterates.
    pub(crate) fn live_subnames_of_type<T>(&self) -> Vec<(String, MailboxId)>
    where
        T: Any + 'static,
    {
        let actors = self
            .actors
            .read()
            .expect("actors lock poisoned; fail-fast per ADR-0063");
        let target = TypeId::of::<T>();
        actors
            .iter()
            .filter_map(|(id, entry)| match entry {
                ActorEntry::Live {
                    type_id, subname, ..
                } if *type_id == target => Some((subname.clone(), *id)),
                _ => None,
            })
            .collect()
    }

    /// Issue 607 Phase 4a (ADR-0079): flip the slot at `id` from
    /// `Live` to `Dead` and insert the id into `tombstones`. Called
    /// by the instanced-actor dispatcher after `unwire` runs so
    /// future `spawn_child` calls reject reuse of the retired name
    /// with `SpawnError::SubnameRetired`. Idempotent — re-running on
    /// an already-`Dead` slot leaves it `Dead` and doesn't double-
    /// insert into `tombstones`.
    pub(crate) fn mark_dead(&self, id: MailboxId) {
        let mut actors = self
            .actors
            .write()
            .expect("actors lock poisoned; fail-fast per ADR-0063");
        actors.insert(id, ActorEntry::Dead);
        drop(actors);
        let mut tombstones = self
            .tombstones
            .write()
            .expect("tombstones lock poisoned; fail-fast per ADR-0063");
        tombstones.insert(id);
    }

    /// Issue 607 Phase 4b (ADR-0079): register `watcher` as a monitor
    /// of `target`. Caller is responsible for sending the resulting
    /// [`MonitorEntry`] back through a [`crate::actor::monitor::MonitorHandle`]
    /// so `Drop` deregisters the entry — bare callers (tests, internal
    /// fixtures) own the cleanup themselves.
    ///
    /// Uses the reverse `monitoring[watcher]` index to support the
    /// watcher-died case: when the watcher closes, the close path
    /// walks `monitoring[watcher]` and prunes `watcher` from each
    /// target's `monitors_of` so dead watchers don't accumulate.
    ///
    /// Validation: target must be `Live` (a `Dead` slot or missing
    /// returns `TargetNotFound`); a tombstoned id returns
    /// `TargetTombstoned` (takes priority over `TargetNotFound` when
    /// both apply, so a closed actor surfaces as tombstoned rather
    /// than not-found).
    pub(crate) fn register_monitor(
        &self,
        watcher: MailboxId,
        target: MailboxId,
    ) -> Result<(), MonitorError> {
        if self.is_tombstoned(target) {
            return Err(MonitorError::TargetTombstoned);
        }
        // Live check goes through the actors map so callers see a
        // consistent "is the actor running" answer regardless of
        // whether the target slot is `Live`, `Dead`, or never inserted.
        // Singletons booted through the chassis builder land in this
        // map alongside instanced actors (issue 607 Phase 4b lifts the
        // boot path to insert `Live`); callers who reach for monitor
        // before that lift see `TargetNotFound`, which matches the
        // wire contract for "this id has no live actor."
        let actors = self
            .actors
            .read()
            .expect("actors lock poisoned; fail-fast per ADR-0063");
        if !matches!(actors.get(&target), Some(ActorEntry::Live { .. })) {
            return Err(MonitorError::TargetNotFound);
        }
        drop(actors);
        let entry = MonitorEntry { watcher };
        // Forward + reverse insert under separate locks. Both maps are
        // readable individually under read locks; transient
        // observability of forward without reverse (or vice versa) is
        // benign — the close path also looks at both, and either
        // direction missing just means one cleanup step is a no-op.
        self.monitors_of
            .write()
            .expect("monitors_of lock poisoned; fail-fast per ADR-0063")
            .entry(target)
            .or_default()
            .push(entry);
        self.monitoring
            .write()
            .expect("monitoring lock poisoned; fail-fast per ADR-0063")
            .entry(watcher)
            .or_default()
            .push(target);
        Ok(())
    }

    /// Issue 607 Phase 4b (ADR-0079): undo a prior `register_monitor`
    /// call. Idempotent — removing a monitor that was already pruned
    /// (e.g. because the target closed and the close path drained the
    /// forward index) is a no-op. Called by [`crate::actor::monitor::MonitorHandle::Drop`]
    /// when the handle goes out of scope.
    pub(crate) fn deregister_monitor(&self, watcher: MailboxId, target: MailboxId) {
        if let Some(entries) = self
            .monitors_of
            .write()
            .expect("monitors_of lock poisoned; fail-fast per ADR-0063")
            .get_mut(&target)
        {
            entries.retain(|e| e.watcher != watcher);
        }
        if let Some(targets) = self
            .monitoring
            .write()
            .expect("monitoring lock poisoned; fail-fast per ADR-0063")
            .get_mut(&watcher)
        {
            targets.retain(|t| *t != target);
        }
    }

    /// Issue 607 Phase 4b (ADR-0079): close path. Drains
    /// `monitors_of[id]` (returning the watcher list for the caller to
    /// fan out [`aether_kinds::MonitorNotice`] mail), walks
    /// `monitoring[id]` to prune `id` from each watched target's
    /// forward index, then calls [`Self::mark_dead`] to flip the slot
    /// `Live` → `Dead` and insert the tombstone.
    ///
    /// One method (rather than three separate calls) so the dispatcher
    /// trampoline can't accidentally skip a step on close — the
    /// fan-out + reverse-prune + tombstone are all part of the same
    /// retire-this-id transaction. Idempotent: a second call on an
    /// already-`Dead` slot returns an empty watcher list and does no
    /// further work.
    pub(crate) fn close_actor(&self, id: MailboxId) -> Vec<MailboxId> {
        // Forward index: take the watcher list whole. Future sends
        // through `register_monitor` against this id now hit the
        // tombstone branch (set by `mark_dead` below), so no race here
        // can leak a registration past the close.
        let watchers: Vec<MailboxId> = self
            .monitors_of
            .write()
            .expect("monitors_of lock poisoned; fail-fast per ADR-0063")
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.watcher)
            .collect();
        // Reverse index: walk each target the closing actor was
        // monitoring and remove it from that target's forward list.
        // Mirrors `deregister_monitor` per-target, but in bulk.
        let monitoring_targets = self
            .monitoring
            .write()
            .expect("monitoring lock poisoned; fail-fast per ADR-0063")
            .remove(&id);
        if let Some(targets) = monitoring_targets {
            let mut forward = self
                .monitors_of
                .write()
                .expect("monitors_of lock poisoned; fail-fast per ADR-0063");
            for target in targets {
                if let Some(entries) = forward.get_mut(&target) {
                    entries.retain(|e| e.watcher != id);
                }
            }
        }
        self.mark_dead(id);
        watchers
    }

    /// Number of watchers registered against `target` right now. Test-
    /// facing only — callers in production don't peek at the index.
    #[cfg(test)]
    pub(crate) fn monitor_count(&self, target: MailboxId) -> usize {
        self.monitors_of
            .read()
            .expect("monitors_of lock poisoned; fail-fast per ADR-0063")
            .get(&target)
            .map_or(0, Vec::len)
    }

    /// Number of targets `watcher` is monitoring right now. Test-facing
    /// only.
    #[cfg(test)]
    pub(crate) fn monitoring_count(&self, watcher: MailboxId) -> usize {
        self.monitoring
            .read()
            .expect("monitoring lock poisoned; fail-fast per ADR-0063")
            .get(&watcher)
            .map_or(0, Vec::len)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn fresh_registry_is_empty() {
        let r = ActorRegistry::new();
        assert!(!r.is_live(MailboxId(1)));
        assert!(r.live_sender(MailboxId(1)).is_none());
        assert!(r.type_id_at(MailboxId(1)).is_none());
        assert!(!r.is_tombstoned(MailboxId(1)));
        assert!(r.namespace_owner("aether.example").is_none());
        assert_eq!(r.monitor_count(MailboxId(1)), 0);
        assert_eq!(r.monitoring_count(MailboxId(1)), 0);
    }

    /// Helper: insert a `Live` slot at `id` so `register_monitor`'s
    /// liveness check passes. Uses a throwaway sender so the test
    /// doesn't drag in `NativeActor`.
    fn insert_live_stub(r: &ActorRegistry, id: MailboxId) {
        struct Stub;
        let (tx, _rx) = mpsc::channel::<Envelope>();
        r.insert_live(id, Arc::new(tx), TypeId::of::<Stub>(), String::new())
            .expect("fresh slot");
    }

    #[test]
    fn register_monitor_rejects_unknown_target() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);
        assert_eq!(
            r.register_monitor(watcher, target),
            Err(MonitorError::TargetNotFound),
        );
    }

    #[test]
    fn register_monitor_rejects_tombstoned_target() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        // close_actor flips the slot Dead and tombstones the id.
        let _ = r.close_actor(target);
        assert!(r.is_tombstoned(target));
        assert_eq!(
            r.register_monitor(watcher, target),
            Err(MonitorError::TargetTombstoned),
        );
    }

    #[test]
    fn register_monitor_populates_both_indices() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        r.register_monitor(watcher, target).expect("live target");
        assert_eq!(r.monitor_count(target), 1);
        assert_eq!(r.monitoring_count(watcher), 1);
    }

    #[test]
    fn deregister_monitor_clears_both_indices() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        r.register_monitor(watcher, target).unwrap();
        r.deregister_monitor(watcher, target);
        assert_eq!(r.monitor_count(target), 0);
        assert_eq!(r.monitoring_count(watcher), 0);
    }

    #[test]
    fn deregister_monitor_is_idempotent() {
        let r = ActorRegistry::new();
        // Calling deregister with no prior register is a no-op (used by
        // MonitorHandle::Drop after the close path already cleaned up).
        r.deregister_monitor(MailboxId(1), MailboxId(2));
    }

    #[test]
    fn close_actor_returns_watchers_and_tombstones() {
        let r = ActorRegistry::new();
        let target = MailboxId(2);
        let watcher_a = MailboxId(10);
        let watcher_b = MailboxId(11);
        insert_live_stub(&r, target);
        r.register_monitor(watcher_a, target).unwrap();
        r.register_monitor(watcher_b, target).unwrap();
        let watchers = r.close_actor(target);
        assert_eq!(watchers.len(), 2);
        assert!(watchers.contains(&watcher_a));
        assert!(watchers.contains(&watcher_b));
        assert!(r.is_tombstoned(target));
        // Forward index for the closed target is empty.
        assert_eq!(r.monitor_count(target), 0);
    }

    #[test]
    fn close_actor_prunes_reverse_index_for_dead_watcher() {
        // A monitors B; A dies. B's forward index must drop A.
        let r = ActorRegistry::new();
        let a = MailboxId(10);
        let b = MailboxId(20);
        insert_live_stub(&r, a);
        insert_live_stub(&r, b);
        r.register_monitor(a, b).unwrap();
        assert_eq!(r.monitor_count(b), 1);
        let _ = r.close_actor(a);
        assert_eq!(
            r.monitor_count(b),
            0,
            "dead watcher should be pruned from b's monitors_of",
        );
    }

    #[test]
    fn close_actor_idempotent_when_already_dead() {
        let r = ActorRegistry::new();
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        let first = r.close_actor(target);
        let second = r.close_actor(target);
        assert!(first.is_empty(), "no monitors registered");
        assert!(second.is_empty(), "no replay of watchers on second call");
    }
}
