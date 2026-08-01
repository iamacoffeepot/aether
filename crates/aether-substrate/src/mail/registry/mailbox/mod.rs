//! The mailbox registry: what a mailbox entry is, the tables behind it,
//! and the crate-facing surface every routing concern hangs off.
//!
//! Each concern lives in a sibling: [`route`] the record itself,
//! [`resolve`] the lookup walk, [`alias`] the inline-child addresses,
//! [`birth`] the reservation a `Starting` route stands on, [`kinds`] the
//! kind table, [`register`] the public claim surface, [`apply`] and
//! [`staged`] the effect fold, [`commands`] the owner drain, and
//! [`publish`] / [`inventory`] what a write publishes outward.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use rustc_hash::FxHashMap;

use crate::mail::registry::effect::{ChangeSubscriber, RegistryInventory};
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};
use crate::mail::registry::owner::RegistryOwnerHandle;
use crate::mail::registry::{ActorAddressInventoryError, address::AddressIndex};
use crate::mail::view::{DoubleBuffer, View, ViewPublisher};
use crate::mail::{KindId, MailboxId};
use crate::scheduler::SeizeHandle;

use birth::PendingBirth;
use inventory::live_inventory;
use kinds::{KindSlot, KindTable};
use route::RouteRecord;

mod alias;
mod apply;
mod birth;
mod commands;
mod inventory;
mod kinds;
mod publish;
mod register;
mod resolve;
mod route;
mod staged;

pub use birth::{CapturedDisposition, RouteContinuation};
pub use resolve::RouteResolution;
pub use route::RouteEndpoint;

/// Deferred cell holding a `Pooled` actor's
/// [`SeizeHandle`], carried on every
/// inbox route (ADR-0087 §4, iamacoffeepot/aether#1135).
///
/// Registration (`register_inbox` / `try_register_inbox`) happens *before*
/// the dispatcher slot exists — the actor isn't built into a
/// `DispatcherSlot` until after `init` / `wire` — so the cell is empty
/// (`None`) at register time and the `Pooled`-branch wiring in
/// `chassis/builder.rs` + `actor/native/spawn/builder.rs` installs the handle
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
    /// Installed by [`Registry::register_inline`]. Distinct from `Inbox` so
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
    inner: Mutex<Inner>,
    routes: View<FxHashMap<MailboxId, RouteRecord>>,
    kinds: View<KindTable>,
    inventory: View<RegistryInventory>,
    addresses: Result<AddressIndex, ActorAddressInventoryError>,
    subscribers: Mutex<Vec<Weak<ChangeSubscriber>>>,
    owner: OnceLock<RegistryOwnerHandle>,
}

struct Inner {
    /// Sparse, keyed on the deterministic `MailboxId` (ADR-0029).
    /// Registration inserts; `drop_mailbox` transitions the entry to
    /// `Dropped` so the id stays addressable until re-registered.
    mailboxes: FxHashMap<MailboxId, RouteRecord>,
    pending_births: FxHashMap<MailboxId, PendingBirth>,
    next_activation_token: u64,
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
    inventory_publisher: ViewPublisher<RegistryInventory>,
    mailbox_generation: u64,
    kind_generation: u64,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        let route_publisher = DoubleBuffer::default();
        let routes = route_publisher.view();
        let kind_publisher = ViewPublisher::new(KindTable::default());
        let kinds = kind_publisher.view();
        let inventory_publisher = ViewPublisher::new(RegistryInventory {
            mailboxes: live_inventory(&FxHashMap::default()),
            kinds: Vec::new(),
            mailbox_generation: 0,
            kind_generation: 0,
        });
        let inventory = inventory_publisher.view();
        Self {
            inner: Mutex::new(Inner {
                mailboxes: FxHashMap::default(),
                pending_births: FxHashMap::default(),
                next_activation_token: 0,
                kinds: FxHashMap::default(),
                name_index: HashMap::default(),
                route_publisher,
                kind_publisher,
                inventory_publisher,
                mailbox_generation: 0,
                kind_generation: 0,
            }),
            routes,
            kinds,
            inventory,
            addresses: AddressIndex::from_inventory(),
            subscribers: Mutex::new(Vec::new()),
            owner: OnceLock::new(),
        }
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

    #[cfg(test)]
    pub(super) fn route_generation(&self) -> u64 {
        self.routes.load().generation()
    }

    pub(super) fn current_route_generation(&self) -> u64 {
        self.routes.load().generation()
    }

    #[cfg(test)]
    pub(super) fn mailbox_generation(&self) -> u64 {
        self.inventory.load().table().mailbox_generation
    }

    #[cfg(test)]
    pub(crate) fn owner_accepting(&self) -> bool {
        self.owner.get().is_some_and(RegistryOwnerHandle::is_accepting)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
