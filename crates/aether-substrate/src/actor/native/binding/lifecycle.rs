//! How a binding is built, wired to its inbox, drained, and told to wind down.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, OnceLock};

use super::identity::BindingIdentity;
use super::outbound::OutboundBuffer;
use super::{ChildReservationTable, NativeBinding};
use crate::actor::native::envelope::Envelope;
use crate::actor::native::identity::ActorRuntimeIdentity;
use crate::chassis::ctx::ChassisCtx;
use crate::chassis::inbox::{ReplyLineage, SettlingInbox};
use crate::mail::mailer::Mailer;
use crate::mail::registry::{AddressResolutionError, RegistrySubscription};
use crate::mail::{KindId, MailId, MailboxId};
use crate::runtime::lifecycle::FatalAborter;
#[cfg(any(test, feature = "test-support"))]
use crate::runtime::lifecycle::PanicAborter;
use aether_actor::{CallerScope, ErasedActorRef, RequestContextTable};
use aether_data::{ActorPath, KindDescriptor};

impl NativeBinding {
    /// Build a fresh transport. Pair `self_mailbox` with the id the
    /// `MailboxClaim` returned (the substrate routes replies back
    /// to it via the `SourceAddr::Component(self_mailbox)` tag the
    /// transport stamps onto outbound mail). The inbox is installed
    /// separately via [`Self::install_inbox`] so capabilities that
    /// build the transport before pulling the receiver out of their
    /// claim aren't forced into a specific construction order.
    ///
    /// `aborter` backs [`Self::fatal_abort`] (wasm trap → clean
    /// substrate exit). Capabilities authored under a [`ChassisCtx`]
    /// should prefer [`Self::from_ctx`], which inherits the chassis's
    /// aborter + spawner automatically; the explicit constructor is
    /// for harnesses that don't go through a chassis (`SubstrateHarness`
    /// internals) or for tests that want to substitute a custom
    /// aborter.
    pub(crate) fn new(
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        carry: u64,
        canonical_name: Arc<str>,
        aborter: Arc<dyn FatalAborter>,
        spawner: Option<Arc<crate::Spawner>>,
    ) -> Self {
        Self::new_with_parent(mailer, self_mailbox, None, carry, canonical_name, aborter, spawner)
    }

    /// Build a typed transport whose actor was logically spawned by
    /// `parent_mailbox`. Root actors use [`Self::new`], which preserves the
    /// existing constructor and records no parent.
    pub(crate) fn new_with_parent(
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        parent_mailbox: Option<MailboxId>,
        carry: u64,
        canonical_name: Arc<str>,
        aborter: Arc<dyn FatalAborter>,
        spawner: Option<Arc<crate::Spawner>>,
    ) -> Self {
        Self {
            mailer,
            identity: BindingIdentity::Typed(ActorRuntimeIdentity::new(
                self_mailbox,
                parent_mailbox,
                carry,
                canonical_name,
            )),
            inbox: OnceLock::new(),
            correlation: AtomicU64::new(0),
            reply_lineage: ReplyLineage::new(),
            aborter,
            spawner,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            outbound: Mutex::new(OutboundBuffer::new()),
            activation_held: AtomicBool::new(false),
            blob_producer: Mutex::new(None),
            inflight: Mutex::new(super::offload::blocking::InflightTable::new()),
            child_reservations: Mutex::new(ChildReservationTable::new()),
            parent_child_reservation: Mutex::new(None),
            request_contexts: Mutex::new(RequestContextTable::new()),
        }
    }

    /// Convenience constructor that pulls the aborter + spawner from a
    /// [`ChassisCtx`]. The natural call site is inside a
    /// [`crate::DriverCapability::boot`] body:
    ///
    /// ```ignore
    /// let claim = ctx.claim_mailbox_drop_on_shutdown(NAME)?;
    /// let transport = NativeBinding::from_ctx::<MyActor>(ctx, claim.id);
    /// ```
    #[must_use]
    pub(crate) fn from_ctx<A: super::NativeActor>(ctx: &ChassisCtx<'_>, self_mailbox: MailboxId) -> Self {
        Self::new(
            ctx.mail_send_handle(),
            self_mailbox,
            // A cap built under a `ChassisCtx` is a root-pinned chassis
            // capability (depth-1), so its lineage carry is its own
            // `ActorId.0` == `self_mailbox.0` — it keeps today's id.
            self_mailbox.0,
            Arc::from(A::NAMESPACE),
            ctx.fatal_aborter(),
            Some(Arc::clone(ctx.spawner_arc())),
        )
    }

    /// Test-only constructor with a [`PanicAborter`] and no spawner.
    /// Lets the substrate's own unit tests build a transport without a
    /// chassis; production capabilities get the binding the chassis builds
    /// at boot. A test outside this crate names no mailbox id: it builds its
    /// binding through [`crate::testing::unrouted_binding`] or
    /// [`crate::testing::registered_binding`].
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_for_test(mailer: Arc<Mailer>, self_mailbox: MailboxId) -> Self {
        Self::new_for_test_with_parent(mailer, self_mailbox, None)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_for_test_with_parent(
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        parent_mailbox: Option<MailboxId>,
    ) -> Self {
        Self {
            mailer,
            // Untyped tests still use relative actor resolution. Preserve the
            // historical depth-1 carry without inventing a logical identity.
            identity: BindingIdentity::Untyped { mailbox: self_mailbox, parent: parent_mailbox, carry: self_mailbox.0 },
            inbox: OnceLock::new(),
            correlation: AtomicU64::new(0),
            reply_lineage: ReplyLineage::new(),
            aborter: Arc::new(PanicAborter),
            spawner: None,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            outbound: Mutex::new(OutboundBuffer::new()),
            activation_held: AtomicBool::new(false),
            blob_producer: Mutex::new(None),
            inflight: Mutex::new(super::offload::blocking::InflightTable::new()),
            child_reservations: Mutex::new(ChildReservationTable::new()),
            parent_child_reservation: Mutex::new(None),
            request_contexts: Mutex::new(RequestContextTable::new()),
        }
    }

    /// Install the receiver half of the actor's inbox: the receiver the
    /// scheduler slots drain through the crate-private `try_recv`.
    /// Called once per transport, before the scheduler starts draining.
    /// Subsequent calls panic — the slot is single-claim by construction.
    ///
    /// # Panics
    /// Panics if called more than once — fail-fast per ADR-0063: the
    /// inbox slot is single-claim, so a second install indicates a
    /// chassis-wiring bug.
    pub(crate) fn install_inbox(&self, inbox: Receiver<Envelope>) {
        let settling = SettlingInbox::new_with_lineage(
            self.self_mailbox(),
            inbox,
            Arc::clone(&self.mailer),
            self.reply_lineage.clone(),
        );
        self.inbox.set(Mutex::new(settling)).unwrap_or_else(|_| panic!("NativeBinding::install_inbox called twice"));
    }

    /// Install an already-built [`SettlingInbox`] as this binding's inbox,
    /// beside [`Self::install_inbox`] (which builds one from a raw
    /// receiver). Additive (ADR-0160 §1): the pumped boot recovers a
    /// driver's Claim-stage [`MailboxClaim`](crate::chassis::ctx::MailboxClaim),
    /// re-lineages its inbox onto this binding's disjoint reply-id space
    /// (via [`SettlingInbox::relineage`]), and installs it here — so the
    /// binding drains the very inbox the claim reserved rather than a fresh
    /// channel. The caller is responsible for re-lineaging first, exactly
    /// as [`Self::install_inbox`] does implicitly through
    /// `SettlingInbox::new_with_lineage`.
    ///
    /// # Panics
    /// Panics if called after another `install_inbox` /
    /// `install_settling_inbox` — the inbox slot is single-claim (ADR-0063
    /// fail-fast).
    pub(crate) fn install_settling_inbox(&self, inbox: SettlingInbox) {
        self.inbox
            .set(Mutex::new(inbox))
            .unwrap_or_else(|_| panic!("NativeBinding::install_settling_inbox called after the inbox was installed"));
    }

    /// The mailbox id the substrate routes inbound mail through to
    /// reach this actor. Exposed for capabilities that need to
    /// publish their address to peers without going through the
    /// transport's send path.
    pub(crate) fn self_mailbox(&self) -> MailboxId {
        self.identity.mailbox()
    }

    /// Subscribe this actor to one `kind` notice when `root` settles, through
    /// the chassis's settlement registry. Returns `false`, and subscribes
    /// nothing, when the chassis wires no settlement registry. The path behind
    /// [`NativeCtx::subscribe_settlement`](crate::actor::native::ctx::NativeCtx::subscribe_settlement).
    pub(crate) fn subscribe_settlement_notice(&self, root: MailId, kind: KindId) -> bool {
        let Some(registry) = self.mailer.settlement_registry() else {
            return false;
        };
        registry.subscribe_settlement_mail(root, self.self_mailbox(), kind, Arc::clone(&self.mailer));
        true
    }

    /// Subscribe this actor to the registry's inventory changes. The path
    /// behind [`NativeCtx::subscribe_inventory`](crate::actor::native::ctx::NativeCtx::subscribe_inventory).
    pub(crate) fn subscribe_inventory(&self) -> RegistrySubscription {
        self.mailer.subscribe_inventory_for(self.self_mailbox())
    }

    /// This actor's lineage carry (ADR-0099 §3) — the rolling fold
    /// state `spawn_child` extends to derive a child's id. Surfaced so
    /// [`super::ctx::NativeCtx::spawn_child`](crate::actor::native::ctx::NativeCtx::spawn_child) can pass it as the parent
    /// carry the spawn machinery folds the new node's `ActorId` onto.
    pub(crate) fn carry(&self) -> u64 {
        self.identity.carry()
    }

    /// The mailbox of this actor's logical parent, or `None` for a chassis
    /// root or a legacy/test binding with no parent metadata.
    pub(crate) fn parent_mailbox(&self) -> Option<MailboxId> {
        self.identity.parent()
    }

    /// Select the caller carry requested by a caller-scoped resolver.
    pub(crate) fn scope_mailbox(&self, scope: CallerScope) -> u64 {
        match (scope, self.parent_mailbox()) {
            // A root-pinned resolver ignores its carry, and no birth folds
            // beneath the empty lineage, so a parentless `Parent` scope
            // resolves to an address that is never live.
            (CallerScope::Root, _) | (CallerScope::Parent, None) => 0,
            (CallerScope::Current, _) => self.self_mailbox().0,
            (CallerScope::Parent, Some(parent)) => parent.0,
        }
    }

    pub(in crate::actor::native) fn runtime_identity(&self) -> Option<&ActorRuntimeIdentity> {
        self.identity.runtime_identity()
    }

    /// Borrow the wired `Mailer`. Surfaced so cross-file producer
    /// hooks (`slot::dispatch`, `slot::dispatcher`, `offload::thread`) can
    /// reach the trace handle via `binding.mailer().record_*(...)`
    /// without the field having to be `pub(crate)`. Filed under
    /// iamacoffeepot/aether#953 (per-chassis trace state).
    pub(crate) fn mailer(&self) -> &Arc<Mailer> {
        &self.mailer
    }

    /// A kind's display label from the wired registry. The path behind
    /// [`NativeCtx::kind_label`](crate::actor::native::ctx::NativeCtx::kind_label).
    pub(crate) fn kind_label(&self, kind: KindId) -> String {
        self.mailer.kind_label(kind)
    }

    /// Every registered kind descriptor from the wired registry. The path
    /// behind [`NativeCtx::kind_descriptors`](crate::actor::native::ctx::NativeCtx::kind_descriptors).
    pub(crate) fn kind_descriptors(&self) -> Vec<KindDescriptor> {
        self.mailer.kind_descriptors()
    }

    /// The origin name of one tagged id. The path behind
    /// [`NativeCtx::tagged_id_name`](crate::actor::native::ctx::NativeCtx::tagged_id_name).
    pub(crate) fn tagged_id_name(&self, tagged: &str) -> Option<String> {
        self.mailer.tagged_id_name(tagged)
    }

    /// The canonical path of the live actor an address names. The path behind
    /// [`NativeCtx::canonical_path`](crate::actor::native::ctx::NativeCtx::canonical_path).
    pub(crate) fn canonical_path(&self, address: &ActorPath) -> Result<String, AddressResolutionError> {
        self.mailer.canonical_path(address)
    }

    /// The canonical path of the route a reference proves. The path behind
    /// [`NativeCtx::actor_path`](crate::actor::native::ctx::NativeCtx::actor_path).
    pub(crate) fn actor_path(&self, reference: ErasedActorRef) -> ActorPath {
        self.mailer.actor_path(reference)
    }

    /// The first declared dependency with no `Live` route for a child placed
    /// under this binding's actor. The path behind
    /// [`NativeCtx::missing_child_dependency`](crate::actor::native::ctx::NativeCtx::missing_child_dependency).
    #[cfg(feature = "wasm")]
    pub(crate) fn missing_child_dependency<'a>(
        &self,
        dependencies: impl IntoIterator<Item = (u8, &'a str)>,
    ) -> Option<&'a str> {
        self.mailer.missing_dependency_under(self.self_mailbox(), dependencies)
    }

    /// #1757: the actor's reply-lineage allocator (a shared-counter
    /// clone). Surfaced so a handler that retains its inbound via
    /// [`NativeCtx::take_inbound`](crate::actor::native::ctx::NativeCtx::take_inbound)
    /// mints the deferred reply's id from the same disjoint
    /// [`ReplyLineage`] space as the binding's own
    /// [`Self::send_reply_for_handler`] path, rather than a fresh
    /// counter that could collide.
    pub(crate) fn reply_lineage(&self) -> ReplyLineage {
        self.reply_lineage.clone()
    }

    /// The chassis's [`crate::Spawner`], if one was wired in at
    /// construction. `Some` for production transports built through
    /// [`Self::from_ctx`] (the chassis builds + threads its `Spawner`
    /// into every cap); `None` for [`Self::new_for_test`] transports
    /// (those tests don't exercise spawn). Used by
    /// `NativeCtx::spawn_child` to reach the spawn machinery without
    /// separate per-handler plumbing.
    pub(crate) fn spawner(&self) -> Option<&Arc<crate::Spawner>> {
        self.spawner.as_ref()
    }

    /// Issue 607 Phase 4a (ADR-0079): set the self-shutdown flag the
    /// actor's dispatcher polls between handler dispatches. Later
    /// dispatches still process incoming mail, but
    /// `should_shutdown` reports `true` so the trampoline can drain
    /// the inbox synchronously, run `unwire`, and exit. Idempotent.
    pub(crate) fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Release);
    }

    /// ADR-0063 fail-fast: bring the substrate down with `reason`.
    /// Diverging — does not return. Production substrates exit via
    /// [`crate::runtime::lifecycle::fatal_abort`] (broadcasts `SubstrateDying`
    /// then calls `process::exit(2)`); test substrates panic instead.
    /// The trampoline calls this when the wasm guest traps, so a
    /// faulty component takes down the substrate cleanly with a useful
    /// log message rather than leaving a tombstoned trampoline whose
    /// failure mode is invisible to callers.
    pub fn fatal_abort(&self, reason: String) -> ! {
        self.aborter.abort(reason);
    }

    /// The chassis aborter behind [`Self::fatal_abort`], for an off-thread
    /// worker that must escalate a panic (ADR-0063) without holding the
    /// binding: a `dispatch_blocking` worker keeps only a weak binding
    /// reference, so it takes this handle before it spawns.
    pub(crate) fn fatal_aborter(&self) -> Arc<dyn FatalAborter> {
        Arc::clone(&self.aborter)
    }

    /// Read the self-shutdown flag. Polled by the dispatcher trampoline
    /// after each handler dispatch — substrate-shutdown
    /// (channel-disconnect) flows through the same drain path without
    /// setting this flag, so the trampoline takes either signal as a
    /// trigger to wind down.
    pub(crate) fn should_shutdown(&self) -> bool {
        self.shutdown_flag.load(Ordering::Acquire)
    }

    /// The scheduler slots' non-blocking drain: take the next queued
    /// envelope. Returns `None` when the inbox is empty, disconnected,
    /// or not yet installed; a slot drains by calling until `None`.
    ///
    /// # Panics
    /// Panics if the inbox mutex is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked inside the
    /// guard, which is itself a substrate-level invariant violation.
    pub(crate) fn try_recv(&self) -> Option<Envelope> {
        let inbox = self.inbox.get()?;
        inbox.lock().expect("inbox mutex poisoned; fail-fast per ADR-0063").try_recv()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test-setup unwraps: fixture construction panic on failure is the assertion")]
mod tests {
    use super::*;
    use crate::mail::registry::OwnedDispatch;
    use crate::mail::{KindId, MailId, MailRef, Source};
    use crate::testing::bare_substrate;
    use aether_kinds::trace::Nanos;
    use std::sync::mpsc;

    /// `install_inbox` is single-claim — a second install panics.
    #[test]
    #[should_panic(expected = "install_inbox called twice")]
    fn install_inbox_twice_panics() {
        let (_registry, mailer) = bare_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (_tx1, rx1) = mpsc::channel::<Envelope>();
        let (_tx2, rx2) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx1);
        transport.install_inbox(rx2);
    }

    #[test]
    fn binding_scope_selection_distinguishes_current_and_parent() {
        let (_registry, mailer) = bare_substrate();
        let current = MailboxId(0x4a01);
        let parent = MailboxId(0x4a00);
        let binding = NativeBinding::new_for_test_with_parent(mailer, current, Some(parent));

        assert_eq!(binding.scope_mailbox(CallerScope::Current), current.0);
        assert_eq!(binding.scope_mailbox(CallerScope::Parent), parent.0);
    }

    /// #1716 / step 2: an armed envelope left queued in the dispatcher's
    /// inbox at binding teardown settles (no ADR-0094 guard panic,
    /// `Finished` observed). Dropping the `NativeBinding` drops the
    /// `OnceLock<Mutex<SettlingInbox>>`, which drops the `SettlingInbox`,
    /// whose `Drop` drains and settles residue.
    #[test]
    fn binding_teardown_settles_queued_armed_envelope() {
        use crate::chassis::settlement::SettlementRegistry;

        let (_registry, mailer) = bare_substrate();
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));

        let id = MailboxId(0x1756);
        let (tx, rx) = mpsc::channel::<Envelope>();

        let root = MailId::new(id, 1);
        mailer.record_sent_inflight(root);
        let settle = settlement.subscribe_settlement(root);

        let transport = NativeBinding::new_for_test(Arc::clone(&mailer), id);
        transport.install_inbox(rx);

        // Queue an armed envelope directly — bypasses the registry sink,
        // mirrors the production route_mail Inbox arm result.
        let armed = OwnedDispatch::armed(
            KindId(7),
            None,
            Source::NONE,
            MailRef::from(Vec::new()),
            1,
            Some(MailId::new(id, 11)),
            Some(root),
            None,
            Nanos(0),
            0,
            id,
        );
        tx.send(armed).unwrap();

        // Drop the transport; the SettlingInbox inside settles the queued mail.
        drop(transport);
        settle.recv().expect("binding teardown settles queued armed mail (#1716)");
    }
}
