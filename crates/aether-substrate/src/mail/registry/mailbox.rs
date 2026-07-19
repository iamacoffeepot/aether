use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use rustc_hash::FxHashMap;

use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};
use aether_data::{
    KindDescriptor, MailboxCategory, MailboxDescriptor, SchemaType, mailbox_id_from_path, validate_scope_path,
};

use crate::mail::registry::errors::{DropError, KindConflict, NameConflict};
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};
use crate::mail::registry::names::categorise_mailbox_name;
use crate::mail::{KindId, MailboxId};
use crate::scheduler::SeizeHandle;

/// Deferred cell holding a `Pooled` actor's
/// [`SeizeHandle`], carried on every
/// [`MailboxEntry::Inbox`] entry (ADR-0087 §4, iamacoffeepot/aether#1135).
///
/// Registration (`register_inbox` / `try_register_inbox`) happens *before*
/// the dispatcher slot exists — the actor isn't built into a
/// `DispatcherSlot` until after `init` / `wire` — so the cell is empty
/// (`None`) at register time and the `Pooled`-branch wiring in
/// `chassis/builder.rs` + `actor/native/spawn.rs` installs the handle
/// once the slot is constructed (mirroring the `MailboxWakeSlot`
/// deferred-population pattern). The same `Arc` is shared between the
/// registry entry and the wiring caller. Closure / `Inline` handlers
/// have no slot to seize, so their cell stays empty forever and the blob
/// demuxer deposits their mail as usual.
pub type SeizeCell = Arc<OnceLock<SeizeHandle>>;

/// What a given mailbox actually is. The registry records this so the
/// scheduler can dispatch appropriately without a per-mail type check.
/// `Clone` so readers can pull the entry out from under the `RwLock`
/// guard without holding it for the duration of the handler call.
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
    /// iamacoffeepot/aether#1135: `seize` is the deferred
    /// `SeizeCell` — populated once the recipient's dispatcher slot
    /// exists so the blob demuxer can resolve recipient → slot and
    /// dispatch in place (ADR-0087 §4). Empty for closure-backed inboxes
    /// (no pool slot behind them).
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

/// One mailbox's bookkeeping. Grouped so a single lookup hits name,
/// entry, and any future per-mailbox fields together.
struct Mailbox {
    name: String,
    entry: MailboxEntry,
}

/// Everything [`Registry::route_lookup`] hands `route_mail` for one
/// mail, resolved under a single read guard.
pub struct RouteLookup {
    pub(crate) entry: Option<MailboxEntry>,
    pub(crate) kind_name: String,
    /// iamacoffeepot/aether#1135: the recipient's
    /// [`SeizeHandle`], resolved under the
    /// same read guard. `Some` only when the recipient is an `Inbox`
    /// entry whose deferred [`SeizeCell`] was populated (a `Pooled`
    /// actor's slot). `None` for closure / `Inline` / `Dropped` / unknown
    /// recipients — the blob demuxer deposits their mail through
    /// `route_mail` instead of dispatching in place.
    pub(crate) seize: Option<SeizeHandle>,
}

/// One kind's bookkeeping, keyed in the registry on the hashed id.
struct KindSlot {
    name: String,
    descriptor: KindDescriptor,
}

#[derive(Default)]
struct Inner {
    /// Sparse, keyed on the deterministic `MailboxId` (ADR-0029).
    /// Registration inserts; `drop_mailbox` transitions the entry to
    /// `Dropped` so the id stays addressable until re-registered.
    mailboxes: FxHashMap<MailboxId, Mailbox>,
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
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self { inner: RwLock::new(Inner::default()), on_mailbox_change: RwLock::new(None) }
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

    /// Snapshot the inventory and invoke the hook (if installed).
    /// Called from every successful `register_inbox` /
    /// `try_register_inbox`. Snapshot is taken with the inner read
    /// lock — separate from the write lock the registration just
    /// released — so a concurrent registration sees a consistent
    /// (post-this-insert) view rather than a torn one.
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
        match inner.mailboxes.get_mut(&id) {
            Some(slot) if matches!(slot.entry, MailboxEntry::Dropped) && slot.name == name => {
                slot.entry = entry;
                Ok(id)
            }
            Some(_) => Err(NameConflict { name }),
            None => {
                inner.mailboxes.insert(id, Mailbox { name, entry });
                Ok(id)
            }
        }
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
        match slot.entry {
            MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_) => {}
            MailboxEntry::Dropped => return Err(DropError::AlreadyDropped(id)),
        }
        slot.entry = MailboxEntry::Dropped;
        Ok(slot.name.clone())
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
    /// `TestBench::run_frame` the test surfaced as a 5s
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
            Some(slot) if matches!(slot.entry, MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => {
                inner.mailboxes.remove(&id);
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
        // ADR-0098 wire boundary: `name` is user-controlled (the MCP
        // `recipient_name` surface resolves here), so cap its scope depth
        // / byte size before it folds to a registry key. An over-cap name
        // is a resolution miss, not a key-space bloat.
        let segments: Vec<&str> = name.split('/').collect();
        if let Err(err) = validate_scope_path(&segments) {
            tracing::warn!(name, ?err, "scope path over cap; resolution miss");
            return None;
        }
        // ADR-0099 §4: resolve a written name by the parse → fold (the
        // inverse of the `/`-render), not `hash(name)` — a hosted /
        // nested actor's id is the lineage fold, so the whole-string hash
        // would miss it. The depth-1 case (every root cap) folds to the
        // same id `hash(name)` gives.
        #[allow(clippy::disallowed_methods)]
        // the runtime-name resolution path itself — the registry is the one owner of the parse → fold
        let id = mailbox_id_from_path(name);
        let inner = self.inner.read().expect("registry lock poisoned; fail-fast per ADR-0063");
        match inner.mailboxes.get(&id) {
            Some(slot) if slot.name == name && !matches!(slot.entry, MailboxEntry::Dropped) => Some(id),
            _ => None,
        }
    }

    /// Fetch the entry for a mailbox id. Returns an owned clone so the
    /// caller can drop the internal lock before invoking the handler
    /// (whether `Inbox` or `Inline`) — avoids holding the registry
    /// lock across arbitrary user code.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .get(&id)
            .map(|m| m.entry.clone())
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
        let inner = self.inner.read().expect("registry lock poisoned; fail-fast per ADR-0063");
        let Some(MailboxEntry::Inbox { seize, .. }) = inner.mailboxes.get(&id).map(|m| &m.entry) else {
            return false;
        };
        seize.set(handle).is_ok()
    }

    /// Hot-path combined lookup for the mailer's route step: resolves
    /// the recipient's [`MailboxEntry`] and the kind's name under a
    /// single read guard, where `route_mail` previously took separate
    /// reads (`entry` + `kind_name`).
    ///
    /// Like [`entry`](Self::entry), everything is cloned out so the
    /// caller drops the lock before touching a handler. The common case
    /// clones only the (cheap) kind name + entry.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn route_lookup(&self, kind: KindId, recipient: MailboxId) -> RouteLookup {
        let inner = self.inner.read().expect("registry lock poisoned; fail-fast per ADR-0063");
        let kind_slot = inner.kinds.get(&kind);
        let kind_name = kind_slot.map(|s| s.name.clone()).unwrap_or_default();
        let entry = inner.mailboxes.get(&recipient).map(|m| m.entry.clone());
        // iamacoffeepot/aether#1135: hand the demuxer the recipient's
        // seize handle under the same guard. Cloned out of the deferred
        // cell — `Some` only when the recipient is a `Pooled` actor whose
        // slot was wired in.
        let seize = entry.as_ref().and_then(|e| match e {
            MailboxEntry::Inbox { seize, .. } => seize.get().cloned(),
            MailboxEntry::Inline(_) | MailboxEntry::Dropped => None,
        });
        RouteLookup { entry, kind_name, seize }
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is unknown. Used by the closure dispatch path to stamp
    /// `origin` on observation mail (ADR-0011).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn mailbox_name(&self, id: MailboxId) -> Option<String> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .get(&id)
            .map(|m| m.name.clone())
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
        Ok(id)
    }

    /// Look up a kind's id by its canonical name. Under hashed ids the
    /// id is a function of `(name, schema)` — so this only finds a
    /// match if `register_kind_with_descriptor` was called with the
    /// exact descriptor the caller is thinking of. Primarily used by
    /// the hub-inbound dispatch path, which needs to convert an
    /// incoming `kind_name` back to the registered id.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn kind_id(&self, name: &str) -> Option<KindId> {
        self.inner.read().expect("registry lock poisoned; fail-fast per ADR-0063").name_index.get(name).copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// isn't registered. Used by the dispatch path to hand mailbox
    /// closure handlers a kind name without them keeping their own
    /// map.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn kind_name(&self, kind: KindId) -> Option<String> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .kinds
            .get(&kind)
            .map(|s| s.name.clone())
    }

    /// The descriptor stored for a given kind id, or `None` if the id
    /// isn't registered. Returned as an owned clone so callers don't
    /// hold the read lock while inspecting the encoding.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn kind_descriptor(&self, kind: KindId) -> Option<KindDescriptor> {
        self.inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .kinds
            .get(&kind)
            .map(|s| s.descriptor.clone())
    }

    /// Snapshot of every kind descriptor currently registered. Sorted
    /// by name so the hub sees a deterministic ordering (ids are a
    /// hash of declaration-time data, so sorting on id would scramble
    /// unrelated kinds; name order preserves a human-readable grouping).
    /// Used by the control plane to ship an authoritative view to the
    /// hub after a runtime load or replace (ADR-0010 §4).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn list_kind_descriptors(&self) -> Vec<KindDescriptor> {
        let mut out: Vec<KindDescriptor> = self
            .inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .kinds
            .values()
            .map(|s| s.descriptor.clone())
            .collect();
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
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn list_mailbox_descriptors(&self) -> Vec<MailboxDescriptor> {
        let mut out: Vec<MailboxDescriptor> = self
            .inner
            .read()
            .expect("registry lock poisoned; fail-fast per ADR-0063")
            .mailboxes
            .iter()
            .map(|(id, m)| MailboxDescriptor {
                id: *id,
                name: m.name.clone(),
                category: categorise_mailbox_name(&m.name),
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
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn len(&self) -> usize {
        self.inner.read().expect("registry lock poisoned; fail-fast per ADR-0063").mailboxes.len()
    }

    /// `true` when no mailbox has ever been registered.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn is_empty(&self) -> bool {
        self.inner.read().expect("registry lock poisoned; fail-fast per ADR-0063").mailboxes.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
