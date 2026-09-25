// Mail routing loads a point-in-time registry view and dispatches through
// its endpoint facade. A concurrent replacement affects the next lookup.

// Inline router (ADR-0038 Phase 3 + issue 603).
//
// Phase 2 retired the VecDeque + router thread; Phase 3 retired the
// global `outstanding` / `done_cv` barrier in favour of per-component
// drains. Issue 603 retired the shared `ComponentTable` Arc.
//
// Issue 634 Phase 4 retired the wasm-component-specific routing path
// entirely: every loaded wasm component is now a `WasmTrampoline`
// `NativeActor` registered as a `MailboxEntry::Inbox` like every
// other actor, so the dedicated `ComponentRouter` slot + `route()`
// method + `MailboxEntry::Component` variant are gone. PR 2 retired
// the `drain_all_with_budget` polling barrier in favour of direct
// trap-abort at the trampoline (the trampoline holds a
// `FatalAborter` and aborts on `Component::deliver` Err).
//
// `push(mail)` still resolves the recipient inline on the caller's
// thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::actor::native::ActorProbe;
#[cfg(feature = "wasm")]
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::ResolvePathError;
#[cfg(feature = "wasm")]
use crate::actor::wasm::component::ComponentCtx;
use crate::chassis::settlement::SettlementRegistry;
use crate::mail::capability::CapabilityRegistry;
use crate::mail::cost::CostTable;
use crate::mail::outbound::HubOutbound;
use crate::mail::registry::effect::ACTIVATION_BARRIER_KIND;
use crate::mail::registry::{
    AddressResolutionError, CapturedDisposition, MailDispatch, OwnedDispatch, ParkAdmission, Registry,
    RegistryQueueMetrics, RegistrySubscription, ResolvedAddress, RouteContinuation, RouteEndpoint, RouteRelayHandle,
};
use crate::mail::{Mail, Source, SourceAddr};
use crate::runtime::thread_name;
use crate::runtime::trace::{SettlementHold, TraceHandle};
use crate::scheduler::pending_depth;
use aether_actor::ErasedActorRef;
use aether_data::tagged_id::{self, Tag};
use aether_data::{ActorPath, Kind, KindDescriptor, KindId};
use aether_kinds::ComponentCapabilities;
use aether_kinds::trace::{Nanos, TraceTail, TraceTailResult};
use std::sync::OnceLock;

pub struct Mailer {
    /// Registry handle for resolving recipients on `push`. Owned for
    /// the `Mailer`'s lifetime; supplied at construction time (issue 657
    /// collapsed the prior `wire`-after-`new` setter pair into a
    /// required-argument constructor).
    registry: Arc<Registry>,
    /// Hub outbound handle. When set and connected, mail to unknown
    /// mailbox ids bubbles up to the hub-substrate (ADR-0037
    /// Phase 1) instead of being warn-dropped locally. Optional —
    /// chassis that skip hub connection (today: the hub chassis
    /// itself) construct a `Mailer` without an outbound and keep
    /// local warn-drop semantics intact (the hub is the end of the
    /// bubbles-up line). Pre-issue-657 this rode an `OnceLock` set
    /// post-construction via `wire_outbound`; the constructor +
    /// `with_outbound` pair below replaces that.
    outbound: Option<Arc<HubOutbound>>,
    /// ADR-0080 §5 chassis-mail router. When mail is addressed to
    /// [`aether_data::MailboxId::CHASSIS_MAILBOX_ID`], `route_mail` short-circuits
    /// the registry lookup and invokes this closure instead. Today
    /// the chassis installs a router that decodes `Settled { root }`
    /// and signals the [`SettlementRegistry`]
    /// — keeping the Mailer ignorant of trace kinds while still
    /// providing the dispatch surface those kinds need.
    ///
    /// `OnceLock` so the chassis builder installs exactly once at
    /// boot. `None` for tests / chassis that don't bring up the
    /// trace pipeline — chassis-addressed mail is silently dropped
    /// in that case.
    chassis_router: OnceLock<Box<dyn Fn(Mail) + Send + Sync>>,
    /// ADR-0080 §6 settlement registry handle, exposed so capabilities
    /// hosting external entry points (`RpcServer`, future event-source
    /// caps) can subscribe to settlement of mail they dispatch from
    /// their handlers. Threaded through the Mailer rather than down
    /// every `NativeBinding` because settlement is a chassis-wide
    /// service used at runtime — the cap reaches it via
    /// `ctx.mailer().settlement_registry()`.
    ///
    /// `OnceLock` so the chassis builder installs exactly once at
    /// boot alongside [`Self::chassis_router`]. `None` on test fixtures
    /// / chassis that don't bring up the trace pipeline; callers that
    /// expect to subscribe must check for the presence and surface a
    /// clear error otherwise.
    settlement_registry: OnceLock<Arc<SettlementRegistry>>,
    /// ADR-0080 per-chassis trace handle (slimmed by ADR-0086 Phase 3c).
    /// Holds the emit-time `SettlementCounter`, the chassis-host trace
    /// ring, and the boot-time anchor for `Nanos` timestamps. Producer-
    /// side hooks (`record_sent` / `record_finished` /
    /// `acquire_settlement_hold`) reach for it via the shortcut methods
    /// on this `Mailer`. Always present (allocated by [`Self::new`]);
    /// tests can swap in a pre-seeded handle via [`Self::with_trace_handle`].
    trace_handle: TraceHandle,
    route_relay: OnceLock<RouteRelayHandle>,
    /// iamacoffeepot/aether#1037 queryable capability registry. A
    /// sibling of [`Self::registry`] — the routing registry resolves
    /// recipients on the hot path; this one answers the DAG
    /// validator's submit-path dispatchability questions
    /// (`accepts(MailboxId, KindId)` / `has_fallback(MailboxId)`). The
    /// component-load / native-cap-boot path populates it, replace
    /// re-registers, drop clears. Allocated empty by [`Self::new`]
    /// (like `trace_handle`) so no call site changes.
    capability_registry: Arc<CapabilityRegistry>,
    /// iamacoffeepot/aether#1128 global per-handler execution-cost
    /// table — Phase 0 of the cost-aware recruiter. A sibling of
    /// [`Self::capability_registry`]: the cold-path index over every
    /// actor's per-handler [`crate::mail::cost::CostCell`]s, shared as
    /// the same `Arc<CostCell>` the actor's lock-free per-actor
    /// `CostCells` cache holds. The component-load / native-cap-boot
    /// path seeds it (alongside the cap-registry accept-set); the
    /// `cost.tail` dump and a future iamacoffeepot/aether#1178
    /// producer-side `total_work` read it. Never touched on the per-dispatch
    /// fold (that runs lock-free through the per-actor cache). Allocated
    /// empty by [`Self::new`] (like `trace_handle`).
    cost_table: Arc<CostTable>,
    /// ADR-0080 §6: the engine's one chassis-root correlation counter. Every
    /// chassis-root push mints `MailId(CHASSIS_MAILBOX_ID, n)` from here, so no
    /// two senders can mint the same root. Starts at 1; 0 is the sentinel.
    chassis_roots: AtomicU64,
}

/// A chassis root minted from the [`Mailer`]'s counter, its `Sent` recorded,
/// not yet pushed. Only [`Mailer::mint_chassis_root`] builds one and
/// [`Mailer::push_minted_root`] consumes it, so a root is pushed at most once
/// and never from a caller-chosen correlation.
#[must_use]
pub(crate) struct MintedRoot {
    root: aether_data::MailId,
    recipient: aether_data::MailboxId,
    kind: KindId,
}

impl MintedRoot {
    /// The minted root.
    pub(crate) const fn id(&self) -> aether_data::MailId {
        self.root
    }

    /// Consume the minted root into what its push needs.
    const fn into_parts(self) -> (aether_data::MailId, aether_data::MailboxId, KindId) {
        (self.root, self.recipient, self.kind)
    }
}

impl Mailer {
    /// Construct a `Mailer` against the substrate's registry.
    /// `SubstrateBoot::build` is the production caller; tests build the
    /// same pair with `Registry::new()`. Call [`Self::with_outbound`] to
    /// attach a hub outbound if the chassis needs ADR-0037 bubble-up.
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            outbound: None,
            chassis_router: OnceLock::new(),
            settlement_registry: OnceLock::new(),
            trace_handle: TraceHandle::new(),
            route_relay: OnceLock::new(),
            capability_registry: Arc::new(CapabilityRegistry::new()),
            cost_table: Arc::new(CostTable::new()),
            chassis_roots: AtomicU64::new(1),
        }
    }

    /// ADR-0080 §5 chassis-mail router installation. Called once by
    /// the chassis builder at boot to wire the closure that handles
    /// mail addressed to [`aether_data::MailboxId::CHASSIS_MAILBOX_ID`].
    /// Subsequent calls are no-ops — the router slot is single-claim.
    pub fn install_chassis_router(&self, router: Box<dyn Fn(Mail) + Send + Sync>) {
        let _ = self.chassis_router.set(router);
    }

    /// ADR-0080 §6 settlement-registry installation. Called once by the
    /// chassis builder at boot alongside [`Self::install_chassis_router`]
    /// so capability handlers can reach the registry via
    /// `ctx.mailer().settlement_registry()`. Single-claim; subsequent
    /// calls are no-ops.
    pub fn install_settlement_registry(&self, registry: Arc<SettlementRegistry>) {
        let _ = self.settlement_registry.set(registry);
    }

    /// Borrow the wired [`SettlementRegistry`], or `None` if no
    /// registry was installed (test fixtures, chassis
    /// that don't bring up the trace pipeline). Capabilities subscribe
    /// via
    /// [`SettlementRegistry::subscribe_settlement_mail`].
    pub fn settlement_registry(&self) -> Option<&Arc<SettlementRegistry>> {
        self.settlement_registry.get()
    }

    /// Swap in a non-default [`TraceHandle`]. Production chassis use the
    /// default handle that [`Self::new`] allocates; tests that want a
    /// pre-seeded handle construct one and pass it here before the `Arc`
    /// wrap.
    #[must_use]
    pub fn with_trace_handle(mut self, handle: TraceHandle) -> Self {
        self.trace_handle = handle;
        self
    }

    /// Borrow the [`TraceHandle`]. Always present — `Mailer::new`
    /// allocates a default handle, and chassis swap in via
    /// [`Self::with_trace_handle`]. Producer-side call sites usually
    /// reach for the shortcut methods on this `Mailer` instead
    /// (`record_sent` / `record_finished` / `acquire_settlement_hold` /
    /// `now_nanos`).
    pub fn trace_handle(&self) -> &TraceHandle {
        &self.trace_handle
    }

    /// ADR-0080 §2 producer hook for the `Sent` event: pushes the trace
    /// event into the producing actor's ring and bumps the root's
    /// emit-time `in_flight` count.
    pub fn record_sent(
        &self,
        mail_id: aether_data::MailId,
        root: aether_data::MailId,
        parent_mail: Option<aether_data::MailId>,
        sender: aether_data::MailboxId,
        recipient: aether_data::MailboxId,
        kind: KindId,
    ) {
        self.trace_handle.record_sent(mail_id, root, parent_mail, sender, recipient, kind);
    }

    /// iamacoffeepot/aether#1150: push the `Sent` trace event with an
    /// explicit flush-begin timestamp (no settlement bump — that fired
    /// eagerly via [`Self::record_sent_inflight`]). The buffered send
    /// path calls this once per mail at flush.
    ///
    /// iamacoffeepot/aether#1158: `t_construct_start` is the instant the
    /// blob opened (the first buffered send of the flush window); `t −
    /// t_construct_start` is the **construct** span.
    #[allow(clippy::too_many_arguments)]
    pub fn record_sent_event_at(
        &self,
        mail_id: aether_data::MailId,
        root: aether_data::MailId,
        parent_mail: Option<aether_data::MailId>,
        sender: aether_data::MailboxId,
        recipient: aether_data::MailboxId,
        kind: KindId,
        t_construct_start: Nanos,
        t: Nanos,
    ) {
        self.trace_handle.record_sent_event_at(
            mail_id,
            root,
            parent_mail,
            sender,
            recipient,
            kind,
            t_construct_start,
            t,
        );
    }

    /// iamacoffeepot/aether#1150: eager settlement-counter increment for
    /// an outbound mail's `root`, split from the `Sent` trace event so
    /// the buffered path keeps `in_flight` exact at send time.
    pub fn record_sent_inflight(&self, root: aether_data::MailId) {
        self.trace_handle.record_sent_inflight(root);
    }

    /// ADR-0080 §2 settlement hook for the `Finished` event: decrements
    /// the root's emit-time `in_flight` count and fires `Settled` on the
    /// zero-transition. (The `Finished` trace event itself is pushed into
    /// the recipient's ring by the dispatch loop.)
    pub fn record_finished(&self, mail_id: Option<aether_data::MailId>, root: Option<aether_data::MailId>) {
        self.trace_handle.record_finished(mail_id, root);
    }

    /// ADR-0080 §12 / iamacoffeepot/aether#716: acquire a settlement
    /// hold on `root`. The returned guard fires `Release` on drop;
    /// every `Mailer` carries a real handle so the contract is
    /// structural.
    ///
    /// `root` is a real chain by type (ADR-0168 §2): a caller with no
    /// causing chain holds nothing and decides itself what its effect
    /// does.
    #[must_use = "a SettlementHold gates root settlement; storing _ silently fires Release"]
    pub fn acquire_settlement_hold(&self, root: aether_data::MailId) -> SettlementHold {
        self.trace_handle.acquire_settlement_hold(root)
    }

    /// Current monotonic `Nanos` timestamp relative to the trace
    /// handle's boot anchor.
    #[must_use]
    pub fn now_nanos(&self) -> Nanos {
        self.trace_handle.now_nanos()
    }

    /// Mint the next chassis root for `kind` to `recipient` and record its
    /// `Sent` (ADR-0080 §6). The root is `MailId(CHASSIS_MAILBOX_ID, n)` with
    /// `n` drawn from this engine's one chassis-root counter, so no two
    /// senders can mint the same root. The caller pushes the returned root
    /// with [`Self::push_minted_root`]; `Spawner::push_tracked` subscribes
    /// its settlement between the two, so the receiver cannot miss an
    /// immediate settle.
    pub(crate) fn mint_chassis_root(&self, recipient: aether_data::MailboxId, kind: KindId) -> MintedRoot {
        let root =
            aether_data::MailId::new(aether_data::MailboxId::CHASSIS_MAILBOX_ID, self.next_chassis_correlation());
        self.record_sent(root, root, None, aether_data::MailboxId::CHASSIS_MAILBOX_ID, recipient, kind);
        MintedRoot { root, recipient, kind }
    }

    /// Push the minted root's mail with `reply_to` (`Source::NONE` for no
    /// reply target) and return the root.
    pub(crate) fn push_minted_root(
        &self,
        minted: MintedRoot,
        payload: Vec<u8>,
        reply_to: Source,
    ) -> aether_data::MailId {
        let (root, recipient, kind) = minted.into_parts();
        self.push(
            Mail::new(recipient, kind, payload, 1).with_lineage(Some(root), Some(root), None).with_reply_to(reply_to),
        );
        root
    }

    /// Draw the next chassis-root correlation, skipping zero, which is the
    /// "no correlation" sentinel.
    fn next_chassis_correlation(&self) -> u64 {
        let id = self.chassis_roots.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            self.chassis_roots.fetch_add(1, Ordering::Relaxed)
        } else {
            id
        }
    }

    /// Attach a `HubOutbound` so mail to unknown mailbox ids bubbles
    /// up to the hub-substrate (ADR-0037 Phase 1) instead of being
    /// warn-dropped. Fluent — returns `self` so the call site can
    /// chain after `Mailer::new`. Skip the call entirely for chassis
    /// that are their own hub or for tests that want local warn-drop
    /// semantics (the hub chassis, the warn-drop test in
    /// `actor::wasm::component`).
    #[must_use]
    pub fn with_outbound(mut self, outbound: Arc<HubOutbound>) -> Self {
        self.outbound = Some(outbound);
        self
    }

    /// Borrow the wired [`HubOutbound`], or `None` if no outbound was
    /// attached (the hub chassis, or tests). Surfaced for chassis caps
    /// that thread egress events (replies, log batches, etc.) back to
    /// the hub without the substrate holding a registry closure for
    /// them.
    pub fn outbound(&self) -> Option<&Arc<HubOutbound>> {
        self.outbound.as_ref()
    }

    /// Borrow the wired [`Registry`]. Issue 603: surfaced so
    /// `ComponentHostCapability::init` can pull the registry for its
    /// internal state without requiring it on `ComponentHostConfig` —
    /// per Resolved Decision §2 registry arrives via init ctx, not
    /// via the cap's config struct.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// A kind's display label, as [`Registry::kind_label`] returns it: the
    /// registered name, or the tagged id's text when the kind is unknown. The
    /// crate-private path behind
    /// [`NativeCtx::kind_label`](crate::actor::native::ctx::NativeCtx::kind_label).
    pub(crate) fn kind_label(&self, kind: KindId) -> String {
        self.registry.kind_label(kind)
    }

    /// Every registered kind descriptor, sorted by name, as
    /// [`Registry::list_kind_descriptors`] returns them. The crate-private
    /// path behind
    /// [`NativeCtx::kind_descriptors`](crate::actor::native::ctx::NativeCtx::kind_descriptors).
    pub(crate) fn kind_descriptors(&self) -> Vec<KindDescriptor> {
        self.registry.list_kind_descriptors()
    }

    /// The origin name of one ADR-0064 tagged id, looked up in the one table
    /// its tag names: the process thread-name registry for `thr-…`, this
    /// registry's route names for `mbx-…`, its kind names for `knd-…`. A miss,
    /// another tag, or malformed text is `None`. The crate-private path behind
    /// [`NativeCtx::tagged_id_name`](crate::actor::native::ctx::NativeCtx::tagged_id_name).
    pub(crate) fn tagged_id_name(&self, tagged: &str) -> Option<String> {
        let raw = tagged_id::decode(tagged).ok()?;
        match tagged_id::tag_of(raw) {
            Some(Tag::Thread) => thread_name::resolve_runtime(raw),
            Some(Tag::Mailbox) => self.registry.mailbox_name(aether_data::MailboxId(raw)),
            Some(Tag::Kind) => self.registry.kind_name(KindId(raw)),
            _ => None,
        }
    }

    /// The canonical path of the live actor `address` names, through
    /// [`Registry::resolve_address`], keeping only the path. The
    /// crate-private path behind
    /// [`NativeCtx::canonical_path`](crate::actor::native::ctx::NativeCtx::canonical_path).
    pub(crate) fn canonical_path(&self, address: &ActorPath) -> Result<String, AddressResolutionError> {
        self.registry.resolve_address(address).map(|resolved| resolved.canonical_path)
    }

    /// The canonical path of the route `reference` proves, as
    /// [`Registry::actor_path`] reads it. The crate-private path behind
    /// [`NativeCtx::actor_path`](crate::actor::native::ctx::NativeCtx::actor_path).
    ///
    /// # Panics
    ///
    /// When the route table holds no record for `reference`: the registry
    /// mints a reference only for a route that holds a record, keeps that
    /// route's proven name through `Dropped`, and removes a route only when
    /// a `Starting` reservation is cancelled, so a missing record is a broken
    /// invariant (ADR-0063).
    pub(crate) fn actor_path(&self, reference: ErasedActorRef) -> ActorPath {
        self.registry
            .actor_path(reference)
            .expect("a minted reference names a route whose proven canonical name the registry keeps for the session")
    }

    /// The reference for a host-stamped position that holds a route record,
    /// as [`Registry::stamped_sender`] mints it. The crate-private path behind
    /// [`NativeCtx::sender`](crate::actor::native::ctx::NativeCtx::sender).
    pub(crate) fn stamped_sender(&self, position: aether_data::MailboxId) -> Option<ErasedActorRef> {
        self.registry.stamped_sender(position)
    }

    /// Prove an address that arrived in a payload: [`Registry::resolve_address`]
    /// expands and resolves it, and the answered position is proven at once
    /// through [`Registry::resolve_live`]. The crate-private path behind
    /// [`NativeCtx::resolve_path`](crate::actor::native::ctx::NativeCtx::resolve_path).
    pub(crate) fn resolve_path(&self, address: &ActorPath) -> Result<ErasedActorRef, ResolvePathError> {
        let ResolvedAddress { mailbox_id, canonical_path } =
            self.registry.resolve_address(address).map_err(ResolvePathError::Unresolved)?;
        self.registry.resolve_live(mailbox_id).map_err(|_| ResolvePathError::NotLive { canonical_path })
    }

    /// The first declared dependency with no `Live` route for an actor placed
    /// under `parent` (`None` for a root placement), as
    /// [`Registry::missing_dependency`] answers it. The crate-private path
    /// behind [`NativeCtx::missing_dependency`](crate::actor::native::ctx::NativeCtx::missing_dependency)
    /// and [`NativeCtx::missing_child_dependency`](crate::actor::native::ctx::NativeCtx::missing_child_dependency).
    #[cfg(feature = "wasm")]
    pub(crate) fn missing_dependency<'a>(
        &self,
        parent: Option<aether_data::MailboxId>,
        dependencies: impl IntoIterator<Item = (u8, &'a str)>,
    ) -> Option<&'a str> {
        self.registry.missing_dependency(parent, dependencies)
    }

    /// The receive surface the capability registry retains for the actor
    /// `actor` proves. The crate-private path behind
    /// [`NativeCtx::receive_surface`](crate::actor::native::ctx::NativeCtx::receive_surface).
    pub(crate) fn receive_surface(&self, actor: ErasedActorRef) -> Option<ComponentCapabilities> {
        self.capability_registry.describe(actor.id())
    }

    /// Build a guest ctx over `binding`, which is always a binding over this
    /// mailer, reading the registry from this mailer's own field. The
    /// crate-private builder behind
    /// [`NativeInitCtx::guest_ctx`](crate::actor::native::NativeInitCtx::guest_ctx) and
    /// [`NativeCtx::guest_ctx`](crate::actor::native::ctx::NativeCtx::guest_ctx).
    #[cfg(feature = "wasm")]
    pub(crate) fn guest_ctx(&self, binding: Arc<NativeBinding>, outbound: Arc<HubOutbound>) -> ComponentCtx {
        ComponentCtx::new(binding, Arc::clone(&self.registry), outbound)
    }

    /// Subscribe `target` to the registry's inventory changes, as
    /// [`Registry::subscribe_inventory`] does, through this mailer. The
    /// crate-private path behind
    /// [`NativeCtx::subscribe_inventory`](crate::actor::native::ctx::NativeCtx::subscribe_inventory),
    /// which passes its own binding's mailbox as `target`.
    pub(crate) fn subscribe_inventory_for(self: &Arc<Self>, target: aether_data::MailboxId) -> RegistrySubscription {
        self.registry.subscribe_inventory(target, Arc::clone(self))
    }

    /// Borrow the wired [`CapabilityRegistry`]
    /// (iamacoffeepot/aether#1037). The component-load / native-cap-boot
    /// path registers mailbox caps through this handle; the DAG
    /// validator (iamacoffeepot/aether#975) reads `accepts` /
    /// `has_fallback` on the submit path. Shared via the `Mailer` so any
    /// actor with `ctx.mailer()` reaches the same registry — mirroring
    /// how [`Self::registry`] surfaces the routing table.
    pub fn capability_registry(&self) -> &Arc<CapabilityRegistry> {
        &self.capability_registry
    }

    /// The crate-private builder behind [`NativeInitCtx::actor_probe`](crate::actor::native::NativeInitCtx::actor_probe).
    pub(crate) fn actor_probe(&self) -> ActorProbe {
        ActorProbe::new(Arc::clone(&self.registry), Arc::clone(&self.capability_registry))
    }

    /// Borrow the wired [`CostTable`] (iamacoffeepot/aether#1128). The
    /// component-load / native-cap-boot path seeds it (alongside the
    /// capability registry's accept-set); the `cost.tail` dispatch arm
    /// dumps it, and a future iamacoffeepot/aether#1178 recruiter sums
    /// recipient-group cells from it at flush. Shared via the `Mailer`
    /// so any actor with `ctx.mailer()` reaches the same table —
    /// mirroring how [`Self::capability_registry`] surfaces its sibling
    /// index.
    pub fn cost_table(&self) -> &Arc<CostTable> {
        &self.cost_table
    }

    /// Hand `mail` to the substrate for dispatch. `Inbox`-bound
    /// mailboxes run their handler on the caller thread; dropped /
    /// unknown recipients warn-and-discard (or bubble up to the
    /// hub-substrate when a `HubOutbound` is connected, per ADR-0037).
    pub fn push(&self, mail: Mail) {
        route_mail(mail, self);
    }

    pub(super) fn install_route_relay(&self, relay: RouteRelayHandle) {
        assert!(self.route_relay.set(relay).is_ok(), "a mailer can attach only one registry route relay");
    }

    /// The route relay queue's admission and drain accounting (issue 4122),
    /// or `None` before a relay is attached. The sibling of
    /// [`Registry::owner_queue_metrics`] — read together they say which of
    /// the two ADR-0165 drainers is the one falling behind.
    #[must_use]
    pub fn route_relay_metrics(&self) -> Option<RegistryQueueMetrics> {
        self.route_relay.get().map(RouteRelayHandle::metrics)
    }

    pub(super) fn relay_captured(&self, continuation: RouteContinuation) {
        let relay = self.route_relay.get().expect("registry-owner routing requires an attached route relay");
        assert!(
            relay.submit(continuation).is_none(),
            "registry owner remained active after its route relay stopped accepting"
        );
    }

    pub(super) fn relay_mail(&self, mail: Mail) {
        let disposition = self.registry.route_lookup(mail.kind, mail.recipient).into_captured();
        self.relay_captured(RouteContinuation { mail, disposition });
    }

    pub(super) fn route_captured(&self, continuation: RouteContinuation) {
        route_tail(continuation.mail, continuation.disposition, self);
    }

    /// Route a `*Result` reply to `sender` with a single encode and **no
    /// inherited lineage** — the reply opens as a fresh, lineage-less mail
    /// (no id, root, or parent), detached from any caller settlement
    /// chain. The name says so loudly: detachment is a deliberate choice,
    /// not the default a short name invites.
    ///
    /// Reach for this only on the two arms where lineage is structurally
    /// moot: wire-terminal replies whose `sender` is a `Session` /
    /// `EngineMailbox` (the hub-wire boundary is terminal for engine-side
    /// chain accounting — the lineage would not be applied anyway), and
    /// substrate-internal synthesized replies with no caller chain to join.
    ///
    /// A reply that should join the caller's ADR-0080 causal chain goes
    /// through the lineage-carrying path instead:
    /// [`Self::send_reply`] (the
    /// `NativeBinding::send_reply_for_handler` form),
    /// [`InboundMail::reply`](crate::chassis::inbox::InboundMail::reply)
    /// (the claimed-inbox drain guard), or the typed `ctx.reply()` on a
    /// handler context. Routing a `Component`-addressed reply through this
    /// unchained form silently detaches it from the caller's settlement
    /// window — the bug class #1701 fixed for handler replies.
    ///
    /// Equivalent to [`Self::send_reply`] with an absent lineage
    /// triple — the bare form is the lineage form's chassis-root case,
    /// as `NativeBinding::push_envelope_buffered` is with `None` / `None`.
    pub fn send_reply_unchained<K>(&self, sender: Source, result: &K) -> bool
    where
        K: Kind,
    {
        self.send_reply(sender, result, None, None, None)
    }

    /// ADR-0080 §5/§6: route a `*Result` reply that joins the caller's
    /// causal chain. `Session` / `EngineMailbox` hand off to the hub
    /// outbound (unchanged hub-wire format — the wire boundary is
    /// terminal for engine-side chain accounting, so the lineage is not
    /// applied there); `Component` records the reply's `Sent` and pushes
    /// a lineage-stamped `Mail` into the target component's inbox so the
    /// guest's normal dispatch path delivers the reply. `None` is a
    /// silent drop — nobody asked for a reply.
    ///
    /// The reply mail carries `reply_to = None`: the receiver isn't
    /// expected to reply to a reply, and decorating with the
    /// closure-bound mailbox's id would produce a
    /// `ReplyEntry::Component` pointing at an entry that can't itself
    /// receive mail.
    ///
    /// The producer hook (`record_sent`) is what keeps the §6 hold-
    /// contract exact: a synchronous in-handler reply's `Sent` is
    /// recorded before the replying handler's own `Finished`, so the
    /// caller's chain stays open until the reply's `Finished` lands. The
    /// replier is `reply_id.sender` — minted in its own id space by
    /// `NativeBinding::send_reply_for_handler`. The hook runs only when
    /// both `reply_id` and `root` are present; an absent `reply_id`
    /// stamps no lineage, reproducing the pre-lineage reply shape for
    /// callers without a handler chain (the bare
    /// [`Self::send_reply_unchained`]).
    pub fn send_reply<K>(
        &self,
        sender: Source,
        result: &K,
        reply_id: Option<aether_data::MailId>,
        root: Option<aether_data::MailId>,
        parent: Option<aether_data::MailId>,
    ) -> bool
    where
        K: Kind,
    {
        // ADR-0100: each arm encodes the reply through the kind's declared
        // codec (cast or wire), not a hardcoded codec path.
        match sender.addr {
            SourceAddr::None => false,
            SourceAddr::Session(_) | SourceAddr::EngineMailbox { .. } => {
                self.send_hub_reply(sender, K::ID, K::NAME, result.encode_into_bytes(), reply_id)
            }
            SourceAddr::Component(_) => {
                self.send_component_reply(sender, K::ID, result.encode_into_bytes(), reply_id, root, parent)
            }
        }
    }

    /// [`Self::send_reply`] for an already-encoded reply of `kind`, which
    /// routes, stamps, and records exactly as the typed form does. The hub
    /// arms name the kind from its registry descriptor, since no `K::NAME`
    /// is in hand; a kind the registry cannot name is warned and not sent.
    /// Its caller is `NativeBinding::send_reply_envelope_for_handler`.
    pub(crate) fn send_reply_envelope(
        &self,
        sender: Source,
        kind: KindId,
        bytes: &[u8],
        reply_id: Option<aether_data::MailId>,
        root: Option<aether_data::MailId>,
        parent: Option<aether_data::MailId>,
    ) -> bool {
        match sender.addr {
            SourceAddr::None => false,
            SourceAddr::Session(_) | SourceAddr::EngineMailbox { .. } => {
                let Some(kind_name) = self.registry.kind_name_shared(kind) else {
                    tracing::warn!(target: "aether_substrate::mail", kind = %kind, "reply of an unregistered kind not sent");
                    return false;
                };
                self.send_hub_reply(sender, kind, &kind_name, bytes.to_vec(), reply_id)
            }
            SourceAddr::Component(_) => self.send_component_reply(sender, kind, bytes.to_vec(), reply_id, root, parent),
        }
    }

    /// The `Session` / `EngineMailbox` arm of the reply path: hand the
    /// encoded reply to the hub outbound, stamped with the replying actor.
    fn send_hub_reply(
        &self,
        sender: Source,
        kind: KindId,
        kind_name: &str,
        payload: Vec<u8>,
        reply_id: Option<aether_data::MailId>,
    ) -> bool {
        // The replier minted `reply_id` in its own id space, so its sender
        // half is the replying actor: stamped on the session reply for an
        // embedder to read (ADR-0230 §3), when that position holds a route.
        let stamp = reply_id.and_then(|id| self.registry.stamped_sender(id.sender));
        self.outbound
            .as_ref()
            .is_some_and(|outbound| outbound.send_reply_envelope_stamped(sender, kind, kind_name, payload, stamp))
    }

    /// The `Component` arm of the reply path: record the reply's `Sent` and
    /// push a lineage-stamped `Mail` into the target component's inbox.
    fn send_component_reply(
        &self,
        sender: Source,
        kind: KindId,
        payload: Vec<u8>,
        reply_id: Option<aether_data::MailId>,
        root: Option<aether_data::MailId>,
        parent: Option<aether_data::MailId>,
    ) -> bool {
        let SourceAddr::Component(mailbox) = sender.addr else {
            return false;
        };
        // ADR-0042: echo the caller's correlation_id onto the reply envelope
        // so the originating handler can pick the right reply out of the
        // mpsc by correlation. Reply target is None — nobody replies to a
        // reply.
        let reply_to = Source::with_correlation(SourceAddr::None, sender.correlation_id);
        // ADR-0080 §2 producer hook: record the reply's `Sent` before pushing
        // it, so the caller's `root` counts the reply in-flight until its
        // `Finished`. Skipped when the reply carries no id (the bare
        // `send_reply_unchained` path) or no root, since a rootless `Sent`
        // counts against no chain.
        if let (Some(reply_id), Some(root)) = (reply_id, root) {
            self.record_sent(reply_id, root, parent, reply_id.sender, mailbox, kind);
        }
        self.push(Mail::new(mailbox, kind, payload, 1).with_reply_to(reply_to).with_lineage(reply_id, root, parent));
        true
    }
}

/// Resolve `mail.recipient` against the registry and dispatch
/// inline. `Inbox`-bound mailboxes forward to an actor's mpsc on the caller
/// thread (or fan out via the cap's mpsc, depending on the closure).
/// Dropped / unknown recipients warn-log and drop the mail.
// Routing pipeline runs as one function: chassis-mail switch, registry
// lookup, dispatch, outbound forward. Splitting the steps would scatter
// the per-mail buffer handling and lose the linear "where does this
// envelope go?" read.
#[allow(clippy::too_many_lines)]
fn route_mail(mail: Mail, mailer: &Mailer) {
    let registry = &mailer.registry;
    let outbound = mailer.outbound.as_ref();
    let chassis_router = mailer.chassis_router.get().map(|router| &**router);
    let trace_handle = &mailer.trace_handle;
    // ADR-0080 §5 chassis-mail switch — routed ahead of the registry
    // lookup so mail to `CHASSIS_MAILBOX_ID` reaches the chassis-
    // installed router without bubbling up as `UnresolvedMail`. Today
    // the router decodes `Settled { root }` and signals the
    // `SettlementRegistry`; future chassis-internal kinds add
    // matching arms inside the router closure.
    if mail.recipient == aether_data::MailboxId::CHASSIS_MAILBOX_ID {
        // ADR-0080 §2 producer hook: balance the `Sent` so settlement
        // chains drain (issue 838). Today the only chassis-addressed
        // kind is `Settled` itself, which is pushed bare without
        // lineage by `TraceDispatchCapability::fire_settled`, so the
        // absent-id short-circuit inside `record_finished`
        // no-ops. Stamped kinds (future debugger / describe_tree
        // replies) get the symmetric `Received`/`Finished` bracket.
        let inbound_mail_id = mail.mail_id;
        let inbound_root = mail.root;
        // Chassis-addressed mail records only `Finished` (settlement
        // balance) below — post-ADR-0086 Phase 3c there is no central
        // `Received` trace event; the chassis-host ring's `Sent`s arrive
        // via `record_sent` directly.
        if mail.kind == TraceTail::ID {
            // ADR-0086 Phase 3b: the chassis-host trace ring holds the
            // off-actor root `Sent`s — every injected root carries
            // `sender = CHASSIS_MAILBOX_ID`, so the guided walk seeds at
            // `root.sender`, which lands here over the wire. Answer the
            // tail and reply to the caller. (In-process callers reach
            // the same ring via `TraceHandle::chassis_host_tail`.)
            let result = TraceTail::decode_from_bytes(mail.payload.bytes()).map_or_else(
                || TraceTailResult::Err { error: "undecodable TraceTail to chassis-host ring".to_owned() },
                |request| trace_handle.chassis_host_tail(&request),
            );
            match mail.reply_to.addr {
                SourceAddr::Session(_) | SourceAddr::EngineMailbox { .. } => {
                    if let Some(outbound) = outbound {
                        outbound.send_reply(mail.reply_to, &result);
                    }
                }
                SourceAddr::Component(target) => {
                    // The MCP Call path replies via Component (mirrors
                    // the `LogTail`-to-unknown arm below): re-route a
                    // fresh, un-lineaged reply into the target's inbox.
                    // ADR-0100: encode through the kind's declared codec.
                    let payload = result.encode_into_bytes();
                    let reply_to = Source::with_correlation(SourceAddr::None, mail.reply_to.correlation_id);
                    route_mail(Mail::new(target, TraceTailResult::ID, payload, 1).with_reply_to(reply_to), mailer);
                }
                SourceAddr::None => {}
            }
        } else if let Some(router) = chassis_router {
            router(mail);
        } else {
            tracing::warn!(
                target: "aether_substrate::queue",
                kind = %mail.kind,
                "chassis-addressed mail dropped — no chassis router installed",
            );
        }
        trace_handle.record_finished(inbound_mail_id, inbound_root);
        return;
    }

    // Route and immutable kind metadata come from wait-free point-in-time
    // publications. The lookup owns the endpoint facade used below.
    let mut current = mail;
    loop {
        let lookup = registry.route_lookup(current.kind, current.recipient);
        let activation_control = current.kind == ACTIVATION_BARRIER_KIND;
        if activation_control || lookup.is_starting() || lookup.is_unknown() {
            match registry.park_or_drop(current, lookup.generation()) {
                ParkAdmission::Queued => return,
                ParkAdmission::Retry(returned) => {
                    current = returned;
                    continue;
                }
                ParkAdmission::Closed(returned) if activation_control => {
                    mailer.record_finished(returned.mail_id, returned.root);
                    return;
                }
                // A closed owner and a shed miss (issue 4122) end the same
                // way: the envelope never reaches an owner turn, so the
                // terminal tail runs here. For the shed that is precisely
                // ADR-0165's "absent -> apply the existing unknown-recipient
                // policy" — bubble to the hub if one is attached, otherwise
                // warn-drop, balancing settlement either way so no chain is
                // left open. Only an ordinary miss is sheddable (an
                // activation barrier takes the `activation_control` arm
                // above), so the disposition here is always `Unknown`.
                ParkAdmission::Closed(returned) | ParkAdmission::Shed(returned) => {
                    route_tail(returned, lookup.into_captured(), mailer);
                    return;
                }
            }
        }
        route_tail(current, lookup.into_captured(), mailer);
        return;
    }
}

#[allow(clippy::too_many_lines)]
fn route_tail(mail: Mail, disposition: CapturedDisposition, mailer: &Mailer) {
    let recipient = mail.recipient;
    let inbound_mail_id = mail.mail_id;
    let inbound_root = mail.root;

    match disposition {
        CapturedDisposition::Live { endpoint: RouteEndpoint::Inbox { handler, .. } } => {
            // Mail reaching a closure-bound mailbox through `push`
            // came from substrate core or a chassis (e.g. the frame
            // loop's FrameStats push, platform input fan-out). Per
            // ADR-0011 origin is `None`. Components reach
            // closure-bound mailboxes via `ComponentCtx::send` inline
            // and never enter `push`.
            //
            // ADR-0080 §2 producer-hook note (issue 838): no
            // `Received`/`Finished` bracket fires here. `Inbox`
            // is the actor-enqueue variant — the handler body
            // pushes the envelope onto an mpsc inbox, and the
            // actor's dispatch loop at `actor/native/slot/dispatch.rs`
            // records the bracket downstream when its worker picks
            // the envelope up. Adding a bracket here would
            // double-count `Finished` and fire settlement
            // prematurely (surfaced by
            // the rpc engine-routing suite as
            // ReplyEnd before ReplyEvent). Synchronous endpoints take
            // the inline facade path below and get the bracket because
            // nothing downstream owns it.
            //
            // iamacoffeepot/aether#848: the payload moves into
            // [`OwnedDispatch`] rather than being borrowed.
            // The handler is `Arc<dyn InboxHandler>` whose `enqueue`
            // takes owned bytes — cap closures move directly into
            // their downstream envelope (via
            // `Envelope::from(OwnedDispatch)`) with zero payload
            // copies.
            // ADR-0094: the first of two production mint sites. The
            // dispatch is armed here; the downstream actor dispatcher
            // (`slot::dispatcher::dispatch_one`) discharges it beside its
            // `record_finished`. The relay closure that forwards it onto
            // the actor's mpsc (`spawn.rs` / `chassis/ctx.rs`) is a
            // transfer — the obligation rides the moved value.
            handler.enqueue(OwnedDispatch::armed(
                mail.kind,
                None,
                mail.reply_to,
                mail.payload,
                mail.count,
                mail.mail_id,
                mail.root,
                mail.parent_mail,
                // iamacoffeepot/aether#1134: stamp the deposit instant +
                // scheduler backlog here — the single Inbox chokepoint
                // every mail-to-an-actor funnels through. Read back at the
                // recipient's `Received` hook to split the hop into
                // send→enqueue vs queue residence. One clock read on the
                // already-traced path; depth is `0` off a pool worker.
                mailer.trace_handle.now_nanos(),
                pending_depth(),
                recipient,
            ));
        }
        CapturedDisposition::Live { endpoint: RouteEndpoint::Inline(handler) } => {
            handler.dispatch(MailDispatch {
                kind: mail.kind,
                origin: None,
                sender: mail.reply_to,
                payload: mail.payload.bytes(),
                count: mail.count,
                mail_id: mail.mail_id,
                root: mail.root,
                parent_mail: mail.parent_mail,
            });
            // ADR-0080 §2: synchronous handler. Records `Finished`
            // (settlement) after the inline call so the chain's
            // `in_flight` balances and settlement subscribers wake
            // (issue 838). Distinct from `Inbox` above — see that arm's
            // doc for the double-count-prematurely-settle hazard the
            // split avoids. (Post-ADR-0086 Phase 3c there is no
            // `Received` trace event here — inline mailboxes have no
            // per-actor ring; only settlement is recorded.)
            mailer.trace_handle.record_finished(inbound_mail_id, inbound_root);
        }
        CapturedDisposition::Dropped => {
            // A dropped route keeps its name, so the line names the actor
            // rather than its position.
            tracing::warn!(
                target: "aether_substrate::queue",
                actor = %mailer.registry.mailbox_name(recipient).unwrap_or_else(|| recipient.to_string()),
                "mail to dropped mailbox — discarded",
            );
            // ADR-0080 §2: balance the `Sent` so settlement chains
            // drain (issue 838). No `Received` — no handler ran.
            mailer.trace_handle.record_finished(inbound_mail_id, inbound_root);
        }
        CapturedDisposition::Unknown => {
            // ADR-0037 Phase 1: unknown-locally mailboxes bubble up
            // to the hub-substrate when a live outbound is wired.
            // The hub resolves the id against its own registry and
            // dispatches; if it doesn't know the id either, it
            // warns on its side (end-of-line). Fall back to the
            // local warn-drop when no hub is attached (single-host
            // dev, or the hub chassis itself).
            if let Some(outbound) = mailer.outbound.as_ref()
                && outbound.is_connected()
            {
                // ADR-0037 Phase 2: carry the local sending
                // component's mailbox id so the hub can build a
                // `Source::EngineMailbox { engine_id, mailbox_id }`
                // for the receiving component. `None` for mail
                // with no local component origin (substrate-generated).
                // Recovered from
                // `reply_to.addr = Component(_)` set by
                // `ComponentCtx::send` / `NativeBinding::send_mail`
                // (issue #644).
                let source_mailbox_id = match mail.reply_to.addr {
                    SourceAddr::Component(id) => Some(id),
                    _ => None,
                };
                // ADR-0042: carry the correlation through the bubble-
                // up frame so a reply coming back via Phase-2 reply
                // routing carries the id the originating handler matches on.
                let correlation_id = mail.reply_to.correlation_id;
                outbound.egress_unresolved_mail(
                    recipient,
                    mail.kind,
                    mail.payload.into_vec(),
                    mail.count,
                    source_mailbox_id,
                    correlation_id,
                );
                // ADR-0080 §2: per-engine settlement (issue 838) —
                // the local engine treats egress-to-hub as "Finished
                // from our perspective." The hub processes the
                // bubbled-up mail on its own settlement domain; no
                // wire signal exists today for federated cross-engine
                // settlement (and the issue body parks that design).
                mailer.trace_handle.record_finished(inbound_mail_id, inbound_root);
                return;
            }
            // Issue 963: `aether.log.tail` (`actor_logs`) to an
            // unresolved mailbox synthesizes a `LogTailResult::Err`
            // reply instead of silently warn-dropping, so the MCP
            // caller's `call_one` sees one `ReplyEvent` (a clean "that
            // mailbox doesn't exist" signal) rather than `got 0
            // replies`. Narrow per-kind treatment (Option B) — the
            // general `aether.mail.unresolved` reply for every
            // reply-expecting kind is parked (Option C, lib.rs:86).
            // Only the `Component` reply target (the path the MCP Call
            // takes via `RpcServerCapability::handle_call`) is routed;
            // `Session`/`EngineMailbox` targets fall through to the
            // warn-drop, keeping the blast radius minimal.
            if mail.kind.0 == <aether_kinds::LogTail as Kind>::ID.0 {
                let err =
                    aether_kinds::LogTailResult::Err { error: format!("mailbox {recipient} not registered on engine") };
                // ADR-0100: encode through the kind's declared codec.
                let payload = err.encode_into_bytes();
                if let SourceAddr::Component(target) = mail.reply_to.addr {
                    let reply_to = Source::with_correlation(SourceAddr::None, mail.reply_to.correlation_id);
                    route_mail(
                        Mail::new(target, <aether_kinds::LogTailResult as Kind>::ID, payload, 1)
                            .with_reply_to(reply_to),
                        mailer,
                    );
                }
                // The synthesized reply is a fresh un-lineaged mail
                // (no mail id); the inbound still records `Finished`
                // so its settlement chain balances (issue 838).
                mailer.trace_handle.record_finished(inbound_mail_id, inbound_root);
                return;
            }
            tracing::warn!(
                target: "aether_substrate::queue",
                mailbox = %recipient,
                "mail to unknown mailbox — dropped",
            );
            // ADR-0080 §2: balance the `Sent` so settlement chains
            // drain (issue 838). Without this, a component that sends
            // to an unloaded mailbox every tick would leave every Tick
            // chain with an orphaned `Sent` that never settles.
            mailer.trace_handle.record_finished(inbound_mail_id, inbound_root);
        }
    }
}

#[cfg(test)]
// Mailer integration tests stage senders / receivers / multi-step
// dispatch fixtures inline so the round-trip read top-to-bottom;
// extracting helpers would split the path-under-test across files.
#[allow(clippy::too_many_lines)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::mail::MailboxId;
    use crate::mail::outbound::EgressEvent;
    use crate::mail::registry::{InboxHandler, InlineHandler};
    use crate::testing::boot_authority;
    use aether_data::Kind;
    use aether_data::wire;
    use aether_data::{KindDescriptor, NamedField, Primitive, SchemaType};
    use std::borrow::Cow;

    /// ADR-0037 Phase 1: a live outbound + unknown mailbox id
    /// forwards `MailToHubSubstrate` upstream instead of
    /// warn-dropping. The forwarded frame carries the exact
    /// mailbox id / kind / payload / count the caller pushed.
    #[test]
    fn unknown_mailbox_with_connected_outbound_bubbles_up() {
        let (outbound, outbound_rx) = HubOutbound::attached_loopback();
        let registry = Arc::new(Registry::new());

        let mailer = Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound));

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        let kind = KindId(0xABCD_u64);
        let payload = vec![1, 2, 3];
        mailer.push(Mail::new(unknown, kind, payload.clone(), 1));

        let event = outbound_rx.try_recv().expect("bubble-up event emitted");
        match event {
            EgressEvent::UnresolvedMail { recipient_mailbox_id, kind_id, payload: p, count, .. } => {
                assert_eq!(recipient_mailbox_id, unknown);
                assert_eq!(kind_id, kind);
                assert_eq!(p, payload);
                assert_eq!(count, 1);
            }
            other => panic!("expected UnresolvedMail, got {other:?}"),
        }
    }

    /// No outbound wired (or disconnected): unknown mailbox stays
    /// warn-drop. Asserts by showing the outbound channel stays
    /// empty even though we pushed — the warn-drop path doesn't
    /// generate a frame.
    #[test]
    fn unknown_mailbox_without_outbound_warn_drops() {
        let registry = Arc::new(Registry::new());
        let mailer = Mailer::new(Arc::clone(&registry));
        // Deliberately no `with_outbound` — exercises the local
        // warn-drop path (the hub chassis path).

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        mailer.push(Mail::new(unknown, KindId(0xABCD), vec![], 0));
        // No panic is the test; the warn path logs and returns.
    }

    /// Issue 963: recorder capture row — `(kind, correlation, payload)`
    /// for each reply a stand-in RPC-server mailbox receives. Aliased
    /// to keep the `Arc<RwLock<Vec<...>>>` off the `type_complexity`
    /// lint at the two recorder sites below.
    type RecordedReplies = Arc<RwLock<Vec<(KindId, u64, Vec<u8>)>>>;

    /// Register an inline mailbox that records each reply's kind,
    /// correlation, and payload — a stand-in for the RPC-server reply
    /// target the MCP Call's `Component` reply hop lands at. Returns
    /// the recorder's `MailboxId` plus the shared capture buffer.
    fn record_inline(registry: &Registry) -> (MailboxId, RecordedReplies) {
        let recorded: RecordedReplies = Arc::new(RwLock::new(Vec::new()));
        let recorded_for_handler = Arc::clone(&recorded);
        let recorder_id = registry.register_inline(
            &boot_authority(),
            "test.rpc_server_reply",
            Arc::new(move |dispatch: MailDispatch<'_>| {
                recorded_for_handler.write().unwrap().push((
                    dispatch.kind,
                    dispatch.sender.correlation_id,
                    dispatch.payload.to_vec(),
                ));
            }),
        );
        (recorder_id, recorded)
    }

    /// Issue 963: `aether.log.tail` to an unregistered mailbox (no
    /// outbound) synthesizes a `LogTailResult::Err` reply routed back
    /// to the inbound's `Component` reply target instead of warn-
    /// dropping. Stands in the RPC-server reply mailbox with a
    /// recording inline handler; asserts exactly one reply of kind
    /// `LogTailResult::ID`, decoding to `Err { error }` naming the
    /// recipient id, with the correlation echoed.
    #[test]
    fn unknown_mailbox_log_tail_synthesizes_err_reply() {
        use aether_kinds::{LogTail, LogTailResult};

        let registry = Arc::new(Registry::new());
        let mailer = Mailer::new(Arc::clone(&registry));

        let (recorder_id, recorded) = record_inline(&registry);

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        mailer.push(
            Mail::new(unknown, <LogTail as Kind>::ID, vec![], 1)
                .with_reply_to(Source::with_correlation(SourceAddr::Component(recorder_id), 0xCAFE)),
        );

        let recorded = recorded.read().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one synthesized reply");
        let (kind, correlation, payload) = &recorded[0];
        assert_eq!(*kind, <LogTailResult as Kind>::ID);
        assert_eq!(*correlation, 0xCAFE, "correlation echoed onto the reply");
        match LogTailResult::decode_from_bytes(payload).unwrap() {
            LogTailResult::Err { error } => {
                assert!(error.contains(&unknown.to_string()), "error names the recipient id: {error}");
            }
            other @ LogTailResult::Ok { .. } => {
                panic!("expected LogTailResult::Err, got {other:?}")
            }
        }
        drop(recorded);
    }

    /// Issue 963: the synthesized-Err branch is narrow — a non-
    /// `LogTail` kind to an unregistered mailbox still warn-drops with
    /// no reply, even with a live `Component` reply target. Pins the
    /// scope so the change doesn't start replying for every kind
    /// (Option B, not Option C).
    #[test]
    fn unknown_mailbox_non_log_tail_still_warn_drops() {
        let registry = Arc::new(Registry::new());
        let mailer = Mailer::new(Arc::clone(&registry));

        let (recorder_id, recorded) = record_inline(&registry);

        let unknown = MailboxId(0xDEAD_BEEF_u64);
        // Arbitrary non-`LogTail` kind id — the reply branch must not fire.
        mailer.push(
            Mail::new(unknown, KindId(0xABCD), vec![], 1)
                .with_reply_to(Source::with_correlation(SourceAddr::Component(recorder_id), 0xCAFE)),
        );

        assert!(recorded.read().unwrap().is_empty(), "non-LogTail unknown-mailbox mail warn-drops with no reply");
    }

    /// ADR-0086 Phase 3b: `aether.trace.tail` to `CHASSIS_MAILBOX_ID`
    /// answers from the chassis-host ring and replies to the inbound's
    /// `Component` target — the hop the MCP's `send_mail_traced` guided
    /// walk takes to fetch the off-actor root `Sent` over the wire (the
    /// in-process harness reaches the same ring via
    /// `TraceHandle::chassis_host_tail`). Seeds the ring with one
    /// chassis-root `Sent`, queries it, and asserts the recorded reply
    /// is a `TraceTailResult::Ok` carrying that `Sent`, correlation
    /// echoed.
    #[test]
    fn chassis_host_trace_tail_replies_to_component_target() {
        use aether_kinds::trace::{TraceEvent, TraceTail, TraceTailResult};

        let registry = Arc::new(Registry::new());
        let mailer = Mailer::new(Arc::clone(&registry));

        let (recorder_id, recorded) = record_inline(&registry);

        // An off-actor chassis-root mail records its `Sent` in the
        // chassis-host ring (the recipient is unregistered and the mail
        // itself warn-drops, but the off-actor `Sent` still lands).
        let root =
            mailer.push_minted_root(mailer.mint_chassis_root(MailboxId(0x1234), KindId(0xFEED)), vec![], Source::NONE);

        // Query the chassis-host ring for that root, replying to a
        // `Component` target (the MCP RPC-server reply hop).
        let request = TraceTail { max: 0, since: None, root: Some(root) };
        mailer.push(
            Mail::new(MailboxId::CHASSIS_MAILBOX_ID, TraceTail::ID, request.encode_into_bytes(), 1)
                .with_reply_to(Source::with_correlation(SourceAddr::Component(recorder_id), 0xCAFE)),
        );

        let recorded = recorded.read().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one TraceTailResult reply");
        let (kind, correlation, payload) = &recorded[0];
        assert_eq!(*kind, TraceTailResult::ID);
        assert_eq!(*correlation, 0xCAFE, "correlation echoed onto the reply");
        match TraceTailResult::decode_from_bytes(payload).unwrap() {
            TraceTailResult::Ok { entries, .. } => assert!(
                entries.iter().any(|e| e.root == root && matches!(e.event, TraceEvent::Sent { .. })),
                "the chassis-host root Sent came back: {entries:?}",
            ),
            TraceTailResult::Err { error } => panic!("expected Ok, got Err {error}"),
        }
        drop(recorded);
    }

    // ADR-0045 Ref-resolution integration

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct Note {
        body: String,
        seq: u32,
    }
    impl Kind for Note {
        const NAME: &'static str = "test.mailer_note";
        // Stable test sentinel — distinct from real schema-hashed kind ids.
        const ID: KindId = KindId(0xDEAD_BEEF_0003_0001);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            wire::from_bytes(bytes).ok()
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            wire::to_vec(self).expect("wire encode to Vec is infallible")
        }
    }

    fn note_schema() -> SchemaType {
        SchemaType::Struct {
            fields: Cow::Owned(vec![
                NamedField { name: Cow::Borrowed("body"), ty: SchemaType::String },
                NamedField { name: Cow::Borrowed("seq"), ty: SchemaType::Scalar(Primitive::U32) },
            ]),
            repr_c: false,
        }
    }

    /// Capture-bytes sink: records every payload it receives so a
    /// test can assert what bytes the dispatcher delivered.
    struct CapturingSink {
        captured: Arc<RwLock<Vec<Vec<u8>>>>,
        delivery_count: Arc<AtomicUsize>,
    }
    impl CapturingSink {
        fn new() -> Self {
            Self { captured: Arc::new(RwLock::new(Vec::new())), delivery_count: Arc::new(AtomicUsize::new(0)) }
        }
        /// Inline-variant handler: synchronous capture body. Used by
        /// the `register_inline` test sites; mailer brackets the
        /// call so settlement balances.
        fn inline_handler(&self) -> Arc<dyn InlineHandler> {
            let captured = Arc::clone(&self.captured);
            let count = Arc::clone(&self.delivery_count);
            Arc::new(move |dispatch: MailDispatch<'_>| {
                captured.write().unwrap().push(dispatch.payload.to_vec());
                count.fetch_add(1, Ordering::SeqCst);
            })
        }

        /// Inbox-variant handler: receives [`OwnedDispatch`] and
        /// moves the payload into the captured Vec (no `to_vec()`
        /// clone). Used by `register_inbox` test sites that exercise
        /// the actor-enqueue dispatch path. Bracket owned downstream
        /// — these tests don't subscribe to settlement so the
        /// "downstream never finishes" path is intentional and
        /// harmless.
        fn inbox_handler(&self) -> Arc<dyn InboxHandler> {
            let captured = Arc::clone(&self.captured);
            let count = Arc::clone(&self.delivery_count);
            Arc::new(move |dispatch: OwnedDispatch| {
                // ADR-0094: terminal test consumer — discharge the
                // obligation (these tests don't subscribe to settlement;
                // the obligation ends here) before the partial-move below.
                dispatch.discharge();
                captured.write().unwrap().push(dispatch.payload.into_vec());
                count.fetch_add(1, Ordering::SeqCst);
            })
        }
    }

    fn make_mailer() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        (registry, mailer)
    }

    /// Cast-shaped reply kind with a non-`f32` field — its
    /// `encode_into_bytes` is the raw cast image.
    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
    struct CastReply {
        code: u32,
        flag: u16,
        _pad: u16,
    }
    impl Kind for CastReply {
        const NAME: &'static str = "test.mailer_cast_reply";
        const ID: KindId = KindId(0xDEAD_BEEF_0003_0003);

        fn encode_into_bytes(&self) -> Vec<u8> {
            bytemuck::bytes_of(self).to_vec()
        }
    }

    /// ADR-0100: `Mailer::send_reply_unchained` to a `Component` target encodes the
    /// reply through `Kind::encode_into_bytes`. A cast reply kind reaches
    /// the sink as its raw cast image, not a wire image — and
    /// a `Pod`-without-`Serialize` kind is repliable at all.
    #[test]
    fn send_reply_cast_kind_delivers_cast_image() {
        let (registry, mailer) = make_mailer();
        let sink = CapturingSink::new();
        let sink_id = registry.register_inbox(&boot_authority(), "test.sink", sink.inbox_handler());

        let reply = CastReply { code: 0x1122_3344, flag: 0xABCD, _pad: 0 };
        let sent = mailer.send_reply_unchained(Source::with_correlation(SourceAddr::Component(sink_id), 1), &reply);
        assert!(sent, "Component reply target routes");

        let captured = sink.captured.read().unwrap().clone();
        assert_eq!(captured, vec![bytemuck::bytes_of(&reply).to_vec()], "reply payload is the cast image");
    }

    /// #1695 / ADR-0080 §5/§6: a `Component`-addressed reply routed
    /// through `send_reply` carries the caller's `root` +
    /// `parent` and a real `mail_id`, and records the
    /// reply's `Sent` against the caller root so the chain stays open
    /// until the reply's `Finished` (the conformance fix — replies were
    /// lineage-less mail). The follow-up `record_finished`
    /// (standing in for the recipient dispatcher) balances that `Sent`
    /// exactly, reclaiming the root cell.
    #[test]
    fn send_reply_stamps_caller_chain() {
        use aether_data::MailId;

        // Capture the delivered reply's lineage triple off the
        // `OwnedDispatch` the inbox receives.
        type CapturedLineage = Arc<RwLock<Vec<(Option<MailId>, Option<MailId>, Option<MailId>)>>>;

        let (registry, mailer) = make_mailer();
        let captured: CapturedLineage = Arc::new(RwLock::new(Vec::new()));
        let captured_for_handler = Arc::clone(&captured);
        let sink_id = registry.register_inbox(
            &boot_authority(),
            "test.reply_lineage_sink",
            Arc::new(move |dispatch: OwnedDispatch| {
                // Terminal test consumer — discharge the obligation, then
                // read the lineage fields off the dispatch.
                dispatch.discharge();
                captured_for_handler.write().unwrap().push((dispatch.mail_id, dispatch.root, dispatch.parent_mail));
            }),
        );

        let replier = MailboxId(0x5151);
        let reply_id = MailId::new(replier, 9);
        let root = MailId::new(MailboxId(0x00CA_11E2), 1);
        let parent = MailId::new(MailboxId(0x00CA_11E2), 1);

        let counter = mailer.trace_handle().settlement_counter();
        assert_eq!(counter.live_roots(), 0, "no live root before the reply");

        let sent = mailer.send_reply(
            Source::with_correlation(SourceAddr::Component(sink_id), 7),
            &CastReply { code: 1, flag: 2, _pad: 0 },
            Some(reply_id),
            Some(root),
            Some(parent),
        );
        assert!(sent, "Component reply target routes");

        let captured = captured.read().unwrap().clone();
        assert_eq!(captured.len(), 1, "one reply delivered");
        assert_eq!(
            captured[0],
            (Some(reply_id), Some(root), Some(parent)),
            "reply carries the caller's root + parent and a real mail id"
        );

        // The §6 producer hook fired: the bare inbox records no
        // `Finished`, so the reply's `Sent` keeps the root live.
        assert_eq!(counter.live_roots(), 1, "send_reply records the reply's Sent on the caller root");

        // The recipient's dispatcher would record the matching `Finished`;
        // doing so here drives the root to zero and reclaims the cell —
        // proving the reply's `Sent` is balanced, not a leak.
        mailer.record_finished(Some(reply_id), Some(root));
        assert_eq!(counter.live_roots(), 0, "the reply's Finished balances its Sent exactly");
    }

    /// Mail to a registered-kind sink is delivered verbatim — the
    /// payload bytes the producer pushed reach the handler unchanged.
    #[test]
    fn registered_kind_passes_through_mailer() {
        let (registry, mailer) = make_mailer();
        let note_id = registry
            .register_kind_with_descriptor(
                &boot_authority(),
                KindDescriptor { name: Note::NAME.into(), schema: note_schema() },
            )
            .unwrap();
        let sink = CapturingSink::new();
        let sink_id = registry.register_inbox(&boot_authority(), "test.sink", sink.inbox_handler());

        let note = Note { body: "verbatim".into(), seq: 1 };
        let bytes = wire::to_vec(&note).unwrap();
        mailer.push(Mail::new(sink_id, note_id, bytes.clone(), 1));

        let captured = sink.captured.read().unwrap().clone();
        assert_eq!(captured, vec![bytes]);
    }

    use aether_data::MailId;
    use crossbeam_channel::Receiver;

    /// Issue 838 settlement-balance coverage, observed through the
    /// emit-time `SettlementCounter` (post-ADR-0086 Phase 3c the central
    /// trace queue these tests used to drain is gone). `settle_probe`
    /// installs a fresh `SettlementRegistry` on `mailer`'s trace handle,
    /// subscribes to `root`, and seeds the chain's `Sent` so a downstream
    /// `Finished` — recorded by whichever `route_mail` arm handles the
    /// mail — drives the `(in_flight, held_open)` zero-transition. The
    /// returned `Receiver` fires iff the chain settles; a path that
    /// declines to record `Finished` (actor-enqueue `Inbox`) leaves it
    /// silent.
    fn settle_probe(mailer: &Mailer, root: MailId) -> Receiver<()> {
        let settle = Arc::new(SettlementRegistry::new());
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settle));
        let rx = settle.subscribe_settlement(root);
        mailer.record_sent(root, root, None, root.sender, MailboxId(0), KindId(0));
        rx
    }

    /// Issue 838 diff 2 (re-pointed to settlement by ADR-0086 Phase 3c):
    /// exhaustive meta-test asserting every `Mailer::push` short-circuit
    /// either balances a stamped chain's `Sent` with a
    /// `Finished` (so the chain settles) or correctly declines to. The
    /// Registry-backed cases exercise every current endpoint-facade result,
    /// while the remaining cases cover each `route_mail` short-circuit.
    /// A contributor adding a path must extend this table so settlement
    /// ownership remains explicit.
    ///
    /// Pre-3c this inspected `Received`/`Finished` trace events drained
    /// from the central queue; post-3c the queue is gone and settlement
    /// is the observable. The `Received`-vs-`Finished` distinction the
    /// old test drew collapses (`route_mail` no longer records
    /// `Received`), so the expectation is binary: does the chain settle?
    ///
    /// Both production bugs this work shipped would still fail it:
    /// - The iamacoffeepot/aether#838 leak: the `Inline` case would not
    ///   settle (no `Finished`), but `Settles` is expected.
    /// - The iamacoffeepot/aether#839-attempt-1 double-count: the `Inbox`
    ///   case would settle from the Mailer side, but `DoesNotSettle` is
    ///   expected.
    #[test]
    fn every_mailer_push_path_produces_correct_lifecycle_events() {
        enum Expect {
            /// A terminal `route_mail` arm records `Finished`, balancing
            /// the seeded `Sent` so the chain settles (Inline, Dropped,
            /// warn-drop, egress, chassis router present or missing).
            Settles,
            /// The Mailer declines to record `Finished` here: actor-enqueue
            /// `Inbox` (the downstream dispatch loop owns it). The chain
            /// stays elevated.
            DoesNotSettle,
        }

        struct Case {
            name: &'static str,
            expect: Expect,
            // Builds the case's `Mailer` fixture, arms a settlement probe
            // (`settle_probe` installs a registry + seeds the chain's
            // `Sent`), drives the path, and returns whether the chain
            // settled. Cases construct their own fixtures so
            // chassis-router / outbound setups vary independently.
            run: Box<dyn FnOnce() -> bool>,
        }

        // Each case: (case-name, expectation, run-fn returning whether
        // the chain settled).

        let cases: Vec<Case> = vec![
            // 1. Inline arm — bracket.
            Case {
                name: "Inline",
                expect: Expect::Settles,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD01_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer) = make_mailer();
                    let rx = settle_probe(&mailer, mail_id);
                    let sink = CapturingSink::new();
                    let id = registry.register_inline(&boot_authority(), "test.meta.sink", sink.inline_handler());
                    mailer.push(Mail::new(id, KindId(0xFEED), vec![], 1).with_lineage(
                        Some(mail_id),
                        Some(mail_id),
                        None,
                    ));
                    rx.try_recv().is_ok()
                }),
            },
            // 2. Inbox arm — no bracket from Mailer (regression
            // guard for actor-enqueue contract).
            Case {
                name: "Inbox (actor-enqueue)",
                expect: Expect::DoesNotSettle,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD02_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer) = make_mailer();
                    let rx = settle_probe(&mailer, mail_id);
                    let sink = CapturingSink::new();
                    let id = registry.register_inbox(&boot_authority(), "test.meta.closure", sink.inbox_handler());
                    mailer.push(Mail::new(id, KindId(0xFEED), vec![], 1).with_lineage(
                        Some(mail_id),
                        Some(mail_id),
                        None,
                    ));
                    rx.try_recv().is_ok()
                }),
            },
            // 3. Dropped arm — Finished only.
            Case {
                name: "Dropped",
                expect: Expect::Settles,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD03_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (registry, mailer) = make_mailer();
                    let rx = settle_probe(&mailer, mail_id);
                    let id = registry.register_inbox(&boot_authority(), "test.meta.dropped", Arc::new(|_| {}));
                    let _ = registry.drop_mailbox(&boot_authority(), id).expect("drop");
                    mailer.push(Mail::new(id, KindId(0xFEED), vec![], 1).with_lineage(
                        Some(mail_id),
                        Some(mail_id),
                        None,
                    ));
                    rx.try_recv().is_ok()
                }),
            },
            // 4. None warn-drop (no outbound) — Finished only.
            Case {
                name: "None warn-drop (no outbound)",
                expect: Expect::Settles,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD04_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (_registry, mailer) = make_mailer();
                    let rx = settle_probe(&mailer, mail_id);
                    mailer.push(Mail::new(MailboxId(0xDEAD_BEEF_0001), KindId(0xFEED), vec![], 1).with_lineage(
                        Some(mail_id),
                        Some(mail_id),
                        None,
                    ));
                    rx.try_recv().is_ok()
                }),
            },
            // 5. None egress to hub — Finished only (per-engine
            // settlement; hub settlement is its own domain).
            Case {
                name: "None egress (outbound wired)",
                expect: Expect::Settles,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD05_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (outbound, _rx) = HubOutbound::attached_loopback();
                    let registry = Arc::new(Registry::new());
                    let mailer = Mailer::new(registry).with_outbound(outbound);
                    let rx = settle_probe(&mailer, mail_id);
                    mailer.push(Mail::new(MailboxId(0xDEAD_BEEF_0002), KindId(0xFEED), vec![], 1).with_lineage(
                        Some(mail_id),
                        Some(mail_id),
                        None,
                    ));
                    rx.try_recv().is_ok()
                }),
            },
            // 8. CHASSIS_MAILBOX_ID with router installed — bracket.
            Case {
                name: "Chassis router installed",
                expect: Expect::Settles,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD08_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (_registry, mailer) = make_mailer();
                    let rx = settle_probe(&mailer, mail_id);
                    mailer.install_chassis_router(Box::new(|_| {}));
                    mailer.push(Mail::new(MailboxId::CHASSIS_MAILBOX_ID, KindId(0xFEED), vec![], 1).with_lineage(
                        Some(mail_id),
                        Some(mail_id),
                        None,
                    ));
                    rx.try_recv().is_ok()
                }),
            },
            // 9. CHASSIS_MAILBOX_ID with no router — Finished only.
            Case {
                name: "Chassis router missing",
                expect: Expect::Settles,
                run: Box::new(|| {
                    let sender = MailboxId(0x8380_DD09_0000_0000);
                    let mail_id = MailId::new(sender, 1);
                    let (_registry, mailer) = make_mailer();
                    let rx = settle_probe(&mailer, mail_id);
                    mailer.push(Mail::new(MailboxId::CHASSIS_MAILBOX_ID, KindId(0xFEED), vec![], 1).with_lineage(
                        Some(mail_id),
                        Some(mail_id),
                        None,
                    ));
                    rx.try_recv().is_ok()
                }),
            },
        ];

        for case in cases {
            let name = case.name;
            let expect = case.expect;
            let settled = (case.run)();
            match expect {
                Expect::Settles => assert!(
                    settled,
                    "{name}: expected the chain to settle (a Finished balanced the seeded Sent), but it did not"
                ),
                Expect::DoesNotSettle => assert!(
                    !settled,
                    "{name}: expected the chain to stay elevated (no Finished from the Mailer side), but it settled"
                ),
            }
        }
    }
}
