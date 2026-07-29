use std::collections::HashMap;
use std::process::abort;
use std::sync::{Arc, OnceLock, RwLock};

use rustc_hash::FxHashMap;

use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};
use aether_data::{
    KindDescriptor, MailboxCategory, MailboxDescriptor, SchemaType, ScopePathError, mailbox_id_from_path,
    validate_scope_path,
};

use crate::mail::registry::errors::{DropError, KindConflict, NameConflict};
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};
use crate::mail::registry::names::categorise_mailbox_name;
use crate::mail::registry::{
    ActorAddressInventoryError, AddressResolutionError, MailDispatch, OwnedDispatch, ResolvedAddress,
    address::AddressIndex,
};
use crate::mail::view::{DoubleBuffer, Update, View, ViewPublisher};
use crate::mail::{KindId, MailboxId};
use crate::scheduler::SeizeHandle;

/// Deferred cell holding a `Pooled` actor's
/// [`SeizeHandle`], carried on every
/// inbox route (ADR-0087 §4, iamacoffeepot/aether#1135).
///
/// Registration (`register_inbox` / `try_register_inbox`) happens *before*
/// the dispatcher slot exists — the actor isn't built into a
/// `DispatcherSlot` until after `init` / `wire` — so the cell is empty
/// (`None`) at register time and the `Pooled`-branch wiring in
/// `chassis/builder.rs` + `actor/native/spawn.rs` installs the handle
/// once the slot is constructed (mirroring the `MailboxWakeSlot`
/// deferred-population pattern). Installation replaces the route with a
/// populated cell so an older published snapshot remains unchanged.
/// Closure / `Inline` handlers have no slot to seize, so their cell stays
/// empty forever and the blob demuxer deposits their mail as usual.
pub type SeizeCell = Arc<OnceLock<SeizeHandle>>;

/// What a given mailbox actually is. The registry records this so the
/// scheduler can dispatch appropriately without a per-mail type check.
/// `Clone` so compatibility callers can own a projection of one published
/// route generation for the duration of the handler call.
///
/// Issue 634 Phase 4 retired the dedicated `Component` variant —
/// every loaded wasm component is now a `WasmTrampoline` registered
/// here as an `Inbox` like every other actor.
///
/// Issue 838 / iamacoffeepot/aether#841: `Inbox` and `Inline` are
/// intentionally distinct variants — they *name where the handler
/// runs*. `Inbox` defers the work to an actor's dispatch thread,
/// `Inline` runs the work on the pushing thread. That decides who
/// owns the `Received`/`Finished` lifecycle bracket: the downstream
/// dispatch loop for `Inbox`, the mailer itself for `Inline`. See
/// each variant's docs and `Mailer::push`'s `route_mail` for the
/// bracket semantics.
///
/// Issue iamacoffeepot/aether#848 PR 2 + 3: the variants wrap
/// distinct trait objects ([`InboxHandler`] vs [`InlineHandler`])
/// whose dispatch types (`OwnedDispatch` vs `MailDispatch<'_>`) make
/// the wrong-shape body uneconomical to write at compile time. Not
/// a hard proof of correctness, but the affordance gap is wide
/// enough that the wrong shape genuinely doesn't fit.
#[derive(Clone)]
pub enum MailboxEntry {
    /// The handler body forwards the envelope into an actor's mpsc
    /// inbox; the actor's dispatch loop on another thread runs the
    /// work and records the `Received`/`Finished` lifecycle hooks.
    /// `Mailer::push` does NOT bracket this arm — the downstream
    /// dispatch loop owns the bracket. Installed by
    /// `claim_mailbox` / `Spawner::register_inbox` (instanced +
    /// singleton actors, including the wasm trampoline) and by the
    /// public [`Registry::register_inbox`] /
    /// [`Registry::try_register_inbox`] for callers that own a
    /// separate dispatcher loop. Handler receives
    /// [`OwnedDispatch`](crate::mail::registry::OwnedDispatch)
    /// so payload + `kind_name` move into the downstream envelope —
    /// see [`InboxHandler`] for the full contract.
    ///
    /// iamacoffeepot/aether#1135: `seize` is the point-in-time
    /// `SeizeCell`. The route is replaced with a populated cell once the
    /// recipient's dispatcher slot exists so older published generations
    /// remain immutable. Empty for closure-backed inboxes (no pool slot
    /// behind them).
    Inbox { handler: Arc<dyn InboxHandler>, seize: SeizeCell },
    /// The handler body does its work inline on the pushing thread;
    /// there is no actor dispatch loop behind it. `Mailer::push`
    /// brackets this arm with `Received` and `Finished` so the
    /// chain's `in_flight` balances and settlement subscribers
    /// (`SettlementRegistry`) wake (ADR-0080 §2, issue 838).
    /// Installed by [`Registry::register_inline`] /
    /// [`Registry::try_register_inline`]. Distinct from `Inbox` so
    /// the bracket isn't double-counted when the closure was an
    /// actor-enqueue (which would fire settlement prematurely).
    /// Handler receives borrowed
    /// [`MailDispatch<'_>`](crate::mail::registry::MailDispatch) — zero-copy
    /// reads; see [`InlineHandler`] for the full contract.
    Inline(Arc<dyn InlineHandler>),
    /// Mailbox has been explicitly dropped (ADR-0010). Mail addressed
    /// to a `Dropped` slot is discarded by the scheduler / ctx dispatch
    /// until the same name is re-registered, at which point the slot
    /// transitions back to `Inbox` under the same id (ADR-0029 ids
    /// are a function of name, so they're stable across drop/reload).
    Dropped,
}

pub struct Registry {
    inner: RwLock<Inner>,
    routes: View<FxHashMap<MailboxId, RouteRecord>>,
    kinds: View<KindTable>,
    addresses: Result<AddressIndex, ActorAddressInventoryError>,
    /// Issue iamacoffeepot/aether#742: notification hook fired after
    /// every successful mailbox registration. The chassis (or any
    /// hub-aware boot path) installs a closure that pushes the full
    /// inventory snapshot to the hub via `HubOutbound::egress_mailboxes_changed`,
    /// keeping the hub's per-engine mailbox cache in sync without
    /// requiring callers (chassis caps, the component-load cap) to
    /// remember to publish manually after each registration. Default
    /// `None` — registry stays decoupled from the hub layer.
    on_mailbox_change: RwLock<Option<MailboxChangeHook>>,
}

/// Issue iamacoffeepot/aether#742: hook signature. Receives the full
/// post-registration mailbox inventory so the chassis-installed
/// implementation can hand it straight to `HubOutbound::egress_mailboxes_changed`,
/// matching the existing `MailboxesChanged` wire shape (full snapshot
/// per replace, not deltas).
pub type MailboxChangeHook = Arc<dyn Fn(Vec<MailboxDescriptor>) + Send + Sync>;

#[derive(Clone)]
struct RouteRecord {
    canonical_name: String,
    endpoint: RouteEndpoint,
}

#[derive(Clone)]
enum RouteEndpoint {
    Inbox { handler: Arc<dyn InboxHandler>, seize: SeizeCell },
    Inline(Arc<dyn InlineHandler>),
    Dropped,
}

impl RouteEndpoint {
    fn from_entry(entry: MailboxEntry) -> Self {
        match entry {
            MailboxEntry::Inbox { handler, seize } => Self::Inbox { handler, seize },
            MailboxEntry::Inline(handler) => Self::Inline(handler),
            MailboxEntry::Dropped => Self::Dropped,
        }
    }

    fn as_entry(&self) -> MailboxEntry {
        match self {
            Self::Inbox { handler, seize } => {
                MailboxEntry::Inbox { handler: Arc::clone(handler), seize: Arc::clone(seize) }
            }
            Self::Inline(handler) => MailboxEntry::Inline(Arc::clone(handler)),
            Self::Dropped => MailboxEntry::Dropped,
        }
    }
}

/// One point-in-time route and kind-name lookup.
pub struct RouteLookup {
    endpoint: Option<RouteEndpoint>,
    kind_name: String,
    generation: u64,
}

impl RouteLookup {
    pub(crate) fn accepts_owned_dispatch(&self) -> bool {
        matches!(self.endpoint, Some(RouteEndpoint::Inbox { .. }))
    }

    pub(crate) fn enqueue_owned(&self, dispatch: OwnedDispatch) {
        let Some(RouteEndpoint::Inbox { handler, .. }) = &self.endpoint else {
            panic!("enqueue_owned called for a route that does not accept owned dispatch")
        };
        handler.enqueue(dispatch);
    }

    pub(crate) fn dispatch_inline(&self, dispatch: MailDispatch<'_>) -> bool {
        let Some(RouteEndpoint::Inline(handler)) = &self.endpoint else {
            return false;
        };
        handler.dispatch(dispatch);
        true
    }

    pub(crate) fn is_dropped(&self) -> bool {
        matches!(self.endpoint, Some(RouteEndpoint::Dropped))
    }

    pub(crate) fn is_unknown(&self) -> bool {
        self.endpoint.is_none()
    }

    pub(crate) fn kind_name(&self) -> &str {
        &self.kind_name
    }

    pub(crate) fn seize_handle(&self) -> Option<&SeizeHandle> {
        match &self.endpoint {
            Some(RouteEndpoint::Inbox { seize, .. }) => seize.get(),
            Some(RouteEndpoint::Inline(_) | RouteEndpoint::Dropped) | None => None,
        }
    }

    /// Returns the route publication generation used for this lookup.
    #[must_use]
    #[allow(dead_code, reason = "carried now so later route coordinates do not change the lookup contract")]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// One kind's bookkeeping, keyed in the registry on the hashed id.
#[derive(Clone)]
struct KindSlot {
    name: String,
    descriptor: KindDescriptor,
}

#[derive(Clone, Default)]
struct KindTable {
    kinds: FxHashMap<KindId, KindSlot>,
    name_index: HashMap<String, KindId>,
}

struct Inner {
    /// Sparse, keyed on the deterministic `MailboxId` (ADR-0029).
    /// Registration inserts; `drop_mailbox` transitions the entry to
    /// `Dropped` so the id stays addressable until re-registered.
    mailboxes: FxHashMap<MailboxId, RouteRecord>,
    /// Sparse, keyed on the `kind_id_from_parts(name, schema)` hash
    /// (ADR-0030 Phase 2). Every descriptor registered with a given
    /// (name, schema) maps to the same id everywhere it's ever
    /// computed — derive-emitted `K::ID`, hub re-derived from
    /// `KindDescriptor`, substrate boot from `descriptors::all()`.
    kinds: FxHashMap<KindId, KindSlot>,
    /// O(1) name → id reverse lookup. Kept as a parallel map rather
    /// than scanning `kinds` because the dispatch path (`reply_mail` kind
    /// validation, `hub_client` inbound-mail name→id) runs on every mail.
    /// Every insert into `kinds` mirrors into `name_index`; every slot
    /// has exactly one entry here.
    name_index: HashMap<String, KindId>,
    route_publisher: DoubleBuffer<MailboxId, RouteRecord>,
    kind_publisher: ViewPublisher<KindTable>,
}

impl Inner {
    fn publish_route(&mut self, id: MailboxId, route: RouteRecord) {
        if self.route_publisher.publish([Update::Insert(id, route)]).is_err() {
            tracing::error!("route publication generation exhausted; registry cannot remain coherent");
            abort();
        }
    }

    fn publish_route_removal(&mut self, id: MailboxId) {
        if self.route_publisher.publish([Update::Remove(id)]).is_err() {
            tracing::error!("route publication generation exhausted; registry cannot remain coherent");
            abort();
        }
    }

    fn publish_kinds(&mut self) {
        if self
            .kind_publisher
            .publish(KindTable { kinds: self.kinds.clone(), name_index: self.name_index.clone() })
            .is_err()
        {
            tracing::error!("kind publication generation exhausted; registry cannot remain coherent");
            abort();
        }
    }
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        let route_publisher = DoubleBuffer::default();
        let routes = route_publisher.view();
        let kind_publisher = ViewPublisher::new(KindTable::default());
        let kinds = kind_publisher.view();
        Self {
            inner: RwLock::new(Inner {
                mailboxes: FxHashMap::default(),
                kinds: FxHashMap::default(),
                name_index: HashMap::default(),
                route_publisher,
                kind_publisher,
            }),
            routes,
            kinds,
            addresses: AddressIndex::from_inventory(),
            on_mailbox_change: RwLock::new(None),
        }
    }

    /// Issue iamacoffeepot/aether#742: install the post-registration
    /// hook. The chassis calls this once during boot — typically
    /// inside `connect_hub_client` — to wire up automatic
    /// `MailboxesChanged` republishing for any subsequent registration
    /// (chassis-builder `.with_actor::<...>` chain, runtime
    /// `load_component`, etc.). Subsequent calls overwrite the
    /// previous hook.
    ///
    /// # Panics
    /// Panics if the `on_mailbox_change` `RwLock` is poisoned —
    /// fail-fast per ADR-0063: a poisoned lock means a prior holder
    /// panicked under the guard.
    pub fn set_on_mailbox_change(&self, hook: MailboxChangeHook) {
        *self.on_mailbox_change.write().expect("on_mailbox_change lock poisoned; fail-fast per ADR-0063") = Some(hook);
    }

    /// Snapshot the published inventory and invoke the hook (if installed).
    /// Called from every successful `register_inbox` /
    /// `try_register_inbox`. Successful registration publishes before
    /// this method runs, so the hook sees at least that registration.
    fn notify_mailbox_change(&self) {
        let hook =
            self.on_mailbox_change.read().expect("on_mailbox_change lock poisoned; fail-fast per ADR-0063").clone();
        if let Some(hook) = hook {
            hook(self.list_mailbox_descriptors());
        }
    }

    /// Insert a mailbox, allocating its id from the name hash (ADR-0029).
    /// On a `Dropped` entry at the same id (same name re-registered
    /// after a drop), the entry transitions back to live. Any other
    /// occupied entry is a collision.
    fn insert(&self, name: String, entry: MailboxEntry) -> Result<MailboxId, NameConflict> {
        // Depth-1 / root registrations derive the id from the name
        // (ADR-0029) — the lineage fold's fixed point.
        self.insert_with_id(MailboxId::from_name(&name), name, entry)
    }

    /// ADR-0099 §3: register under an explicit, caller-computed `id`
    /// (the lineage fold) with `name` retained as the display /
    /// reverse-map string. `MailboxId = hash(name)` no longer holds for
    /// a hosted / spawned actor — its id is the fold over its lineage of
    /// `ActorId`s — so the spawn path passes the folded id here instead
    /// of letting the name derive it. [`Self::insert`] is the depth-1
    /// case where the two coincide.
    fn insert_with_id(&self, id: MailboxId, name: String, entry: MailboxEntry) -> Result<MailboxId, NameConflict> {
        if id == MailboxId::NONE || id == MailboxId::CHASSIS_MAILBOX_ID {
            // Sentinel collisions are reserved: NONE shadows the
            // "absent/uninit" id (Option<MailboxId> semantics break if
            // a real mailbox claims it), and CHASSIS_MAILBOX_ID is the
            // chassis-router short-circuit target — registering a real
            // handler at that name would silently shadow chassis routing
            // (issue iamacoffeepot/aether#725). Hash collision against
            // either is practically impossible at 64 bits, but the
            // CHASSIS check also blocks the obvious footgun: a caller
            // literally registering "aether.chassis".
            return Err(NameConflict { name });
        }
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let route = RouteRecord { canonical_name: name.clone(), endpoint: RouteEndpoint::from_entry(entry) };
        let result = match inner.mailboxes.get(&id) {
            Some(slot) if matches!(slot.endpoint, RouteEndpoint::Dropped) && slot.canonical_name == name => {
                inner.mailboxes.insert(id, route.clone());
                inner.publish_route(id, route);
                Ok(id)
            }
            Some(_) => Err(NameConflict { name }),
            None => {
                inner.mailboxes.insert(id, route.clone());
                inner.publish_route(id, route);
                Ok(id)
            }
        };
        drop(inner);
        result
    }

    /// Invalidate a live mailbox (ADR-0010). Transitions the entry
    /// to `Dropped` so dispatch-path readers can distinguish an
    /// intentional drop from an unknown id; the id itself (a function
    /// of the name per ADR-0029) stays addressable and a subsequent
    /// `try_register_inbox` / `try_register_inline` with the same
    /// name reuses it. Returns the released name on success.
    ///
    /// Issue 634 Phase 4 retired the dedicated `Component` variant,
    /// so this now drops any live `Inbox` or `Inline` mailbox.
    /// Production has exactly one caller — `WasmTrampoline`'s
    /// shutdown path transitioning its own slot — chassis-cap
    /// mailboxes never route here.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn drop_mailbox(&self, id: MailboxId) -> Result<String, DropError> {
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let Some(slot) = inner.mailboxes.get_mut(&id) else {
            return Err(DropError::UnknownId(id));
        };
        match slot.endpoint {
            RouteEndpoint::Inbox { .. } | RouteEndpoint::Inline(_) => {}
            RouteEndpoint::Dropped => return Err(DropError::AlreadyDropped(id)),
        }
        slot.endpoint = RouteEndpoint::Dropped;
        let name = slot.canonical_name.clone();
        let route = slot.clone();
        inner.publish_route(id, route);
        drop(inner);
        Ok(name)
    }

    /// Register a mailbox whose handler body forwards the envelope
    /// into an actor's mpsc inbox. The actor's dispatch loop on its
    /// own thread runs the work and records the lifecycle
    /// `Received`/`Finished` bracket — `Mailer::push` does NOT
    /// bracket this arm. Use this for any registration where a
    /// dispatch loop downstream owns the per-handler invocation
    /// (chassis caps via `claim_mailbox*`, instanced + singleton
    /// actors via the spawner).
    ///
    /// **Wrong-variant symptom.** Picking [`Self::register_inbox`]
    /// for a synchronous handler — one that does immediate work on
    /// the pushing thread rather than enqueueing onto a downstream
    /// mpsc — leaks `in_flight` forever, because nothing downstream
    /// ever fires the `Finished` half of the bracket. Settlement
    /// subscribers on the parent chain hang. iamacoffeepot/aether#846
    /// is the canonical incident: `tick_fanout_propagates_chassis_root_lineage`
    /// used `register_inbox` for a `captured.push(...)` closure
    /// (synchronous Vec append, no downstream dispatcher), and once
    /// strict settlement propagation landed in
    /// `SubstrateHarness::run_frame` the test surfaced as a 5s
    /// `SettlementTimeout`. Fix: switch to [`Self::register_inline`].
    ///
    /// The dispatch-type asymmetry helps catch this — Inbox
    /// handlers receive [`OwnedDispatch`](crate::mail::registry::OwnedDispatch)
    /// so moving `payload` into a
    /// channel is natural; a synchronous body that doesn't move
    /// payload reads as "I should be Inline."
    ///
    /// # Panics
    /// Panics on a name collision (or if the inner `RwLock` is
    /// poisoned) — fail-fast per ADR-0063: substrate-internal
    /// registrations should never collide; use
    /// [`Self::try_register_inbox`] when a collision is a recoverable
    /// outcome rather than a bug.
    pub fn register_inbox(&self, name: impl Into<String>, handler: Arc<dyn InboxHandler>) -> MailboxId {
        match self.insert(name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() }) {
            Ok(id) => {
                self.notify_mailbox_change();
                id
            }
            Err(NameConflict { name }) => {
                panic!("mailbox name already registered: {name}")
            }
        }
    }

    /// Non-panicking variant of [`Self::register_inbox`]. Returns
    /// `NameConflict` on a collision so callers that legitimately
    /// race (ADR-0070 capability boots, where the side-by-side
    /// extraction period puts legacy registrations and a new
    /// capability claim against the same mailbox during the
    /// transition diff) can surface the collision as a typed error
    /// rather than aborting the chassis.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn try_register_inbox(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        let result = self.insert(name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() });
        if result.is_ok() {
            self.notify_mailbox_change();
        }
        result
    }

    /// ADR-0099 §3: [`Self::try_register_inbox`] but under an explicit,
    /// caller-computed `id` (the lineage fold) rather than `hash(name)`.
    /// The spawn path uses this for hosted / nested actors, whose id is
    /// the fold over their lineage; `name` stays the rendered display /
    /// reverse-map string. The returned id is `id` on success.
    pub fn try_register_inbox_with_id(
        &self,
        id: MailboxId,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        let result = self.insert_with_id(id, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() });
        if result.is_ok() {
            self.notify_mailbox_change();
        }
        result
    }

    /// Issue 838: register a mailbox whose handler runs inline on
    /// the pushing thread. `Mailer::push` brackets the call with
    /// `Received`/`Finished` so the chain's `in_flight` balances
    /// and settlement subscribers
    /// ([`crate::chassis::settlement::SettlementRegistry`]) wake
    /// (ADR-0080 §2).
    ///
    /// **Wrong-variant symptom.** Picking [`Self::register_inline`]
    /// for an actor-enqueue handler — one whose body forwards onto
    /// a downstream mpsc that another thread drains — double-counts
    /// `Finished`. The mailer fires the bracket around the enqueue,
    /// then the downstream dispatcher fires its own bracket when
    /// the envelope is picked up. Settlement subscribers wake on
    /// the first `Finished` — before the actual work runs — so
    /// callers proceed past the gate while the dispatcher is still
    /// processing the mail. Fix: switch to [`Self::register_inbox`].
    ///
    /// The dispatch-type asymmetry helps catch this — Inline
    /// handlers receive borrowed
    /// [`MailDispatch<'_>`](crate::mail::registry::MailDispatch) whose
    /// `payload: &[u8]` can't be moved into an mpsc without a
    /// `to_vec()` clone; that clone is the visible "I should be
    /// Inbox" smell.
    ///
    /// # Panics
    /// Panics on a name collision (or if the inner `RwLock` is
    /// poisoned) — fail-fast per ADR-0063: substrate-internal
    /// registrations should never collide; use
    /// [`Self::try_register_inline`] when a collision is a recoverable
    /// outcome rather than a bug.
    pub fn register_inline(&self, name: impl Into<String>, handler: Arc<dyn InlineHandler>) -> MailboxId {
        match self.insert(name.into(), MailboxEntry::Inline(handler)) {
            Ok(id) => {
                self.notify_mailbox_change();
                id
            }
            Err(NameConflict { name }) => {
                panic!("mailbox name already registered: {name}")
            }
        }
    }

    /// Non-panicking variant of [`Self::register_inline`], symmetric
    /// with [`Self::try_register_inbox`].
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn try_register_inline(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InlineHandler>,
    ) -> Result<MailboxId, NameConflict> {
        let result = self.insert(name.into(), MailboxEntry::Inline(handler));
        if result.is_ok() {
            self.notify_mailbox_change();
        }
        result
    }

    /// Issue 607 Phase 7: fully remove a registered mailbox. Used in
    /// the chassis-boot unwind path when a singleton's `init` fails
    /// after `try_register_inbox` claimed the slot — the partial-
    /// boot state must not leak into a later cap's namespace lookup.
    /// Returns `true` if the entry existed and was a live (`Inbox`
    /// or `Inline`) variant and was removed; `false` if the id is
    /// unknown or already in `Dropped` state. Component entries go
    /// through [`Self::drop_mailbox`] (which transitions to
    /// `Dropped` rather than removing) — the lifecycle difference
    /// is intentional: components can re-register the same id after
    /// a drop, chassis-bound mailboxes are torn down on cap
    /// teardown and the id can be freshly recreated.
    pub(crate) fn remove_closure(&self, id: MailboxId) -> bool {
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        match inner.mailboxes.get(&id) {
            Some(slot) if matches!(slot.endpoint, RouteEndpoint::Inbox { .. } | RouteEndpoint::Inline(_)) => {
                inner.mailboxes.remove(&id);
                inner.publish_route_removal(id);
                true
            }
            _ => false,
        }
    }

    /// Does a live (non-`Dropped`) mailbox exist under `name`? Returns
    /// its id if so. The id itself is deterministic (ADR-0029) —
    /// callers that just want the id without a liveness check can use
    /// `MailboxId::from_name` directly.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn lookup(&self, name: &str) -> Option<MailboxId> {
        match self.resolve_address(name) {
            Ok(resolved) => Some(resolved.mailbox_id),
            Err(error @ (AddressResolutionError::PathTooDeep { .. } | AddressResolutionError::PathTooLong { .. })) => {
                tracing::warn!(name, ?error, "scope path over cap; resolution miss");
                None
            }
            Err(_) => None,
        }
    }

    /// Resolve a canonical or ADR-0166 abbreviated actor address to one
    /// live mailbox. Canonical inputs preserve the existing
    /// validate/fold/exact-name lookup. An abbreviated input expands
    /// through the generated root/child inventory before that canonical
    /// lookup, so aliases are never hashed, stored, or reverse-reported.
    pub fn resolve_address(&self, address: &str) -> Result<ResolvedAddress, AddressResolutionError> {
        let canonical_path = match address.split_once("://") {
            None => address.to_owned(),
            Some((root, relative)) => self.addresses.as_ref().map_err(Clone::clone)?.expand(root, relative)?,
        };
        let mailbox_id = self
            .lookup_canonical(&canonical_path)?
            .ok_or_else(|| AddressResolutionError::NoLiveMailbox { canonical_path: canonical_path.clone() })?;
        Ok(ResolvedAddress { mailbox_id, canonical_path })
    }

    fn lookup_canonical(&self, name: &str) -> Result<Option<MailboxId>, ScopePathError> {
        // ADR-0098 wire boundary: `name` is user-controlled (the MCP
        // `recipient_name` surface resolves here), so cap its scope depth
        // / byte size before it folds to a registry key. An over-cap name
        // is a resolution miss, not a key-space bloat.
        let segments: Vec<&str> = name.split('/').collect();
        validate_scope_path(&segments)?;
        // ADR-0099 §4: resolve a written name by the parse → fold (the
        // inverse of the `/`-render), not `hash(name)` — a hosted /
        // nested actor's id is the lineage fold, so the whole-string hash
        // would miss it. The depth-1 case (every root cap) folds to the
        // same id `hash(name)` gives.
        #[allow(clippy::disallowed_methods)]
        // the runtime-name resolution path itself — the registry is the one owner of the parse → fold
        let id = mailbox_id_from_path(name);
        let routes = self.routes.load();
        Ok(match routes.entry_for(&id) {
            Some(route) if route.canonical_name == name && !matches!(route.endpoint, RouteEndpoint::Dropped) => {
                Some(id)
            }
            _ => None,
        })
    }

    /// Fetch the entry for a mailbox id from a point-in-time view.
    /// Returns an owned compatibility projection of the private route.
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.routes.load().entry_for(&id).map(|route| route.endpoint.as_entry())
    }

    /// Install a `Pooled` actor's [`SeizeHandle`]
    /// onto its `Inbox` entry's deferred [`SeizeCell`] so the blob
    /// demuxer can resolve recipient → slot and dispatch in place
    /// (ADR-0087 §4, iamacoffeepot/aether#1135). Called by the
    /// `Pooled`-branch wiring in `chassis/builder.rs` +
    /// `actor/native/spawn.rs` once the dispatcher slot exists. Returns
    /// `true` on a successful install; `false` if the id isn't a live
    /// `Inbox` entry or the cell was already populated (idempotent — one
    /// install per slot in production).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn install_seize_handle(&self, id: MailboxId, handle: SeizeHandle) -> bool {
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let Some(route) = inner.mailboxes.get(&id) else {
            return false;
        };
        let RouteEndpoint::Inbox { handler, seize } = &route.endpoint else {
            return false;
        };
        if seize.get().is_some() {
            return false;
        }

        let replacement = SeizeCell::default();
        assert!(replacement.set(handle).is_ok(), "fresh seize cell must accept its first handle");
        let route = RouteRecord {
            canonical_name: route.canonical_name.clone(),
            endpoint: RouteEndpoint::Inbox { handler: Arc::clone(handler), seize: replacement },
        };
        inner.mailboxes.insert(id, route.clone());
        inner.publish_route(id, route);
        true
    }

    /// Hot-path combined lookup for the mailer's route step.
    ///
    /// Route and kind snapshots are loaded independently. This is coherent
    /// because kind definitions are immutable after their first successful
    /// registration: a later kind publication cannot change an existing id.
    pub(crate) fn route_lookup(&self, kind: KindId, recipient: MailboxId) -> RouteLookup {
        let routes = self.routes.load();
        let kinds = self.kinds.load();
        RouteLookup {
            endpoint: routes.entry_for(&recipient).map(|route| route.endpoint.clone()),
            kind_name: kinds.table().kinds.get(&kind).map(|slot| slot.name.clone()).unwrap_or_default(),
            generation: routes.generation(),
        }
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is unknown. Used by the closure dispatch path to stamp
    /// `origin` on observation mail (ADR-0011).
    pub fn mailbox_name(&self, id: MailboxId) -> Option<String> {
        self.routes.load().entry_for(&id).map(|route| route.canonical_name.clone())
    }

    /// Register a mail kind by name, defaulting the schema to `Bytes`
    /// (raw byte payload, no agent-encodable structure). The id is
    /// derived from `(name, SchemaType::Bytes)` — so the name-only path
    /// only collides with a `register_kind_with_descriptor` call that
    /// also uses the `Bytes` schema. Mostly a convenience for tests and
    /// substrate-internal registrations that don't need the hub to
    /// encode params; production init should prefer
    /// `register_kind_with_descriptor` so the descriptor stored here
    /// matches the type definition and the derived id agrees with
    /// `<K as Kind>::ID` on the guest side.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard. The internal `expect("Bytes default cannot produce a
    /// conflict")` is unreachable by construction.
    pub fn register_kind(&self, name: impl Into<String>) -> KindId {
        let descriptor = KindDescriptor { name: name.into(), schema: SchemaType::Bytes };
        // A fresh `Bytes` descriptor can only conflict with a prior
        // `Bytes` registration under the same name — in which case the
        // schemas match and the call is idempotent. Not reachable.
        self.register_kind_internal(descriptor, /*reject_conflict=*/ false)
            .expect("Bytes default cannot produce a conflict")
    }

    /// Register a mail kind along with the descriptor the hub will
    /// use to encode agent-supplied params (ADR-0007). Per ADR-0030
    /// Phase 2:
    ///
    /// - Fresh `(name, schema)` hash → insert, return the id.
    /// - Existing id with identical descriptor → return the id
    ///   (idempotent — same kind registered twice, e.g. boot + load).
    /// - Existing id with a different descriptor → `KindConflict`. At
    ///   64-bit hash width this is only reachable via a genuine hash
    ///   collision between two distinct kinds; loud failure rather
    ///   than silent data corruption.
    ///
    /// Used by substrate boot (`descriptors::all()`) and `load_component`.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn register_kind_with_descriptor(&self, descriptor: KindDescriptor) -> Result<KindId, KindConflict> {
        self.register_kind_internal(descriptor, /*reject_conflict=*/ true)
    }

    fn register_kind_internal(
        &self,
        descriptor: KindDescriptor,
        reject_conflict: bool,
    ) -> Result<KindId, KindConflict> {
        let id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        if let Some(slot) = inner.kinds.get(&id) {
            if reject_conflict
                && canonical_kind_bytes(&slot.descriptor.name, &slot.descriptor.schema)
                    != canonical_kind_bytes(&descriptor.name, &descriptor.schema)
            {
                // Same 64-bit id but distinct canonical bytes — a real
                // hash collision, keep the loud failure. Comparing
                // canonical bytes (not `SchemaType` PartialEq) means
                // nominal-only differences — named fields vs stripped
                // names from a manifest round-trip — are treated as
                // identical, since the canonical form is exactly the
                // structure the id hashes over.
                return Err(KindConflict {
                    name: descriptor.name,
                    existing: slot.descriptor.schema.clone(),
                    requested: descriptor.schema,
                });
            }
            return Ok(id);
        }
        inner.name_index.insert(descriptor.name.clone(), id);
        inner.kinds.insert(id, KindSlot { name: descriptor.name.clone(), descriptor });
        inner.publish_kinds();
        drop(inner);
        Ok(id)
    }

    /// Look up a kind's id by its canonical name. Under hashed ids the
    /// id is a function of `(name, schema)` — so this only finds a
    /// match if `register_kind_with_descriptor` was called with the
    /// exact descriptor the caller is thinking of. Primarily used by
    /// the hub-inbound dispatch path, which needs to convert an
    /// incoming `kind_name` back to the registered id.
    pub fn kind_id(&self, name: &str) -> Option<KindId> {
        self.kinds.load().table().name_index.get(name).copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// isn't registered. Used by the dispatch path to hand mailbox
    /// closure handlers a kind name without them keeping their own
    /// map.
    pub fn kind_name(&self, kind: KindId) -> Option<String> {
        self.kinds.load().table().kinds.get(&kind).map(|slot| slot.name.clone())
    }

    /// The descriptor stored for a given kind id, or `None` if the id
    /// isn't registered. Returned as an owned clone from a published view.
    pub fn kind_descriptor(&self, kind: KindId) -> Option<KindDescriptor> {
        self.kinds.load().table().kinds.get(&kind).map(|slot| slot.descriptor.clone())
    }

    /// Snapshot of every kind descriptor currently registered. Sorted
    /// by name so the hub sees a deterministic ordering (ids are a
    /// hash of declaration-time data, so sorting on id would scramble
    /// unrelated kinds; name order preserves a human-readable grouping).
    /// Used by the control plane to ship an authoritative view to the
    /// hub after a runtime load or replace (ADR-0010 §4).
    pub fn list_kind_descriptors(&self) -> Vec<KindDescriptor> {
        let kinds = self.kinds.load();
        let mut out: Vec<KindDescriptor> = kinds.table().kinds.values().map(|slot| slot.descriptor.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Snapshot of every mailbox descriptor currently registered, plus
    /// a synthetic entry for the chassis-router sentinel
    /// (`aether.chassis` / [`MailboxId::CHASSIS_MAILBOX_ID`]). Sorted
    /// by name. Used by the hub-client handshake to ship the
    /// authoritative inventory in `Hello.mailboxes`, and by the
    /// component cap to re-ship via `MailboxesChanged` after a load
    /// registers a new trampoline mailbox (issue iamacoffeepot/aether#730).
    ///
    /// `Dropped` entries are included with their last-known name so a
    /// trace tool can still resolve a mailbox that died after the
    /// trace was captured. Categorisation is a pure function of the
    /// mailbox name (`categorise_name`); the registry stores no
    /// per-mailbox category state.
    pub fn list_mailbox_descriptors(&self) -> Vec<MailboxDescriptor> {
        let routes = self.routes.load();
        let mut out: Vec<MailboxDescriptor> = routes
            .entries()
            .map(|(id, route)| MailboxDescriptor {
                id: *id,
                name: route.canonical_name.clone(),
                category: categorise_mailbox_name(&route.canonical_name),
            })
            .collect();
        out.push(MailboxDescriptor {
            id: MailboxId::CHASSIS_MAILBOX_ID,
            name: "aether.chassis".to_owned(),
            category: Some(MailboxCategory::ChassisSentinel),
        });
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Number of registered mailbox entries (live + `Dropped`).
    pub fn len(&self) -> usize {
        self.routes.load().len()
    }

    /// `true` when no mailbox has ever been registered.
    pub fn is_empty(&self) -> bool {
        self.routes.load().is_empty()
    }

    #[cfg(test)]
    pub(super) fn kind_generation(&self) -> u64 {
        self.kinds.load().generation()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
