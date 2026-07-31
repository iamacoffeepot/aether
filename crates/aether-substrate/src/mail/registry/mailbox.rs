// Perf control for iamacoffeepot/aether#4177: this comment is the entire diff,
// so the perf lane compares this commit's binary against a byte-identical one
// built from its own merge-base. Not for merge.
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::process::abort;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use rustc_hash::FxHashMap;

use aether_actor::{HandlesKind, RegistryChanged};
use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};
use aether_data::{
    KindDescriptor, MailboxCategory, MailboxDescriptor, ScopePathError, mailbox_id_from_path, validate_scope_path,
};

use crate::actor::native::dispatch_blocking::DeferredCompletion;
use crate::mail::mailer::Mailer;
use crate::mail::registry::authority::BootAuthority;
use crate::mail::registry::effect::{
    ACTIVATION_BARRIER_KIND, ActivationReservation, ActivationToken, ChangeSubscriber, EffectBatch, PreparedCostCells,
    PreparedMail, PreparedSpawnFailure, RegistryApplied, RegistryBatchCompletionSink, RegistryBatchError,
    RegistryBatchResult, RegistryCompletion, RegistryEffect, RegistryEffectError, RegistryInventory,
    RegistrySubscription, StartingCancellation, barrier_token, bytes_kind, subscriber,
};
use crate::mail::registry::errors::{DropError, KindConflict, NameConflict};
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};
use crate::mail::registry::names::categorise_mailbox_name;
use crate::mail::registry::owner::{BatchEnvelope, OwnerCommand, ParkAdmission, RegistryOwnerHandle};
use crate::mail::registry::{
    ActorAddressInventoryError, AddressResolutionError, RegistryQueueMetrics, ResolvedAddress, address::AddressIndex,
};
use crate::mail::view::{DoubleBuffer, Update, View, ViewPublisher};
use crate::mail::{KindId, Mail, MailId, MailboxId};
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
    inner: RwLock<Inner>,
    routes: View<FxHashMap<MailboxId, RouteRecord>>,
    kinds: View<KindTable>,
    empty_kind_name: Arc<str>,
    inventory: View<RegistryInventory>,
    addresses: Result<AddressIndex, ActorAddressInventoryError>,
    subscribers: Mutex<Vec<Weak<ChangeSubscriber>>>,
    owner: OnceLock<RegistryOwnerHandle>,
}

#[derive(Clone)]
struct RouteRecord {
    canonical_name: String,
    lifecycle: RouteLifecycle,
}

#[derive(Clone)]
enum RouteLifecycle {
    Starting {
        token: ActivationToken,
    },
    Live {
        endpoint: RouteEndpoint,
    },
    /// Logical Wasm inline-child route. Dispatch follows the target's
    /// current lifecycle and endpoint while preserving the alias as the
    /// routed recipient for guest membrane demux.
    Alias {
        target_parent: MailboxId,
    },
    Dropped,
}

#[derive(Clone)]
pub enum RouteEndpoint {
    Inbox { handler: Arc<dyn InboxHandler>, seize: SeizeCell },
    Inline(Arc<dyn InlineHandler>),
}

impl RouteEndpoint {
    fn from_entry(entry: MailboxEntry) -> Self {
        match entry {
            MailboxEntry::Inbox { handler, seize } => Self::Inbox { handler, seize },
            MailboxEntry::Inline(handler) => Self::Inline(handler),
            MailboxEntry::Dropped => unreachable!("Dropped is a lifecycle, not a live route endpoint"),
        }
    }

    fn as_entry(&self) -> MailboxEntry {
        match self {
            Self::Inbox { handler, seize } => {
                MailboxEntry::Inbox { handler: Arc::clone(handler), seize: Arc::clone(seize) }
            }
            Self::Inline(handler) => MailboxEntry::Inline(Arc::clone(handler)),
        }
    }
}

struct PendingBirth {
    id: MailboxId,
    token: ActivationToken,
    parked: VecDeque<Mail>,
    activation: Option<Arc<dyn ActivationReservation>>,
    costs: Option<PreparedCostCells>,
    after_init: Vec<PreparedMail>,
    armed: bool,
    cancel_requested: bool,
}

impl PendingBirth {
    fn placeholder(id: MailboxId, token: ActivationToken) -> Self {
        Self {
            id,
            token,
            parked: VecDeque::new(),
            activation: None,
            costs: None,
            after_init: Vec::new(),
            armed: false,
            cancel_requested: false,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.activation = None;
        self.costs = None;
    }
}

impl Drop for PendingBirth {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(activation) = self.activation.take() {
            activation.reject(PreparedSpawnFailure::ActivationRejected);
        }
        if let Some(costs) = self.costs.take() {
            costs.rollback(self.id, self.token);
        }
    }
}

pub enum CapturedDisposition {
    Live { endpoint: RouteEndpoint, kind_name: Arc<str> },
    Dropped,
    Unknown,
}

pub struct RouteContinuation {
    pub(crate) mail: Mail,
    pub(crate) disposition: CapturedDisposition,
}

enum ResolvedRoute<'a> {
    Starting { target: MailboxId },
    Live { endpoint: &'a RouteEndpoint },
    Dropped,
    Unknown,
}

fn resolve_route<'a, F>(recipient: MailboxId, route_for: F) -> ResolvedRoute<'a>
where
    F: Fn(MailboxId) -> Option<&'a RouteRecord>,
{
    let Some(route) = route_for(recipient) else {
        return ResolvedRoute::Unknown;
    };
    match &route.lifecycle {
        RouteLifecycle::Starting { .. } => ResolvedRoute::Starting { target: recipient },
        RouteLifecycle::Live { endpoint } => ResolvedRoute::Live { endpoint },
        RouteLifecycle::Alias { target_parent } => match route_for(*target_parent).map(|route| &route.lifecycle) {
            Some(RouteLifecycle::Starting { .. }) => ResolvedRoute::Starting { target: *target_parent },
            Some(RouteLifecycle::Live { endpoint }) => ResolvedRoute::Live { endpoint },
            Some(RouteLifecycle::Dropped) => ResolvedRoute::Dropped,
            Some(RouteLifecycle::Alias { .. }) | None => ResolvedRoute::Unknown,
        },
        RouteLifecycle::Dropped => ResolvedRoute::Dropped,
    }
}

/// One point-in-time route and kind-name lookup.
pub struct RouteLookup {
    endpoint: Option<RouteEndpoint>,
    starting: bool,
    dropped: bool,
    kind_name: Arc<str>,
    generation: u64,
}

impl RouteLookup {
    pub(crate) fn is_starting(&self) -> bool {
        self.starting
    }

    pub(crate) fn is_unknown(&self) -> bool {
        self.endpoint.is_none() && !self.starting && !self.dropped
    }

    pub(crate) fn kind_name_shared(&self) -> Arc<str> {
        Arc::clone(&self.kind_name)
    }

    pub(crate) fn seize_handle(&self) -> Option<&SeizeHandle> {
        match &self.endpoint {
            Some(RouteEndpoint::Inbox { seize, .. }) => seize.get(),
            Some(RouteEndpoint::Inline(_)) | None => None,
        }
    }

    pub(crate) fn into_captured(self) -> CapturedDisposition {
        match self.endpoint {
            Some(endpoint) => CapturedDisposition::Live { endpoint, kind_name: self.kind_name },
            None if self.dropped => CapturedDisposition::Dropped,
            None => CapturedDisposition::Unknown,
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
    name: Arc<str>,
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
    empty_kind_name: Arc<str>,
    route_publisher: DoubleBuffer<MailboxId, RouteRecord>,
    kind_publisher: ViewPublisher<KindTable>,
    inventory_publisher: ViewPublisher<RegistryInventory>,
    mailbox_generation: u64,
    kind_generation: u64,
}

impl Inner {
    fn publish(&mut self, publication: Publication) -> bool {
        let inventory_dirty = publication.inventory_dirty;
        let kinds_dirty = publication.kinds_dirty;
        if !publication.route_updates.is_empty() && self.route_publisher.publish(publication.route_updates).is_err() {
            tracing::error!("route publication generation exhausted; registry cannot remain coherent");
            abort();
        }
        if kinds_dirty
            && self
                .kind_publisher
                .publish(KindTable { kinds: self.kinds.clone(), name_index: self.name_index.clone() })
                .is_err()
        {
            tracing::error!("kind publication generation exhausted; registry cannot remain coherent");
            abort();
        }
        if inventory_dirty || kinds_dirty {
            if inventory_dirty {
                self.mailbox_generation = self.mailbox_generation.checked_add(1).unwrap_or_else(|| {
                    tracing::error!("mailbox inventory generation exhausted; registry cannot remain coherent");
                    abort();
                });
            }
            if kinds_dirty {
                self.kind_generation = self.kind_generation.checked_add(1).unwrap_or_else(|| {
                    tracing::error!("kind inventory generation exhausted; registry cannot remain coherent");
                    abort();
                });
            }
            if self
                .inventory_publisher
                .publish(RegistryInventory {
                    mailboxes: live_inventory(&self.mailboxes),
                    kinds: kind_inventory(&self.kinds),
                    mailbox_generation: self.mailbox_generation,
                    kind_generation: self.kind_generation,
                })
                .is_err()
            {
                tracing::error!(
                    "combined registry inventory publication generation exhausted; registry cannot remain coherent"
                );
                abort();
            }
        }
        inventory_dirty || kinds_dirty
    }
}

#[derive(Default)]
struct Publication {
    route_updates: Vec<Update<MailboxId, RouteRecord>>,
    kinds_dirty: bool,
    inventory_dirty: bool,
}

impl Publication {
    fn append(&mut self, mut other: Self) {
        self.route_updates.append(&mut other.route_updates);
        self.kinds_dirty |= other.kinds_dirty;
        self.inventory_dirty |= other.inventory_dirty;
    }
}

fn live_inventory(mailboxes: &FxHashMap<MailboxId, RouteRecord>) -> Vec<MailboxDescriptor> {
    let mut inventory = mailboxes
        .iter()
        .filter(|(_, route)| match &route.lifecycle {
            RouteLifecycle::Live { .. } => true,
            RouteLifecycle::Alias { target_parent } => mailboxes
                .get(target_parent)
                .is_some_and(|target| matches!(&target.lifecycle, RouteLifecycle::Live { .. })),
            RouteLifecycle::Starting { .. } | RouteLifecycle::Dropped => false,
        })
        .map(|(id, route)| MailboxDescriptor {
            id: *id,
            name: route.canonical_name.clone(),
            category: categorise_mailbox_name(&route.canonical_name),
        })
        .collect::<Vec<_>>();
    inventory.push(MailboxDescriptor {
        id: MailboxId::CHASSIS_MAILBOX_ID,
        name: "aether.chassis".to_owned(),
        category: Some(MailboxCategory::ChassisSentinel),
    });
    inventory.sort_by(|left, right| left.name.cmp(&right.name));
    inventory
}

fn kind_inventory(kinds: &FxHashMap<KindId, KindSlot>) -> Vec<KindDescriptor> {
    let mut descriptors = kinds.values().map(|slot| slot.descriptor.clone()).collect::<Vec<_>>();
    descriptors.sort_by(|left, right| left.name.cmp(&right.name));
    descriptors
}

fn staged_route<'a>(
    staged: &'a FxHashMap<MailboxId, Option<RouteRecord>>,
    inner: &'a Inner,
    id: MailboxId,
) -> Option<&'a RouteRecord> {
    staged.get(&id).map_or_else(|| inner.mailboxes.get(&id), |route| route.as_ref())
}

fn staged_kind<'a>(staged: &'a FxHashMap<KindId, KindSlot>, inner: &'a Inner, id: KindId) -> Option<&'a KindSlot> {
    staged.get(&id).or_else(|| inner.kinds.get(&id))
}

fn commit_staged(
    inner: &mut Inner,
    routes: FxHashMap<MailboxId, Option<RouteRecord>>,
    kinds: FxHashMap<KindId, KindSlot>,
    pending: FxHashMap<MailboxId, Option<ActivationToken>>,
) -> Vec<RouteContinuation> {
    let mut continuations = Vec::new();
    for (id, route) in routes {
        if let Some(route) = route {
            inner.mailboxes.insert(id, route);
        } else {
            inner.mailboxes.remove(&id);
        }
    }
    for (id, slot) in kinds {
        inner.name_index.insert(slot.descriptor.name.clone(), id);
        inner.kinds.insert(id, slot);
    }
    for (id, token) in pending {
        let unchanged =
            token.is_some_and(|token| inner.pending_births.get(&id).is_some_and(|birth| birth.token == token));
        if unchanged {
            continue;
        }
        if let Some(mut birth) = inner.pending_births.remove(&id) {
            continuations.extend(
                birth
                    .parked
                    .drain(..)
                    .map(|mail| RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
            );
        }
        if let Some(token) = token {
            inner.pending_births.insert(id, PendingBirth::placeholder(id, token));
        }
    }
    continuations
}

fn staged_pending_token(
    staged: &FxHashMap<MailboxId, Option<ActivationToken>>,
    inner: &Inner,
    id: MailboxId,
) -> Option<ActivationToken> {
    staged.get(&id).copied().unwrap_or_else(|| inner.pending_births.get(&id).map(|birth| birth.token))
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        let empty_kind_name: Arc<str> = Arc::from("");
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
            inner: RwLock::new(Inner {
                mailboxes: FxHashMap::default(),
                pending_births: FxHashMap::default(),
                next_activation_token: 0,
                kinds: FxHashMap::default(),
                name_index: HashMap::default(),
                empty_kind_name: Arc::clone(&empty_kind_name),
                route_publisher,
                kind_publisher,
                inventory_publisher,
                mailbox_generation: 0,
                kind_generation: 0,
            }),
            routes,
            kinds,
            empty_kind_name,
            inventory,
            addresses: AddressIndex::from_inventory(),
            subscribers: Mutex::new(Vec::new()),
            owner: OnceLock::new(),
        }
    }

    pub(super) fn install_owner(&self, owner: RegistryOwnerHandle) {
        assert!(self.owner.set(owner).is_ok(), "a registry can attach only one owner");
    }

    pub(crate) fn submit(&self, batch: EffectBatch) -> Option<RegistryCompletion<Vec<RegistryApplied>>> {
        let Some(owner) = self.owner.get() else {
            drop(batch.discard_prepared());
            return None;
        };
        owner.submit(batch)
    }

    pub(crate) fn submit_deferred(
        &self,
        batch: EffectBatch,
        completion: DeferredCompletion<RegistryBatchResult>,
    ) -> bool {
        let Some(owner) = self.owner.get() else {
            drop(batch.discard_prepared());
            completion.complete(Err(RegistryBatchError::OwnerClosed));
            return false;
        };
        owner.submit_deferred(batch, completion)
    }

    pub(crate) fn park_or_drop(&self, mail: Mail, observed_generation: u64) -> ParkAdmission {
        match self.owner.get() {
            Some(owner) => owner.park_or_drop(mail, observed_generation),
            None => ParkAdmission::Closed(mail),
        }
    }

    /// The registry owner queue's admission and drain accounting (issue
    /// 4122), or `None` before an owner is attached. ADR-0165 §Consequences
    /// makes owner throughput the input to its own sharding decision; this is
    /// where that measurement is read.
    #[must_use]
    pub fn owner_queue_metrics(&self) -> Option<RegistryQueueMetrics> {
        self.owner.get().map(RegistryOwnerHandle::metrics)
    }

    pub(crate) fn activation_cancelled(&self, id: MailboxId, token: ActivationToken) {
        if let Some(owner) = self.owner.get() {
            let _ = owner.activation_cancelled(id, token);
        }
    }

    #[doc(hidden)]
    pub fn subscribe_inventory<A>(&self, target: MailboxId, mailer: Arc<Mailer>) -> RegistrySubscription
    where
        A: HandlesKind<RegistryChanged>,
    {
        let mut subscribers =
            self.subscribers.lock().expect("registry subscriber lock poisoned; fail-fast per ADR-0063");
        let (subscriber, subscription) = subscriber(target, mailer, self.inventory.clone());
        subscribers.retain(|subscriber| subscriber.strong_count() != 0);
        subscribers.push(Arc::downgrade(&subscriber));
        drop(subscribers);
        subscriber.notify();
        subscription
    }

    #[doc(hidden)]
    #[must_use]
    pub fn inventory(&self) -> RegistryInventory {
        self.inventory.load().table().clone()
    }

    fn notify_inventory_changed(&self) {
        for subscriber in self.inventory_subscribers() {
            subscriber.notify();
        }
    }

    fn relay_inventory_changed(&self) {
        for subscriber in self.inventory_subscribers() {
            subscriber.notify_via_relay();
        }
    }

    fn inventory_subscribers(&self) -> Vec<Arc<ChangeSubscriber>> {
        let mut retained = self.subscribers.lock().expect("registry subscriber lock poisoned; fail-fast per ADR-0063");
        let subscribers = retained.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
        retained.retain(|subscriber| subscriber.strong_count() != 0);
        subscribers
    }

    pub(super) fn apply_owner_commands(&self, commands: Vec<OwnerCommand>, mailer: &Mailer) -> u64 {
        enum AfterLock {
            Batch(RegistryBatchCompletionSink, Result<Vec<RegistryApplied>, RegistryEffectError>),
            Route(RouteContinuation),
            Schedule(Arc<dyn ActivationReservation>),
            CatchUp(Box<dyn FnOnce() + Send>),
        }

        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        let mut after_lock = Vec::new();
        let mut readiness = Vec::new();
        for command in commands {
            match command {
                OwnerCommand::Batch(BatchEnvelope { batch, completion }) => {
                    let result = match Self::apply_batch_locked(&mut inner, batch) {
                        Ok((applied, batch_publication, continuations, schedules)) => {
                            publication.append(batch_publication);
                            after_lock.extend(continuations.into_iter().map(AfterLock::Route));
                            after_lock.extend(schedules.into_iter().map(AfterLock::Schedule));
                            Ok(applied)
                        }
                        Err(error) => Err(error),
                    };
                    after_lock.push(AfterLock::Batch(completion, result));
                }
                OwnerCommand::ParkOrDrop { mail, observed_generation: _ } => {
                    if mail.kind == ACTIVATION_BARRIER_KIND {
                        let token = barrier_token(&mail);
                        mailer.record_finished(mail.mail_id, mail.root);
                        if let Some(token) = token {
                            readiness.push((mail.recipient, token, Some(mail.mail_id)));
                        }
                    } else if let Some(continuation) = Self::capture_mail_locked(&mut inner, mail) {
                        after_lock.push(AfterLock::Route(continuation));
                    }
                }
                OwnerCommand::ActivationCancelled { id, token } => {
                    after_lock.extend(
                        Self::cancel_completed_locked(&mut inner, id, token, &mut publication)
                            .into_iter()
                            .map(AfterLock::Route),
                    );
                }
            }
        }
        for (id, token, barrier_mail_id) in readiness {
            if let Some(catch_up) = Self::promote_locked(&mut inner, id, token, barrier_mail_id, &mut publication) {
                after_lock.push(AfterLock::CatchUp(catch_up));
            }
        }
        let inventory_changed = inner.publish(publication);
        drop(inner);
        if inventory_changed {
            self.relay_inventory_changed();
        }
        for continuation in after_lock {
            match continuation {
                AfterLock::Batch(completion, result) => {
                    completion.complete(result);
                }
                AfterLock::Route(continuation) => mailer.relay_captured(continuation),
                AfterLock::Schedule(activation) => activation.schedule(),
                AfterLock::CatchUp(catch_up) => catch_up(),
            }
        }
        self.routes.load().generation()
    }

    pub(super) fn close_owner_commands(&self, commands: Vec<OwnerCommand>, mailer: &Mailer) -> u64 {
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut completions = Vec::new();
        let mut continuations = Vec::new();
        let mut discarded = Vec::new();
        for command in commands {
            match command {
                OwnerCommand::Batch(envelope) => {
                    discarded.extend(envelope.batch.discard_prepared());
                    completions.push(envelope.completion);
                }
                OwnerCommand::ParkOrDrop { mail, observed_generation: _ } => {
                    if mail.kind == ACTIVATION_BARRIER_KIND {
                        mailer.record_finished(mail.mail_id, mail.root);
                    } else if let Some(continuation) = Self::capture_mail_locked(&mut inner, mail) {
                        continuations.push(continuation);
                    }
                }
                OwnerCommand::ActivationCancelled { .. } => {}
            }
        }
        let mut pending_births = inner.pending_births.drain().collect::<Vec<_>>();
        for (_, birth) in &mut pending_births {
            continuations.extend(
                birth
                    .parked
                    .drain(..)
                    .map(|mail| RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
            );
        }
        drop(inner);

        for (_, birth) in &mut pending_births {
            if let Some(activation) = &birth.activation {
                activation.reject(PreparedSpawnFailure::OwnerClosed);
                activation.join();
            }
            if let Some(costs) = &birth.costs {
                costs.rollback(birth.id, birth.token);
            }
            birth.disarm();
        }
        for done in discarded {
            let _ = done.recv();
        }

        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        for (id, birth) in &pending_births {
            if matches!(
                inner.mailboxes.get(id).map(|route| &route.lifecycle),
                Some(RouteLifecycle::Starting { token }) if *token == birth.token
            ) {
                inner.mailboxes.remove(id);
                publication.route_updates.push(Update::Remove(*id));
            }
        }
        let inventory_changed = inner.publish(publication);
        drop(inner);
        if inventory_changed {
            self.relay_inventory_changed();
        }
        for completion in completions {
            completion.complete(Err(RegistryEffectError::OwnerClosed));
        }
        for continuation in continuations {
            mailer.relay_captured(continuation);
        }
        self.routes.load().generation()
    }

    /// The direct write path itself. Named only by [`Self::apply_one`],
    /// which every eager mutator funnels through, so the
    /// [`BootAuthority`] taken here is the single structural gate on the
    /// pre-owner writer (iamacoffeepot/aether#4161): a caller that cannot
    /// produce the token cannot reach this `inner.write()` at all.
    fn apply_batches(
        &self,
        _authority: &BootAuthority,
        batches: Vec<EffectBatch>,
    ) -> Vec<Result<Vec<RegistryApplied>, RegistryEffectError>> {
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        let results = batches
            .into_iter()
            .map(|batch| match Self::apply_batch_locked(&mut inner, batch) {
                Ok((applied, batch_publication, continuations, schedules)) => {
                    assert!(continuations.is_empty(), "direct legacy effects cannot cancel a pending birth");
                    assert!(schedules.is_empty(), "prepared births must run through the registry owner");
                    publication.append(batch_publication);
                    Ok(applied)
                }
                Err(error) => Err(error),
            })
            .collect();
        let inventory_changed = inner.publish(publication);
        drop(inner);
        if inventory_changed {
            self.notify_inventory_changed();
        }
        results
    }

    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    fn apply_batch_locked(
        inner: &mut Inner,
        batch: EffectBatch,
    ) -> Result<
        (Vec<RegistryApplied>, Publication, Vec<RouteContinuation>, Vec<Arc<dyn ActivationReservation>>),
        RegistryEffectError,
    > {
        let mut staged_routes = FxHashMap::<MailboxId, Option<RouteRecord>>::default();
        let mut staged_kinds = FxHashMap::<KindId, KindSlot>::default();
        let mut staged_pending = FxHashMap::<MailboxId, Option<ActivationToken>>::default();
        let mut next_activation_token = inner.next_activation_token;
        let mut publication = Publication::default();
        let mut applied = Vec::with_capacity(batch.effects.len());
        let mut prepared_births = FxHashMap::<MailboxId, PendingBirth>::default();
        let mut prepared_cancellations = HashSet::<(MailboxId, ActivationToken)>::new();

        for effect in batch.effects {
            match effect {
                RegistryEffect::PreparedSpawn(mut commit) => {
                    let id = commit.route.id;
                    if id == MailboxId::NONE || id == MailboxId::CHASSIS_MAILBOX_ID {
                        let name = commit.route.canonical_name.clone();
                        drop(commit.reject_at_home(PreparedSpawnFailure::SubnameInUse { full_name: name.clone() }));
                        return Err(RegistryEffectError::Name(NameConflict { name }));
                    }
                    match staged_route(&staged_routes, inner, id) {
                        // Same-name reuse of a `Dropped` route. Only
                        // `Registry::drop_mailbox` produces that lifecycle, and
                        // it is a public routing primitive no chassis or cap
                        // calls today (issue 4152 audited every caller: all are
                        // tests). Retiring an actor leaves its route in place
                        // and tombstones the id in the `ActorRegistry` instead,
                        // which is what the conflict arm below reads.
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == commit.route.canonical_name => {}
                        // A route already occupies this id. `reserve` — where
                        // the authoritative retired-name answer lives — is
                        // still two steps away and will never run for this
                        // birth, so classify the conflict here instead of
                        // reporting every one of them as a live occupant.
                        Some(_) => {
                            let name = commit.route.canonical_name.clone();
                            let failure = commit.route_conflict_failure();
                            drop(commit.reject_at_home(failure));
                            return Err(RegistryEffectError::Name(NameConflict { name }));
                        }
                        None => {}
                    }
                    let token = ActivationToken::next(&mut next_activation_token);
                    let activation = match commit.take_activation().reserve(token) {
                        Ok(activation) => activation,
                        Err((prepared, failure)) => {
                            drop(prepared.discard_at_home(failure));
                            return Err(RegistryEffectError::ActivationRejected);
                        }
                    };
                    if !commit.costs.prepare(id, token) {
                        activation.reject(PreparedSpawnFailure::ActivationRejected);
                        return Err(RegistryEffectError::ActivationRejected);
                    }
                    let route = commit.route;
                    let record = RouteRecord {
                        canonical_name: route.canonical_name,
                        lifecycle: RouteLifecycle::Starting { token },
                    };
                    staged_routes.insert(route.id, Some(record.clone()));
                    staged_pending.insert(route.id, Some(token));
                    publication.route_updates.push(Update::Insert(route.id, record));
                    prepared_births.insert(
                        route.id,
                        PendingBirth {
                            id: route.id,
                            token,
                            parked: VecDeque::new(),
                            activation: Some(Arc::clone(&activation)),
                            costs: Some(commit.costs),
                            after_init: commit.after_init,
                            armed: true,
                            cancel_requested: false,
                        },
                    );
                    applied.push(RegistryApplied::Starting { id: route.id, token });
                }
                RegistryEffect::PublishAlias(alias) => {
                    let name = alias.rendered_name.to_string();
                    if alias.alias == MailboxId::NONE || alias.alias == MailboxId::CHASSIS_MAILBOX_ID {
                        return Err(RegistryEffectError::Name(NameConflict { name }));
                    }
                    let target_live =
                        match staged_route(&staged_routes, inner, alias.target_parent).map(|route| &route.lifecycle) {
                            Some(RouteLifecycle::Starting { .. }) => false,
                            Some(RouteLifecycle::Live { endpoint: RouteEndpoint::Inbox { .. } }) => true,
                            _ => {
                                return Err(RegistryEffectError::AliasTargetUnavailable {
                                    alias: alias.alias,
                                    target_parent: alias.target_parent,
                                });
                            }
                        };
                    match staged_route(&staged_routes, inner, alias.alias) {
                        Some(RouteRecord { canonical_name, lifecycle: RouteLifecycle::Alias { target_parent } })
                            if canonical_name == alias.rendered_name.as_ref()
                                && *target_parent == alias.target_parent =>
                        {
                            applied.push(RegistryApplied::Mailbox(alias.alias));
                            continue;
                        }
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == alias.rendered_name.as_ref() => {}
                        Some(_) => return Err(RegistryEffectError::Name(NameConflict { name })),
                        None => {}
                    }
                    let record = RouteRecord {
                        canonical_name: name,
                        lifecycle: RouteLifecycle::Alias { target_parent: alias.target_parent },
                    };
                    staged_routes.insert(alias.alias, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(alias.alias, record));
                    publication.inventory_dirty |= target_live;
                    applied.push(RegistryApplied::Mailbox(alias.alias));
                }
                RegistryEffect::ReserveStarting { route } => {
                    if route.id == MailboxId::NONE || route.id == MailboxId::CHASSIS_MAILBOX_ID {
                        return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                    }
                    match staged_route(&staged_routes, inner, route.id) {
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == route.canonical_name => {}
                        Some(_) => {
                            return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                        }
                        None => {}
                    }
                    let token = ActivationToken::next(&mut next_activation_token);
                    let record = RouteRecord {
                        canonical_name: route.canonical_name,
                        lifecycle: RouteLifecycle::Starting { token },
                    };
                    staged_routes.insert(route.id, Some(record.clone()));
                    staged_pending.insert(route.id, Some(token));
                    publication.route_updates.push(Update::Insert(route.id, record));
                    applied.push(RegistryApplied::Starting { id: route.id, token });
                }
                RegistryEffect::CancelStarting { id, token } => {
                    let prepared_cancel = !staged_routes.contains_key(&id)
                        && inner.pending_births.get(&id).is_some_and(|birth| {
                            birth.token == token && birth.activation.is_some() && !birth.cancel_requested
                        });
                    let cancellation = if prepared_cancel {
                        prepared_cancellations.insert((id, token));
                        StartingCancellation::Cancelled(id)
                    } else {
                        match staged_route(&staged_routes, inner, id) {
                            Some(RouteRecord { lifecycle: RouteLifecycle::Starting { token: current }, .. })
                                if *current == token
                                    && staged_pending_token(&staged_pending, inner, id) == Some(token) =>
                            {
                                staged_routes.insert(id, None);
                                staged_pending.insert(id, None);
                                publication.route_updates.push(Update::Remove(id));
                                StartingCancellation::Cancelled(id)
                            }
                            Some(RouteRecord { lifecycle: RouteLifecycle::Starting { .. }, .. }) => {
                                StartingCancellation::TokenMismatch(id)
                            }
                            _ => StartingCancellation::NotStarting(id),
                        }
                    };
                    applied.push(RegistryApplied::StartingCancellation(cancellation));
                }
                RegistryEffect::PublishLive { route, activation } => {
                    if route.id == MailboxId::NONE || route.id == MailboxId::CHASSIS_MAILBOX_ID {
                        return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                    }
                    let record = RouteRecord {
                        canonical_name: route.canonical_name.clone(),
                        lifecycle: RouteLifecycle::Live {
                            endpoint: RouteEndpoint::from_entry(activation.into_legacy()),
                        },
                    };
                    match staged_route(&staged_routes, inner, route.id) {
                        Some(existing)
                            if matches!(existing.lifecycle, RouteLifecycle::Dropped)
                                && existing.canonical_name == route.canonical_name => {}
                        Some(_) => {
                            return Err(RegistryEffectError::Name(NameConflict { name: route.canonical_name }));
                        }
                        None => {}
                    }
                    staged_routes.insert(route.id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(route.id, record));
                    publication.inventory_dirty = true;
                    applied.push(RegistryApplied::Mailbox(route.id));
                }
                RegistryEffect::DropMailbox(id) => {
                    let Some(mut record) = staged_route(&staged_routes, inner, id).cloned() else {
                        return Err(RegistryEffectError::Drop(DropError::UnknownId(id)));
                    };
                    let inventory_live = match &record.lifecycle {
                        RouteLifecycle::Starting { .. } => {
                            return Err(RegistryEffectError::Drop(DropError::UnknownId(id)));
                        }
                        RouteLifecycle::Dropped => {
                            return Err(RegistryEffectError::Drop(DropError::AlreadyDropped(id)));
                        }
                        RouteLifecycle::Live { .. } => true,
                        RouteLifecycle::Alias { target_parent } => staged_route(&staged_routes, inner, *target_parent)
                            .is_some_and(|target| matches!(target.lifecycle, RouteLifecycle::Live { .. })),
                    };
                    record.lifecycle = RouteLifecycle::Dropped;
                    let name = record.canonical_name.clone();
                    staged_routes.insert(id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(id, record.clone()));
                    publication.inventory_dirty |= inventory_live;
                    applied.push(RegistryApplied::Dropped(name));
                }
                RegistryEffect::RemoveMailbox(id) => {
                    let (removable, inventory_live) =
                        staged_route(&staged_routes, inner, id).map_or((false, false), |record| {
                            match &record.lifecycle {
                                RouteLifecycle::Live { .. } => (true, true),
                                RouteLifecycle::Alias { target_parent } => (
                                    true,
                                    staged_route(&staged_routes, inner, *target_parent)
                                        .is_some_and(|target| matches!(target.lifecycle, RouteLifecycle::Live { .. })),
                                ),
                                RouteLifecycle::Starting { .. } | RouteLifecycle::Dropped => (false, false),
                            }
                        });
                    if removable {
                        staged_routes.insert(id, None);
                        publication.route_updates.push(Update::Remove(id));
                        publication.inventory_dirty |= inventory_live;
                    }
                    applied.push(RegistryApplied::Removed(removable));
                }
                RegistryEffect::InstallSeize { id, handle } => {
                    let Some(mut record) = staged_route(&staged_routes, inner, id).cloned() else {
                        applied.push(RegistryApplied::SeizeInstalled(false));
                        continue;
                    };
                    let RouteLifecycle::Live { endpoint: RouteEndpoint::Inbox { handler, seize } } = &record.lifecycle
                    else {
                        applied.push(RegistryApplied::SeizeInstalled(false));
                        continue;
                    };
                    if seize.get().is_some() {
                        applied.push(RegistryApplied::SeizeInstalled(false));
                        continue;
                    }
                    let replacement = SeizeCell::default();
                    assert!(replacement.set(handle).is_ok(), "fresh seize cell must accept its first handle");
                    record.lifecycle = RouteLifecycle::Live {
                        endpoint: RouteEndpoint::Inbox { handler: Arc::clone(handler), seize: replacement },
                    };
                    staged_routes.insert(id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(id, record.clone()));
                    applied.push(RegistryApplied::SeizeInstalled(true));
                }
                RegistryEffect::RegisterKind { descriptor, reject_conflict } => {
                    let id = KindId(kind_id_from_parts(&descriptor.name, &descriptor.schema));
                    if let Some(slot) = staged_kind(&staged_kinds, inner, id) {
                        if reject_conflict
                            && canonical_kind_bytes(&slot.descriptor.name, &slot.descriptor.schema)
                                != canonical_kind_bytes(&descriptor.name, &descriptor.schema)
                        {
                            return Err(RegistryEffectError::Kind(KindConflict {
                                name: descriptor.name,
                                existing: slot.descriptor.schema.clone(),
                                requested: descriptor.schema,
                            }));
                        }
                    } else {
                        let name = Arc::from(descriptor.name.as_str());
                        staged_kinds.insert(id, KindSlot { name, descriptor });
                        publication.kinds_dirty = true;
                    }
                    applied.push(RegistryApplied::Kind(id));
                }
            }
        }

        inner.next_activation_token = next_activation_token;
        let continuations = commit_staged(inner, staged_routes, staged_kinds, staged_pending);
        for (id, token) in prepared_cancellations {
            let birth = inner.pending_births.get_mut(&id).expect("validated prepared cancellation remains pending");
            assert_eq!(birth.token, token, "validated prepared cancellation retains its exact token");
            birth.cancel_requested = true;
            birth.activation.as_ref().expect("prepared birth retains activation").cancel();
        }
        let mut schedules = Vec::with_capacity(prepared_births.len());
        for (id, birth) in prepared_births {
            schedules.push(Arc::clone(birth.activation.as_ref().expect("prepared birth retains activation")));
            inner.pending_births.insert(id, birth);
        }
        Ok((applied, publication, continuations, schedules))
    }

    fn cancel_completed_locked(
        inner: &mut Inner,
        id: MailboxId,
        token: ActivationToken,
        publication: &mut Publication,
    ) -> Vec<RouteContinuation> {
        let valid = inner.pending_births.get(&id).is_some_and(|birth| birth.token == token && birth.cancel_requested);
        if !valid {
            return Vec::new();
        }
        let mut birth = inner.pending_births.remove(&id).expect("validated pending cancellation exists");
        let continuations = birth
            .parked
            .drain(..)
            .map(|mail| RouteContinuation { mail, disposition: CapturedDisposition::Unknown })
            .collect();
        birth.costs.as_ref().expect("prepared cancellation retains cost reservation").rollback(id, token);
        birth.disarm();
        if matches!(
            inner.mailboxes.get(&id).map(|route| &route.lifecycle),
            Some(RouteLifecycle::Starting { token: current }) if *current == token
        ) {
            inner.mailboxes.remove(&id);
            publication.route_updates.push(Update::Remove(id));
        }
        continuations
    }

    fn promote_locked(
        inner: &mut Inner,
        id: MailboxId,
        token: ActivationToken,
        barrier_mail_id: Option<MailId>,
        publication: &mut Publication,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        if !matches!(
            inner.mailboxes.get(&id).map(|route| &route.lifecycle),
            Some(RouteLifecycle::Starting { token: current }) if *current == token
        ) {
            return None;
        }
        let mut birth = inner.pending_births.remove(&id).expect("Starting route retains its pending birth");
        if birth.token != token {
            inner.pending_births.insert(id, birth);
            return None;
        }
        if birth.cancel_requested {
            inner.pending_births.insert(id, birth);
            return None;
        }
        let activation = birth.activation.as_ref().expect("prepared Starting birth retains activation");
        if barrier_mail_id.is_some_and(|mail_id| !activation.barrier_matches(mail_id)) {
            inner.pending_births.insert(id, birth);
            return None;
        }
        let Some(live) = activation.take_live() else {
            inner.pending_births.insert(id, birth);
            return None;
        };
        let bootstrap = mem::take(&mut birth.after_init);
        let parked = birth
            .parked
            .drain(..)
            .map(|mail| {
                let kind_name = inner
                    .kinds
                    .get(&mail.kind)
                    .map_or_else(|| Arc::clone(&inner.empty_kind_name), |slot| Arc::clone(&slot.name));
                PreparedMail::parked(mail, kind_name)
            })
            .collect();
        let installed = live.install(bootstrap, parked);
        birth.costs.as_ref().expect("prepared Starting birth retains cost reservation").promote(id, token);

        let endpoint = match installed.entry {
            MailboxEntry::Inbox { handler, seize } => RouteEndpoint::Inbox { handler, seize },
            MailboxEntry::Inline(_) | MailboxEntry::Dropped => {
                panic!("prepared actor activation must install an inbox endpoint")
            }
        };
        let canonical_name =
            inner.mailboxes.get(&id).expect("Starting route exists while promoting").canonical_name.clone();
        let record = RouteRecord { canonical_name, lifecycle: RouteLifecycle::Live { endpoint } };
        inner.mailboxes.insert(id, record.clone());
        publication.route_updates.push(Update::Insert(id, record));
        publication.inventory_dirty = true;
        birth.disarm();
        Some(installed.catch_up)
    }

    fn capture_mail_locked(inner: &mut Inner, mail: Mail) -> Option<RouteContinuation> {
        match resolve_route(mail.recipient, |id| inner.mailboxes.get(&id)) {
            ResolvedRoute::Starting { target } => {
                let token = match inner.mailboxes.get(&target).map(|route| &route.lifecycle) {
                    Some(RouteLifecycle::Starting { token }) => *token,
                    _ => unreachable!("resolved Starting target remains Starting under the owner lock"),
                };
                let pending = inner
                    .pending_births
                    .get_mut(&target)
                    .unwrap_or_else(|| panic!("published Starting route missing its owner-private pending birth"));
                assert_eq!(pending.token, token, "published Starting token disagrees with pending birth");
                pending.parked.push_back(mail);
                None
            }
            ResolvedRoute::Live { endpoint } => Some(RouteContinuation {
                disposition: CapturedDisposition::Live {
                    endpoint: endpoint.clone(),
                    kind_name: inner
                        .kinds
                        .get(&mail.kind)
                        .map_or_else(|| Arc::clone(&inner.empty_kind_name), |slot| Arc::clone(&slot.name)),
                },
                mail,
            }),
            ResolvedRoute::Dropped => Some(RouteContinuation { mail, disposition: CapturedDisposition::Dropped }),
            ResolvedRoute::Unknown => Some(RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
        }
    }

    fn apply_one(
        &self,
        authority: &BootAuthority,
        effect: RegistryEffect,
    ) -> Result<RegistryApplied, RegistryEffectError> {
        self.apply_batches(authority, vec![EffectBatch::new(vec![effect])])
            .pop()
            .expect("one submitted batch returns one result")?
            .pop()
            .ok_or_else(|| RegistryEffectError::Name(NameConflict { name: "empty registry effect".to_owned() }))
    }

    /// Insert a mailbox, allocating its id from the name hash (ADR-0029).
    /// On a `Dropped` entry at the same id (same name re-registered
    /// after a drop), the entry transitions back to live. Any other
    /// occupied entry is a collision.
    fn insert(&self, authority: &BootAuthority, name: String, entry: MailboxEntry) -> Result<MailboxId, NameConflict> {
        // Depth-1 / root registrations derive the id from the name
        // (ADR-0029) — the lineage fold's fixed point.
        self.insert_with_id(authority, MailboxId::from_name(&name), name, entry)
    }

    /// ADR-0099 §3: register under an explicit, caller-computed `id`
    /// (the lineage fold) with `name` retained as the display /
    /// reverse-map string. `MailboxId = hash(name)` no longer holds for
    /// a hosted / spawned actor — its id is the fold over its lineage of
    /// `ActorId`s — so the spawn path passes the folded id here instead
    /// of letting the name derive it. [`Self::insert`] is the depth-1
    /// case where the two coincide.
    fn insert_with_id(
        &self,
        authority: &BootAuthority,
        id: MailboxId,
        name: String,
        entry: MailboxEntry,
    ) -> Result<MailboxId, NameConflict> {
        match self.apply_one(authority, RegistryEffect::publish_with_id(id, name, entry)) {
            Ok(RegistryApplied::Mailbox(id)) => Ok(id),
            Err(RegistryEffectError::Name(error)) => Err(error),
            Ok(_) | Err(_) => unreachable!("publish-live returns mailbox or name conflict"),
        }
    }

    /// Invalidate a live mailbox (ADR-0010). Transitions the entry
    /// to `Dropped` so dispatch-path readers can distinguish an
    /// intentional drop from an unknown id; the id itself (a function
    /// of the name per ADR-0029) stays addressable and a subsequent
    /// `try_register_inbox` / `register_inline` with the same
    /// name reuses it. Returns the released name on success.
    ///
    /// Issue 634 Phase 4 retired the dedicated `Component` variant,
    /// so this now drops any live `Inbox` or `Inline` mailbox.
    ///
    /// No production caller (iamacoffeepot/aether#4152 audited every one:
    /// all are tests). The `WasmTrampoline` shutdown path this comment
    /// used to name reaches [`CostTable::drop_mailbox`](crate::mail::cost::CostTable::drop_mailbox)
    /// now, which clears the per-handler cost cells and never touches a
    /// registry route.
    ///
    /// Direct write path — takes a [`BootAuthority`] like every other
    /// eager mutator (iamacoffeepot/aether#4161), so the remaining callers
    /// (all tests, which use it to clear a deliberately-installed collision
    /// route) name the same authority production boot would.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn drop_mailbox(&self, authority: &BootAuthority, id: MailboxId) -> Result<String, DropError> {
        match self.apply_one(authority, RegistryEffect::DropMailbox(id)) {
            Ok(RegistryApplied::Dropped(name)) => Ok(name),
            Err(RegistryEffectError::Drop(error)) => Err(error),
            Ok(_) | Err(_) => unreachable!("drop effect returns a name or drop error"),
        }
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
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// claim passes can name it (iamacoffeepot/aether#4161). A handler
    /// stages a `RegistryEffect` through the ADR-0165 owner instead.
    pub fn register_inbox(
        &self,
        authority: &BootAuthority,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> MailboxId {
        match self.insert(authority, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() }) {
            Ok(id) => id,
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
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// claim passes can name it (iamacoffeepot/aether#4161). A handler
    /// stages a `RegistryEffect` through the ADR-0165 owner instead.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn try_register_inbox(
        &self,
        authority: &BootAuthority,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        self.insert(authority, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() })
    }

    /// ADR-0099 §3: [`Self::try_register_inbox`] but under an explicit,
    /// caller-computed `id` (the lineage fold) rather than `hash(name)`.
    /// The spawn path uses this for hosted / nested actors, whose id is
    /// the fold over their lineage; `name` stays the rendered display /
    /// reverse-map string. The returned id is `id` on success.
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot /
    /// embedder eager spawn can name it (iamacoffeepot/aether#4156). A
    /// handler stages a `RegistryEffect` through the ADR-0165 owner
    /// instead.
    pub fn try_register_inbox_with_id(
        &self,
        authority: &BootAuthority,
        id: MailboxId,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        self.insert_with_id(authority, id, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() })
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
    /// registrations should never collide.
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// claim passes and the chassis diagnostic sinks can name it
    /// (iamacoffeepot/aether#4161). A handler stages a `RegistryEffect`
    /// through the ADR-0165 owner instead.
    pub fn register_inline(
        &self,
        authority: &BootAuthority,
        name: impl Into<String>,
        handler: Arc<dyn InlineHandler>,
    ) -> MailboxId {
        match self.insert(authority, name.into(), MailboxEntry::Inline(handler)) {
            Ok(id) => id,
            Err(NameConflict { name }) => {
                panic!("mailbox name already registered: {name}")
            }
        }
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
    ///
    /// Direct write path — takes a [`BootAuthority`] so only the boot
    /// unwind and the boot / embedder eager spawn can name it
    /// (iamacoffeepot/aether#4156).
    pub(crate) fn remove_closure(&self, authority: &BootAuthority, id: MailboxId) -> bool {
        match self.apply_one(authority, RegistryEffect::RemoveMailbox(id)) {
            Ok(RegistryApplied::Removed(removed)) => removed,
            Ok(_) | Err(_) => unreachable!("remove effect is infallible and returns a bool"),
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
            Some(route)
                if route.canonical_name == name
                    && matches!(
                        resolve_route(id, |candidate| routes.entry_for(&candidate)),
                        ResolvedRoute::Starting { .. } | ResolvedRoute::Live { .. }
                    ) =>
            {
                Some(id)
            }
            _ => None,
        })
    }

    /// Fetch the entry for a mailbox id from a point-in-time view.
    /// Returns an owned compatibility projection of the private route.
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        let routes = self.routes.load();
        match resolve_route(id, |candidate| routes.entry_for(&candidate)) {
            ResolvedRoute::Live { endpoint } => Some(endpoint.as_entry()),
            ResolvedRoute::Dropped => Some(MailboxEntry::Dropped),
            ResolvedRoute::Starting { .. } | ResolvedRoute::Unknown => None,
        }
    }

    /// Test whether `alias` is the logical inline-child route owned by
    /// `target_parent`. This checks route identity, not endpoint identity.
    pub(crate) fn is_alias_to(&self, alias: MailboxId, target_parent: MailboxId) -> bool {
        matches!(
            self.routes.load().entry_for(&alias).map(|route| &route.lifecycle),
            Some(RouteLifecycle::Alias { target_parent: target }) if *target == target_parent
        )
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
    /// Direct write path — takes a [`BootAuthority`] so only the boot /
    /// embedder eager spawn can name it (iamacoffeepot/aether#4156).
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn install_seize_handle(&self, authority: &BootAuthority, id: MailboxId, handle: SeizeHandle) -> bool {
        match self.apply_one(authority, RegistryEffect::InstallSeize { id, handle }) {
            Ok(RegistryApplied::SeizeInstalled(installed)) => installed,
            Ok(_) | Err(_) => unreachable!("seize effect is infallible and returns a bool"),
        }
    }

    /// Hot-path combined lookup for the mailer's route step.
    ///
    /// Route and kind snapshots are loaded independently. This is coherent
    /// because kind definitions are immutable after their first successful
    /// registration: a later kind publication cannot change an existing id.
    pub(crate) fn route_lookup(&self, kind: KindId, recipient: MailboxId) -> RouteLookup {
        let (endpoint, starting, dropped, generation) = {
            let routes = self.routes.load();
            let (endpoint, starting, dropped) = match resolve_route(recipient, |id| routes.entry_for(&id)) {
                ResolvedRoute::Starting { .. } => (None, true, false),
                ResolvedRoute::Live { endpoint } => (Some(endpoint.clone()), false, false),
                ResolvedRoute::Dropped => (None, false, true),
                ResolvedRoute::Unknown => (None, false, false),
            };
            (endpoint, starting, dropped, routes.generation())
        };
        RouteLookup {
            endpoint,
            starting,
            dropped,
            kind_name: self
                .kinds
                .load()
                .table()
                .kinds
                .get(&kind)
                .map_or_else(|| Arc::clone(&self.empty_kind_name), |slot| Arc::clone(&slot.name)),
            generation,
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
    ///
    /// Direct write path — takes a [`BootAuthority`] like its descriptor
    /// sibling (iamacoffeepot/aether#4161); `load_component` stages a
    /// `RegistryBatch::register_kinds` through the ADR-0165 owner instead.
    pub fn register_kind(&self, authority: &BootAuthority, name: impl Into<String>) -> KindId {
        let descriptor = bytes_kind(name.into());
        // A fresh `Bytes` descriptor can only conflict with a prior
        // `Bytes` registration under the same name — in which case the
        // schemas match and the call is idempotent. Not reachable.
        self.register_kind_internal(authority, descriptor, /*reject_conflict=*/ false)
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
    /// Used by substrate boot (`descriptors::all()`). Direct write path —
    /// takes a [`BootAuthority`] so only boot can name it
    /// (iamacoffeepot/aether#4156); `load_component` stages a
    /// `RegistryBatch::register_kinds` through the ADR-0165 owner instead.
    ///
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn register_kind_with_descriptor(
        &self,
        authority: &BootAuthority,
        descriptor: KindDescriptor,
    ) -> Result<KindId, KindConflict> {
        self.register_kind_internal(authority, descriptor, /*reject_conflict=*/ true)
    }

    fn register_kind_internal(
        &self,
        authority: &BootAuthority,
        descriptor: KindDescriptor,
        reject_conflict: bool,
    ) -> Result<KindId, KindConflict> {
        match self.apply_one(authority, RegistryEffect::RegisterKind { descriptor, reject_conflict }) {
            Ok(RegistryApplied::Kind(id)) => Ok(id),
            Err(RegistryEffectError::Kind(error)) => Err(error),
            Ok(_) | Err(_) => unreachable!("register-kind returns a kind id or kind conflict"),
        }
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
        self.kind_name_shared(kind).map(|name| name.to_string())
    }

    /// Crate-private shared projection for dispatch paths that must not
    /// reallocate the immutable registered kind name.
    pub(crate) fn kind_name_shared(&self, kind: KindId) -> Option<Arc<str>> {
        self.kinds.load().table().kinds.get(&kind).map(|slot| Arc::clone(&slot.name))
    }

    pub(crate) fn kind_name_or_empty_shared(&self, kind: KindId) -> Arc<str> {
        self.kind_name_shared(kind).unwrap_or_else(|| Arc::clone(&self.empty_kind_name))
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
    /// Only live entries are included. Keyed routes retain `Dropped`
    /// records for dispatch and trace-name resolution, but public inventory
    /// is a distinct publication and removes them.
    pub fn list_mailbox_descriptors(&self) -> Vec<MailboxDescriptor> {
        self.inventory.load().table().mailboxes.clone()
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
