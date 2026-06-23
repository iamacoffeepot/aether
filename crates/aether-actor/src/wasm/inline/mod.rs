//! Inline-child registry + receive membrane (ADR-0114 decisions #2/#3).
//!
//! An inline child shares its parent's WASM instance, slot, and
//! run-token (ADR-0114). [`WasmCtx::spawn_inline_child`] inserts
//! the constructed child into the per-component [`InlineRegistry`] the
//! [`crate::export!`] macro emits as a `static __AETHER_INLINE` (one per
//! component, mirroring the parent's `static __AETHER_COMPONENT` slot),
//! keyed by the child's alias [`MailboxId`]. The `export!` `receive_p32`
//! shims hand that registry to [`membrane_dispatch`], which dispatches the
//! parent when the routed recipient is the parent's own id and otherwise
//! demuxes to the co-located child the producer addressed.
//!
//! The registry is a `BTreeMap<MailboxId, InlineSlot>` — every keyed
//! operation (`take`, `reinsert`, `with_child_mut`, `remove`,
//! `insert_child`) is O(log n) in the resident child count. `MailboxId`
//! derives `Ord` and `BTreeMap::new()` is `const`, so the map still backs
//! a `static __AETHER_INLINE` with no init-time cost.
//!
//! The registry is slot-shaped (take-out / dispatch / reinsert) so a
//! running child can spawn or mutate siblings through `ctx` while it is
//! itself dispatched — the registry borrow is never held across a child's
//! `erased_dispatch`. The guest is single-threaded (ADR-0010 §5) and the
//! substrate serializes delivery under the run token, so an `UnsafeCell`
//! with a blanket `Sync` impl is sound — the same argument that licenses
//! [`crate::Slot`].
//!
//! Beyond child demux the registry is also the cluster's runtime structure
//! and router (ADR-0114 addressing amendment). It holds the instance's real
//! folded [`MailboxId`] — `self_id`, captured from the `init` / `wire`
//! argument so the instance is addressable at any lineage depth rather than
//! only at the ADR-0099 depth-1 `hash(NAMESPACE)` fixed point — and each
//! child's logical `parent`, so relative addressing (parent / sibling /
//! child) resolves by registry lookup, never by folding (a `MailboxId` is a
//! one-way hash chain, so the guest cannot reproduce a relative's id). A
//! send to a cluster member (own id or a resident child alias) is pushed to
//! the per-component queue and [`drain_cluster_queue`] dispatches it in
//! place through the membrane after the top-level dispatch returns, so the
//! whole intra-cluster cascade settles inside one `receive_p32` call under
//! one run-token — only cross-cluster mail hands off to the scheduler.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, UnsafeCell};

use aether_data::MailboxId;

use crate::mail::{Mail, NO_REPLY_HANDLE};
use crate::wasm::ErasedWasmActor;
use crate::wasm::bridge::mail;
use crate::wasm::ctx::WasmCtx;

mod bundle;
pub mod compose;

/// One inline child's slot. `actor` is `None` while the child is taken
/// out for dispatch (the slot-shaped take / reinsert) and `Some` at rest.
/// The child's alias [`MailboxId`] is carried as the map key in
/// [`InlineRegistry`]; there is no redundant `id` field here.
///
/// ADR-0114 §5: the slot also records the metadata a `replace_component`
/// swap needs to reconstruct the child in the fresh instance — the
/// actor-type tag (`mailbox_id_from_name(NAMESPACE)`, the same tag
/// `init_typed_p32` matches a reconstruct on) plus the resolved
/// `full_subname` / `is_counter` the alias id was folded from, so the
/// rehydrate path re-folds the identical alias and re-`init`s the child
/// by type.
struct InlineSlot {
    /// `mailbox_id_from_name(A::NAMESPACE)` — the actor-type tag the
    /// rehydrate reconstruct matches against the module's exported types.
    type_tag: u64,
    /// The resolved discriminator the alias id was folded from (a counter
    /// child's monotonic value is already resolved here, not the
    /// unresolved `Counter` marker), so re-folding on rehydrate is
    /// deterministic.
    full_subname: String,
    /// Whether the host should treat `full_subname` as a counter prefix on
    /// re-fold; always `false` after resolution, but carried so the
    /// rehydrate call mirrors the original `spawn_inline_child` shape.
    is_counter: bool,
    /// The real folded [`MailboxId`] of the actor that spawned this child
    /// (the spawning ctx's own id at `spawn_inline_child` time). The
    /// logical-tree link the relative-addressing lookups
    /// ([`InlineRegistry::parent_of`] / [`InlineRegistry::child_of`] /
    /// [`InlineRegistry::sibling_of`]) walk — resolution is pure registry
    /// lookup, never a fold (a `MailboxId` is a one-way hash chain, so the
    /// guest cannot reproduce a relative's id by folding; it looks the
    /// recorded id up instead).
    parent: u64,
    actor: Option<Box<dyn ErasedWasmActor>>,
}

/// A cloneable snapshot of one resident inline child's reconstruct
/// metadata (no actor box), produced by [`InlineRegistry::child_metas`]
/// for the dehydrate walk. The compose path reads each child's state
/// through [`InlineRegistry::with_child_mut`] keyed by `id`.
#[derive(Clone)]
pub(crate) struct InlineChildMeta {
    /// The child's alias [`MailboxId`] (the registry key).
    pub(crate) id: MailboxId,
    /// The actor-type tag — `mailbox_id_from_name(NAMESPACE)`.
    pub(crate) type_tag: u64,
    /// The resolved subname the alias id was folded from.
    pub(crate) full_subname: String,
    /// Whether the original spawn used a counter discriminator.
    pub(crate) is_counter: bool,
}

/// One intra-cluster send buffered on the per-component queue
/// ([`InlineRegistry`]). A send whose recipient is a member of this cluster
/// — the instance itself or one of its inline children — is pushed here
/// rather than handed to the host; [`drain_cluster_queue`] dispatches each
/// one through the membrane after the top-level dispatch returns, so the
/// whole local cascade settles inside one `receive_p32` call under one
/// run-token (no scheduler hop). The `bytes` are owned so they outlive the
/// drain's `Mail` borrow.
struct QueuedMail {
    recipient: u64,
    kind: u64,
    bytes: Vec<u8>,
    count: u32,
    /// The sending actor's own folded [`MailboxId`] raw value — the "from"
    /// half of an in-place send. An in-place dispatch carries `NO_REPLY_HANDLE`
    /// (the local fast path is fire-and-forget), so the host reply table holds
    /// no immediate-sender for it; [`drain_cluster_queue`] instead threads this
    /// value onto the recipient's [`WasmCtx`] as its inbound source (issue
    /// 1987), so the recipient's `ctx.source_mailbox()` resolves it. `0`
    /// (`MailboxId::NONE`) when the sender is unknown.
    sender: u64,
}

/// Whether a resolved recipient is in this cluster (dispatch in place
/// through the queue) or outside it (hand to the host). The membership
/// decision is factored out of [`InlineRegistry::route_or_enqueue`] as this
/// pure value so it is unit-testable without a live `MAIL_BRIDGE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteDecision {
    /// The recipient is a cluster member; enqueue for in-place dispatch.
    Local,
    /// The recipient is outside the cluster; send through the host.
    Remote,
}

/// The per-component inline-child registry (ADR-0114 decision #3), keyed
/// by each child's alias [`MailboxId`]. The [`crate::export!`] macro emits
/// one as a `static __AETHER_INLINE` per component (mirroring the parent's
/// `static __AETHER_COMPONENT` slot) and threads it to the membrane; the
/// membrane demuxes the inbound recipient against it. Every keyed
/// operation is O(log n) in the resident child count (`BTreeMap` lookup).
///
/// Beyond the child slot map the registry also holds the cluster's runtime
/// structure and router (ADR-0114 addressing amendment): `self_id` is the
/// instance's real folded [`MailboxId`] (captured from the `init` / `wire`
/// argument, not recomputed from `hash(NAMESPACE)`), so the instance is
/// addressable at any lineage depth; `queue` is the cluster-local mail
/// queue an intra-cluster send is pushed to and [`drain_cluster_queue`]
/// drains in place.
pub struct InlineRegistry {
    inner: UnsafeCell<BTreeMap<MailboxId, InlineSlot>>,
    /// The instance's real folded [`MailboxId`] (`Tag::Mailbox`-tagged),
    /// set once from the `init` / `wire` shim's `mailbox_id` argument — the
    /// id the substrate registered for this trampoline (`store.data()
    /// .sender.0`). `0` until set; the receive shim falls back to
    /// `hash(NAMESPACE)` only while it is still `0` (a receive before
    /// `wire`, which should not happen). The instance's runtime identity at
    /// any depth, not the ADR-0099 depth-1 fixed point.
    self_id: Cell<u64>,
    /// The cluster-local mail queue. A send to a cluster member is pushed
    /// here ([`Self::route_or_enqueue`]) instead of going to the host;
    /// [`drain_cluster_queue`] dispatches each item through the membrane
    /// after the top-level dispatch returns, so a child → parent → sibling
    /// cascade settles in one `receive_p32` call. Reentrancy and cycles are
    /// handled by the queue — a busy target is just a later queue item —
    /// not by nested dispatch.
    queue: UnsafeCell<VecDeque<QueuedMail>>,
}

// SAFETY: identical argument to [`crate::Slot`] — the WASM guest is
// single-threaded (ADR-0010 §5) and the substrate serializes delivery
// under the run token, so a `static __AETHER_INLINE` is only ever touched
// from one thread at a time. On the host unit-test build each test owns a
// local registry, reached from one test thread. The same argument covers
// the added interior-mutable fields (`self_id`, `queue`): each is touched
// only from the single run-token thread, and every borrow of `queue` is
// taken fresh and released before return (never spanning a dispatch).
unsafe impl Sync for InlineRegistry {}

impl InlineRegistry {
    /// An empty registry. `const` so it can back a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(BTreeMap::new()),
            self_id: Cell::new(0),
            queue: UnsafeCell::new(VecDeque::new()),
        }
    }

    /// Record the instance's real folded [`MailboxId`] — the `mailbox_id`
    /// argument the substrate passes the `init` / `wire` shims, which is the
    /// id it registered for this trampoline (`store.data().sender.0`). Set
    /// once from the first shim that runs; the receive path then uses it as
    /// the cluster's self-identity for the membrane and the ctx, so the
    /// instance is addressable at any lineage depth rather than only at the
    /// ADR-0099 depth-1 fixed point. Idempotent re-sets (each `init` /
    /// `wire` shim sets it) write the same value.
    pub fn set_self_id(&self, id: u64) {
        self.self_id.set(id);
    }

    /// The instance's real folded [`MailboxId`] raw value, or `0` if no
    /// `init` / `wire` shim has run yet (the receive path falls back to
    /// `hash(NAMESPACE)` only in that should-not-happen window).
    #[must_use]
    pub fn self_id(&self) -> u64 {
        self.self_id.get()
    }

    /// Register a freshly-spawned (or reconstructed) inline child under
    /// `id`, recording the reconstruct metadata + the spawner's `parent` id
    /// alongside the actor box. Replaces the actor + metadata if `id` is
    /// already present (a re-spawn / rehydrate re-register of the same
    /// alias). O(log n).
    pub(crate) fn insert_child(
        &self,
        id: MailboxId,
        type_tag: u64,
        full_subname: String,
        is_counter: bool,
        parent: u64,
        actor: Box<dyn ErasedWasmActor>,
    ) {
        // SAFETY: single-threaded guest + serialized delivery — no other
        // live borrow of the cell (the `Sync` argument). The borrow is
        // released before this returns, so it never spans a dispatch.
        let map = unsafe { &mut *self.inner.get() };
        if let Some(slot) = map.get_mut(&id) {
            slot.type_tag = type_tag;
            slot.full_subname = full_subname;
            slot.is_counter = is_counter;
            slot.parent = parent;
            slot.actor = Some(actor);
        } else {
            map.insert(
                id,
                InlineSlot {
                    type_tag,
                    full_subname,
                    is_counter,
                    parent,
                    actor: Some(actor),
                },
            );
        }
    }

    /// Take the child out for dispatch, leaving its slot (and its
    /// reconstruct metadata) intact but the actor box empty. Returns
    /// `None` if `id` names no resident inline child (already taken out,
    /// or never registered). The borrow drops before the returned box is
    /// dispatched, so a child may re-enter the registry mid-dispatch.
    /// O(log n).
    pub(crate) fn take(&self, id: MailboxId) -> Option<Box<dyn ErasedWasmActor>> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &mut *self.inner.get() };
        map.get_mut(&id).and_then(|s| s.actor.take())
    }

    /// Put a child back after dispatch, into its existing slot (metadata
    /// preserved). Pairs with [`Self::take`]; the slot is guaranteed to
    /// exist because `take` left it in place with an empty actor box.
    ///
    /// The lookup-then-set is deliberately a no-op when no slot matches `id`:
    /// a child despawned mid-dispatch (its slot already
    /// [`removed`](Self::remove) while it was taken out) has nowhere to go
    /// back to, so the live box drops at end of scope rather than
    /// re-entering the registry. This no-op is what makes self-despawn
    /// fall out for free (ADR-0114) — no pending-removal flag. O(log n).
    pub(crate) fn reinsert(&self, id: MailboxId, actor: Box<dyn ErasedWasmActor>) {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &mut *self.inner.get() };
        if let Some(slot) = map.get_mut(&id) {
            slot.actor = Some(actor);
        }
    }

    /// Tear down the inline child registered under `id` (ADR-0114
    /// teardown): remove its slot, dropping the resident
    /// `Box<dyn ErasedWasmActor>` so the child's `Drop` runs. Returns
    /// `true` if a slot was present, `false` if `id` named no inline child
    /// (idempotent — a re-despawn of an already-gone alias is a clean
    /// `false`, not an error). Backs [`WasmCtx::despawn_inline_child`].
    /// O(log n).
    ///
    /// If the child is currently taken out for dispatch (a self-despawn:
    /// its slot's actor box is `None`, the live box held on the stack by
    /// [`membrane_dispatch`]), removing the empty slot makes the matching
    /// [`Self::reinsert`] find nothing and no-op, so the box drops at end
    /// of dispatch instead of re-entering — the reentrant case the
    /// slot-shaped take/reinsert design handles with no extra state.
    pub(crate) fn remove(&self, id: MailboxId) -> bool {
        // SAFETY: see [`Self::insert_child`] — the borrow is taken fresh
        // and released before return, never spanning a dispatch.
        let map = unsafe { &mut *self.inner.get() };
        map.remove(&id).is_some()
    }

    /// Snapshot the reconstruct metadata of every resident inline child
    /// (ADR-0114 §5 dehydrate walk). The actor boxes stay in the
    /// registry; the compose path reads each child's state through
    /// [`Self::with_child_mut`] keyed by the returned `id`. Children are
    /// returned in [`MailboxId`] key order; the dehydrate/rehydrate walk
    /// reconstructs each child independently by its own `alias_id` /
    /// `type_tag` / `full_subname`, so order is irrelevant.
    #[must_use]
    pub(crate) fn child_metas(&self) -> Vec<InlineChildMeta> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &*self.inner.get() };
        map.iter()
            .map(|(key, slot)| InlineChildMeta {
                id: *key,
                type_tag: slot.type_tag,
                full_subname: slot.full_subname.clone(),
                is_counter: slot.is_counter,
            })
            .collect()
    }

    /// Run `f` against the child registered under `id` with a unique
    /// mutable borrow held only for the call, returning its result (or
    /// `None` if `id` names no resident child). Used by the dehydrate
    /// compose to drive each child's `erased_on_dehydrate` in place. The
    /// borrow drops before this returns, so it never spans a dispatch.
    /// O(log n).
    pub(crate) fn with_child_mut<R>(
        &self,
        id: MailboxId,
        f: impl FnOnce(&mut dyn ErasedWasmActor) -> R,
    ) -> Option<R> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &mut *self.inner.get() };
        map.get_mut(&id).and_then(|s| s.actor.as_deref_mut()).map(f)
    }

    /// The recorded parent of the inline child registered under `id`, or
    /// `None` if `id` names no resident child (`id` is the cluster root —
    /// the instance itself, whose parent is cross-cluster — or a stray
    /// address). Pure registry lookup, never a fold. O(log n).
    #[must_use]
    pub(crate) fn parent_of(&self, id: MailboxId) -> Option<MailboxId> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &*self.inner.get() };
        map.get(&id).map(|slot| MailboxId(slot.parent))
    }

    /// The inline child of `parent` whose resolved subname is `subname`, or
    /// `None` if no resident child matches. A child's id is recorded at
    /// spawn time, so this is a scan over resident children for one whose
    /// `(parent, full_subname)` matches — pure lookup, never a fold. The
    /// resident child count is small (a cluster's widget set), so the linear
    /// scan is cheap.
    #[must_use]
    pub(crate) fn child_of(&self, parent: MailboxId, subname: &str) -> Option<MailboxId> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &*self.inner.get() };
        map.iter()
            .find(|(_, slot)| slot.parent == parent.0 && slot.full_subname == subname)
            .map(|(key, _)| *key)
    }

    /// The sibling of the inline child registered under `id` whose resolved
    /// subname is `subname` — the child of `id`'s parent named `subname`.
    /// `None` if `id` has no recorded parent or no such sibling resides.
    /// Pure registry lookup, never a fold.
    #[must_use]
    pub(crate) fn sibling_of(&self, id: MailboxId, subname: &str) -> Option<MailboxId> {
        let parent = self.parent_of(id)?;
        self.child_of(parent, subname)
    }

    /// Whether `recipient` is a member of this cluster (the instance's real
    /// `self_id`, or a resident inline-child alias). The pure membership
    /// decision behind [`Self::route_or_enqueue`], split out so the
    /// local-vs-host routing is unit-testable without a live `MAIL_BRIDGE`.
    #[must_use]
    pub(crate) fn route_decision(&self, recipient: u64) -> RouteDecision {
        if recipient == self.self_id.get() {
            return RouteDecision::Local;
        }
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &*self.inner.get() };
        if map.contains_key(&MailboxId(recipient)) {
            RouteDecision::Local
        } else {
            RouteDecision::Remote
        }
    }

    /// Route an outbound send. If `recipient` is a cluster member (the
    /// instance itself or a resident inline child) the send is pushed to
    /// the cluster-local queue for in-place dispatch by
    /// [`drain_cluster_queue`]; otherwise it goes to the host
    /// (`MAIL_BRIDGE.send_mail`) like any cross-cluster send. `detached`
    /// rides through to the host on the remote path (the lineage signal,
    /// ADR-0080 §7); an in-place dispatch carries no host trace ids, so the
    /// flag is irrelevant on the local path.
    ///
    /// `sender` is the sending actor's own folded [`MailboxId`] raw value —
    /// the "from" half. On the `Local` branch it is stored in the
    /// [`QueuedMail`] so [`drain_cluster_queue`] can thread it onto the
    /// recipient's [`WasmCtx`] as its inbound source (the in-place reply
    /// table is empty, so this is the only carrier of an in-place send's
    /// immediate sender). On the `Remote` branch it is threaded to the host
    /// as the send's `from` (issue 1987), so the host stamps origin from the
    /// sending actor's id without an ambient per-receive cell.
    ///
    /// Cross-cluster from in place (issue 1987): a cross-cluster send made by
    /// an inline child *during the in-place drain* takes this `Remote` branch
    /// and threads `sender` (the dispatched member's own id, which the drain
    /// set as the ctx's identity) as the host send's `from`, so the host stamps
    /// the member as origin rather than the cluster's inbound recipient. The
    /// host validates the claim to this cluster, so a member's own outbound
    /// mail carries the member as origin and a guest cannot spoof a foreign id.
    pub(crate) fn route_or_enqueue(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        detached: bool,
        sender: u64,
    ) {
        match self.route_decision(recipient) {
            RouteDecision::Local => {
                // SAFETY: see [`Self::insert_child`] — the queue borrow is
                // taken fresh and released before return, never spanning a
                // dispatch (the drain re-borrows per item).
                let queue = unsafe { &mut *self.queue.get() };
                queue.push_back(QueuedMail {
                    recipient,
                    kind,
                    bytes: bytes.to_vec(),
                    count,
                    sender,
                });
            }
            RouteDecision::Remote => {
                mail::send_mail(recipient, kind, bytes, count, detached, sender);
            }
        }
    }

    /// Pop the next buffered intra-cluster send, or `None` when the queue is
    /// drained. Backs [`drain_cluster_queue`]; each popped item is
    /// dispatched through the membrane before the next pop, so an item whose
    /// dispatch enqueues more work drains in the same loop.
    fn pop_queued(&self) -> Option<QueuedMail> {
        // SAFETY: see [`Self::insert_child`] — borrow taken fresh, released
        // before return.
        let queue = unsafe { &mut *self.queue.get() };
        queue.pop_front()
    }

    /// The number of buffered intra-cluster sends currently on the queue.
    /// Crate-internal test observability for the local-routing path (the
    /// `queue` field itself is private).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn queued_len(&self) -> usize {
        // SAFETY: see [`Self::insert_child`] — borrow taken fresh, released
        // before return.
        let queue = unsafe { &*self.queue.get() };
        queue.len()
    }
}

impl Default for InlineRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// ADR-0114 decision #3: the receive membrane every `export!`
/// `receive_p32` shim routes inbound mail through. The shim passes its
/// component's own `registry` (the emitted `static __AETHER_INLINE`).
/// When the routed recipient is the parent's own mailbox id, dispatch the
/// parent (`dispatch_own`); otherwise take the inline child the producer
/// addressed out of `registry`, dispatch it with a ctx self-identified as
/// the child and carrying the same `registry` ([`WasmCtx::__new`]), and
/// reinsert. An unrecognised recipient falls back to the parent's dispatch
/// — the existing unmatched path (the parent's `#[fallback]`, or the
/// `DISPATCH_UNKNOWN_KIND` sentinel for a strict receiver), never a
/// short-circuit drop.
///
/// `source` is the inbound source threaded onto the dispatched child's ctx
/// (issues 1987 + 2001): the enqueuing member's id when called off the drain,
/// the host-resolved inbound source for a top-level dispatch (the `receive_p32`
/// membrane threads the same value it received over the ABI), or
/// [`MailboxId::NONE`] (`0`) when there is no peer-component origin. The child's
/// `ctx.source_mailbox()` is a single read of this field. The own-id path's ctx
/// is built by `dispatch_own`, which the caller has already bound to the same
/// `source`.
///
/// For a normal (non-inline) actor the routed recipient equals the
/// parent's own id, so the membrane no-ops straight to `dispatch_own` —
/// the regression guard the whole demux rests on.
pub fn membrane_dispatch<F>(
    own_mailbox_id: u64,
    mail: Mail<'_>,
    registry: &InlineRegistry,
    source: u64,
    dispatch_own: F,
) -> u32
where
    F: FnOnce(Mail<'_>) -> u32,
{
    let recipient = mail.recipient().0;
    if recipient == own_mailbox_id {
        return dispatch_own(mail);
    }
    let id = MailboxId(recipient);
    match registry.take(id) {
        Some(mut child) => {
            let mut ctx = WasmCtx::__new(recipient, registry, source);
            let rc = child.erased_dispatch(&mut ctx, mail);
            registry.reinsert(id, child);
            rc
        }
        // An alias whose child isn't resident (a race against teardown, or
        // a stray address) runs the parent's unmatched path rather than
        // dropping the mail silently.
        None => dispatch_own(mail),
    }
}

/// Drain the cluster-local queue (ADR-0114 addressing amendment): dispatch
/// every buffered intra-cluster send in place through the membrane until the
/// queue empties, so a child → parent → sibling cascade settles inside one
/// `receive_p32` call under one run-token — zero scheduler hops.
///
/// `self_id` is the cluster's real folded id (the membrane's own-recipient
/// discriminator). `dispatch_own` is re-evaluated per item by the caller —
/// the `receive_p32` shim acquires `__AETHER_COMPONENT.get_mut()` fresh
/// inside the closure for each item, so no two `&mut` instance borrows ever
/// overlap (the borrow-aliasing the #1945 bounce proved). `mk_own` is that
/// per-item factory: it is called once per drained item *with that item's
/// sender* (the "from" half) so the own-path ctx the closure builds carries
/// the same inbound source the child path threads through
/// [`membrane_dispatch`]; the resulting closure is handed straight to
/// `membrane_dispatch` and dropped before the next iteration.
///
/// Issue 1987: each drained member's identity (its own id, `item.recipient`)
/// rides on the sends it makes (the ctx threads it as the `from` half through
/// `route_or_enqueue`), and each item's inbound source (`item.sender`) rides
/// on the dispatched ctx — no ambient host re-stamp, no registry cell.
///
/// Reentrancy and cycles are handled by the queue, not by nested dispatch:
/// a drained item's handler that sends to a busy cluster member just pushes
/// a later queue item, which this same loop picks up.
pub fn drain_cluster_queue<MakeOwn, Own>(registry: &InlineRegistry, mut mk_own: MakeOwn)
where
    MakeOwn: FnMut(u64) -> Own,
    Own: FnOnce(Mail<'_>) -> u32,
{
    let self_id = registry.self_id();
    while let Some(item) = registry.pop_queued() {
        // Keep the owned bytes alive for the duration of this item's
        // dispatch; the `Mail` borrows them by raw pointer + length.
        let bytes = item.bytes;
        // SAFETY: `bytes` lives for the rest of this loop iteration, longer
        // than the `Mail` built from its pointer; `Mail::__from_ptr` bounds
        // the slice to `bytes.len()`. A queued intra-cluster send carries no
        // reply handle (the local fast path is fire-and-forget), so
        // `NO_REPLY_HANDLE` is the correct sender.
        let mail = unsafe {
            Mail::__from_ptr(
                item.kind,
                bytes.as_ptr() as usize,
                bytes.len().try_into().unwrap_or(u32::MAX),
                item.count,
                NO_REPLY_HANDLE,
                item.recipient,
            )
        };
        // The dispatched member's inbound source is this item's "from" half
        // (`item.sender`): the child path threads it onto the membrane-built
        // ctx, and `mk_own(item.sender)` threads it onto the own-path ctx, so
        // both read the same source via `ctx.source_mailbox()`. The member's
        // *own* sends carry the member's id (`item.recipient`, the ctx's
        // identity) as their `from` through `route_or_enqueue` — no host
        // re-stamp.
        let dispatch_own = mk_own(item.sender);
        membrane_dispatch(self_id, mail, registry, item.sender, dispatch_own);
    }
}

#[cfg(test)]
mod tests {
    use super::{InlineRegistry, RouteDecision, drain_cluster_queue, membrane_dispatch};
    use crate::WasmCtx;
    use crate::actor::ctx::OutboundReply;
    use crate::mail::{Mail, PriorState};
    use crate::wasm::ErasedWasmActor;
    use aether_data::MailboxId;
    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

    /// Shared cell a [`RecordingChild`] writes the `source_mailbox()` it
    /// observed into, read back by the source-attribution tests.
    type SourceCell = Rc<Cell<Option<MailboxId>>>;

    /// Distinct return codes so an assertion can tell which dispatch path
    /// the membrane took.
    const OWN_CODE: u32 = 0xA0;
    const CHILD_CODE: u32 = 0xC0;

    /// Minimal `ErasedWasmActor` for the membrane tests: bumps a
    /// test-local dispatch counter (shared via [`Rc`] so the test reads it
    /// back without a process-global), records the `ctx.source_mailbox()`
    /// it observed on the most recent dispatch (the in-place "from" half),
    /// and returns [`CHILD_CODE`]. The lifecycle hooks are unreachable in
    /// these tests.
    struct RecordingChild {
        dispatches: Rc<Cell<u32>>,
        /// The `source_mailbox()` the child read on its last dispatch,
        /// shared with the test so it can assert the in-place sender. `None`
        /// until the first dispatch and whenever the source resolves to
        /// none.
        observed_source: SourceCell,
    }

    impl RecordingChild {
        /// A recording child plus the shared dispatch counter a test reads to
        /// confirm how many times the membrane dispatched it.
        fn new() -> (Self, Rc<Cell<u32>>) {
            let (child, dispatches, _source) = Self::new_with_source();
            (child, dispatches)
        }

        /// A recording child plus both its shared dispatch counter and its
        /// shared observed-source cell, for the tests that assert the
        /// in-place sender a drained dispatch reads.
        fn new_with_source() -> (Self, Rc<Cell<u32>>, SourceCell) {
            let dispatches = Rc::new(Cell::new(0));
            let observed_source = Rc::new(Cell::new(None));
            (
                Self {
                    dispatches: Rc::clone(&dispatches),
                    observed_source: Rc::clone(&observed_source),
                },
                dispatches,
                observed_source,
            )
        }
    }

    impl ErasedWasmActor for RecordingChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.recording_child"
        }
        fn erased_dispatch(
            &mut self,
            ctx: &mut WasmCtx<'_, crate::Manual>,
            _mail: Mail<'_>,
        ) -> u32 {
            self.dispatches.set(self.dispatches.get() + 1);
            self.observed_source.set(ctx.source_mailbox());
            CHILD_CODE
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, crate::Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, crate::Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut crate::WasmDropCtx<'_>) {}
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Manual>,
            _prior: PriorState<'_>,
        ) {
        }
    }

    /// A child that despawns *itself* during its own dispatch — through the
    /// `ctx`, whose inline registry the membrane threaded in — and bumps a
    /// test-local drop counter (shared via [`Rc`]) when dropped, so the
    /// reentrancy test can prove the box dropped (rather than being
    /// reinserted) after it removed its own slot mid-dispatch. Carries its
    /// own alias id so `erased_dispatch` can despawn the matching slot.
    struct SelfDespawningChild {
        id: MailboxId,
        drops: Rc<Cell<u32>>,
    }

    impl Drop for SelfDespawningChild {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    impl ErasedWasmActor for SelfDespawningChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.self_despawning_child"
        }
        fn erased_dispatch(
            &mut self,
            ctx: &mut WasmCtx<'_, crate::Manual>,
            _mail: Mail<'_>,
        ) -> u32 {
            // Self-despawn mid-dispatch through the threaded registry: this
            // box is currently taken out (held on the membrane's stack), so
            // the ctx's despawn clears the empty slot and the membrane's
            // `reinsert` will find nothing.
            ctx.despawn_inline_child(self.id);
            CHILD_CODE
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, crate::Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, crate::Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut crate::WasmDropCtx<'_>) {}
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Manual>,
            _prior: PriorState<'_>,
        ) {
        }
    }

    /// Build a host-side `Mail` with the given routed recipient; the
    /// payload pointer is never dereferenced by these tests (the
    /// recording child doesn't decode), so a dangling-but-unread `ptr`
    /// with `byte_len = 0` is fine.
    fn mail_to(recipient: u64) -> Mail<'static> {
        // SAFETY: `byte_len = 0` so no bytes at `ptr` are ever read; the
        // membrane and `RecordingChild` only inspect `recipient`.
        unsafe { Mail::__from_ptr(0, 1, 0, 1, crate::NO_REPLY_HANDLE, recipient) }
    }

    /// Step 3 coverage: the slot-shaped registry round-trips a child
    /// through insert → take → reinsert → take.
    #[test]
    fn registry_insert_take_reinsert_round_trips() {
        let registry = InlineRegistry::new();
        let id = MailboxId(0x1111);

        assert!(registry.take(id).is_none(), "empty registry has no child");
        registry.insert_child(
            id,
            0,
            String::from("widget"),
            false,
            0,
            Box::new(RecordingChild::new().0),
        );
        let taken = registry
            .take(id)
            .expect("insert then take returns the child");
        assert!(
            registry.take(id).is_none(),
            "a taken-out slot is empty until reinsert",
        );
        registry.reinsert(id, taken);
        assert!(
            registry.take(id).is_some(),
            "reinsert refills the slot for the next dispatch",
        );
    }

    /// Step 1 coverage: a spawned child's slot carries its actor-type tag
    /// and resolved subname, surfaced through `child_metas` for the
    /// dehydrate walk.
    #[test]
    fn child_metas_carry_type_tag_and_subname() {
        let registry = InlineRegistry::new();
        let id = MailboxId(0x7777);
        let tag = 0xABCD_u64;
        registry.insert_child(
            id,
            tag,
            String::from("widget"),
            false,
            0,
            Box::new(RecordingChild::new().0),
        );

        let metas = registry.child_metas();
        let meta = match metas.as_slice() {
            [one] => one,
            other => panic!("expected exactly one child meta, got {}", other.len()),
        };
        assert_eq!(meta.id, id, "the meta carries the alias id");
        assert_eq!(meta.type_tag, tag, "the meta carries the actor-type tag");
        assert_eq!(meta.full_subname, "widget", "the meta carries the subname");
        assert!(!meta.is_counter, "a Named subname is not a counter");
    }

    /// Step 4 coverage: recipient == own id dispatches the parent, never
    /// the child registry.
    #[test]
    fn membrane_routes_own_recipient_to_parent() {
        let registry = InlineRegistry::new();
        let own = 0x2000_u64;
        let rc = membrane_dispatch(own, mail_to(own), &registry, MailboxId::NONE.0, |_mail| {
            OWN_CODE
        });
        assert_eq!(rc, OWN_CODE, "own-id recipient runs the parent dispatch");
    }

    /// Step 4 coverage: a child-addressed recipient dispatches the child
    /// and reinserts it, so a second send to the same alias dispatches
    /// again (the take/reinsert round-trip under the membrane).
    #[test]
    fn membrane_routes_child_recipient_and_reinserts() {
        let registry = InlineRegistry::new();
        let own = 0x3000_u64;
        let child = 0x3001_u64;
        let (recording, dispatches) = RecordingChild::new();
        registry.insert_child(
            MailboxId(child),
            0,
            String::from("widget"),
            false,
            0,
            Box::new(recording),
        );

        let rc = membrane_dispatch(own, mail_to(child), &registry, MailboxId::NONE.0, |_mail| {
            panic!("own dispatch must not run for a child recipient")
        });
        assert_eq!(rc, CHILD_CODE, "child recipient runs the child dispatch");

        // Reinserted: a second send to the same alias dispatches again.
        let rc2 = membrane_dispatch(own, mail_to(child), &registry, MailboxId::NONE.0, |_mail| {
            panic!("own dispatch must not run for a reinserted child")
        });
        assert_eq!(rc2, CHILD_CODE, "the child was reinserted after dispatch");
        assert_eq!(dispatches.get(), 2, "both sends reached the child");
    }

    /// Step 4 coverage: an unrecognised recipient (no resident child) runs
    /// the parent's unmatched path rather than short-circuit dropping.
    #[test]
    fn membrane_routes_unknown_recipient_to_parent_unmatched_path() {
        let registry = InlineRegistry::new();
        let own = 0x4000_u64;
        let stray = 0x4999_u64;
        let rc = membrane_dispatch(own, mail_to(stray), &registry, MailboxId::NONE.0, |_mail| {
            OWN_CODE
        });
        assert_eq!(
            rc, OWN_CODE,
            "an unknown recipient falls back to the parent's unmatched path",
        );
    }

    /// Step 3 coverage: a child that despawns itself mid-dispatch drops
    /// correctly. `membrane_dispatch` takes it out, the dispatch removes the
    /// now-empty slot via `ctx.despawn_inline_child` (driving the same
    /// registry the membrane threaded in), the membrane's `reinsert` finds
    /// nothing and no-ops, and the live box drops at end of scope — proving
    /// the slot-shaped take/reinsert handles the reentrant drop with no
    /// pending-removal flag. A subsequent send to the same alias then falls
    /// through to the parent's unmatched path.
    #[test]
    fn membrane_self_despawn_drops_box_and_falls_through() {
        let registry = InlineRegistry::new();
        let own = 0x5000_u64;
        let child = 0x5001_u64;
        let drops = Rc::new(Cell::new(0));
        registry.insert_child(
            MailboxId(child),
            0,
            String::from("widget"),
            false,
            0,
            Box::new(SelfDespawningChild {
                id: MailboxId(child),
                drops: Rc::clone(&drops),
            }),
        );

        // Dispatch the child; it despawns its own slot mid-dispatch.
        let rc = membrane_dispatch(own, mail_to(child), &registry, MailboxId::NONE.0, |_mail| {
            panic!("own dispatch must not run while the child is resident")
        });
        assert_eq!(rc, CHILD_CODE, "the child handled the despawning dispatch");
        assert_eq!(
            drops.get(),
            1,
            "the self-despawned box dropped at end of dispatch, not reinserted",
        );

        // The alias is gone: a second send falls through to the parent's
        // unmatched path rather than re-dispatching a dropped child.
        let rc2 = membrane_dispatch(own, mail_to(child), &registry, MailboxId::NONE.0, |_mail| {
            OWN_CODE
        });
        assert_eq!(
            rc2, OWN_CODE,
            "the torn-down alias falls through to the parent",
        );
    }

    /// Install a recording child under `id` with `parent`, returning the
    /// shared dispatch counter. Shared helper for the addressing /
    /// route / drain tests below.
    fn install_recording(registry: &InlineRegistry, id: u64, parent: u64) -> Rc<Cell<u32>> {
        let (recording, dispatches) = RecordingChild::new();
        registry.insert_child(
            MailboxId(id),
            0,
            String::from("recording"),
            false,
            parent,
            Box::new(recording),
        );
        dispatches
    }

    /// Addressing amendment: parent / child / sibling resolve by registry
    /// lookup over the recorded logical tree, and a missing relative is a
    /// clean `None` — never a fold.
    #[test]
    fn relative_resolution_walks_recorded_parent_links() {
        let registry = InlineRegistry::new();
        let root = 0x1000_u64;
        registry.set_self_id(root);

        // Two children of the root with distinct subnames, plus a grandchild
        // of the first child.
        let bar = MailboxId(0x1001);
        let baz = MailboxId(0x1002);
        let button = MailboxId(0x1003);
        registry.insert_child(
            bar,
            0,
            String::from("bar"),
            false,
            root,
            Box::new(RecordingChild::new().0),
        );
        registry.insert_child(
            baz,
            0,
            String::from("baz"),
            false,
            root,
            Box::new(RecordingChild::new().0),
        );
        registry.insert_child(
            button,
            0,
            String::from("button"),
            false,
            bar.0,
            Box::new(RecordingChild::new().0),
        );

        // parent_of: bar's parent is the root; the root itself has no slot,
        // so parent_of(root) is None (its parent is cross-cluster).
        assert_eq!(registry.parent_of(bar), Some(MailboxId(root)));
        assert_eq!(registry.parent_of(button), Some(bar));
        assert_eq!(
            registry.parent_of(MailboxId(root)),
            None,
            "the cluster root has no registry parent (its parent is cross-cluster)",
        );
        assert_eq!(
            registry.parent_of(MailboxId(0xDEAD)),
            None,
            "a stray id resolves to no parent",
        );

        // child_of: the root's child named "bar"/"baz"; bar's child "button".
        assert_eq!(registry.child_of(MailboxId(root), "bar"), Some(bar));
        assert_eq!(registry.child_of(MailboxId(root), "baz"), Some(baz));
        assert_eq!(registry.child_of(bar, "button"), Some(button));
        assert_eq!(
            registry.child_of(MailboxId(root), "missing"),
            None,
            "no child named 'missing' resides",
        );
        assert_eq!(
            registry.child_of(MailboxId(root), "button"),
            None,
            "'button' is bar's child, not the root's — scoping is by parent",
        );

        // sibling_of: bar and baz are siblings under the root.
        assert_eq!(registry.sibling_of(bar, "baz"), Some(baz));
        assert_eq!(registry.sibling_of(baz, "bar"), Some(bar));
        assert_eq!(
            registry.sibling_of(button, "bar"),
            None,
            "button's parent (bar) has no child named 'bar'",
        );
    }

    /// Addressing amendment: `route_decision` classifies the cluster's own
    /// id and any resident inline-child alias as `Local` (in-place dispatch)
    /// and any other recipient as `Remote` (host hand-off) — the pure
    /// membership decision behind `route_or_enqueue`, testable without a
    /// live `MAIL_BRIDGE`.
    #[test]
    fn route_decision_classifies_cluster_membership() {
        let registry = InlineRegistry::new();
        let root = 0x2000_u64;
        let child = 0x2001_u64;
        registry.set_self_id(root);
        install_recording(&registry, child, root);

        assert_eq!(
            registry.route_decision(root),
            RouteDecision::Local,
            "the cluster's own id is local",
        );
        assert_eq!(
            registry.route_decision(child),
            RouteDecision::Local,
            "a resident inline-child alias is local",
        );
        assert_eq!(
            registry.route_decision(0x9999),
            RouteDecision::Remote,
            "a non-member recipient is remote (host hand-off)",
        );
    }

    /// Addressing amendment: `route_or_enqueue` to a cluster member pushes to
    /// the local queue and makes no host call (the host stub would panic).
    /// The queue grows by one per local send.
    #[test]
    fn route_or_enqueue_buffers_local_sends() {
        let registry = InlineRegistry::new();
        let root = 0x3000_u64;
        let child = 0x3001_u64;
        registry.set_self_id(root);
        install_recording(&registry, child, root);

        assert_eq!(registry.queued_len(), 0, "the queue starts empty");
        registry.route_or_enqueue(root, 7, &[1, 2, 3], 1, false, root);
        assert_eq!(
            registry.queued_len(),
            1,
            "an own-id send enqueues locally, no host call",
        );
        registry.route_or_enqueue(child, 8, &[4], 1, false, root);
        assert_eq!(
            registry.queued_len(),
            2,
            "a child-alias send enqueues locally too",
        );
    }

    /// Addressing amendment: a seeded local item drains through the membrane
    /// (dispatched once) and the queue empties in one `drain_cluster_queue`
    /// call.
    #[test]
    fn drain_dispatches_a_seeded_local_item() {
        let registry = InlineRegistry::new();
        let root = 0x4000_u64;
        let child = 0x4001_u64;
        registry.set_self_id(root);
        let dispatches = install_recording(&registry, child, root);

        // Seed one local send addressed to the child.
        registry.route_or_enqueue(child, 1, &[0xAB], 1, false, root);

        let own_dispatches = Rc::new(Cell::new(0));
        let own_counter = Rc::clone(&own_dispatches);
        drain_cluster_queue(&registry, |_source| {
            let own_counter = Rc::clone(&own_counter);
            move |_mail| {
                own_counter.set(own_counter.get() + 1);
                OWN_CODE
            }
        });

        assert_eq!(
            dispatches.get(),
            1,
            "the child-addressed item dispatched the child once",
        );
        assert_eq!(
            own_dispatches.get(),
            0,
            "a child-addressed item never ran the parent dispatch",
        );
        assert_eq!(
            registry.queued_len(),
            0,
            "the queue is empty after the drain",
        );
    }

    /// Addressing amendment: a cascade — a drained item whose dispatch
    /// enqueues another local item — drains fully in one
    /// `drain_cluster_queue` call (the queue, not nested dispatch, carries
    /// the cascade). Here the parent dispatch enqueues a follow-up to the
    /// child the first time it runs.
    #[test]
    fn drain_runs_a_cascade_in_one_call() {
        let registry = InlineRegistry::new();
        let root = 0x5000_u64;
        let child = 0x5001_u64;
        registry.set_self_id(root);
        let child_dispatches = install_recording(&registry, child, root);

        // Seed one own-addressed item; the parent dispatch, on its first
        // run, enqueues a follow-up addressed to the child.
        registry.route_or_enqueue(root, 1, &[0x01], 1, false, root);

        // Record the order dispatches happened in, to prove a single drain
        // loop carried both the seed and the cascaded follow-up.
        let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let own_ran = Rc::new(Cell::new(false));

        let order_for_own = Rc::clone(&order);
        let own_ran_inner = Rc::clone(&own_ran);
        let registry_ref = &registry;
        drain_cluster_queue(&registry, |_source| {
            let order_for_own = Rc::clone(&order_for_own);
            let own_ran_inner = Rc::clone(&own_ran_inner);
            move |mail| {
                // Only the own-addressed seed lands in `dispatch_own`; a
                // child-addressed item is demuxed to the child by the
                // membrane before this closure runs.
                if mail.recipient().0 == root {
                    order_for_own.borrow_mut().push("own");
                    if !own_ran_inner.get() {
                        own_ran_inner.set(true);
                        // Cascade: enqueue a follow-up to the child mid-drain.
                        registry_ref.route_or_enqueue(child, 2, &[0x02], 1, false, root);
                    }
                }
                OWN_CODE
            }
        });

        assert!(own_ran.get(), "the seeded own item dispatched");
        assert_eq!(
            child_dispatches.get(),
            1,
            "the cascaded follow-up reached the child in the same drain call",
        );
        assert_eq!(
            order.borrow().as_slice(),
            ["own"],
            "exactly one own dispatch ran (the seed); the cascade went to the child",
        );
        assert_eq!(
            registry.queued_len(),
            0,
            "the cascade drained fully — queue empty",
        );
    }

    /// Install a recording child under `id` with `parent`, returning both the
    /// shared dispatch counter and the shared observed-source cell, so a test
    /// can assert the in-place "from" half a drained dispatch reads.
    fn install_recording_with_source(
        registry: &InlineRegistry,
        id: u64,
        parent: u64,
    ) -> (Rc<Cell<u32>>, SourceCell) {
        let (recording, dispatches, source) = RecordingChild::new_with_source();
        registry.insert_child(
            MailboxId(id),
            0,
            String::from("recording"),
            false,
            parent,
            Box::new(recording),
        );
        (dispatches, source)
    }

    /// Task 1: a child dispatched off the drain reads
    /// `ctx.source_mailbox()` == the enqueuing sender — the child → parent
    /// direction. The parent (the cluster root) enqueues a send to the child
    /// stamped with the parent's own id; the drained child observes exactly
    /// that id, not `None`.
    #[test]
    fn drained_child_reads_enqueuing_sender_parent() {
        let registry = InlineRegistry::new();
        let root = 0x6000_u64;
        let child = 0x6001_u64;
        registry.set_self_id(root);
        let (dispatches, observed) = install_recording_with_source(&registry, child, root);

        // The parent (root) sends to the child, stamping its own id as sender.
        registry.route_or_enqueue(child, 1, &[0x00], 1, false, root);

        drain_cluster_queue(&registry, |_source| {
            |_mail| panic!("own dispatch must not run for a child-addressed item")
        });

        assert_eq!(dispatches.get(), 1, "the child was dispatched once");
        assert_eq!(
            observed.get(),
            Some(MailboxId(root)),
            "the drained child reads the enqueuing parent's id as its source, not None",
        );
    }

    /// A `membrane_dispatch` called with a `NONE` source threads it verbatim,
    /// so the dispatched child reads `source_mailbox() == None`. (In
    /// production the `receive_p32` shim threads the host-resolved inbound
    /// source instead of `NONE`; this exercises the function's `NONE`
    /// contract directly — there is no host reply-table fallback.)
    #[test]
    fn membrane_dispatch_with_none_source_reads_no_source() {
        let registry = InlineRegistry::new();
        let own = 0x8000_u64;
        let child = 0x8001_u64;
        registry.set_self_id(own);
        let (dispatches, observed) = install_recording_with_source(&registry, child, own);

        // Dispatch the child directly — not through the drain — with a `NONE`
        // source, so the ctx carries no in-place sender.
        let rc = membrane_dispatch(own, mail_to(child), &registry, MailboxId::NONE.0, |_mail| {
            panic!("own dispatch must not run for a child recipient")
        });
        assert_eq!(rc, CHILD_CODE, "the child handled the direct dispatch");
        assert_eq!(dispatches.get(), 1, "the child was dispatched once");
        assert_eq!(
            observed.get(),
            None,
            "a top-level dispatch reads no in-place source (NONE source on the ctx)",
        );
    }
}
