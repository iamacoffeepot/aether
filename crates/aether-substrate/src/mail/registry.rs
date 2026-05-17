// Name registries. Two tables: mailboxes (MailboxId → name + entry,
// ids derived from name via ADR-0029's stable hash) and kinds (u64
// kind id → name + descriptor, ids derived from (name, schema) via
// ADR-0030 Phase 2's `kind_id_from_parts`). Both id spaces are a pure
// function of declaration-time data — no sequential allocation, no
// registration order dependence. The registry uses interior mutability
// (`RwLock`) so mailboxes and kinds can be added at runtime —
// ADR-0010's runtime component loading mutates both tables after an
// `Arc<Registry>` has already been shared with the scheduler and hub
// client. Reads take a shared lock and are cheap; writes are rare
// (boot + load/replace/drop).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::mail::{KindId, MailId, MailboxId, ReplyTo};

/// Test-only helper that builds a [`MailDispatch`] with empty
/// `origin` / `ReplyTo::NONE` / `MailId::NONE` defaults from the
/// minimum positional args. Used by chassis and capability tests
/// that drive a registered handler synchronously without going
/// through the full `Mail` → `Mailer::push` path.
#[cfg(test)]
pub(crate) fn test_dispatch<'a>(
    kind: KindId,
    kind_name: &'a str,
    payload: &'a [u8],
    count: u32,
) -> MailDispatch<'a> {
    MailDispatch {
        kind,
        kind_name,
        origin: None,
        sender: ReplyTo::NONE,
        payload,
        count,
        mail_id: MailId::NONE,
        root: MailId::NONE,
        parent_mail: None,
    }
}

/// Test-only owned mirror of [`test_dispatch`]. Used by tests that
/// poke an `Inbox` handler directly through
/// [`InboxHandler::enqueue`] — the trait's owned-dispatch contract
/// makes the borrowed [`test_dispatch`] unsuitable. Same defaults
/// (empty origin, `ReplyTo::NONE`, `MailId::NONE`).
///
/// Issue iamacoffeepot/aether#848 PR 2: added alongside the
/// [`OwnedDispatch`] migration so cap-side dispatcher tests stay
/// terse without each rebuilding the full struct literal.
#[cfg(test)]
pub(crate) fn test_owned_dispatch(
    kind: KindId,
    kind_name: &str,
    payload: &[u8],
    count: u32,
) -> OwnedDispatch {
    OwnedDispatch {
        kind,
        kind_name: kind_name.to_owned(),
        origin: None,
        sender: ReplyTo::NONE,
        payload: payload.to_vec(),
        count,
        mail_id: MailId::NONE,
        root: MailId::NONE,
        parent_mail: None,
    }
}

/// No-op [`InboxHandler`] for tests that just need a registered
/// mailbox to route to *somewhere* without observing the mail. The
/// explicit named helper documents intent at the call site.
///
/// Defaults to the Inbox variant because every current caller pairs
/// it with `register_inbox` / `try_register_inbox`. Tests that need
/// the Inline variant (e.g. asserting bracket recording paths)
/// build their own `Arc::new(|_d: MailDispatch<'_>| {}) as
/// Arc<dyn InlineHandler>`.
pub fn noop_handler() -> Arc<dyn InboxHandler> {
    Arc::new(|_dispatch: OwnedDispatch| {})
}

/// Issue iamacoffeepot/aether#848 PR 2 adapter: wrap a legacy
/// `Fn(MailDispatch<'_>)` closure (the pre-PR-2 cap shape) into an
/// `Arc<dyn InboxHandler>` so existing cap closures keep compiling
/// through the staged migration. The adapter re-borrows the inbound
/// [`OwnedDispatch`] back into a [`MailDispatch<'_>`] for the legacy
/// body. Cost-neutral with today's path (the borrow itself is free;
/// the legacy body's `to_vec()` / `to_owned()` clones inside the
/// closure are unchanged), so PR 2 doesn't introduce a perf
/// regression on the cap dispatch hot path.
///
/// Each production cap that's still wrapped here migrates in PR 3
/// to take `Fn(OwnedDispatch)` directly — moving payload + kind_name
/// rather than cloning them, which is where iamacoffeepot/aether#848's
/// documented hot-path win materializes. PR 5 retires this helper
/// once every wrap is gone.
pub fn legacy_inbox_handler<F>(closure: F) -> Arc<dyn InboxHandler>
where
    F: for<'a> Fn(MailDispatch<'a>) + Send + Sync + 'static,
{
    struct LegacyAdapter<F>(F);
    impl<F> InboxHandler for LegacyAdapter<F>
    where
        F: for<'a> Fn(MailDispatch<'a>) + Send + Sync + 'static,
    {
        fn enqueue(&self, dispatch: OwnedDispatch) {
            let borrowed = MailDispatch {
                kind: dispatch.kind,
                kind_name: &dispatch.kind_name,
                origin: dispatch.origin.as_deref(),
                sender: dispatch.sender,
                payload: &dispatch.payload,
                count: dispatch.count,
                mail_id: dispatch.mail_id,
                root: dispatch.root,
                parent_mail: dispatch.parent_mail,
            };
            (self.0)(borrowed);
        }
    }
    Arc::new(LegacyAdapter(closure))
}
use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};
use aether_data::{KindDescriptor, MailboxCategory, MailboxDescriptor, SchemaType};

/// One mail's worth of dispatch metadata handed to a [`MailboxHandler`].
/// Bundled into a single struct (rather than a positional argument
/// list) so the producer-minted ADR-0080 §1 / §5 lineage fields
/// (`mail_id` / `root` / `parent_mail`) ride alongside the existing
/// envelope-style fields without exploding the closure's call shape.
///
/// Handlers that build an [`crate::actor::native::envelope::Envelope`]
/// for an mpsc downstream copy `mail_id` / `root` / `parent_mail`
/// onto it (the dispatcher reads them to populate the per-handler
/// `NativeCtx`'s `in_flight()` accessors). Chassis-bound sinks that
/// consume mail inline can ignore the lineage triple.
#[derive(Copy, Clone, Debug)]
pub struct MailDispatch<'a> {
    /// Kind id (`K::ID`, ADR-0030 schema hash) the producer stamped.
    pub kind: KindId,
    /// Kind's registered name. Resolved by the dispatcher for
    /// diagnostic logging; handlers that only match on `kind` ignore.
    pub kind_name: &'a str,
    /// Sending mailbox's registered name, if the mail came from a
    /// component. `None` for substrate-core pushes with no sending
    /// mailbox (ADR-0011).
    pub origin: Option<&'a str>,
    /// Remote reply target of the mail (ADR-0008 / ADR-0037 /
    /// ADR-0042). Carries the correlation id for reply-routing.
    pub sender: ReplyTo,
    /// Payload bytes (the kind's encoded representation per ADR-0019).
    pub payload: &'a [u8],
    /// Kind-implied item count.
    pub count: u32,
    /// ADR-0080 §1: the producer-minted identity of this mail.
    /// `MailId::NONE` for legacy paths that haven't migrated.
    pub mail_id: MailId,
    /// ADR-0080 §5: the root of this mail's causal chain.
    pub root: MailId,
    /// ADR-0080 §5: the in-flight mail at the sender, or `None` for
    /// chassis-root sends.
    pub parent_mail: Option<MailId>,
}

/// Owned mirror of [`MailDispatch`] handed to [`InboxHandler::enqueue`].
/// Built by the mailer at the `Inbox` arm by moving `mail.payload`
/// and `kind_name` out of the inbound `Mail`, so the receiving
/// closure can forward the bytes onto a downstream mpsc without an
/// intervening `payload.to_vec()` clone. The `MailDispatch<'_>`
/// borrow shape is wrong for actor-enqueue handlers — the borrow
/// can't outlive the synchronous push call, so any handler that
/// wants to enqueue must first clone. `OwnedDispatch` owns its
/// payload + kind_name so it can be moved cross-thread directly.
///
/// Issue iamacoffeepot/aether#848 (Phase 1): introduced alongside
/// the [`InboxHandler`] trait. No call sites consume this in PR 1 —
/// the existing `MailboxHandler` keeps the `MailDispatch<'_>` shape
/// until PR 2 migrates the variant types and the mailer arms.
#[derive(Clone, Debug)]
pub struct OwnedDispatch {
    /// Kind id (`K::ID`, ADR-0030 schema hash) the producer stamped.
    pub kind: KindId,
    /// Kind's registered name. Owned `String` so the handler can move
    /// it into a downstream envelope without cloning.
    pub kind_name: String,
    /// Sending mailbox's registered name, if the mail came from a
    /// component. `None` for substrate-core pushes with no sending
    /// mailbox (ADR-0011).
    pub origin: Option<String>,
    /// Remote reply target of the mail (ADR-0008 / ADR-0037 /
    /// ADR-0042). Carries the correlation id for reply-routing.
    pub sender: ReplyTo,
    /// Payload bytes (the kind's encoded representation per ADR-0019).
    /// Owned `Vec<u8>` — handlers move this into the downstream
    /// envelope rather than cloning the borrowed slice every
    /// dispatch (the perf win called out in iamacoffeepot/aether#848).
    pub payload: Vec<u8>,
    /// Kind-implied item count.
    pub count: u32,
    /// ADR-0080 §1: the producer-minted identity of this mail.
    /// `MailId::NONE` for legacy paths that haven't migrated.
    pub mail_id: MailId,
    /// ADR-0080 §5: the root of this mail's causal chain.
    pub root: MailId,
    /// ADR-0080 §5: the in-flight mail at the sender, or `None` for
    /// chassis-root sends.
    pub parent_mail: Option<MailId>,
}

/// Closure invoked when mail is delivered to a chassis-bound mailbox.
/// Called on the caller's thread (or the platform thread for input
/// fan-out); must be `Send + Sync`. The single [`MailDispatch`]
/// argument bundles the per-mail metadata.
///
/// Issue iamacoffeepot/aether#848 (Phase 1): retained alongside the
/// new [`InboxHandler`] / [`InlineHandler`] traits. PR 2 migrates
/// `MailboxEntry::{Inbox, Inline}` to wrap `Arc<dyn InboxHandler>` /
/// `Arc<dyn InlineHandler>` and PR 5 retires this alias entirely.
pub type MailboxHandler = Arc<dyn for<'a> Fn(MailDispatch<'a>) + Send + Sync + 'static>;

/// Synchronous handler installed under [`MailboxEntry::Inline`]. Runs
/// on the mailer thread inside `Mailer::push`; the mailer brackets
/// the call with `record_received` / `record_finished` so the
/// chain's `in_flight` balances (ADR-0080 §2). The borrowed
/// [`MailDispatch<'_>`] argument is zero-copy — the handler may read
/// `payload` directly without owning it, which is the right shape
/// for "do the work right here and return" bodies. Bodies that need
/// to enqueue the payload across a channel should pick
/// [`InboxHandler`] instead so the bytes move rather than copy.
///
/// Blanket impl below covers any `Fn(MailDispatch<'_>)` closure;
/// hand-rolled `impl InlineHandler for MyType` is also supported
/// for handlers that hold state.
///
/// Issue iamacoffeepot/aether#848 (Phase 1): introduced. No call
/// sites consume this in PR 1 — `MailboxEntry::Inline` still wraps
/// the legacy `MailboxHandler` alias until PR 2.
pub trait InlineHandler: Send + Sync + 'static {
    fn dispatch(&self, dispatch: MailDispatch<'_>);
}

/// Actor-enqueue handler installed under [`MailboxEntry::Inbox`]. The
/// handler is expected to move `dispatch` onto a downstream channel
/// (typically a cap-local mpsc); the downstream consumer — an actor
/// dispatcher or chassis-side recv loop — records
/// `Received`/`Finished` per envelope. **Contract:** every
/// [`OwnedDispatch`] you receive must eventually have `Finished`
/// recorded for its `mail_id` — otherwise the chain's `in_flight`
/// leaks and any settlement subscriber hangs (the failure mode that
/// surfaced in iamacoffeepot/aether#846).
///
/// The owned dispatch type is the structural hint: payload arrives
/// as `Vec<u8>`, so moving it into an mpsc Sender is a single move,
/// not a clone. A handler that does immediate synchronous work
/// against the dispatch wastes the move and double-pays the bracket
/// (the dispatcher downstream finishes the bracket once the
/// enqueued envelope is picked up; running synchronously here means
/// nothing picks it up) — those bodies belong on
/// [`InlineHandler`] instead.
///
/// Blanket impl below covers any `Fn(OwnedDispatch)` closure;
/// hand-rolled `impl InboxHandler for MyType` is supported for caps
/// that want to bundle the channel sender with handler state.
///
/// Issue iamacoffeepot/aether#848 (Phase 1): introduced. No call
/// sites consume this in PR 1; PR 2 wires it through
/// `MailboxEntry::Inbox` and PR 3 migrates production cap call
/// sites onto it.
pub trait InboxHandler: Send + Sync + 'static {
    fn enqueue(&self, dispatch: OwnedDispatch);
}

impl<F> InlineHandler for F
where
    F: for<'a> Fn(MailDispatch<'a>) + Send + Sync + 'static,
{
    #[inline]
    fn dispatch(&self, dispatch: MailDispatch<'_>) {
        self(dispatch)
    }
}

impl<F> InboxHandler for F
where
    F: Fn(OwnedDispatch) + Send + Sync + 'static,
{
    #[inline]
    fn enqueue(&self, dispatch: OwnedDispatch) {
        self(dispatch)
    }
}

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
/// intentionally distinct even though both wrap a [`MailboxHandler`].
/// The variant *names where the handler runs* — `Inbox` defers the
/// work to an actor's dispatch thread, `Inline` runs the work on the
/// pushing thread. That decides who owns the `Received`/`Finished`
/// lifecycle bracket: the downstream dispatch loop for `Inbox`, the
/// mailer itself for `Inline`. See each variant's docs and
/// `Mailer::push`'s `route_mail` for the bracket semantics.
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
    /// separate dispatcher loop.
    ///
    /// Issue iamacoffeepot/aether#848 PR 2: the variant now wraps
    /// `Arc<dyn InboxHandler>` (was `MailboxHandler`). Handler
    /// bodies receive [`OwnedDispatch`] so payload bytes move into
    /// the downstream envelope rather than being cloned via
    /// `to_vec()` — the hot-path perf win documented in iamacoffeepot/aether#848.
    /// Legacy `Fn(MailDispatch<'_>)` cap closures bridge via
    /// [`legacy_inbox_handler`] during the staged migration; PR 3
    /// rewrites each cap to take `OwnedDispatch` directly and the
    /// adapter retires in PR 5.
    Inbox(Arc<dyn InboxHandler>),
    /// The handler body does its work inline on the pushing thread;
    /// there is no actor dispatch loop behind it. `Mailer::push`
    /// brackets this arm with `Received` and `Finished` so the
    /// chain's `in_flight` balances and settlement subscribers
    /// (`SettlementRegistry`) wake (ADR-0080 §2, issue 838).
    /// Installed by [`Registry::register_inline`] /
    /// [`Registry::try_register_inline`]. Distinct from `Inbox` so
    /// the bracket isn't double-counted when the closure was an
    /// actor-enqueue (which would fire settlement prematurely).
    ///
    /// Issue iamacoffeepot/aether#848 PR 2: the variant now wraps
    /// `Arc<dyn InlineHandler>` (was `MailboxHandler`). The handler
    /// body shape — `Fn(MailDispatch<'_>)` — is unchanged; the
    /// blanket impl on the `InlineHandler` trait makes existing
    /// closures coerce into `Arc<dyn InlineHandler>` automatically.
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
    mailboxes: HashMap<MailboxId, Mailbox>,
    /// Sparse, keyed on the `kind_id_from_parts(name, schema)` hash
    /// (ADR-0030 Phase 2). Every descriptor registered with a given
    /// (name, schema) maps to the same id everywhere it's ever
    /// computed — derive-emitted `K::ID`, hub re-derived from
    /// `KindDescriptor`, substrate boot from `descriptors::all()`.
    kinds: HashMap<KindId, KindSlot>,
    /// O(1) name → id reverse lookup. Kept as a parallel map rather
    /// than scanning `kinds` because the dispatch path (reply_mail kind
    /// validation, hub_client inbound-mail name→id) runs on every mail.
    /// Every insert into `kinds` mirrors into `name_index`; every slot
    /// has exactly one entry here.
    name_index: HashMap<String, KindId>,
}

/// Rejected-load error returned when a runtime kind registration
/// names an existing kind but supplies a different descriptor than the
/// one first seen. Per ADR-0010, the load fails rather than silently
/// reinterpreting; agents rename, evolve the existing descriptor, or
/// restart the substrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindConflict {
    pub name: String,
    pub existing: SchemaType,
    pub requested: SchemaType,
}

impl fmt::Display for KindConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "kind {:?} already registered with a different encoding (existing={:?}, requested={:?})",
            self.name, self.existing, self.requested
        )
    }
}

impl std::error::Error for KindConflict {}

/// A runtime mailbox registration lost to name collision. Returned
/// from `try_register_inbox` (ADR-0010) so a runtime caller can
/// reply with an error instead of panicking. The boot path that
/// registers hard-coded mailbox names still uses `register_inbox` /
/// `register_inline` and panics — collisions there are bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameConflict {
    pub name: String,
}

impl fmt::Display for NameConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mailbox name {:?} already registered", self.name)
    }
}

impl std::error::Error for NameConflict {}

/// Reasons `Registry::drop_mailbox` can refuse. Distinct from the
/// post-drop dispatch log, which the scheduler handles independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropError {
    UnknownId(MailboxId),
    AlreadyDropped(MailboxId),
}

impl fmt::Display for DropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DropError::UnknownId(id) => write!(f, "unknown mailbox id {:?}", id),
            DropError::AlreadyDropped(id) => write!(f, "mailbox {:?} already dropped", id),
        }
    }
}

impl std::error::Error for DropError {}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
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
    pub fn set_on_mailbox_change(&self, hook: MailboxChangeHook) {
        *self.on_mailbox_change.write().unwrap() = Some(hook);
    }

    /// Snapshot the inventory and invoke the hook (if installed).
    /// Called from every successful `register_inbox` /
    /// `try_register_inbox`. Snapshot is taken with the inner read
    /// lock — separate from the write lock the registration just
    /// released — so a concurrent registration sees a consistent
    /// (post-this-insert) view rather than a torn one.
    fn notify_mailbox_change(&self) {
        let hook = self.on_mailbox_change.read().unwrap().clone();
        if let Some(hook) = hook {
            hook(self.list_mailbox_descriptors());
        }
    }

    /// Insert a mailbox, allocating its id from the name hash (ADR-0029).
    /// On a `Dropped` entry at the same id (same name re-registered
    /// after a drop), the entry transitions back to live. Any other
    /// occupied entry is a collision.
    fn insert(&self, name: String, entry: MailboxEntry) -> Result<MailboxId, NameConflict> {
        let id = MailboxId::from_name(&name);
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
        let mut inner = self.inner.write().unwrap();
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
    pub fn drop_mailbox(&self, id: MailboxId) -> Result<String, DropError> {
        let mut inner = self.inner.write().unwrap();
        let Some(slot) = inner.mailboxes.get_mut(&id) else {
            return Err(DropError::UnknownId(id));
        };
        match slot.entry {
            MailboxEntry::Inbox(_) | MailboxEntry::Inline(_) => {}
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
    /// Panics on a name collision — these are substrate-internal
    /// names, collisions are bugs.
    pub fn register_inbox(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> MailboxId {
        let name = name.into();
        match self.insert(name.clone(), MailboxEntry::Inbox(handler)) {
            Ok(id) => {
                self.notify_mailbox_change();
                id
            }
            Err(_) => panic!("mailbox name already registered: {name}"),
        }
    }

    /// Non-panicking variant of [`Self::register_inbox`]. Returns
    /// `NameConflict` on a collision so callers that legitimately
    /// race (ADR-0070 capability boots, where the side-by-side
    /// extraction period puts legacy registrations and a new
    /// capability claim against the same mailbox during the
    /// transition diff) can surface the collision as a typed error
    /// rather than aborting the chassis.
    pub fn try_register_inbox(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        let result = self.insert(name.into(), MailboxEntry::Inbox(handler));
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
    /// Distinct from [`Self::register_inbox`] which is for
    /// actor-inbox enqueue closures whose downstream dispatch loop
    /// owns the bracket. Miscategorisation is silent: a synchronous
    /// handler registered as an `Inbox` leaks `in_flight` (chains
    /// never settle); an actor-enqueue closure registered as
    /// `Inline` double-counts `Finished` (settlement fires
    /// prematurely). Panics on a name collision — these are
    /// substrate-internal names, collisions are bugs.
    pub fn register_inline(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InlineHandler>,
    ) -> MailboxId {
        let name = name.into();
        match self.insert(name.clone(), MailboxEntry::Inline(handler)) {
            Ok(id) => {
                self.notify_mailbox_change();
                id
            }
            Err(_) => panic!("mailbox name already registered: {name}"),
        }
    }

    /// Non-panicking variant of [`Self::register_inline`], symmetric
    /// with [`Self::try_register_inbox`].
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
        let mut inner = self.inner.write().unwrap();
        match inner.mailboxes.get(&id) {
            Some(slot)
                if matches!(slot.entry, MailboxEntry::Inbox(_) | MailboxEntry::Inline(_)) =>
            {
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
    pub fn lookup(&self, name: &str) -> Option<MailboxId> {
        let id = MailboxId::from_name(name);
        let inner = self.inner.read().unwrap();
        match inner.mailboxes.get(&id) {
            Some(slot) if slot.name == name && !matches!(slot.entry, MailboxEntry::Dropped) => {
                Some(id)
            }
            _ => None,
        }
    }

    /// Fetch the entry for a mailbox id. Returns an owned clone so the
    /// caller can drop the internal lock before invoking the handler
    /// (whether `Inbox` or `Inline`) — avoids holding the registry
    /// lock across arbitrary user code.
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.inner
            .read()
            .unwrap()
            .mailboxes
            .get(&id)
            .map(|m| m.entry.clone())
    }

    /// Reverse of `lookup`: name for a given mailbox id, or `None` if
    /// the id is unknown. Used by the closure dispatch path to stamp
    /// `origin` on observation mail (ADR-0011).
    pub fn mailbox_name(&self, id: MailboxId) -> Option<String> {
        self.inner
            .read()
            .unwrap()
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
    pub fn register_kind(&self, name: impl Into<String>) -> KindId {
        let name = name.into();
        let descriptor = KindDescriptor {
            name: name.clone(),
            schema: SchemaType::Bytes,
        };
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
    pub fn register_kind_with_descriptor(
        &self,
        descriptor: KindDescriptor,
    ) -> Result<KindId, KindConflict> {
        self.register_kind_internal(descriptor, /*reject_conflict=*/ true)
    }

    fn register_kind_internal(
        &self,
        descriptor: KindDescriptor,
        reject_conflict: bool,
    ) -> Result<KindId, KindConflict> {
        let id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
        let mut inner = self.inner.write().unwrap();
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
        inner.kinds.insert(
            id,
            KindSlot {
                name: descriptor.name.clone(),
                descriptor,
            },
        );
        Ok(id)
    }

    /// Look up a kind's id by its canonical name. Under hashed ids the
    /// id is a function of `(name, schema)` — so this only finds a
    /// match if `register_kind_with_descriptor` was called with the
    /// exact descriptor the caller is thinking of. Primarily used by
    /// the hub-inbound dispatch path, which needs to convert an
    /// incoming `kind_name` back to the registered id.
    pub fn kind_id(&self, name: &str) -> Option<KindId> {
        self.inner.read().unwrap().name_index.get(name).copied()
    }

    /// Reverse of `kind_id`: name for a given id, or `None` if the id
    /// isn't registered. Used by the dispatch path to hand mailbox
    /// closure handlers a kind name without them keeping their own
    /// map.
    pub fn kind_name(&self, kind: KindId) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .kinds
            .get(&kind)
            .map(|s| s.name.clone())
    }

    /// The descriptor stored for a given kind id, or `None` if the id
    /// isn't registered. Returned as an owned clone so callers don't
    /// hold the read lock while inspecting the encoding.
    pub fn kind_descriptor(&self, kind: KindId) -> Option<KindDescriptor> {
        self.inner
            .read()
            .unwrap()
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
    pub fn list_kind_descriptors(&self) -> Vec<KindDescriptor> {
        let mut out: Vec<KindDescriptor> = self
            .inner
            .read()
            .unwrap()
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
    pub fn list_mailbox_descriptors(&self) -> Vec<MailboxDescriptor> {
        let mut out: Vec<MailboxDescriptor> = self
            .inner
            .read()
            .unwrap()
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

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().mailboxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().mailboxes.is_empty()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Categorise a mailbox name for the inventory snapshot (issue 730).
/// Pure function of the name string. The hub uses this categorisation
/// (round-tripped through `MailboxDescriptor.category`) to render
/// type-prefixed labels in trace tool output.
fn categorise_mailbox_name(name: &str) -> Option<MailboxCategory> {
    if name == "aether.chassis" {
        // Reachable via [`MailboxId::CHASSIS_MAILBOX_ID`] short-circuit;
        // never registered with a real handler. The synthetic entry in
        // [`Registry::list_mailbox_descriptors`] uses the same
        // categorisation so re-registration would be redundant.
        Some(MailboxCategory::ChassisSentinel)
    // Literal kept in sync with `aether_capabilities::trampoline::WasmTrampoline::NAMESPACE`
    // (issue 654 made that the single source of truth). Substrate can't
    // import from capabilities (wrong dep direction), so this routing
    // categorisation duplicates the prefix; if it drifts, every
    // loaded-component test fails immediately because the mailbox
    // categorisation no longer matches.
    } else if name.starts_with("aether.component.trampoline:") {
        Some(MailboxCategory::Trampoline)
    } else if name.starts_with("aether.") {
        // Chassis caps and substrate-owned actors live under the
        // `aether.` namespace (post-ADR-0074). Anything else is
        // user-space and falls through to `None`.
        Some(MailboxCategory::Actor)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn register_and_lookup_closure_mailbox() {
        let r = Registry::new();
        let id = r.register_inbox("physics", noop_handler());
        assert_eq!(id, MailboxId::from_name("physics"));
        assert_eq!(r.lookup("physics"), Some(id));
        assert!(matches!(r.entry(id), Some(MailboxEntry::Inbox(_))));
    }

    #[test]
    fn closure_handler_runs_on_call() {
        let r = Registry::new();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let id = r.register_inbox(
            "heartbeat",
            Arc::new(move |dispatch: OwnedDispatch| {
                c2.fetch_add(dispatch.count, Ordering::SeqCst);
            }),
        );
        let Some(MailboxEntry::Inbox(h)) = r.entry(id) else {
            panic!("expected closure entry")
        };
        // Test-side id is irrelevant — the handler ignores it.
        h.enqueue(test_owned_dispatch(KindId(0), "aether.tick", &[], 7));
        h.enqueue(OwnedDispatch {
            kind: KindId(0),
            kind_name: "aether.tick".to_owned(),
            origin: Some("physics".to_owned()),
            sender: ReplyTo::NONE,
            payload: Vec::new(),
            count: 3,
            mail_id: MailId::NONE,
            root: MailId::NONE,
            parent_mail: None,
        });
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn mailbox_ids_are_name_derived() {
        let r = Registry::new();
        let a = r.register_inbox("a", noop_handler());
        let b = r.register_inbox("b", noop_handler());
        let c = r.register_inbox("c", noop_handler());
        assert_eq!(a, MailboxId::from_name("a"));
        assert_eq!(b, MailboxId::from_name("b"));
        assert_eq!(c, MailboxId::from_name("c"));
        // All three distinct names produce distinct ids.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(r.len(), 3);
    }

    #[test]
    #[should_panic(expected = "mailbox name already registered")]
    fn duplicate_name_panics() {
        let r = Registry::new();
        r.register_inbox("x", noop_handler());
        r.register_inbox("x", noop_handler());
    }

    #[test]
    fn lookup_missing_returns_none() {
        let r = Registry::new();
        assert!(r.lookup("nope").is_none());
        assert!(r.entry(MailboxId(42)).is_none());
    }

    #[test]
    fn mailbox_name_reverse_lookup() {
        let r = Registry::new();
        let a = r.register_inbox("physics", noop_handler());
        let b = r.register_inbox("graphics", noop_handler());
        assert_eq!(r.mailbox_name(a).as_deref(), Some("physics"));
        assert_eq!(r.mailbox_name(b).as_deref(), Some("graphics"));
        assert!(r.mailbox_name(MailboxId(999)).is_none());
    }

    #[test]
    fn kind_ids_are_derived_from_name_and_schema() {
        let r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        let c = r.register_kind("hello.npc_health");
        // Ids are the fnv1a hash of canonical (name, schema) bytes —
        // distinct names under the same default schema must produce
        // distinct ids, and matching the expected const derivation
        // pins the hash contract with the derive.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(
            a,
            KindId(kind_id_from_parts("aether.tick", &SchemaType::Bytes))
        );
    }

    #[test]
    fn kind_registration_is_idempotent() {
        let r = Registry::new();
        let first = r.register_kind("aether.tick");
        let second = r.register_kind("aether.tick");
        assert_eq!(first, second);
        // Different name produces a different id — the id is a pure
        // function of the input, not an allocation order.
        assert_ne!(r.register_kind("aether.key"), first);
    }

    #[test]
    fn kind_id_lookup() {
        let r = Registry::new();
        let id = r.register_kind("aether.tick");
        assert_eq!(r.kind_id("aether.tick"), Some(id));
        assert!(r.kind_id("absent").is_none());
    }

    #[test]
    fn kind_name_reverse_lookup() {
        let r = Registry::new();
        let a = r.register_kind("aether.tick");
        let b = r.register_kind("aether.key");
        assert_eq!(r.kind_name(a).as_deref(), Some("aether.tick"));
        assert_eq!(r.kind_name(b).as_deref(), Some("aether.key"));
        assert!(r.kind_name(KindId(999)).is_none());
    }

    fn unit_desc(name: &str) -> KindDescriptor {
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Unit,
        }
    }

    fn cast_struct_desc(name: &str) -> KindDescriptor {
        use aether_data::{NamedField, Primitive};
        KindDescriptor {
            name: name.to_string(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "x".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }]
                .into(),
            },
        }
    }

    #[test]
    fn register_kind_with_descriptor_stores_schema() {
        let r = Registry::new();
        let id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("fresh name");
        let stored = r.kind_descriptor(id).expect("descriptor present");
        assert_eq!(stored.schema, cast_struct_desc("aether.foo").schema);
    }

    #[test]
    fn register_kind_with_descriptor_is_idempotent_on_match() {
        let r = Registry::new();
        let first = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("first");
        let second = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("same schema should succeed");
        assert_eq!(first, second);
    }

    /// The first registration stores the schema with named fields
    /// (e.g. substrate boot via `aether_kinds::descriptors::all()`); a
    /// second registration of the same structural kind with stripped
    /// names (e.g. reconstructed from a component's `aether.kinds`
    /// canonical bytes) must be accepted as idempotent because both
    /// produce the same kind id. This is the path `#[actor]`
    /// consumer-crate retention relies on for cross-crate kinds that
    /// duplicate boot-registered ones.
    #[test]
    fn register_kind_with_descriptor_accepts_nominal_only_differences() {
        use aether_data::{NamedField, Primitive};
        let r = Registry::new();
        let named_id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("first");

        let unnamed = KindDescriptor {
            name: "aether.foo".into(),
            schema: SchemaType::Struct {
                repr_c: true,
                fields: vec![NamedField {
                    name: "".into(),
                    ty: SchemaType::Scalar(Primitive::U32),
                }]
                .into(),
            },
        };
        let unnamed_id = r
            .register_kind_with_descriptor(unnamed)
            .expect("same canonical bytes = same id = idempotent");
        assert_eq!(named_id, unnamed_id);

        // Named version stays in the stored slot — first writer wins.
        let stored = r.kind_descriptor(named_id).expect("still there");
        if let SchemaType::Struct { fields, .. } = &stored.schema {
            assert_eq!(fields[0].name, "x");
        } else {
            panic!("expected struct schema");
        }
    }

    #[test]
    fn register_kind_with_descriptor_distinct_schemas_take_distinct_ids() {
        // Pre-ADR-0030-Phase-2 behavior was: same name + different
        // schema = `KindConflict`. Under hashed ids the id IS the
        // `(name, schema)` pair, so two schemas under the same name
        // land in two separate slots — conflict is only reachable via
        // a genuine hash collision. Document the post-Phase-2 shape
        // and let the conflict path stay exercised via the
        // `_is_idempotent_on_match` test (same-id reentry).
        let r = Registry::new();
        let unit_id = r
            .register_kind_with_descriptor(unit_desc("aether.foo"))
            .expect("first");
        let struct_id = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("second — different schema, no conflict under hashed ids");
        assert_ne!(unit_id, struct_id);
        assert_eq!(r.kind_descriptor(unit_id).unwrap().schema, SchemaType::Unit);
        assert!(matches!(
            r.kind_descriptor(struct_id).unwrap().schema,
            SchemaType::Struct { .. }
        ));
    }

    #[test]
    fn register_kind_defaults_to_bytes() {
        let r = Registry::new();
        let id = r.register_kind("aether.bar");
        let stored = r.kind_descriptor(id).expect("descriptor present");
        assert_eq!(stored.schema, SchemaType::Bytes);
    }

    #[test]
    fn name_only_and_with_descriptor_resolve_to_distinct_ids() {
        // Under hashed ids the id is a function of (name, schema).
        // The same name registered with two different schemas —
        // `Bytes` (via `register_kind`) and a real struct (via
        // `register_kind_with_descriptor`) — produces two *different*
        // ids, each stored under its own slot. `kind_id(name)` returns
        // whichever id was written to `name_index` most recently; this
        // is a test-only hazard and production callers go through
        // `register_kind_with_descriptor` exclusively.
        let r = Registry::new();
        let real = r
            .register_kind_with_descriptor(cast_struct_desc("aether.foo"))
            .expect("real schema");
        let bytes = r.register_kind("aether.foo");
        assert_ne!(real, bytes);
        assert!(matches!(
            r.kind_descriptor(real).unwrap().schema,
            SchemaType::Struct { .. }
        ));
        assert!(matches!(
            r.kind_descriptor(bytes).unwrap().schema,
            SchemaType::Bytes,
        ));
    }

    #[test]
    fn try_register_inbox_is_non_panicking_on_collision() {
        let r = Registry::new();
        let first = r
            .try_register_inbox("loaded", noop_handler())
            .expect("fresh name");
        let err = r
            .try_register_inbox("loaded", noop_handler())
            .expect_err("collision must not panic");
        assert_eq!(err.name, "loaded");
        assert_eq!(r.lookup("loaded"), Some(first));
        // Entries count unchanged after the failed second attempt.
        assert_eq!(r.len(), 1);
    }

    /// Issue iamacoffeepot/aether#725: registering a real handler at the
    /// reserved `"aether.chassis"` name would silently shadow the
    /// chassis-router short-circuit in `Mailer::route_mail` (mail to
    /// `CHASSIS_MAILBOX_ID` never reaches the registry). Reject at the
    /// registration boundary so the routing path stays unambiguous.
    #[test]
    fn try_register_inbox_rejects_reserved_chassis_name() {
        let r = Registry::new();
        let err = r
            .try_register_inbox("aether.chassis", noop_handler())
            .expect_err("reserved name must reject");
        assert_eq!(err.name, "aether.chassis");
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn drop_mailbox_frees_name_and_marks_entry_dropped() {
        let r = Registry::new();
        let id = r.try_register_inbox("loaded", noop_handler()).unwrap();
        let name = r.drop_mailbox(id).expect("drop");
        assert_eq!(name, "loaded");
        assert!(r.lookup("loaded").is_none(), "name should be reusable");
        assert!(
            matches!(r.entry(id), Some(MailboxEntry::Dropped)),
            "entry must mark id as dropped"
        );
        // Under ADR-0029 the id is a function of the name, so a
        // re-register produces the *same* id and flips the entry back
        // to `Component`.
        let reloaded = r.try_register_inbox("loaded", noop_handler()).unwrap();
        assert_eq!(reloaded, id);
        assert_eq!(r.lookup("loaded"), Some(reloaded));
        assert!(matches!(r.entry(reloaded), Some(MailboxEntry::Inbox(_))));
    }

    #[test]
    fn drop_mailbox_rejects_unknown_and_repeat() {
        let r = Registry::new();
        assert!(matches!(
            r.drop_mailbox(MailboxId(999)),
            Err(DropError::UnknownId(_))
        ));
        let c = r.try_register_inbox("x", noop_handler()).unwrap();
        r.drop_mailbox(c).unwrap();
        assert!(matches!(
            r.drop_mailbox(c),
            Err(DropError::AlreadyDropped(_))
        ));
    }

    /// Issue iamacoffeepot/aether#730: `list_mailbox_descriptors`
    /// snapshots the table sorted by name, categorises each entry by
    /// its name prefix, and inserts a synthetic `ChassisSentinel`
    /// entry under `aether.chassis` (which is never a real registry
    /// row — `insert` rejects the reserved name).
    #[test]
    fn list_mailbox_descriptors_snapshots_sorted_with_categories() {
        let r = Registry::new();
        r.register_inbox("aether.input", noop_handler());
        r.register_inbox("aether.component.trampoline:cam", noop_handler());
        r.register_inbox("user_thing", noop_handler());

        let snap = r.list_mailbox_descriptors();
        // Four entries: 3 registered + 1 synthetic chassis sentinel.
        assert_eq!(snap.len(), 4, "got: {snap:#?}");

        // Sorted by name.
        let names: Vec<&str> = snap.iter().map(|d| d.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "snapshot must be sorted by name");

        // Each name maps to the expected category.
        let cat = |n: &str| {
            snap.iter()
                .find(|d| d.name == n)
                .and_then(|d| d.category)
                .unwrap_or_else(|| panic!("missing entry for {n}"))
        };
        assert_eq!(cat("aether.chassis"), MailboxCategory::ChassisSentinel);
        assert_eq!(cat("aether.input"), MailboxCategory::Actor);
        assert_eq!(
            cat("aether.component.trampoline:cam"),
            MailboxCategory::Trampoline
        );
        // User-space names fall outside any of the recognised
        // categories; the hub's downstream renderer treats them as
        // raw tagged ids without a type prefix.
        assert!(
            snap.iter()
                .find(|d| d.name == "user_thing")
                .unwrap()
                .category
                .is_none(),
            "non-aether names categorise as None",
        );

        // The synthetic chassis sentinel uses the canonical id —
        // hub-side resolution of trace senders against this id finds
        // the right name without re-hashing.
        let chassis = snap.iter().find(|d| d.name == "aether.chassis").unwrap();
        assert_eq!(chassis.id, MailboxId::CHASSIS_MAILBOX_ID);
    }

    /// Each registered descriptor's id matches the deterministic hash
    /// of its name (ADR-0029) — same id space the hub already knows.
    #[test]
    fn list_mailbox_descriptors_ids_match_name_hashes() {
        let r = Registry::new();
        let id = r.register_inbox("aether.audio", noop_handler());
        let entry = r
            .list_mailbox_descriptors()
            .into_iter()
            .find(|d| d.name == "aether.audio")
            .expect("audio entry");
        assert_eq!(entry.id, id);
        assert_eq!(entry.id, MailboxId::from_name("aether.audio"));
    }

    /// Issue iamacoffeepot/aether#742: every successful
    /// `register_inbox` fires the installed change hook with the
    /// post-registration inventory snapshot. The chassis wires this
    /// hook to push to the hub via `egress_mailboxes_changed` so any
    /// chassis-builder cap that registers post-Hello shows up in the
    /// hub's inventory cache without an explicit publish.
    #[test]
    fn mailbox_change_hook_fires_on_register_inbox() {
        use std::sync::Mutex;

        let r = Arc::new(Registry::new());
        let snapshots: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let snapshots_for_hook = Arc::clone(&snapshots);
        r.set_on_mailbox_change(Arc::new(move |descriptors| {
            let names: Vec<String> = descriptors.into_iter().map(|d| d.name).collect();
            snapshots_for_hook.lock().unwrap().push(names);
        }));

        r.register_inbox("aether.input", noop_handler());
        r.register_inbox("aether.render", noop_handler());

        let captured = snapshots.lock().unwrap();
        assert_eq!(
            captured.len(),
            2,
            "hook should fire once per successful register_inbox"
        );
        // Each snapshot is the FULL inventory at that moment (matches
        // the wire `MailboxesChanged` semantics — full replace, not
        // delta), so the second snapshot strictly contains the first.
        assert!(captured[0].contains(&"aether.input".to_owned()));
        assert!(captured[1].contains(&"aether.input".to_owned()));
        assert!(captured[1].contains(&"aether.render".to_owned()));
    }

    /// Issue 742: `try_register_inbox` fires the hook on the Ok
    /// branch and stays silent on `NameConflict`.
    #[test]
    fn mailbox_change_hook_fires_on_try_register_inbox_ok_only() {
        use std::sync::Mutex;

        let r = Arc::new(Registry::new());
        let count: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let count_for_hook = Arc::clone(&count);
        r.set_on_mailbox_change(Arc::new(move |_| {
            *count_for_hook.lock().unwrap() += 1;
        }));

        let _ = r
            .try_register_inbox("aether.input", noop_handler())
            .expect("first register OK");
        // Second registration with the same name conflicts.
        let _ = r
            .try_register_inbox("aether.input", noop_handler())
            .expect_err("second register should NameConflict");

        assert_eq!(*count.lock().unwrap(), 1, "hook fires once on Ok only");
    }

    #[test]
    fn registration_through_shared_arc() {
        // Interior mutability means Arc<Registry> can register after
        // it's already been shared — the dispatch path today never
        // exercises this, but PR 2+ will when `load_component` adds
        // mailboxes and kinds from a handler that holds an Arc.
        let r = Arc::new(Registry::new());
        let r2 = Arc::clone(&r);
        let id = r2.register_inbox("late", noop_handler());
        assert_eq!(r.lookup("late"), Some(id));
        let kind_id = r.register_kind("aether.late");
        assert_eq!(
            r.kind_id("aether.late"),
            Some(kind_id),
            "shared Arc registrations are visible through the original handle"
        );
    }

    /// Issue iamacoffeepot/aether#848 Phase 1: a bare
    /// `Fn(MailDispatch<'_>)` closure satisfies `InlineHandler` via
    /// the blanket impl, and dispatching through
    /// `<dyn InlineHandler>::dispatch` invokes the body once per
    /// call. No mailer / registry plumbing is wired through yet —
    /// that lands in PR 2.
    #[test]
    fn inline_handler_blanket_impl_dispatches_closure_body() {
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let handler: Arc<dyn InlineHandler> = Arc::new(move |dispatch: MailDispatch<'_>| {
            c2.fetch_add(dispatch.count, Ordering::SeqCst);
        });
        handler.dispatch(test_dispatch(KindId(0), "aether.tick", &[], 5));
        handler.dispatch(test_dispatch(KindId(0), "aether.tick", &[], 7));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            12,
            "blanket InlineHandler impl should forward each dispatch to the closure body once",
        );
    }

    /// Issue iamacoffeepot/aether#848 Phase 1: a bare
    /// `Fn(OwnedDispatch)` closure satisfies `InboxHandler` via the
    /// blanket impl. The closure body moves the payload into a
    /// captured Vec, demonstrating the ownership transfer the trait
    /// exists to enable — the hot-path "no `to_vec()` clone" win
    /// called out in iamacoffeepot/aether#848.
    #[test]
    fn inbox_handler_blanket_impl_moves_owned_payload() {
        let collected = Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
        let collected_for_handler = Arc::clone(&collected);
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: OwnedDispatch| {
            // Payload moves straight into the captured Vec — no clone
            // or `to_vec()` on a borrowed slice.
            collected_for_handler.lock().unwrap().push(dispatch.payload);
        });

        handler.enqueue(OwnedDispatch {
            kind: KindId(0),
            kind_name: "aether.audio.note_on".to_owned(),
            origin: None,
            sender: ReplyTo::NONE,
            payload: vec![1, 2, 3],
            count: 1,
            mail_id: MailId::NONE,
            root: MailId::NONE,
            parent_mail: None,
        });
        handler.enqueue(OwnedDispatch {
            kind: KindId(0),
            kind_name: "aether.audio.note_on".to_owned(),
            origin: None,
            sender: ReplyTo::NONE,
            payload: vec![4, 5, 6, 7],
            count: 1,
            mail_id: MailId::NONE,
            root: MailId::NONE,
            parent_mail: None,
        });

        let collected = collected.lock().unwrap();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0], vec![1, 2, 3]);
        assert_eq!(collected[1], vec![4, 5, 6, 7]);
    }

    /// Issue iamacoffeepot/aether#848 Phase 1: hand-rolled
    /// `impl InboxHandler for MyStruct` compiles and dispatches
    /// alongside the blanket-impl path. This is the cap-authoring
    /// shape PR 3 will reach for (a struct holding the mpsc Sender);
    /// a regression here means caps can't migrate.
    #[test]
    fn inbox_handler_hand_rolled_impl_dispatches_per_call() {
        use std::sync::mpsc;

        struct ChannelForwarder {
            tx: mpsc::Sender<OwnedDispatch>,
        }
        impl InboxHandler for ChannelForwarder {
            fn enqueue(&self, dispatch: OwnedDispatch) {
                let _ = self.tx.send(dispatch);
            }
        }

        let (tx, rx) = mpsc::channel();
        let handler: Arc<dyn InboxHandler> = Arc::new(ChannelForwarder { tx });
        handler.enqueue(OwnedDispatch {
            kind: KindId(42),
            kind_name: "aether.fs.write".to_owned(),
            origin: Some("aether.fs".to_owned()),
            sender: ReplyTo::NONE,
            payload: vec![0xAB, 0xCD],
            count: 1,
            mail_id: MailId::NONE,
            root: MailId::NONE,
            parent_mail: None,
        });

        let received = rx.try_recv().expect("hand-rolled enqueue should send");
        assert_eq!(received.kind, KindId(42));
        assert_eq!(received.kind_name, "aether.fs.write");
        assert_eq!(received.payload, vec![0xAB, 0xCD]);
        assert!(
            rx.try_recv().is_err(),
            "exactly one enqueue should send exactly one envelope",
        );
    }
}
