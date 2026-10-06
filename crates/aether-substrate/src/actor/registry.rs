//! Actor-lifecycle registry (ADR-0079, issue 607). Keyed by full-name
//! `MailboxId`, tracks actor slots, tombstones (retired full names), and
//! the bidirectional monitor indices. Namespace ownership lives in the
//! publication table (ADR-0241 §3).
//!
//! Distinct from [`crate::mail::registry::Registry`], which owns
//! mailbox-name → handler routing and kind descriptors, and which owns the
//! one [`ActorRegistry`] built per engine: everything that holds the routes
//! reaches this table through them.
//!
//! # Monitors and the close
//!
//! A monitor registration never refuses. `ActorRegistry::register_monitor`
//! answers whether the target is being watched or had already closed, and
//! in the second case its caller posts the target's
//! [`MonitorNotice`](aether_kinds::MonitorNotice) itself. Every registration
//! is therefore answered by exactly one notice if the target ever closes:
//! from the close when the entry was in the index, from the registration
//! when it was not.
//!
//! One table decides which: `tombstones`. A registration reads nothing
//! else, so a target with no slot (a composed or pumped root, an
//! inline-child alias) registers like any other. A close writes its
//! tombstone first, releases that lock, and only then drains
//! `monitors_of`; a registration holds the `monitors_of` write guard across
//! its tombstone read and its insert. The two serialize on that one guard:
//!
//! ```text
//! registration first, no tombstone yet   entry inserted; the drain takes it    notice from the close
//! registration first, tombstone written  nothing inserted; drain finds nothing notice from the registration
//! drain first                            tombstone was written before it       notice from the registration
//! ```
//!
//! No order gives no notice and none gives two. The only nested
//! acquisition is `monitors_of` then `tombstones`, in the registration; no
//! path holds `tombstones` or `actors` while it waits for `monitors_of`, so
//! the pair cannot deadlock.

// Registry RwLock guards are intentionally held across the full
// read-then-update or match-then-mutate sequence — releasing the
// guard mid-sequence would open a TOCTOU window where another writer
// could mutate the map between the `get` and the dependent action.
#![allow(clippy::significant_drop_tightening)]

use std::any::TypeId;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use aether_actor::ErasedActorRef;

use crate::mail::MailboxId;
use crate::mail::registry::effect::ActivationToken;

/// One actor slot in the registry. `Live` carries the actor's
/// `TypeId`. `Dead` is a sentinel for entries
/// whose dispatcher has joined and whose actor has dropped — mail
/// addressed to the slot warn-drops, and `spawn_child` rejects the
/// name for reuse. ADR-0079 §Drop / lifecycle.
///
/// Issue 629 / Phase A: the pre-629 `actor: Arc<dyn Any + Send + Sync>`
/// field retired. The actor itself is owned exclusively by its
/// dispatcher thread as `Box<A>`; the registry no longer holds a
/// cross-thread share.
///
/// The entry holds no part of the actor's inbox channel: the inbox owns
/// its channel's only strong sender
/// ([`SettlingInbox`](crate::chassis::inbox::SettlingInbox)). Mail
/// addressed to a dead instanced mailbox is discarded at its `Dropped`
/// route, or, once the closed actor's slot and inbox are freed, refused
/// and settled at the relay; mail that slips in between is settled by the
/// inbox's drop.
#[derive(Clone)]
pub enum ActorEntry {
    Starting { token: ActivationToken },
    Live { type_id: TypeId },
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
    pub(crate) watcher: MailboxId,
}

impl ActorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue 629 / Phase A: `true` only if the slot `actor` proves is still
    /// `Live`. Replaces the pre-629 `live_actor(id) -> Option<Arc<dyn Any +
    /// Send + Sync>>` accessor; the actor itself no longer escapes its
    /// dispatcher thread. Callers that needed the actor reference now
    /// read a cap-exported handle (drivers) or send mail (peers).
    /// `Dead` and missing both return `false` — callers can't
    /// distinguish via this path, by design (ADR-0079: `Dead` is opaque
    /// to lookup; spawn-time retirement check goes through
    /// `is_tombstoned`).
    ///
    /// Takes the proof the caller was handed at spawn (ADR-0230): a
    /// reference claims the actor reached `Live`, and this answers whether
    /// it still is.
    ///
    /// # Panics
    /// Panics if the `actors` `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior writer panicked under
    /// the guard, a substrate-level invariant violation.
    #[must_use]
    pub fn is_live(&self, actor: ErasedActorRef) -> bool {
        self.is_live_at(actor.id())
    }

    /// The positional body of [`Self::is_live`]: substrate-internal glue for
    /// the crate's own callers that hold a slot position rather than a proof.
    ///
    /// # Panics
    /// Panics if the `actors` `RwLock` is poisoned (see [`Self::is_live`]).
    pub(crate) fn is_live_at(&self, id: MailboxId) -> bool {
        let actors = self.actors.read().expect("actors lock poisoned; fail-fast per ADR-0063");
        matches!(actors.get(&id), Some(ActorEntry::Live { .. }))
    }

    /// Has the actor at `id` run its registry close? True only for a
    /// `Dead` slot, which [`Self::close_actor`] leaves before the route
    /// drop is queued. An actor that is still up answers `false`, and so
    /// does an id that never owned a slot (an inline-child alias, a name
    /// nothing was born at), which is what lets the test-support close wait
    /// tell an actor that closed and was released from one it does not
    /// cover.
    ///
    /// # Panics
    /// Panics if the `actors` `RwLock` is poisoned (see [`Self::is_live`]).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn is_closed_at(&self, id: MailboxId) -> bool {
        let actors = self.actors.read().expect("actors lock poisoned; fail-fast per ADR-0063");
        matches!(actors.get(&id), Some(ActorEntry::Dead))
    }

    /// Has this id been tombstoned (its actor closed)? `spawn_child`
    /// uses this in Phase 3 to reject reuse of retired full names.
    ///
    /// # Panics
    /// Panics if the `tombstones` `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior writer panicked under
    /// the guard, a substrate-level invariant violation.
    pub(crate) fn is_tombstoned(&self, id: MailboxId) -> bool {
        self.tombstones.read().expect("tombstones lock poisoned; fail-fast per ADR-0063").contains(&id)
    }

    /// Insert a `Live` actor entry under `id`, which must be empty.
    /// Returns `Err(())` for any occupied slot: a `Starting` or `Live`
    /// entry is a collision, and a `Dead` one is a retired name, which is
    /// never registered again (ADR-0079 §7). The spawn primitive already
    /// refuses a tombstoned id before it gets here; this holds the same
    /// rule in the actor registry itself. Used by the spawn primitive
    /// after init succeeds.
    pub(crate) fn insert_live(&self, id: MailboxId, type_id: TypeId) -> Result<(), ()> {
        match self.actors.write().expect("actors lock poisoned; fail-fast per ADR-0063").entry(id) {
            Entry::Occupied(_) => Err(()),
            Entry::Vacant(slot) => {
                slot.insert(ActorEntry::Live { type_id });
                Ok(())
            }
        }
    }

    /// Reserve lifecycle occupancy before a Starting route is published.
    pub(crate) fn reserve_starting(&self, id: MailboxId, token: ActivationToken) -> bool {
        if self.is_tombstoned(id) {
            return false;
        }
        let mut actors = self.actors.write().expect("actors lock poisoned; fail-fast per ADR-0063");
        if actors.contains_key(&id) {
            return false;
        }
        actors.insert(id, ActorEntry::Starting { token });
        true
    }

    /// Promote the exact token-owned reservation to its live entry.
    pub(crate) fn promote_starting(&self, id: MailboxId, token: ActivationToken, type_id: TypeId) {
        let mut actors = self.actors.write().expect("actors lock poisoned; fail-fast per ADR-0063");
        assert!(
            matches!(actors.get(&id), Some(ActorEntry::Starting { token: current }) if *current == token),
            "valid activation token must own its actor lifecycle reservation"
        );
        actors.insert(id, ActorEntry::Live { type_id });
    }

    /// Remove only the lifecycle reservation owned by `token`.
    pub(crate) fn rollback_starting(&self, id: MailboxId, token: ActivationToken) {
        let mut actors = self.actors.write().expect("actors lock poisoned; fail-fast per ADR-0063");
        if matches!(actors.get(&id), Some(ActorEntry::Starting { token: current }) if *current == token) {
            actors.remove(&id);
        }
    }

    /// Issue 607 Phase 4a (ADR-0079): flip the slot at `id` from
    /// `Live` to `Dead` and insert the id into `tombstones`. Called
    /// by the instanced-actor dispatcher after `unwire` runs so
    /// future `spawn_child` calls reject reuse of the retired name
    /// with `SpawnError::SubnameRetired`. Idempotent — re-running on
    /// an already-`Dead` slot leaves it `Dead` and doesn't double-
    /// insert into `tombstones`.
    pub(crate) fn mark_dead(&self, id: MailboxId) {
        let mut actors = self.actors.write().expect("actors lock poisoned; fail-fast per ADR-0063");
        actors.insert(id, ActorEntry::Dead);
        drop(actors);
        let mut tombstones = self.tombstones.write().expect("tombstones lock poisoned; fail-fast per ADR-0063");
        tombstones.insert(id);
    }

    /// Register `watcher` as a monitor of `target` (ADR-0079 §8). Answers
    /// `true` when the entry is in the index, so `target`'s close will
    /// drain it and notify `watcher`; `false` when `target` had already
    /// closed, with neither index written, so the caller owes `watcher` the
    /// notice that close can no longer send. It never refuses: see the
    /// [module docs](self) for why exactly one of the two notices is sent
    /// under a concurrent close.
    ///
    /// The `monitors_of` write guard is held across the tombstone read and
    /// the forward insert, which is what serializes this against a close's
    /// drain. The tombstone set is the only liveness authority read: a
    /// target owns a slot in `actors` only if it was born instanced, so a
    /// slot check would refuse every root and every inline-child alias.
    ///
    /// The caller pairs a `true` answer with a
    /// [`MonitorHandle`](crate::actor::monitor::MonitorHandle), whose `Drop`
    /// deregisters the entry; a bare caller (a test) owns that cleanup.
    #[must_use]
    pub(crate) fn register_monitor(&self, watcher: MailboxId, target: MailboxId) -> bool {
        {
            let mut forward = self.monitors_of.write().expect("monitors_of lock poisoned; fail-fast per ADR-0063");
            if self.is_tombstoned(target) {
                return false;
            }
            forward.entry(target).or_default().push(MonitorEntry { watcher });
        }
        self.link_watcher(watcher, target);
        true
    }

    /// Insert the reverse `monitoring[watcher]` edge, only ever after the
    /// forward edge committed. Transient observability of forward without
    /// reverse is benign — the close path looks at both, and either
    /// direction missing just makes one cleanup step a no-op.
    fn link_watcher(&self, watcher: MailboxId, target: MailboxId) {
        self.monitoring
            .write()
            .expect("monitoring lock poisoned; fail-fast per ADR-0063")
            .entry(watcher)
            .or_default()
            .push(target);
    }

    /// Issue 607 Phase 4b (ADR-0079): undo a prior `register_monitor`
    /// call. Idempotent — removing a monitor that was already pruned
    /// (e.g. because the target closed and the close path drained the
    /// forward index) is a no-op. Called by
    /// [`crate::actor::monitor::MonitorHandle`]'s `Drop` when the handle
    /// goes out of scope.
    pub(crate) fn deregister_monitor(&self, watcher: MailboxId, target: MailboxId) {
        if let Some(entries) =
            self.monitors_of.write().expect("monitors_of lock poisoned; fail-fast per ADR-0063").get_mut(&target)
        {
            entries.retain(|e| e.watcher != watcher);
        }
        if let Some(targets) =
            self.monitoring.write().expect("monitoring lock poisoned; fail-fast per ADR-0063").get_mut(&watcher)
        {
            targets.retain(|t| *t != target);
        }
    }

    /// Issue 607 Phase 4b (ADR-0079): close path. Calls
    /// [`Self::mark_dead`] first to flip the slot `Live` → `Dead` and
    /// insert the tombstone, then drains `monitors_of[id]` (returning the
    /// watcher list for the caller to fan out
    /// [`aether_kinds::MonitorNotice`] mail) and walks `monitoring[id]` to
    /// prune `id` from each watched target's forward index.
    ///
    /// `mark_dead` runs *first* so the tombstone is written before the
    /// forward index is drained: a concurrent `register_monitor` that takes
    /// the `monitors_of` write guard after the drain then reads the
    /// tombstone, inserts nothing, and answers that the target had closed.
    /// Draining first would let that registration insert an entry no close
    /// will ever take.
    ///
    /// One method (rather than three separate calls) so the dispatcher
    /// trampoline can't accidentally skip a step on close — the
    /// tombstone + fan-out + reverse-prune are all part of the same
    /// retire-this-id transaction. Idempotent: a second call on an
    /// already-`Dead` slot returns an empty watcher list and does no
    /// further work.
    pub(crate) fn close_actor(&self, id: MailboxId) -> Vec<MailboxId> {
        // Tombstone first: a `register_monitor` that takes the `monitors_of`
        // guard after the drain below reads it and inserts nothing.
        self.mark_dead(id);
        self.drain_closed(id)
    }

    /// ADR-0241 §8: close an inline child's `alias`, which ends with its child
    /// — on a despawn, or when the parent it is folded onto closes. The same
    /// transaction as [`Self::close_actor`] in the same order, tombstone first
    /// and then the drain, less the `Dead` slot: an alias is served by its
    /// parent's slot and owns none (ADR-0114 §2).
    ///
    /// The tombstone is the synchronous authority the inline spawn host fn and
    /// [`Self::register_monitor`] read, so a despawned key is refused before
    /// an id reaches the guest, and a watch on a despawned alias is answered
    /// with its notice at once. Idempotent, like `close_actor`.
    pub(crate) fn close_alias(&self, alias: MailboxId) -> Vec<MailboxId> {
        self.tombstones.write().expect("tombstones lock poisoned; fail-fast per ADR-0063").insert(alias);
        self.drain_closed(alias)
    }

    /// The drain half of a close: take `monitors_of[id]` whole as the watcher
    /// list to fan out, then prune `id` from each target it was watching.
    /// Runs only after `id` is tombstoned, so a registration serialized after
    /// the forward drain reads the tombstone and inserts nothing.
    fn drain_closed(&self, id: MailboxId) -> Vec<MailboxId> {
        // Forward index: take the watcher list whole.
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
        let monitoring_targets =
            self.monitoring.write().expect("monitoring lock poisoned; fail-fast per ADR-0063").remove(&id);
        if let Some(targets) = monitoring_targets {
            let mut forward = self.monitors_of.write().expect("monitors_of lock poisoned; fail-fast per ADR-0063");
            for target in targets {
                if let Some(entries) = forward.get_mut(&target) {
                    entries.retain(|e| e.watcher != id);
                }
            }
        }
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
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn fresh_registry_is_empty() {
        let r = ActorRegistry::new();
        assert!(!r.is_live_at(MailboxId(1)));
        assert!(!r.is_tombstoned(MailboxId(1)));
        assert_eq!(r.monitor_count(MailboxId(1)), 0);
        assert_eq!(r.monitoring_count(MailboxId(1)), 0);
    }

    /// Helper: insert a `Live` slot at `id`, as an instanced birth does.
    /// Uses a stub type so the test doesn't drag in `NativeActor`.
    fn insert_live_stub(r: &ActorRegistry, id: MailboxId) {
        struct Stub;
        r.insert_live(id, TypeId::of::<Stub>()).expect("fresh slot");
    }

    /// A target that owns no slot registers: a composed or pumped root and
    /// an inline-child alias are never written to `actors`. A slot check
    /// coming back into the registration would answer `false` here and
    /// leave every watcher of a root unnoticed.
    #[test]
    fn register_monitor_watches_a_target_with_no_slot() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);

        assert!(r.register_monitor(watcher, target), "a target with no slot is watched");
        assert_eq!(r.close_actor(target), vec![watcher], "its close drains the watcher");
    }

    /// A target that had already closed writes neither index. An entry
    /// written here would sit in `monitors_of` for good, since the drain
    /// that takes entries has already run.
    #[test]
    fn register_monitor_on_a_closed_target_writes_neither_index() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        let _ = r.close_actor(target);

        assert!(!r.register_monitor(watcher, target), "the closed target is reported, not watched");
        assert_eq!(r.monitor_count(target), 0);
        assert_eq!(r.monitoring_count(watcher), 0);
    }

    /// The three orders a registration and a close can serialize in, each
    /// forced by calling the close's two halves around the registration.
    /// Every order must produce exactly one notice: the drained list holds
    /// the watcher, or the registration answers `false` and its caller
    /// posts. Zero is a watcher never told; two is a notice handled twice.
    #[test]
    fn every_order_of_registration_and_close_owes_one_notice() {
        let watcher = MailboxId(1);
        let target = MailboxId(2);

        // Registration, then the whole close: the drain takes the entry.
        let r = ActorRegistry::new();
        let watching = r.register_monitor(watcher, target);
        r.mark_dead(target);
        let drained = r.drain_closed(target);
        assert!(watching);
        assert_eq!(drained, vec![watcher], "the close owes the notice");

        // Tombstone, then registration, then the drain: the close has
        // marked the target dead and is waiting for the `monitors_of`
        // guard the registration holds.
        let r = ActorRegistry::new();
        r.mark_dead(target);
        let watching = r.register_monitor(watcher, target);
        let drained = r.drain_closed(target);
        assert!(!watching, "the registration owes the notice");
        assert!(drained.is_empty(), "the drain finds no entry to notify a second time");

        // The whole close, then registration.
        let r = ActorRegistry::new();
        r.mark_dead(target);
        let drained = r.drain_closed(target);
        let watching = r.register_monitor(watcher, target);
        assert!(drained.is_empty());
        assert!(!watching, "the registration owes the notice");
        assert_eq!(r.monitor_count(target), 0, "no entry outlives the close");
    }

    /// An inline-child alias closes through `close_alias`, which writes no
    /// slot. The same three orders hold for it through the one
    /// registration: a regression that tombstoned the alias after draining
    /// it would let the last registration insert an entry nothing takes.
    #[test]
    fn a_closed_alias_is_reported_and_holds_no_entry() {
        let r = ActorRegistry::new();
        let early = MailboxId(1);
        let late = MailboxId(3);
        let alias = MailboxId(2);

        assert!(r.register_monitor(early, alias));
        assert_eq!(r.close_alias(alias), vec![early]);
        assert!(!r.register_monitor(late, alias));
        assert_eq!(r.monitor_count(alias), 0);
    }

    #[test]
    fn register_monitor_populates_both_indices() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        assert!(r.register_monitor(watcher, target));
        assert_eq!(r.monitor_count(target), 1);
        assert_eq!(r.monitoring_count(watcher), 1);
    }

    #[test]
    fn deregister_monitor_clears_both_indices() {
        let r = ActorRegistry::new();
        let watcher = MailboxId(1);
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        assert!(r.register_monitor(watcher, target));
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
        assert!(r.register_monitor(watcher_a, target));
        assert!(r.register_monitor(watcher_b, target));
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
        assert!(r.register_monitor(a, b));
        assert_eq!(r.monitor_count(b), 1);
        let _ = r.close_actor(a);
        assert_eq!(r.monitor_count(b), 0, "dead watcher should be pruned from b's monitors_of");
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

    /// A closed actor's name is never registered again (ADR-0079 §7): a
    /// regression that installs over the `Dead` slot would revive the id
    /// under a new actor.
    #[test]
    fn insert_live_refuses_a_closed_slot() {
        struct Stub;
        let r = ActorRegistry::new();
        let target = MailboxId(2);
        insert_live_stub(&r, target);
        let _ = r.close_actor(target);

        assert!(r.insert_live(target, TypeId::of::<Stub>()).is_err());
        assert!(!r.is_live_at(target), "the closed slot stays dead");
    }

    /// Iterations of the registration-against-close race. Each is two
    /// thread spawns, so the whole loop stays well under a second.
    const RACE_ITERATIONS: u64 = 5_000;

    /// A registration that races a close is answered by exactly one notice.
    /// Each iteration runs one `register_monitor` and one `close_actor` on
    /// the same target from two threads released together by a barrier,
    /// then counts the notices owed: one if the close's drained list holds
    /// the watcher, one if the registration answered `false`. The count
    /// must be one, and the forward index must be empty.
    ///
    /// The count is zero within the first few iterations if the close
    /// drains before it tombstones, and two if the registration reports a
    /// closed target after inserting. With the read and the insert under
    /// one guard no interleaving fails, so the loop cannot flake. It does
    /// not reach a tombstone read hoisted out of the guard, whose window is
    /// a few instructions wide;
    /// `a_close_cannot_pass_between_the_tombstone_read_and_the_insert` holds
    /// that.
    ///
    /// The reverse `monitoring` index is watcher-keyed and cleaned on the
    /// *watcher*'s own close (or `deregister_monitor`), not the target's, so
    /// a reverse edge to a dead target is a normal, benign state even
    /// without a race and is deliberately not asserted here.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "raw thread spawn is the point: this test drives register_monitor and \
                  close_actor on two real OS threads to exercise their interleaving, not \
                  actor work under the settlement umbrella"
    )]
    fn register_racing_close_owes_exactly_one_notice() {
        use std::sync::Barrier;
        use std::thread;

        for i in 0..RACE_ITERATIONS {
            let r = Arc::new(ActorRegistry::new());
            let target = MailboxId(0x0001_0000 + i);
            let watcher = MailboxId(0x0002_0000 + i);
            insert_live_stub(&r, target);

            let gate = Arc::new(Barrier::new(2));
            let (r_reg, g_reg) = (Arc::clone(&r), Arc::clone(&gate));
            let (r_close, g_close) = (Arc::clone(&r), Arc::clone(&gate));
            let t_reg = thread::spawn(move || {
                g_reg.wait();
                r_reg.register_monitor(watcher, target)
            });
            let t_close = thread::spawn(move || {
                g_close.wait();
                r_close.close_actor(target)
            });
            let watching = t_reg.join().expect("register thread joins");
            let drained = t_close.join().expect("close thread joins");

            let from_close = usize::from(drained.contains(&watcher));
            let from_registration = usize::from(!watching);
            assert_eq!(
                from_close + from_registration,
                1,
                "iteration {i}: the close owed {from_close} notice(s) and the registration {from_registration}",
            );
            assert_eq!(r.monitor_count(target), 0, "iteration {i}: an entry outlived the close");
        }
    }

    /// Iterations of the parked-registration tripwire below.
    const PARKED_ITERATIONS: u64 = 200;

    /// Tripwire: the tombstone read and the insert are one critical section,
    /// so no close can pass between them.
    ///
    /// The test holds the `monitors_of` write guard, which parks a
    /// registration started meanwhile, and runs a whole close under that
    /// hold: the tombstone, then the drain's take, through the guard it
    /// holds. A registration that reads the tombstone under the guard reads
    /// it after the close and answers `false`. One that read it before
    /// taking the guard has already read "open": it inserts an entry the
    /// drain has passed and answers `true`, and its watcher is never told.
    ///
    /// A correct registration passes whatever the scheduling. A hoisted read
    /// is caught on every iteration where the registration thread reached
    /// the guard before the close began. The yields after its start signal
    /// make that the usual case, and the loop repeats it so that no one
    /// scheduling decides the result.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "raw thread spawn is the point: the registration has to be parked on a lock \
                  this thread holds, which no actor under the settlement umbrella can be"
    )]
    fn a_close_cannot_pass_between_the_tombstone_read_and_the_insert() {
        use std::sync::mpsc;
        use std::thread;

        for i in 0..PARKED_ITERATIONS {
            let r = Arc::new(ActorRegistry::new());
            let target = MailboxId(0x0003_0000 + i);
            let watcher = MailboxId(0x0004_0000 + i);
            let (started_tx, started) = mpsc::channel();

            let mut forward = r.monitors_of.write().expect("monitors_of lock is free");
            let parked = Arc::clone(&r);
            let registration = thread::spawn(move || {
                let _ = started_tx.send(());
                parked.register_monitor(watcher, target)
            });
            started.recv().expect("the registration thread starts");
            for _ in 0..64 {
                thread::yield_now();
            }
            r.mark_dead(target);
            let drained = forward.remove(&target);
            drop(forward);
            let watching = registration.join().expect("registration thread joins");

            assert!(drained.is_none(), "iteration {i}: the registration inserted without the guard");
            assert!(!watching, "iteration {i}: the registration read the tombstone before it took the guard");
            assert_eq!(r.monitor_count(target), 0, "iteration {i}: an entry outlived the close");
        }
    }
}
