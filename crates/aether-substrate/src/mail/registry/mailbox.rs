use std::collections::{HashMap, VecDeque};
use std::process::abort;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use rustc_hash::FxHashMap;

use aether_actor::{HandlesKind, RegistryChanged};
use aether_data::canonical::{canonical_kind_bytes, kind_id_from_parts};
use aether_data::{
    KindDescriptor, MailboxCategory, MailboxDescriptor, ScopePathError, mailbox_id_from_path, validate_scope_path,
};

use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{
    ActivationToken, ChangeSubscriber, EffectBatch, RegistryApplied, RegistryCompletion, RegistryEffect,
    RegistryEffectError, RegistryInventory, RegistrySubscription, StartingCancellation, bytes_kind, subscriber,
};
use crate::mail::registry::errors::{DropError, KindConflict, NameConflict};
use crate::mail::registry::handlers::{InboxHandler, InlineHandler};
use crate::mail::registry::names::categorise_mailbox_name;
use crate::mail::registry::owner::{BatchEnvelope, OwnerCommand, RegistryOwnerHandle};
use crate::mail::registry::{
    ActorAddressInventoryError, AddressResolutionError, ResolvedAddress, address::AddressIndex,
};
use crate::mail::view::{DoubleBuffer, Update, View, ViewPublisher};
use crate::mail::{KindId, Mail, MailboxId};
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
    Starting { token: ActivationToken },
    Live { endpoint: RouteEndpoint },
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
    token: ActivationToken,
    parked: VecDeque<Mail>,
}

pub enum CapturedDisposition {
    Live { endpoint: RouteEndpoint, kind_name: String },
    Dropped,
    Unknown,
}

pub struct RouteContinuation {
    pub(crate) mail: Mail,
    pub(crate) disposition: CapturedDisposition,
}

/// One point-in-time route and kind-name lookup.
pub struct RouteLookup {
    endpoint: Option<RouteEndpoint>,
    starting: bool,
    dropped: bool,
    kind_name: String,
    generation: u64,
}

impl RouteLookup {
    pub(crate) fn is_starting(&self) -> bool {
        self.starting
    }

    pub(crate) fn is_unknown(&self) -> bool {
        self.endpoint.is_none() && !self.starting && !self.dropped
    }

    pub(crate) fn kind_name(&self) -> &str {
        &self.kind_name
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
        .filter(|(_, route)| matches!(route.lifecycle, RouteLifecycle::Live { .. }))
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
        inner.name_index.insert(slot.name.clone(), id);
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
            inner.pending_births.insert(id, PendingBirth { token, parked: VecDeque::new() });
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

    pub(super) fn install_owner(&self, owner: RegistryOwnerHandle) {
        assert!(self.owner.set(owner).is_ok(), "a registry can attach only one owner");
    }

    #[allow(dead_code, reason = "staged registry writers begin using the owner seam in the next migration issues")]
    pub(crate) fn submit(&self, batch: EffectBatch) -> Option<RegistryCompletion<Vec<RegistryApplied>>> {
        self.owner.get()?.submit(batch)
    }

    pub(crate) fn park_or_drop(&self, mail: Mail) -> Option<Mail> {
        match self.owner.get() {
            Some(owner) => owner.park_or_drop(mail),
            None => Some(mail),
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

    pub(super) fn apply_owner_commands(&self, commands: Vec<OwnerCommand>, mailer: &Mailer) {
        enum AfterLock {
            Batch(
                crossbeam_channel::Sender<Result<Vec<RegistryApplied>, RegistryEffectError>>,
                Result<Vec<RegistryApplied>, RegistryEffectError>,
            ),
            Route(RouteContinuation),
        }

        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        let mut after_lock = Vec::new();
        for command in commands {
            match command {
                OwnerCommand::Batch(BatchEnvelope { batch, completion }) => {
                    let result = match Self::apply_batch_locked(&mut inner, batch) {
                        Ok((applied, batch_publication, continuations)) => {
                            publication.append(batch_publication);
                            after_lock.extend(continuations.into_iter().map(AfterLock::Route));
                            Ok(applied)
                        }
                        Err(error) => Err(error),
                    };
                    after_lock.push(AfterLock::Batch(completion, result));
                }
                OwnerCommand::ParkOrDrop(mail) => {
                    if let Some(continuation) = Self::capture_mail_locked(&mut inner, mail) {
                        after_lock.push(AfterLock::Route(continuation));
                    }
                }
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
                    let _ = completion.send(result);
                }
                AfterLock::Route(continuation) => mailer.relay_captured(continuation),
            }
        }
    }

    pub(super) fn close_owner_commands(&self, commands: Vec<OwnerCommand>, mailer: &Mailer) {
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        let mut completions = Vec::new();
        let mut continuations = Vec::new();
        for command in commands {
            match command {
                OwnerCommand::Batch(envelope) => completions.push(envelope.completion),
                OwnerCommand::ParkOrDrop(mail) => {
                    if let Some(continuation) = Self::capture_mail_locked(&mut inner, mail) {
                        continuations.push(continuation);
                    }
                }
            }
        }
        let pending_births = inner.pending_births.drain().collect::<Vec<_>>();
        for (id, mut birth) in pending_births {
            continuations.extend(
                birth
                    .parked
                    .drain(..)
                    .map(|mail| RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
            );
            if matches!(inner.mailboxes.get(&id).map(|route| &route.lifecycle), Some(RouteLifecycle::Starting { .. })) {
                inner.mailboxes.remove(&id);
                publication.route_updates.push(Update::Remove(id));
            }
        }
        let inventory_changed = inner.publish(publication);
        drop(inner);
        if inventory_changed {
            self.relay_inventory_changed();
        }
        for completion in completions {
            let _ = completion.send(Err(RegistryEffectError::OwnerClosed));
        }
        for continuation in continuations {
            mailer.relay_captured(continuation);
        }
    }

    fn apply_batches(&self, batches: Vec<EffectBatch>) -> Vec<Result<Vec<RegistryApplied>, RegistryEffectError>> {
        let mut inner = self.inner.write().expect("registry lock poisoned; fail-fast per ADR-0063");
        let mut publication = Publication::default();
        let results = batches
            .into_iter()
            .map(|batch| match Self::apply_batch_locked(&mut inner, batch) {
                Ok((applied, batch_publication, continuations)) => {
                    debug_assert!(continuations.is_empty(), "direct legacy effects cannot cancel a pending birth");
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

    #[allow(clippy::too_many_lines)]
    fn apply_batch_locked(
        inner: &mut Inner,
        batch: EffectBatch,
    ) -> Result<(Vec<RegistryApplied>, Publication, Vec<RouteContinuation>), RegistryEffectError> {
        let mut staged_routes = FxHashMap::<MailboxId, Option<RouteRecord>>::default();
        let mut staged_kinds = FxHashMap::<KindId, KindSlot>::default();
        let mut staged_pending = FxHashMap::<MailboxId, Option<ActivationToken>>::default();
        let mut next_activation_token = inner.next_activation_token;
        let mut publication = Publication::default();
        let mut applied = Vec::with_capacity(batch.effects.len());

        for effect in batch.effects {
            match effect {
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
                    let cancellation = match staged_route(&staged_routes, inner, id) {
                        Some(RouteRecord { lifecycle: RouteLifecycle::Starting { token: current }, .. })
                            if *current == token && staged_pending_token(&staged_pending, inner, id) == Some(token) =>
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
                    match record.lifecycle {
                        RouteLifecycle::Starting { .. } => {
                            return Err(RegistryEffectError::Drop(DropError::UnknownId(id)));
                        }
                        RouteLifecycle::Dropped => {
                            return Err(RegistryEffectError::Drop(DropError::AlreadyDropped(id)));
                        }
                        RouteLifecycle::Live { .. } => {}
                    }
                    record.lifecycle = RouteLifecycle::Dropped;
                    let name = record.canonical_name.clone();
                    staged_routes.insert(id, Some(record.clone()));
                    publication.route_updates.push(Update::Insert(id, record.clone()));
                    publication.inventory_dirty = true;
                    applied.push(RegistryApplied::Dropped(name));
                }
                RegistryEffect::RemoveMailbox(id) => {
                    let removable = staged_route(&staged_routes, inner, id)
                        .is_some_and(|record| matches!(record.lifecycle, RouteLifecycle::Live { .. }));
                    if removable {
                        staged_routes.insert(id, None);
                        publication.route_updates.push(Update::Remove(id));
                        publication.inventory_dirty = true;
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
                        staged_kinds.insert(id, KindSlot { name: descriptor.name.clone(), descriptor });
                        publication.kinds_dirty = true;
                    }
                    applied.push(RegistryApplied::Kind(id));
                }
            }
        }

        inner.next_activation_token = next_activation_token;
        let continuations = commit_staged(inner, staged_routes, staged_kinds, staged_pending);
        Ok((applied, publication, continuations))
    }

    fn capture_mail_locked(inner: &mut Inner, mail: Mail) -> Option<RouteContinuation> {
        match inner.mailboxes.get(&mail.recipient).map(|route| route.lifecycle.clone()) {
            Some(RouteLifecycle::Starting { token }) => {
                let pending = inner
                    .pending_births
                    .get_mut(&mail.recipient)
                    .unwrap_or_else(|| panic!("published Starting route missing its owner-private pending birth"));
                assert_eq!(pending.token, token, "published Starting token disagrees with pending birth");
                pending.parked.push_back(mail);
                None
            }
            Some(RouteLifecycle::Live { endpoint }) => Some(RouteContinuation {
                disposition: CapturedDisposition::Live {
                    endpoint,
                    kind_name: inner.kinds.get(&mail.kind).map(|slot| slot.name.clone()).unwrap_or_default(),
                },
                mail,
            }),
            Some(RouteLifecycle::Dropped) => {
                Some(RouteContinuation { mail, disposition: CapturedDisposition::Dropped })
            }
            None => Some(RouteContinuation { mail, disposition: CapturedDisposition::Unknown }),
        }
    }

    fn apply_one(&self, effect: RegistryEffect) -> Result<RegistryApplied, RegistryEffectError> {
        self.apply_batches(vec![EffectBatch::new(vec![effect])])
            .pop()
            .expect("one submitted batch returns one result")?
            .pop()
            .ok_or_else(|| RegistryEffectError::Name(NameConflict { name: "empty registry effect".to_owned() }))
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
        match self.apply_one(RegistryEffect::publish_with_id(id, name, entry)) {
            Ok(RegistryApplied::Mailbox(id)) => Ok(id),
            Err(RegistryEffectError::Name(error)) => Err(error),
            Ok(_) | Err(_) => unreachable!("publish-live returns mailbox or name conflict"),
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
        match self.apply_one(RegistryEffect::DropMailbox(id)) {
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
    pub fn register_inbox(&self, name: impl Into<String>, handler: Arc<dyn InboxHandler>) -> MailboxId {
        match self.insert(name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() }) {
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
    /// # Panics
    /// Panics if the inner `RwLock` is poisoned — fail-fast per
    /// ADR-0063: a poisoned lock means a prior holder panicked under
    /// the guard.
    pub fn try_register_inbox(
        &self,
        name: impl Into<String>,
        handler: Arc<dyn InboxHandler>,
    ) -> Result<MailboxId, NameConflict> {
        self.insert(name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() })
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
        self.insert_with_id(id, name.into(), MailboxEntry::Inbox { handler, seize: SeizeCell::default() })
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
            Ok(id) => id,
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
        self.insert(name.into(), MailboxEntry::Inline(handler))
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
        match self.apply_one(RegistryEffect::RemoveMailbox(id)) {
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
            Some(route) if route.canonical_name == name && !matches!(route.lifecycle, RouteLifecycle::Dropped) => {
                Some(id)
            }
            _ => None,
        })
    }

    /// Fetch the entry for a mailbox id from a point-in-time view.
    /// Returns an owned compatibility projection of the private route.
    pub fn entry(&self, id: MailboxId) -> Option<MailboxEntry> {
        self.routes.load().entry_for(&id).and_then(|route| match &route.lifecycle {
            RouteLifecycle::Live { endpoint } => Some(endpoint.as_entry()),
            RouteLifecycle::Dropped => Some(MailboxEntry::Dropped),
            RouteLifecycle::Starting { .. } => None,
        })
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
        match self.apply_one(RegistryEffect::InstallSeize { id, handle }) {
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
        let routes = self.routes.load();
        let kinds = self.kinds.load();
        let (endpoint, starting, dropped) =
            routes.entry_for(&recipient).map_or((None, false, false), |route| match &route.lifecycle {
                RouteLifecycle::Starting { .. } => (None, true, false),
                RouteLifecycle::Live { endpoint } => (Some(endpoint.clone()), false, false),
                RouteLifecycle::Dropped => (None, false, true),
            });
        RouteLookup {
            endpoint,
            starting,
            dropped,
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
        let descriptor = bytes_kind(name.into());
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
        match self.apply_one(RegistryEffect::RegisterKind { descriptor, reject_conflict }) {
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
