//! Inline-child registry + receive membrane (ADR-0114 decisions #2/#3).
//!
//! An inline child shares its parent's WASM instance, slot, and
//! run-token (ADR-0114). [`WasmCtx::spawn_inline_child`] inserts
//! the constructed child into the per-component [`Registry`] the
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
//! `erased_dispatch`. A slot's life is one enum, `Seat`: its child is
//! seated, wired or not, or out on the stack of the caller running it. The guest is single-threaded (ADR-0010 §5) and the
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
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell, UnsafeCell};
use core::mem;

use aether_data::wire::{Encoder, LedgerEncoder};
use aether_data::{__watch_id_number, Blob, Kind, KindId, MailboxId, RequestId, Source, WatchId};

use crate::blob::guest::EncodedGuestMail;
use crate::mail::{Mail, NO_REPLY_HANDLE};
use crate::request_context::{RequestContextTable, compose_state_envelope};
use crate::wasm::bridge::mail;
use crate::wasm::ctx::{ActorTypeTag, SpawnError, WasmCtx};
use crate::wasm::decode::guest_ctx;
use crate::wasm::{ActorInitError, ErasedWasmActor};

mod bundle;
pub mod compose;
mod slot;
mod tickets;
mod unwire;
mod wire;

pub(crate) use slot::{Child, Reinserted};
pub(crate) use unwire::unwire_child;
pub use unwire::unwire_children;
pub use wire::wire_rebuilt_children;
pub(crate) use wire::wire_seated;

use slot::Seat;
use tickets::{ClaimLedger, ContextLedger, DehydrateLedger, HeldTickets};

/// One inline child's slot. `seat` says where the child is: seated at rest,
/// wired or not, or out on the stack of the one caller running its handler
/// or hook. The child's alias [`MailboxId`] is carried as the map key in
/// [`Registry`]; there is no redundant `id` field here.
///
/// ADR-0114 §5: the slot also records the metadata a `replace_component`
/// swap needs to reconstruct the child in the fresh instance — the
/// actor-type tag (`ActorTypeTag::of::<A>()`, the same tag
/// `init_typed_p32` matches a reconstruct on) plus the resolved
/// `full_subname` / `is_counter` the alias id was folded from, so the
/// rehydrate path re-folds the identical alias and re-`init`s the child
/// by type.
struct InlineSlot {
    /// `ActorTypeTag::of::<A>()` — the actor-type tag the
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
    /// ([`Registry::parent_of`] / [`Registry::child_of`] /
    /// [`Registry::sibling_of`]) walk — resolution is pure registry
    /// lookup, never a fold (a `MailboxId` is a one-way hash chain, so the
    /// guest cannot reproduce a relative's id by folding; it looks the
    /// recorded id up instead).
    parent: u64,
    /// The child's encoded `Config` bytes (`A::Config::encode_into_bytes`
    /// at spawn time). Retained so a `replace_component` swap can decode
    /// the real config on reconstruct (`reconstruct_one_child`) instead of
    /// re-`init`ing a typed-config child from empty bytes (issue 2690).
    config_bytes: Vec<u8>,
    /// Where the child is, and whether it owes an `unwire`.
    seat: Seat,
}

impl InlineSlot {
    fn new(record: ChildRecord, seat: Seat) -> Self {
        let ChildRecord { type_tag, full_subname, is_counter, parent, config_bytes } = record;
        Self { type_tag, full_subname, is_counter, parent, config_bytes, seat }
    }

    /// Whether this slot is the child of `parent`, of type `type_tag`, named
    /// `subname`: the three things its alias was folded from.
    fn stands_at(&self, parent: MailboxId, type_tag: ActorTypeTag, subname: &str) -> bool {
        self.parent == parent.0 && self.type_tag == type_tag.0 && self.full_subname == subname
    }
}

/// An inline child's reconstruct record (ADR-0114 §5): what a slot keeps
/// beside the actor so a `replace_component` swap can rebuild the child.
#[derive(Default)]
pub(crate) struct ChildRecord {
    pub(crate) type_tag: u64,
    pub(crate) full_subname: String,
    pub(crate) is_counter: bool,
    /// Raw id of the logical parent; `0` records none (the init ABI's encoding).
    pub(crate) parent: u64,
    pub(crate) config_bytes: Vec<u8>,
}

/// A cloneable snapshot of one resident inline child's reconstruct
/// metadata (no actor box), produced by [`Registry::child_metas`]
/// for the dehydrate walk. The compose path reads each child's state
/// through [`Registry::with_child_mut`] keyed by `id`.
#[derive(Clone)]
pub(crate) struct InlineChildMeta {
    /// The child's alias [`MailboxId`] (the registry key).
    pub(crate) id: MailboxId,
    /// The actor-type tag — `ActorTypeTag::of::<A>()`.
    pub(crate) type_tag: u64,
    /// The resolved subname the alias id was folded from.
    pub(crate) full_subname: String,
    /// Whether the original spawn used a counter discriminator.
    pub(crate) is_counter: bool,
    /// The real folded [`MailboxId`] of the child's logical parent,
    /// captured from the resident slot so replacement reconstruction can
    /// restore the same relative-addressing tree.
    pub(crate) parent: MailboxId,
    /// The child's encoded `Config` bytes, carried into the dehydrate
    /// bundle's `ChildEntry` so reconstruct can re-init from the real
    /// config instead of empty bytes (issue 2690).
    pub(crate) config_bytes: Vec<u8>,
}

/// A held reply's registration, staged by `hold` until the `receive` shim
/// flushes it to the host (ADR-0243 §6): the reply handle it answers, the
/// reply kind, and the encoded `unanswered` value. The encoding keeps the
/// held values its bytes name until the flush has sent them.
pub(crate) struct StagedUnanswered {
    pub(crate) ticket: u32,
    pub(crate) reply: KindId,
    pub(crate) mail: EncodedGuestMail,
}

/// One intra-cluster send buffered on the per-component queue
/// ([`Registry`]). A send whose recipient is a member of this cluster
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
    /// The held values the `bytes` name by hash (ADR-0238 decision 3), kept
    /// until this mail has been dispatched, so the recipient's decode is
    /// admitted by a live hold even when the sender dropped its own value
    /// before the drain reached this mail. Intra-cluster mail never reaches
    /// the host, so nothing else keeps them.
    keep: Vec<Blob>,
    count: u32,
    /// The sending actor's own folded [`MailboxId`] raw value — the "from"
    /// half of an in-place send. An in-place dispatch carries `NO_REPLY_HANDLE`
    /// (the local fast path is fire-and-forget), so the host reply table holds
    /// no immediate-sender for it; [`drain_cluster_queue`] instead threads this
    /// value onto the recipient's [`WasmCtx`] as its inbound source (issue
    /// 1987), so the recipient's `ctx.sender()` resolves it. `0` (the
    /// no-source encoding) when the sender is unknown.
    sender: u64,
}

/// Whether a resolved recipient is in this cluster (dispatch in place
/// through the queue) or outside it (hand to the host). The membership
/// decision is factored out of [`Registry::route_or_enqueue`] as this
/// pure value so it is unit-testable without a live `MAIL_BRIDGE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteDecision {
    /// The recipient is a cluster member; enqueue for in-place dispatch.
    Local,
    /// The recipient is outside the cluster; send through the host.
    Remote,
}

/// Whether a send inherits the handler's in-flight causal chain or starts
/// a fresh one (ADR-0080 §7). Passed to
/// [`Registry::route_or_enqueue`]; on the remote path it controls
/// whether the host stamps the dispatch's `parent`/`root` onto the outbound
/// send (`Inherit`) or mints a new root chain (`Detached`). The local path
/// carries no host trace ids, so the mode is irrelevant there.
#[derive(Clone, Copy)]
pub(crate) enum ChainMode {
    /// Inherit the handler's in-flight causal chain (the default `send`
    /// path, ADR-0080 §7).
    Inherit,
    /// Start a fresh chain root (the `send_detached` escape hatch).
    Detached,
}

/// Hand one send to the host, threading `sender` as its `from` (issue 1987)
/// and keeping the held values its bytes name alive for the whole host call,
/// whose resolve on send attaches their entries (ADR-0238 decision 3). The
/// remote arm of [`Registry::route_or_enqueue`], and the route every tracked
/// send takes even to a cluster member, so the host mints the correlation its
/// reply comes back on (ADR-0139).
pub(crate) fn send_through_host(
    recipient: u64,
    kind: u64,
    payload: EncodedGuestMail,
    count: u32,
    mode: ChainMode,
    sender: u64,
) {
    let EncodedGuestMail { bytes, keep } = payload;
    mail::send_mail(recipient, kind, &bytes, count, matches!(mode, ChainMode::Detached), sender);
    drop(keep);
}

/// The `export!`-installed resolver a
/// [`WasmCtx::spawn_inline_child_by_tag`] call routes through (issue 2692).
/// A plain `fn` pointer, not a boxed closure: the resolver is a
/// non-capturing tag-match the macro emits over the module's exported type
/// set — the same set [`crate::export!`]'s `@reconstruct_child` arm walks —
/// so it coerces cleanly and stores in a `Cell`. Given the module's
/// registry, the spawning actor's real folded id (`parent`), a runtime
/// [`ActorTypeTag`], the resolved `(is_counter, subname)` pair, and the
/// child's config bytes, its matched branch allocates the alias and runs
/// the shared decode + init core on the selected type; a tag matching none
/// returns [`SpawnError::UnknownActorTag`].
///
/// `parent` is threaded from the caller because a by-tag spawn can be
/// *nested* — an inline child spawning its own child by tag — and the new
/// child's recorded parent must be that spawning actor, not the cluster root,
/// so relative addressing (`ctx.child` / `ctx.parent`) resolves it. The typed
/// [`WasmCtx::spawn_inline_child`] path passes the same `self.mailbox`.
pub type SpawnByTagFn = fn(&Registry, u64, ActorTypeTag, bool, &str, &[u8]) -> Result<MailboxId, SpawnError>;

/// What [`Registry::take_watch_context`] found under a watch's id.
pub(crate) enum TakenWatchContext<C> {
    /// The context, stored as the kind asked for.
    Stored(C),
    /// Nothing was stored, or the stored bytes did not decode.
    Missing,
    /// A context stored as this other kind, now discarded.
    OtherKind(KindId),
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
#[derive(Default)]
pub struct Registry {
    inner: UnsafeCell<BTreeMap<MailboxId, InlineSlot>>,
    /// The instance's real folded [`MailboxId`] (`Tag::Mailbox`-tagged),
    /// set once from the `init` / `wire` shim's `mailbox_id` argument — the
    /// id the substrate registered for this trampoline (`store.data()
    /// .sender.0`). `0` until set; the receive shim falls back to
    /// `hash(NAMESPACE)` only while it is still `0` (a receive before
    /// `wire`, which should not happen). The instance's runtime identity at
    /// any depth, not the ADR-0099 depth-1 fixed point.
    self_id: Cell<u64>,
    /// The logical actor type actually constructed in the module's entry
    /// slot. Combined with each [`InlineSlot::type_tag`], this lets a ctx
    /// recover the actor identity for its own mailbox at any cluster depth.
    /// `None` until a successful export-generated construction records it.
    entry_actor_tag: Cell<Option<ActorTypeTag>>,
    /// The cluster-local mail queue. A send to a cluster member is pushed
    /// here ([`Self::route_or_enqueue`]) instead of going to the host;
    /// [`drain_cluster_queue`] dispatches each item through the membrane
    /// after the top-level dispatch returns, so a child → parent → sibling
    /// cascade settles in one `receive_p32` call. Reentrancy and cycles are
    /// handled by the queue — a busy target is just a later queue item —
    /// not by nested dispatch.
    queue: UnsafeCell<VecDeque<QueuedMail>>,
    /// SDK-owned request contexts keyed by host reply correlation id
    /// (ADR-0139), and the context of each watch keyed by its id, which is
    /// drawn from the same sequence (ADR-0079 §8). Lives beside the inline registry because every wasm ctx and
    /// mailbox already carries this macro-emitted per-component static. The
    /// table never evicts: it grows past its preallocated room and warns at
    /// each new high-water mark. A `RefCell`, so a reentrant borrow panics
    /// instead of aliasing; every borrow is taken inside one method below and
    /// released before it returns.
    request_contexts: RefCell<RequestContextTable>,
    /// ADR-0243 §6: every held-reply ticket this instance owns and where it
    /// sits, plus the requests whose stored context parked one. The ctx,
    /// the request-context codec and the dehydrate encoder reach it only
    /// through this registry. A `RefCell` under the same single-thread
    /// argument as `request_contexts`; every borrow is released before the
    /// method that took it returns.
    held: RefCell<HeldTickets>,
    /// ADR-0243 §6: the reply the dispatch in progress held, registered with
    /// its `unanswered` value. `hold` stages it here and the `export!`
    /// `receive` shim flushes it to the host once the dispatch returns, so
    /// the host can answer the held slot if this instance closes first. A
    /// `RefCell` under the same single-thread argument as `held`.
    unanswered: RefCell<Option<StagedUnanswered>>,
    /// The `export!`-installed by-tag spawn resolver (issue 2692), or `None`
    /// on a raw registry never wired by `export!` (a host-unit registry).
    /// Set once from each init shim — the resolver enumerates the module's
    /// exported type set, knowable only inside the macro expansion, so it is
    /// installed here rather than reached as an SDK-side generic. Mirrors
    /// `self_id`'s set-once-from-the-shim shape; a `fn` pointer is `Copy`, so
    /// a `Cell` suffices.
    spawn_resolver: Cell<Option<SpawnByTagFn>>,
}

// SAFETY: identical argument to [`crate::Slot`] — the WASM guest is
// single-threaded (ADR-0010 §5) and the substrate serializes delivery
// under the run token, so a `static __AETHER_INLINE` is only ever touched
// from one thread at a time. On the host unit-test build each test owns a
// local registry, reached from one test thread. The same argument covers
// the added interior-mutable fields (`self_id`, `queue`,
// `request_contexts`): each is touched only from the single run-token
// thread, and every borrow of `queue` or `request_contexts` is taken fresh
// and released before return (never spanning a dispatch). The
// `request_contexts` `RefCell` needs `Sync` only for that single-thread
// reason; its borrow flag still catches reentrancy. The `spawn_resolver`
// cell is written once from an init shim and read from guest handler code —
// again, only ever from the single run-token thread.
unsafe impl Sync for Registry {}

impl Registry {
    /// An empty registry. `const` so it can back a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(BTreeMap::new()),
            self_id: Cell::new(0),
            entry_actor_tag: Cell::new(None),
            queue: UnsafeCell::new(VecDeque::new()),
            request_contexts: RefCell::new(RequestContextTable::new()),
            held: RefCell::new(HeldTickets::new()),
            unanswered: RefCell::new(None),
            spawn_resolver: Cell::new(None),
        }
    }

    /// Store a typed request context under `request` (ADR-0139), warning
    /// when the table passes a new high-water mark. Each held ticket in the
    /// context parks as it encodes (ADR-0243 §4), and the request is then
    /// recorded for the untaken-reply check. The warning names no actor: a
    /// guest's `tracing` event lands in its own log ring (ADR-0081 §7), which
    /// already attributes it.
    pub(crate) fn insert_request_context<C: Kind>(&self, request: RequestId, context: C) {
        let high_water = {
            let mut table = self.request_contexts.borrow_mut();
            let mut held = self.held.borrow_mut();
            table.insert_with(request, context, &mut ContextLedger::new(&mut held, request, C::NAME));
            table.high_water()
        };
        if let Some(live) = high_water {
            tracing::warn!(
                live,
                "context table grew past its preallocated room; a reply or a departure notice that never arrives \
                 keeps its context"
            );
        }
    }

    /// Store the context of the watch `watch` (ADR-0079 §8) in the request
    /// context table, under the watch's id, replacing the context a standing
    /// watch already stored. A watch id is drawn from the request-id
    /// sequence, so it never names a request's entry.
    pub(crate) fn store_watch_context<C: Kind>(&self, watch: WatchId, context: C) {
        self.insert_request_context(RequestId(__watch_id_number(watch)), context);
    }

    /// Take the context stored for the watch `watch` as a `C`, for the
    /// handler its departure notice runs. A context stored as another kind is
    /// discarded, since the watch has ended and no other handler takes it.
    pub(crate) fn take_watch_context<C: Kind>(&self, watch: WatchId) -> TakenWatchContext<C> {
        let request = RequestId(__watch_id_number(watch));
        let mut table = self.request_contexts.borrow_mut();
        let mut held = self.held.borrow_mut();
        if let Some(context) = table.take_with::<C>(request, &mut ClaimLedger::new(&mut held)) {
            return TakenWatchContext::Stored(context);
        }
        table.discard(request).map_or(TakenWatchContext::Missing, TakenWatchContext::OtherKind)
    }

    /// Drop the context stored for the watch `watch`, undecoded, because the
    /// watch was released before its target departed.
    pub(crate) fn discard_watch_context(&self, watch: WatchId) {
        self.request_contexts.borrow_mut().discard(RequestId(__watch_id_number(watch)));
    }

    /// Remove and decode the typed request context stored under `request`,
    /// claiming each held ticket in it back to live; a wrong-kind take leaves
    /// it stored (ADR-0139 §4).
    pub(crate) fn take_request_context<C: Kind>(&self, request: RequestId) -> Option<C> {
        let mut held = self.held.borrow_mut();
        let context = self.request_contexts.borrow_mut().take_with(request, &mut ClaimLedger::new(&mut held));
        if context.is_some() {
            held.taken(request);
        }
        context
    }

    /// Record a freshly minted held reply as live (ADR-0243 §6).
    pub(crate) fn arm_held(&self, ticket: u32, reply: KindId) {
        self.held.borrow_mut().arm(ticket, reply);
    }

    /// Forget a held reply that was just answered.
    pub(crate) fn release_held(&self, ticket: u32) {
        self.held.borrow_mut().release(ticket);
    }

    /// Stage the reply `ticket`'s requester receives if this instance
    /// closes before answering it (ADR-0243 §6): `reply` is its kind and
    /// `mail` its encoded `unanswered` value. The `export!` `receive` shim
    /// flushes it through [`Self::__flush_unanswered`] once the dispatch
    /// returns.
    ///
    /// # Panics
    ///
    /// When a registration is already staged: `hold` arms one reply per
    /// dispatch, and the shim flushes after each one.
    pub(crate) fn stage_unanswered(&self, ticket: u32, reply: KindId, mail: EncodedGuestMail) {
        let previous = self.unanswered.borrow_mut().replace(StagedUnanswered { ticket, reply, mail });
        assert!(previous.is_none(), "aether-actor: a held reply's unanswered value was staged twice in one dispatch");
    }

    /// Take the staged registration, if the dispatch held a reply.
    pub(crate) fn take_unanswered(&self) -> Option<StagedUnanswered> {
        self.unanswered.borrow_mut().take()
    }

    /// Register the reply the dispatch just held with the host
    /// (ADR-0243 §6), called by the `export!` `receive` shim after the
    /// top-level dispatch. A dispatch that held nothing registers nothing.
    ///
    /// # Panics
    ///
    /// When the host refuses the registration (ADR-0063): an engine-only
    /// or unregistered kind, or a payload naming a blob this instance does
    /// not hold. The host fails the delivery of a held reply it has no
    /// registration for, so the requester is never left without one. On a
    /// host build, whenever a registration is staged: there is no FFI host.
    #[doc(hidden)]
    pub fn __flush_unanswered(&self) {
        if let Some(StagedUnanswered { ticket, reply, mail: encoded }) = self.take_unanswered() {
            let status = mail::held_unanswered(ticket, reply.0, &encoded.bytes);
            assert!(status == 0, "aether-actor: the host refused a held reply's unanswered value (status {status})");
        }
    }

    /// Frame `value` for `save_state` as `K::ID` then its wire bytes,
    /// parking each held ticket in it as saved (ADR-0243 §6).
    ///
    /// # Errors
    /// When `value` does not encode: a length past the `u32` ceiling, or a
    /// ticket this instance does not hold live. Tickets the encode reached
    /// before it failed stay parked as saved; the next dehydrate's
    /// [`Self::__revert_dehydrate`] returns them to live.
    pub(crate) fn encode_saved_state<K: Kind>(&self, value: &K) -> Result<Vec<u8>, ActorInitError> {
        let mut held = self.held.borrow_mut();
        let mut ledger = DehydrateLedger::new(&mut held);
        let mut enc = LedgerEncoder::new(&mut ledger);
        enc.out().extend_from_slice(&K::ID.0.to_le_bytes());
        value
            .encode_with(&mut enc)
            .map_err(|error| ActorInitError::from(format!("saved state `{}` failed to encode: {error}", K::NAME)))?;

        Ok(enc.into_bytes())
    }

    /// Decode saved-state bytes as `K`, claiming each held ticket in them
    /// back to live in this instance (ADR-0243 §6) and proving each
    /// `ProtocolPath` in them against the engine's published routes
    /// (ADR-0231 §3).
    pub(crate) fn decode_saved_state<K: Kind>(&self, payload: &[u8]) -> Option<K> {
        let mut held = self.held.borrow_mut();
        let mut ledger = ClaimLedger::new(&mut held);
        K::decode_with(payload, &mut guest_ctx().held(&mut ledger))
            .inspect_err(|error| tracing::warn!(kind = K::NAME, %error, "prior state decode refused"))
            .ok()
    }

    /// Whether a held reply is still live after `on_dehydrate`: one the
    /// dehydrate neither saved nor answered. The `export!` `on_dehydrate`
    /// shim then refuses the replace (ADR-0243 §6).
    #[doc(hidden)]
    #[must_use]
    pub fn __held_unsaved(&self) -> bool {
        self.held.borrow().any_live()
    }

    /// Return every ticket a dehydrate saved to live. The `export!`
    /// `on_dehydrate` shim calls it before the hooks run, so a ticket saved
    /// by a replace that was later rolled back is checked again even when
    /// the reinstated instance's `on_rehydrate` did not claim it back. A
    /// refused dehydrate does not call it: its state is still saved, and the
    /// reinstated instance claims the saved tickets back as that state
    /// returns through `on_rehydrate` (issue 7125).
    #[doc(hidden)]
    pub fn __revert_dehydrate(&self) {
        self.held.borrow_mut().revert_saved();
    }

    /// The untaken-reply check (ADR-0243 §7), run by the `export!` `receive`
    /// shim after the top-level dispatch: the reply being dispatched must
    /// have taken a stored context that parked a held ticket. With no such
    /// context stored it returns before reading the host correlation, so an
    /// actor that holds nothing pays no host call.
    ///
    /// # Panics
    ///
    /// When it did not, naming the stored context's kind.
    #[doc(hidden)]
    pub fn __check_held_contexts_taken(&self) {
        if !self.held.borrow().any_untaken() {
            return;
        }
        let correlation = mail::reply_correlation();
        if correlation != Source::NO_CORRELATION {
            self.check_held_context_taken(RequestId(correlation));
        }
    }

    pub(crate) fn check_held_context_taken(&self, request: RequestId) {
        if let Some(context) = self.held.borrow().untaken(request) {
            panic!(
                "aether-actor: the reply to request {} left its context `{context}` untaken, and that context holds \
                 a held reply; take it with `take_context`",
                request.0
            );
        }
    }

    /// Wrap the dehydrating guest's user state with the request-context
    /// snapshot, for the `export!` `on_dehydrate` shims.
    #[doc(hidden)]
    #[must_use]
    pub fn compose_request_context_state(&self, user_state: Option<(u32, Vec<u8>)>) -> Option<(u32, Vec<u8>)> {
        compose_state_envelope(&self.request_contexts.borrow(), user_state)
    }

    /// Replace the per-component request-context table during rehydrate.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn restore_request_contexts(&self, table: RequestContextTable) {
        *self.request_contexts.borrow_mut() = table;
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

    /// Record the logical actor type actually constructed in the module's
    /// entry slot. The export-generated init paths call this only after the
    /// selected actor has initialized successfully.
    pub fn set_entry_actor_tag(&self, tag: ActorTypeTag) {
        self.entry_actor_tag.set(Some(tag));
    }

    /// Resolve the logical actor type at `mailbox`: the entry actor when it
    /// matches [`Self::self_id`], or an inline slot's recorded type tag at
    /// any nested depth. Slot lookup is independent of whether its child is
    /// seated or out for a handler or hook.
    #[must_use]
    pub fn actor_type_tag(&self, mailbox: MailboxId) -> Option<ActorTypeTag> {
        if mailbox.0 == self.self_id.get() {
            return self.entry_actor_tag.get();
        }
        // SAFETY: see [`Self::insert_child`]. This immutable borrow is taken
        // fresh and released before return; it never spans dispatch.
        let map = unsafe { &*self.inner.get() };
        map.get(&mailbox).map(|slot| ActorTypeTag(slot.type_tag))
    }

    /// Install the module's by-tag spawn resolver (issue 2692), emitted by
    /// `export!` over the exported type set and set once from each init shim
    /// (mirroring [`Self::set_self_id`]). Idempotent re-sets write the same
    /// `fn` pointer.
    pub fn set_spawn_resolver(&self, resolver: SpawnByTagFn) {
        self.spawn_resolver.set(Some(resolver));
    }

    /// The installed by-tag spawn resolver, or `None` on a registry never
    /// wired by `export!` (a raw host-unit registry — the seam the
    /// [`WasmCtx::spawn_inline_child_by_tag`] host tests drive with a
    /// synthetic resolver). Backs the verb's resolver lookup.
    #[must_use]
    pub fn spawn_resolver(&self) -> Option<SpawnByTagFn> {
        self.spawn_resolver.get()
    }

    /// Seat a rebuilt inline child under `id` as [`Child::Unwired`]: a
    /// republish's rebuild ran its `init` and `on_rehydrate` and no `wire`
    /// (ADR-0114 §5). A slot already at `id` is replaced, record and child,
    /// which is how the reinstated guest's rebuild after a refused republish
    /// lays a saved child over the resident one. A spawn does not come here:
    /// it calls [`Self::reserve`]. O(log n).
    pub(crate) fn insert_child(&self, id: MailboxId, record: ChildRecord, actor: Box<dyn ErasedWasmActor>) {
        // SAFETY: single-threaded guest + serialized delivery — no other
        // live borrow of the cell (the `Sync` argument). The borrow is
        // released before this returns, so it never spans a dispatch.
        let map = unsafe { &mut *self.inner.get() };
        map.insert(id, InlineSlot::new(record, Seat::Seated(Child::Unwired(actor))));
    }

    /// Make the slot of a child being spawned under `id`. The slot is born
    /// [`Seat::Out`]: the fresh box stays on the spawn's stack until its
    /// `wire` has run, and the slot is there meanwhile for the lookups a
    /// nested spawn makes ([`Self::actor_type_tag`], [`Self::parent_of`]).
    /// The spawn seats the child with [`Self::reinsert`]. O(log n).
    pub(crate) fn reserve(&self, id: MailboxId, record: ChildRecord) {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &mut *self.inner.get() };
        map.insert(id, InlineSlot::new(record, Seat::Out));
    }

    /// Take the seated child out for a handler or hook, leaving its slot
    /// (and its reconstruct record) in place and [`Seat::Out`]. The caller
    /// holds the [`Child`], and with it whether an `unwire` is owed, until it
    /// hands it back through [`Self::reinsert`].
    ///
    /// `None` has one meaning: no child is seated at `id`, because no slot
    /// is there or the caller that took it still holds it. The borrow drops
    /// before the returned child is run, so a child may re-enter the
    /// registry mid-dispatch. O(log n).
    pub(crate) fn take(&self, id: MailboxId) -> Option<Child> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &mut *self.inner.get() };
        let slot = map.get_mut(&id)?;
        match mem::replace(&mut slot.seat, Seat::Out) {
            Seat::Seated(child) => Some(child),
            Seat::Out => None,
        }
    }

    /// Seat `child` in its slot, in the case the caller hands over (record
    /// preserved). Pairs with [`Self::take`] and [`Self::reserve`], which
    /// left the slot [`Seat::Out`].
    ///
    /// When no slot is at `id` the child was despawned while it was out, and
    /// it comes back as [`Reinserted::Departed`]: the caller that holds it is
    /// the one that closes it, running its `unwire` if it owes one, so the
    /// registry keeps no pending-removal state. O(log n).
    #[must_use]
    pub(crate) fn reinsert(&self, id: MailboxId, child: Child) -> Reinserted {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &mut *self.inner.get() };
        match map.get_mut(&id) {
            Some(slot) => {
                slot.seat = Seat::Seated(child);
                Reinserted::Seated
            }
            None => Reinserted::Departed(child),
        }
    }

    /// Tear down the inline child registered under `id` (ADR-0114
    /// teardown): remove its slot, dropping a seated child. Returns `true`
    /// if a slot was present, `false` if `id` named no inline child
    /// (idempotent — a re-despawn of an already-gone alias is a clean
    /// `false`, not an error). Backs [`WasmCtx::despawn_inline_child`], which
    /// takes a seated child out and runs its `unwire` first. O(log n).
    ///
    /// A slot that is [`Seat::Out`] holds no child: its child is on the
    /// stack of the caller running it (a child despawning itself, held by
    /// [`membrane_dispatch`]). Removing that slot makes the matching
    /// [`Self::reinsert`] answer [`Reinserted::Departed`], and that caller
    /// closes the child once its handler has returned.
    pub(crate) fn remove(&self, id: MailboxId) -> bool {
        // SAFETY: see [`Self::insert_child`] — the borrow is taken fresh
        // and released before return, never spanning a dispatch.
        let map = unsafe { &mut *self.inner.get() };
        map.remove(&id).is_some()
    }

    /// The alias of the inline child standing beneath `parent`, as type
    /// `type_tag`, at the resolved subname `subname`: the three things an
    /// alias is folded from, which the guest cannot fold itself. `None` when
    /// no slot matches. A name is resident when it has a slot, whichever
    /// seat the slot is in, so a child that is out for its own handler is
    /// still found. A scan like [`Self::child_of`], with the type compared
    /// too.
    #[must_use]
    pub(crate) fn resident(&self, parent: MailboxId, type_tag: ActorTypeTag, subname: &str) -> Option<MailboxId> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &*self.inner.get() };
        map.iter().find(|(_, slot)| slot.stands_at(parent, type_tag, subname)).map(|(key, _)| *key)
    }

    /// Snapshot the reconstruct metadata of every resident inline child
    /// (ADR-0114 §5 dehydrate walk). The actor boxes stay in the
    /// registry; the compose path reads each child's state through
    /// [`Self::with_child_mut`] keyed by the returned `id`. Children are
    /// returned in [`MailboxId`] key order. The dehydrate/rehydrate walk
    /// reconstructs each child by its own `alias_id` / `type_tag` /
    /// `full_subname`, and a close unwires the children of one depth in the
    /// reverse of this order ([`unwire_children`]).
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
                parent: MailboxId(slot.parent),
                config_bytes: slot.config_bytes.clone(),
            })
            .collect()
    }

    /// Run `f` against the child seated under `id`, wired or not, with a
    /// unique mutable borrow held only for the call, returning its result
    /// (or `None` if no child is seated at `id`). Used by the dehydrate
    /// compose to drive each child's `erased_on_dehydrate` in place. The
    /// borrow drops before this returns, so it never spans a dispatch.
    /// O(log n).
    pub(crate) fn with_child_mut<R>(&self, id: MailboxId, f: impl FnOnce(&mut dyn ErasedWasmActor) -> R) -> Option<R> {
        // SAFETY: see [`Self::insert_child`].
        let map = unsafe { &mut *self.inner.get() };
        match &mut map.get_mut(&id)?.seat {
            Seat::Seated(child) => Some(f(child.actor_mut())),
            Seat::Out => None,
        }
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
        map.iter().find(|(_, slot)| slot.parent == parent.0 && slot.full_subname == subname).map(|(key, _)| *key)
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
    /// (`MAIL_BRIDGE.send_mail`) like any cross-cluster send. `mode`
    /// selects [`ChainMode::Inherit`] (the handler's causal chain carries
    /// through to the host on the remote path, ADR-0080 §7) or
    /// [`ChainMode::Detached`] (the host mints a fresh chain root); an
    /// in-place dispatch carries no host trace ids, so the mode is
    /// irrelevant on the local path.
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
    ///
    /// `payload` carries the held values its bytes name by hash (ADR-0238
    /// decision 3). On the `Remote` branch they stay alive for the whole host
    /// call, whose resolve on send attaches their entries, and drop after it.
    /// On the `Local` branch the [`QueuedMail`] keeps them until its
    /// recipient has dispatched.
    pub(crate) fn route_or_enqueue(
        &self,
        recipient: u64,
        kind: u64,
        payload: EncodedGuestMail,
        count: u32,
        mode: ChainMode,
        sender: u64,
    ) {
        match self.route_decision(recipient) {
            RouteDecision::Local => {
                let EncodedGuestMail { bytes, keep } = payload;
                // SAFETY: see [`Self::insert_child`] — the queue borrow is
                // taken fresh and released before return, never spanning a
                // dispatch (the drain re-borrows per item).
                let queue = unsafe { &mut *self.queue.get() };
                queue.push_back(QueuedMail { recipient, kind, bytes, keep, count, sender });
            }
            RouteDecision::Remote => send_through_host(recipient, kind, payload, count, mode, sender),
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

    /// Seat `actor` under `id` as a spawn leaves a child whose `wire`
    /// returned `Ok`, by the two calls a spawn makes. For a host test that
    /// needs a wired child and has no actor type to spawn.
    #[cfg(test)]
    pub(crate) fn seat_wired(&self, id: MailboxId, record: ChildRecord, actor: Box<dyn ErasedWasmActor>) {
        self.reserve(id, record);
        let seated = self.reinsert(id, Child::Wired(actor));
        assert!(matches!(seated, Reinserted::Seated), "a slot just reserved takes its child");
    }
}

/// Where a membrane dispatch came from, which decides whether the dispatched
/// inline child may read the host's reply correlation (ADR-0139).
#[derive(Clone, Copy)]
enum DispatchOrigin {
    /// A top-level `receive_p32` dispatch, whose reply correlation the host
    /// set for this call.
    Host,
    /// An item drained from the cluster queue, which carries no host
    /// correlation.
    Cluster,
}

/// ADR-0114 decision #3: the receive membrane every `export!`
/// `receive_p32` shim routes inbound mail through, for the top-level host
/// dispatch only. The shim passes its component's own `registry` (the
/// emitted `static __AETHER_INLINE`). When the routed recipient is the
/// parent's own mailbox id, dispatch the parent (`dispatch_own`); otherwise
/// take the inline child the producer addressed out of `registry`, dispatch
/// it with a ctx self-identified as the child and carrying the same
/// `registry` ([`WasmCtx::__new`]), and reinsert it in the case it was
/// taken in. A child that despawned itself in its handler has no slot to go
/// back to: it runs its `unwire`, if it wired, now that its dispatch has
/// returned, and then drops (ADR-0249 §4). An unrecognised recipient
/// falls back to the parent's dispatch — the existing unmatched path (the
/// parent's `#[fallback]`, or the `DISPATCH_UNKNOWN_KIND` sentinel for a
/// strict receiver), never a short-circuit drop.
///
/// `source` is the inbound source threaded onto the dispatched child's ctx
/// (issues 1987 + 2001): the host-resolved inbound source (the `receive_p32`
/// membrane threads the same value it received over the ABI), or
/// `NO_INBOUND_SOURCE` (`0`) when there is no peer-component origin. The child's
/// `ctx.sender()` is a single read of this field. The own-id path's ctx
/// is built by `dispatch_own`, which the caller has already bound to the same
/// `source`.
///
/// A child dispatched here reads the host's reply correlation through
/// `in_reply_to()`, so a reply to a request the child sent — delivered to the
/// child's alias — is matched to that request (issue 6530).
///
/// For a normal (non-inline) actor the routed recipient equals the
/// parent's own id, so the membrane no-ops straight to `dispatch_own` —
/// the regression guard the whole demux rests on.
pub fn membrane_dispatch<F>(
    own_mailbox_id: u64,
    mail: Mail<'_>,
    registry: &Registry,
    source: u64,
    dispatch_own: F,
) -> u32
where
    F: FnOnce(Mail<'_>) -> u32,
{
    dispatch_member(own_mailbox_id, mail, registry, source, DispatchOrigin::Host, dispatch_own)
}

/// The membrane body shared by the top-level dispatch and the drain. `origin`
/// picks the dispatched child's ctx: a [`DispatchOrigin::Host`] child reads the
/// host's reply correlation, a [`DispatchOrigin::Cluster`] child never does.
fn dispatch_member<F>(
    own_mailbox_id: u64,
    mail: Mail<'_>,
    registry: &Registry,
    source: u64,
    origin: DispatchOrigin,
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
            let mut ctx = match origin {
                DispatchOrigin::Host => WasmCtx::__new(recipient, registry, source),
                DispatchOrigin::Cluster => WasmCtx::__new_local_dispatch(recipient, registry, source),
            };
            let rc = child.actor_mut().erased_dispatch(&mut ctx, mail);
            if let Reinserted::Departed(departed) = registry.reinsert(id, child) {
                drop(unwire_child(registry, id, departed));
            }
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
/// the same inbound source the child path threads through the membrane;
/// the resulting closure is handed straight to the membrane and dropped
/// before the next iteration.
///
/// Issue 1987: each drained member's identity (its own id, `item.recipient`)
/// rides on the sends it makes (the ctx threads it as the `from` half through
/// `route_or_enqueue`), and each item's inbound source (`item.sender`) rides
/// on the dispatched ctx — no ambient host re-stamp, no registry cell.
///
/// Drained members get a cluster ctx, so `in_reply_to()` never reads the outer
/// dispatch's correlation.
///
/// Reentrancy and cycles are handled by the queue, not by nested dispatch:
/// a drained item's handler that sends to a busy cluster member just pushes
/// a later queue item, which this same loop picks up.
pub fn drain_cluster_queue<M, Own>(registry: &Registry, mut mk_own: M)
where
    M: FnMut(u64) -> Own,
    Own: FnOnce(Mail<'_>) -> u32,
{
    let self_id = registry.self_id();
    while let Some(item) = registry.pop_queued() {
        // Keep the owned bytes alive for the duration of this item's
        // dispatch; the `Mail` borrows them by raw pointer + length. The
        // held values they name stay alive as long, and drop once the
        // dispatch has run.
        let bytes = item.bytes;
        let keep = item.keep;
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
        // both read the same source via `ctx.sender()`. The member's
        // *own* sends carry the member's id (`item.recipient`, the ctx's
        // identity) as their `from` through `route_or_enqueue` — no host
        // re-stamp.
        let dispatch_own = mk_own(item.sender);
        dispatch_member(self_id, mail, registry, item.sender, DispatchOrigin::Cluster, dispatch_own);
        drop(keep);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChainMode, Child, ChildRecord, Registry, Reinserted, RouteDecision, drain_cluster_queue, membrane_dispatch,
    };
    use crate::blob::guest::EncodedGuestMail;
    use crate::blob::guest::tracked::tracked_blob;
    use crate::mail::{Mail, PriorState};
    use crate::reference::ErasedActorRef;
    use crate::wasm::ctx::NO_INBOUND_SOURCE;
    use crate::wasm::{ActorInitError, ErasedWasmActor};
    use crate::{ActorTypeTag, WasmCtx};
    use aether_data::{MailboxId, RequestId};
    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

    /// Shared cell a [`RecordingChild`] writes the `sender()` position it
    /// observed into, read back by the source-attribution tests.
    type SourceCell = Rc<Cell<Option<MailboxId>>>;

    /// Distinct return codes so an assertion can tell which dispatch path
    /// the membrane took.
    const OWN_CODE: u32 = 0xA0;
    const CHILD_CODE: u32 = 0xC0;

    /// Minimal `ErasedWasmActor` for the membrane tests: bumps a
    /// test-local dispatch counter (shared via [`Rc`] so the test reads it
    /// back without a process-global), records the `ctx.sender()` position
    /// it observed on the most recent dispatch (the in-place "from" half),
    /// and returns [`CHILD_CODE`]. The lifecycle hooks are unreachable in
    /// these tests.
    struct RecordingChild {
        dispatches: Rc<Cell<u32>>,
        /// The `sender()` position the child read on its last dispatch,
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
                Self { dispatches: Rc::clone(&dispatches), observed_source: Rc::clone(&observed_source) },
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
            ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
            _mail: Mail<'_>,
        ) -> u32 {
            self.dispatches.set(self.dispatches.get() + 1);
            self.observed_source.set(ctx.sender().map(ErasedActorRef::id));
            CHILD_CODE
        }
        fn erased_wire(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut crate::WasmDropCtx<'_>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
            _prior: PriorState<'_>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
    }

    /// A child that despawns *itself* during its own dispatch — through the
    /// `ctx`, whose inline registry the membrane threaded in — and writes
    /// each step of its close to a test-local log (shared via [`Rc`]): the
    /// end of its handler, its `unwire`, and its drop. The reentrancy test
    /// reads the log to prove the order they ran in. Carries its own alias
    /// id so `erased_dispatch` can despawn the matching slot.
    struct SelfDespawningChild {
        id: ErasedActorRef,
        steps: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Drop for SelfDespawningChild {
        fn drop(&mut self) {
            self.steps.borrow_mut().push("drop");
        }
    }

    impl ErasedWasmActor for SelfDespawningChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.self_despawning_child"
        }
        fn erased_dispatch(
            &mut self,
            ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
            _mail: Mail<'_>,
        ) -> u32 {
            // Self-despawn mid-dispatch through the threaded registry: this
            // box is currently taken out (held on the membrane's stack), so
            // the ctx's despawn removes the slot and the membrane's
            // `reinsert` hands the child back as departed.
            ctx.despawn_inline_child(self.id);
            self.steps.borrow_mut().push("handler returns");
            CHILD_CODE
        }
        fn erased_wire(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>) {
            self.steps.borrow_mut().push("unwire");
        }
        fn erased_on_dehydrate(&mut self, _ctx: &mut crate::WasmDropCtx<'_>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
            _prior: PriorState<'_>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
    }

    /// A child that records the `in_reply_to()` its ctx answered on its last
    /// dispatch and whether it was dispatched at all, so the drain guard can
    /// assert a drained child never reads the outer host correlation.
    struct ReplyProbeChild {
        dispatched: Rc<Cell<bool>>,
        observed_reply: Rc<Cell<Option<RequestId>>>,
    }

    impl ErasedWasmActor for ReplyProbeChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.reply_probe_child"
        }
        fn erased_dispatch(
            &mut self,
            ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
            _mail: Mail<'_>,
        ) -> u32 {
            self.dispatched.set(true);
            self.observed_reply.set(ctx.in_reply_to());
            CHILD_CODE
        }
        fn erased_wire(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut crate::WasmDropCtx<'_>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, crate::Erased, crate::Anyone, crate::Unchecked>,
            _prior: PriorState<'_>,
        ) -> Result<(), ActorInitError> {
            Ok(())
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

    /// A child that is taken out and put back, by the registry's own verbs
    /// or by a dispatch through the membrane, is seated in the case it left.
    /// Catches a dispatch that reseats a wired child as unwired, which would
    /// drop its `unwire` at the close.
    #[test]
    fn registry_insert_take_reinsert_round_trips() {
        let registry = Registry::new();
        let own = 0x1100_u64;
        let wired = MailboxId(0x1111);
        let rebuilt = MailboxId(0x1112);

        assert!(registry.take(wired).is_none(), "empty registry has no child");
        registry.seat_wired(
            wired,
            ChildRecord { full_subname: String::from("wired"), ..ChildRecord::default() },
            Box::new(RecordingChild::new().0),
        );
        registry.insert_child(
            rebuilt,
            ChildRecord { full_subname: String::from("rebuilt"), ..ChildRecord::default() },
            Box::new(RecordingChild::new().0),
        );

        let taken = registry.take(wired).expect("a seated child is taken");
        assert!(registry.take(wired).is_none(), "a slot whose child is out has none to take until reinsert");
        assert!(matches!(registry.reinsert(wired, taken), Reinserted::Seated), "a taken child goes back to its slot");

        for id in [wired, rebuilt] {
            let rc = membrane_dispatch(own, mail_to(id.0), &registry, NO_INBOUND_SOURCE, |_mail| OWN_CODE);
            assert_eq!(rc, CHILD_CODE, "the reseated child handles its mail");
        }

        let wired_after = registry.take(wired).expect("the dispatched child was reseated");
        let rebuilt_after = registry.take(rebuilt).expect("the dispatched child was reseated");
        assert!(matches!(wired_after, Child::Wired(_)), "a wired child is reseated wired");
        assert!(matches!(rebuilt_after, Child::Unwired(_)), "a child that never wired is reseated unwired");
    }

    /// A spawned child's slot carries its actor-type tag, resolved subname,
    /// and logical parent, surfaced through `child_metas` for the dehydrate
    /// walk.
    #[test]
    fn child_metas_carry_reconstruct_metadata() {
        let registry = Registry::new();
        let id = MailboxId(0x7777);
        let parent = MailboxId(0x7000);
        let tag = 0xABCD_u64;
        registry.insert_child(
            id,
            ChildRecord {
                type_tag: tag,
                full_subname: String::from("widget"),
                parent: parent.0,
                ..ChildRecord::default()
            },
            Box::new(RecordingChild::new().0),
        );

        let metas = registry.child_metas();
        let [meta] = metas.as_slice() else {
            panic!("expected exactly one child meta, got {}", metas.len())
        };
        assert_eq!(meta.id, id, "the meta carries the alias id");
        assert_eq!(meta.type_tag, tag, "the meta carries the actor-type tag");
        assert_eq!(meta.full_subname, "widget", "the meta carries the subname");
        assert!(!meta.is_counter, "a Named subname is not a counter");
        assert_eq!(meta.parent, parent, "the meta carries the resident slot's logical parent");
    }

    #[test]
    fn actor_type_tag_resolves_entry_actor() {
        let registry = Registry::new();
        let entry = MailboxId(0x8100);
        let tag = ActorTypeTag(0xA100);
        registry.set_self_id(entry.0);
        registry.set_entry_actor_tag(tag);

        assert_eq!(registry.actor_type_tag(entry), Some(tag));
    }

    #[test]
    fn actor_type_tag_resolves_nested_inline_actor() {
        let registry = Registry::new();
        let entry = MailboxId(0x8200);
        let parent = MailboxId(0x8201);
        let nested = MailboxId(0x8202);
        let nested_tag = ActorTypeTag(0xA202);
        registry.set_self_id(entry.0);
        registry.insert_child(
            parent,
            ChildRecord {
                type_tag: 0xA201,
                full_subname: String::from("parent"),
                parent: entry.0,
                ..ChildRecord::default()
            },
            Box::new(RecordingChild::new().0),
        );
        registry.insert_child(
            nested,
            ChildRecord {
                type_tag: nested_tag.0,
                full_subname: String::from("nested"),
                parent: parent.0,
                ..ChildRecord::default()
            },
            Box::new(RecordingChild::new().0),
        );

        assert_eq!(registry.actor_type_tag(nested), Some(nested_tag));
    }

    #[test]
    fn actor_type_tag_returns_none_for_missing_mailbox() {
        let registry = Registry::new();
        registry.set_self_id(0x8300);
        registry.set_entry_actor_tag(ActorTypeTag(0xA300));

        assert_eq!(registry.actor_type_tag(MailboxId(0x83FF)), None);
    }

    #[test]
    fn actor_type_tag_survives_taken_inline_slot() {
        let registry = Registry::new();
        let child = MailboxId(0x8401);
        let tag = ActorTypeTag(0xA401);
        registry.insert_child(
            child,
            ChildRecord {
                type_tag: tag.0,
                full_subname: String::from("child"),
                parent: 0x8400,
                ..ChildRecord::default()
            },
            Box::new(RecordingChild::new().0),
        );

        let _taken = registry.take(child).expect("the child is resident before dispatch");
        assert_eq!(registry.actor_type_tag(child), Some(tag));
    }

    /// Step 4 coverage: recipient == own id dispatches the parent, never
    /// the child registry.
    #[test]
    fn membrane_routes_own_recipient_to_parent() {
        let registry = Registry::new();
        let own = 0x2000_u64;
        let rc = membrane_dispatch(own, mail_to(own), &registry, NO_INBOUND_SOURCE, |_mail| OWN_CODE);
        assert_eq!(rc, OWN_CODE, "own-id recipient runs the parent dispatch");
    }

    /// Step 4 coverage: a child-addressed recipient dispatches the child
    /// and reinserts it, so a second send to the same alias dispatches
    /// again (the take/reinsert round-trip under the membrane).
    #[test]
    fn membrane_routes_child_recipient_and_reinserts() {
        let registry = Registry::new();
        let own = 0x3000_u64;
        let child = 0x3001_u64;
        let (recording, dispatches) = RecordingChild::new();
        registry.insert_child(
            MailboxId(child),
            ChildRecord { full_subname: String::from("widget"), ..ChildRecord::default() },
            Box::new(recording),
        );

        let rc = membrane_dispatch(own, mail_to(child), &registry, NO_INBOUND_SOURCE, |_mail| {
            panic!("own dispatch must not run for a child recipient")
        });
        assert_eq!(rc, CHILD_CODE, "child recipient runs the child dispatch");

        // Reinserted: a second send to the same alias dispatches again.
        let rc2 = membrane_dispatch(own, mail_to(child), &registry, NO_INBOUND_SOURCE, |_mail| {
            panic!("own dispatch must not run for a reinserted child")
        });
        assert_eq!(rc2, CHILD_CODE, "the child was reinserted after dispatch");
        assert_eq!(dispatches.get(), 2, "both sends reached the child");
    }

    /// Step 4 coverage: an unrecognised recipient (no resident child) runs
    /// the parent's unmatched path rather than short-circuit dropping.
    #[test]
    fn membrane_routes_unknown_recipient_to_parent_unmatched_path() {
        let registry = Registry::new();
        let own = 0x4000_u64;
        let stray = 0x4999_u64;
        let rc = membrane_dispatch(own, mail_to(stray), &registry, NO_INBOUND_SOURCE, |_mail| OWN_CODE);
        assert_eq!(rc, OWN_CODE, "an unknown recipient falls back to the parent's unmatched path");
    }

    /// A wired child that despawns itself mid-dispatch runs its `unwire`
    /// once, after its handler has returned and before its box drops.
    /// `membrane_dispatch` takes it out, the handler removes its own slot via
    /// `ctx.despawn_inline_child` (driving the same registry the membrane
    /// threaded in), and the membrane's `reinsert` hands the child back as
    /// departed, so the membrane closes it. A subsequent send to the same
    /// alias then falls through to the parent's unmatched path. Catches the
    /// box dropped with no `unwire`, and an `unwire` run re-entrantly inside
    /// the handler that asked for the despawn.
    #[test]
    fn membrane_self_despawn_drops_box_and_falls_through() {
        let registry = Registry::new();
        let own = 0x5000_u64;
        let child = 0x5001_u64;
        let steps = Rc::new(RefCell::new(Vec::new()));
        registry.seat_wired(
            MailboxId(child),
            ChildRecord { full_subname: String::from("widget"), ..ChildRecord::default() },
            Box::new(SelfDespawningChild { id: ErasedActorRef::new(MailboxId(child)), steps: Rc::clone(&steps) }),
        );

        // Dispatch the child; it despawns its own slot mid-dispatch.
        let rc = membrane_dispatch(own, mail_to(child), &registry, NO_INBOUND_SOURCE, |_mail| {
            panic!("own dispatch must not run while the child is resident")
        });
        assert_eq!(rc, CHILD_CODE, "the child handled the despawning dispatch");
        assert_eq!(
            steps.borrow().as_slice(),
            ["handler returns", "unwire", "drop"],
            "the departed child ran unwire once, after its handler and before its drop",
        );

        // The alias is gone: a second send falls through to the parent's
        // unmatched path rather than re-dispatching a dropped child.
        let rc2 = membrane_dispatch(own, mail_to(child), &registry, NO_INBOUND_SOURCE, |_mail| OWN_CODE);
        assert_eq!(rc2, OWN_CODE, "the torn-down alias falls through to the parent");
    }

    /// Install a recording child under `id` with `parent`, returning the
    /// shared dispatch counter. Shared helper for the addressing /
    /// route / drain tests below.
    fn install_recording(registry: &Registry, id: u64, parent: u64) -> Rc<Cell<u32>> {
        let (recording, dispatches) = RecordingChild::new();
        registry.insert_child(
            MailboxId(id),
            ChildRecord { full_subname: String::from("recording"), parent, ..ChildRecord::default() },
            Box::new(recording),
        );
        dispatches
    }

    /// Addressing amendment: parent / child / sibling resolve by registry
    /// lookup over the recorded logical tree, and a missing relative is a
    /// clean `None` — never a fold.
    #[test]
    fn relative_resolution_walks_recorded_parent_links() {
        let registry = Registry::new();
        let root = 0x1000_u64;
        registry.set_self_id(root);

        // Two children of the root with distinct subnames, plus a grandchild
        // of the first child.
        let bar = MailboxId(0x1001);
        let baz = MailboxId(0x1002);
        let button = MailboxId(0x1003);
        registry.insert_child(
            bar,
            ChildRecord { full_subname: String::from("bar"), parent: root, ..ChildRecord::default() },
            Box::new(RecordingChild::new().0),
        );
        registry.insert_child(
            baz,
            ChildRecord { full_subname: String::from("baz"), parent: root, ..ChildRecord::default() },
            Box::new(RecordingChild::new().0),
        );
        registry.insert_child(
            button,
            ChildRecord { full_subname: String::from("button"), parent: bar.0, ..ChildRecord::default() },
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
        assert_eq!(registry.parent_of(MailboxId(0xDEAD)), None, "a stray id resolves to no parent");

        // child_of: the root's child named "bar"/"baz"; bar's child "button".
        assert_eq!(registry.child_of(MailboxId(root), "bar"), Some(bar));
        assert_eq!(registry.child_of(MailboxId(root), "baz"), Some(baz));
        assert_eq!(registry.child_of(bar, "button"), Some(button));
        assert_eq!(registry.child_of(MailboxId(root), "missing"), None, "no child named 'missing' resides");
        assert_eq!(
            registry.child_of(MailboxId(root), "button"),
            None,
            "'button' is bar's child, not the root's — scoping is by parent",
        );

        // sibling_of: bar and baz are siblings under the root.
        assert_eq!(registry.sibling_of(bar, "baz"), Some(baz));
        assert_eq!(registry.sibling_of(baz, "bar"), Some(bar));
        assert_eq!(registry.sibling_of(button, "bar"), None, "button's parent (bar) has no child named 'bar'");
    }

    /// Addressing amendment: `route_decision` classifies the cluster's own
    /// id and any resident inline-child alias as `Local` (in-place dispatch)
    /// and any other recipient as `Remote` (host hand-off) — the pure
    /// membership decision behind `route_or_enqueue`, testable without a
    /// live `MAIL_BRIDGE`.
    #[test]
    fn route_decision_classifies_cluster_membership() {
        let registry = Registry::new();
        let root = 0x2000_u64;
        let child = 0x2001_u64;
        registry.set_self_id(root);
        install_recording(&registry, child, root);

        assert_eq!(registry.route_decision(root), RouteDecision::Local, "the cluster's own id is local");
        assert_eq!(registry.route_decision(child), RouteDecision::Local, "a resident inline-child alias is local");
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
        let registry = Registry::new();
        let root = 0x3000_u64;
        let child = 0x3001_u64;
        registry.set_self_id(root);
        install_recording(&registry, child, root);

        assert_eq!(registry.queued_len(), 0, "the queue starts empty");
        registry.route_or_enqueue(root, 7, EncodedGuestMail::plain(vec![1, 2, 3]), 1, ChainMode::Inherit, root);
        assert_eq!(registry.queued_len(), 1, "an own-id send enqueues locally, no host call");
        registry.route_or_enqueue(child, 8, EncodedGuestMail::plain(vec![4]), 1, ChainMode::Inherit, root);
        assert_eq!(registry.queued_len(), 2, "a child-alias send enqueues locally too");
    }

    /// Addressing amendment: a seeded local item drains through the membrane
    /// (dispatched once) and the queue empties in one `drain_cluster_queue`
    /// call.
    #[test]
    fn drain_dispatches_a_seeded_local_item() {
        let registry = Registry::new();
        let root = 0x4000_u64;
        let child = 0x4001_u64;
        registry.set_self_id(root);
        let dispatches = install_recording(&registry, child, root);

        // Seed one local send addressed to the child.
        registry.route_or_enqueue(child, 1, EncodedGuestMail::plain(vec![0xAB]), 1, ChainMode::Inherit, root);

        let own_dispatches = Rc::new(Cell::new(0));
        let own_counter = Rc::clone(&own_dispatches);
        drain_cluster_queue(&registry, |_source| {
            let own_counter = Rc::clone(&own_counter);
            move |_mail| {
                own_counter.set(own_counter.get() + 1);
                OWN_CODE
            }
        });

        assert_eq!(dispatches.get(), 1, "the child-addressed item dispatched the child once");
        assert_eq!(own_dispatches.get(), 0, "a child-addressed item never ran the parent dispatch");
        assert_eq!(registry.queued_len(), 0, "the queue is empty after the drain");
    }

    /// ADR-0238 decision 3: a queued intra-cluster send keeps the values its
    /// bytes name alive past the sender's own value, until the recipient has
    /// dispatched, and no longer. In a guest the kept value is the hold that
    /// admits the recipient's decode of the hash. Catches a queue that drops
    /// the kept values before the drain reaches the mail (the recipient's
    /// decode would find no hold) or keeps them after it (the hold would
    /// leak until the instance ends).
    #[test]
    fn a_queued_send_keeps_its_named_values_until_the_recipient_dispatches() {
        use alloc::sync::Arc;
        use core::sync::atomic::Ordering;

        let registry = Registry::new();
        let root = 0x4100_u64;
        registry.set_self_id(root);
        let (value, dropped) = tracked_blob();

        let payload = EncodedGuestMail { bytes: vec![0xAB], keep: vec![value.clone()] };
        registry.route_or_enqueue(root, 1, payload, 1, ChainMode::Inherit, root);
        drop(value);

        assert!(!dropped.load(Ordering::SeqCst), "the queued mail keeps the value the sender dropped");

        let alive_at_dispatch = Rc::new(Cell::new(None));
        let observed = Rc::clone(&alive_at_dispatch);
        let watched = Arc::clone(&dropped);
        drain_cluster_queue(&registry, |_source| {
            let observed = Rc::clone(&observed);
            let watched = Arc::clone(&watched);
            move |_mail| {
                observed.set(Some(!watched.load(Ordering::SeqCst)));
                OWN_CODE
            }
        });

        assert_eq!(alive_at_dispatch.get(), Some(true), "the value is alive while the recipient dispatches");
        assert!(dropped.load(Ordering::SeqCst), "the value goes once the recipient has dispatched");
    }

    /// Addressing amendment: a cascade — a drained item whose dispatch
    /// enqueues another local item — drains fully in one
    /// `drain_cluster_queue` call (the queue, not nested dispatch, carries
    /// the cascade). Here the parent dispatch enqueues a follow-up to the
    /// child the first time it runs.
    #[test]
    fn drain_runs_a_cascade_in_one_call() {
        let registry = Registry::new();
        let root = 0x5000_u64;
        let child = 0x5001_u64;
        registry.set_self_id(root);
        let child_dispatches = install_recording(&registry, child, root);

        // Seed one own-addressed item; the parent dispatch, on its first
        // run, enqueues a follow-up addressed to the child.
        registry.route_or_enqueue(root, 1, EncodedGuestMail::plain(vec![0x01]), 1, ChainMode::Inherit, root);

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
                        registry_ref.route_or_enqueue(
                            child,
                            2,
                            EncodedGuestMail::plain(vec![0x02]),
                            1,
                            ChainMode::Inherit,
                            root,
                        );
                    }
                }
                OWN_CODE
            }
        });

        assert!(own_ran.get(), "the seeded own item dispatched");
        assert_eq!(child_dispatches.get(), 1, "the cascaded follow-up reached the child in the same drain call");
        assert_eq!(
            order.borrow().as_slice(),
            ["own"],
            "exactly one own dispatch ran (the seed); the cascade went to the child",
        );
        assert_eq!(registry.queued_len(), 0, "the cascade drained fully — queue empty");
    }

    /// Install a recording child under `id` with `parent`, returning both the
    /// shared dispatch counter and the shared observed-source cell, so a test
    /// can assert the in-place "from" half a drained dispatch reads.
    fn install_recording_with_source(registry: &Registry, id: u64, parent: u64) -> (Rc<Cell<u32>>, SourceCell) {
        let (recording, dispatches, source) = RecordingChild::new_with_source();
        registry.insert_child(
            MailboxId(id),
            ChildRecord { full_subname: String::from("recording"), parent, ..ChildRecord::default() },
            Box::new(recording),
        );
        (dispatches, source)
    }

    /// Task 1: a child dispatched off the drain reads
    /// `ctx.sender()` == the enqueuing sender — the child → parent
    /// direction. The parent (the cluster root) enqueues a send to the child
    /// stamped with the parent's own id; the drained child observes exactly
    /// that id, not `None`.
    #[test]
    fn drained_child_reads_enqueuing_sender_parent() {
        let registry = Registry::new();
        let root = 0x6000_u64;
        let child = 0x6001_u64;
        registry.set_self_id(root);
        let (dispatches, observed) = install_recording_with_source(&registry, child, root);

        // The parent (root) sends to the child, stamping its own id as sender.
        registry.route_or_enqueue(child, 1, EncodedGuestMail::plain(vec![0x00]), 1, ChainMode::Inherit, root);

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

    /// A `membrane_dispatch` called with the `NO_INBOUND_SOURCE` source threads
    /// it verbatim, so the dispatched child reads `sender() == None`. (In
    /// production the `receive_p32` shim threads the host-resolved inbound
    /// source instead; this exercises the function's no-source contract
    /// directly — there is no host reply-table fallback.)
    #[test]
    fn membrane_dispatch_with_none_source_reads_no_source() {
        let registry = Registry::new();
        let own = 0x8000_u64;
        let child = 0x8001_u64;
        registry.set_self_id(own);
        let (dispatches, observed) = install_recording_with_source(&registry, child, own);

        // Dispatch the child directly — not through the drain — with no
        // source, so the ctx carries no in-place sender.
        let rc = membrane_dispatch(own, mail_to(child), &registry, NO_INBOUND_SOURCE, |_mail| {
            panic!("own dispatch must not run for a child recipient")
        });
        assert_eq!(rc, CHILD_CODE, "the child handled the direct dispatch");
        assert_eq!(dispatches.get(), 1, "the child was dispatched once");
        assert_eq!(observed.get(), None, "a top-level dispatch reads no in-place source (no source on the ctx)");
    }

    /// A child dispatched off the drain reads `in_reply_to() == None`: a queued
    /// intra-cluster send never crossed the host envelope, so the drained ctx
    /// must not read the outer dispatch's reply correlation (ADR-0139). On the
    /// host build that read would reach the `reply_correlation` stub, which
    /// panics.
    #[test]
    fn drained_child_never_reads_the_host_reply_correlation() {
        let registry = Registry::new();
        let root = 0x9000_u64;
        let child = 0x9001_u64;
        registry.set_self_id(root);
        let dispatched = Rc::new(Cell::new(false));
        let observed_reply = Rc::new(Cell::new(None));
        registry.insert_child(
            MailboxId(child),
            ChildRecord { full_subname: String::from("reply_probe"), parent: root, ..ChildRecord::default() },
            Box::new(ReplyProbeChild {
                dispatched: Rc::clone(&dispatched),
                observed_reply: Rc::clone(&observed_reply),
            }),
        );

        registry.route_or_enqueue(child, 1, EncodedGuestMail::plain(vec![0x00]), 1, ChainMode::Inherit, root);
        drain_cluster_queue(&registry, |_source| {
            |_mail| panic!("own dispatch must not run for a child-addressed item")
        });

        assert!(dispatched.get(), "the drained item reached the child");
        assert_eq!(observed_reply.get(), None, "a drained child reads no host reply correlation");
    }
}
