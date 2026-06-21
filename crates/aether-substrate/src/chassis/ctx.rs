//! Boot machinery shared by the chassis builder (`chassis::builder::Builder`,
//! ADR-0071): mailbox claim helpers, the per-cap [`MailboxClaim`] /
//! [`DropOnShutdownClaim`] result shapes, and the [`ChassisCtx`]
//! threaded through every cap's boot. Sibling modules: error types
//! live in `chassis::error`; the cross-flavour [`Envelope`] shape lives
//! in `actor::native::envelope`.

use std::sync::mpsc;
use std::sync::{Arc, Weak};

use aether_actor::Addressable;
use aether_actor::local::ActorSlots;

use crate::actor::native::envelope::Envelope;
use crate::chassis::error::BootError;
use crate::chassis::inbox::SettlingInbox;
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::OwnedDispatch;
use crate::mail::registry::Registry;
use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::WakeSink;
use std::fmt;
use std::sync::OnceLock;

// iamacoffeepot/aether#848 PR 3: the `build_envelope(&MailDispatch)`
// helper retired. Production cap registration closures now take
// `OwnedDispatch` directly and call `Envelope::from(dispatch)` —
// payload + kind_name + origin all move rather than clone. The
// borrowed-dispatch shape is still available through the
// `MailboxEntry::Inline` path elsewhere.

/// Result returned from [`ChassisCtx::claim_mailbox`].
///
/// The capability owns the receiver afterward; the slot is consumed
/// from the registry, so a second claim for the same name fails
/// loud with [`BootError::MailboxAlreadyClaimed`].
///
/// `actor_slots` carries this claim's per-actor [`ActorSlots`] — the
/// chassis [`crate::chassis::builder::Builder::with_actor`] path
/// stamps this into TLS via [`crate::actor::native::local::with_stamped`]
/// around `init` / `wire` / each dispatch so per-actor `Local<T>`
/// lookups (notably the ADR-0081 `ActorLogRing`) resolve to the
/// caller's storage. Driver-as-actor capabilities (issue 603 Phase 3,
/// today only the desktop window driver) that bypass the standard
/// dispatcher slot need to stamp the same slots around their bespoke
/// drain so the framework-built-in `aether.log.tail` /
/// `aether.trace.tail` / `aether.cost.tail` dispatch arms reach the
/// expected ring (iamacoffeepot/aether#1272).
pub struct MailboxClaim {
    pub id: MailboxId,
    /// ADR-0106: the sealed inbound surface. Replaces the raw
    /// `mpsc::Receiver<Envelope>` the claim used to expose — a capability
    /// reaches inbound envelopes only through the [`SettlingInbox`]'s drain
    /// methods, each of which settles the ADR-0080 §2 bracket on scope
    /// exit. Outside `aether-substrate` it is no longer possible to obtain
    /// an armed [`Envelope`] from a claim.
    pub inbox: SettlingInbox,
    pub actor_slots: SharedActorSlots,
    /// Optional wake hook fired by the registry sink after each
    /// accepted send (iamacoffeepot/aether#1318). The plain claim path
    /// — unlike `claim_mailbox_drop_on_shutdown` — drains on a cadence
    /// the claimer controls (the desktop window driver drains in
    /// `about_to_wait`), so it needs a way to nudge that cadence when
    /// mail arrives while the loop is parked. Defaults to an unset
    /// slot; the desktop window driver installs an `EventLoopProxy`
    /// wake so `aether.window` mail wakes the winit loop even under
    /// `ControlFlow::Wait`. Empty for every other plain-claim consumer.
    pub wake_slot: Arc<MailboxWakeSlot>,
}

impl fmt::Debug for MailboxClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `ActorSlots` doesn't impl `Debug` (interior `RefCell<HashMap>`
        // of type-erased boxes), so hand-roll Debug on `MailboxClaim`
        // and finish non-exhaustively rather than deriving.
        f.debug_struct("MailboxClaim")
            .field("id", &self.id)
            .field("inbox", &self.inbox)
            .finish_non_exhaustive()
    }
}

/// Driver-as-actor's [`ActorSlots`] handle (iamacoffeepot/aether#1272).
///
/// `ActorSlots` uses interior `RefCell` (a single dispatcher thread is
/// the sole owner per ADR-0038), so a bare `Arc<ActorSlots>` is neither
/// `Send` nor `Sync`. Driver-as-actor capabilities access these slots
/// only from their bespoke drain thread (the winit main thread for the
/// desktop window driver), so the interior cell stays effectively
/// single-threaded — mirrors the pooled dispatcher's `PooledSlots`
/// wrapper, but here the access invariant is stricter (one fixed
/// thread, not "at most one pool worker at a time"). The `unsafe impl
/// Sync` / `Send` are the safety story.
#[derive(Clone)]
#[allow(
    clippy::non_send_fields_in_send_ty,
    reason = "driver-as-actor invariant: slots only touched on one fixed thread; see type docs"
)]
pub struct SharedActorSlots(Arc<ActorSlots>);

// SAFETY: see the doc-comment on `SharedActorSlots`. The driver-as-actor
// invariant is that the slots are only ever read inside
// `local::with_stamped` on the driver's bespoke drain thread. No other
// thread holds an `Arc` clone, so the interior `RefCell` accesses are
// single-threaded by construction.
unsafe impl Sync for SharedActorSlots {}
// SAFETY: same justification — single-thread access.
unsafe impl Send for SharedActorSlots {}

impl SharedActorSlots {
    /// Allocate a fresh per-actor slot map.
    #[must_use]
    #[allow(
        clippy::arc_with_non_send_sync,
        reason = "SharedActorSlots's unsafe impl Send/Sync covers this Arc; see type docs"
    )]
    pub fn new() -> Self {
        Self(Arc::new(ActorSlots::new()))
    }

    /// Borrow the inner [`ActorSlots`] for a
    /// [`crate::actor::native::local::with_stamped`] call.
    #[must_use]
    pub fn slots(&self) -> &ActorSlots {
        &self.0
    }
}

impl Default for SharedActorSlots {
    fn default() -> Self {
        Self::new()
    }
}

/// Result returned from [`ChassisCtx::claim_mailbox_drop_on_shutdown`].
///
/// Same as [`MailboxClaim`] plus a strong [`MailboxSender`] the
/// capability is expected to drop during shutdown to break the
/// channel — the channel-drop + join lifecycle ADR-0074 §Decision 5
/// settles on. The registry's sink-handler closure holds only a
/// [`Weak`] back-reference, so once the strong handle
/// goes away, in-flight deliveries warn-drop and the dispatcher's
/// `recv()` returns `Err(Disconnected)`.
///
/// Phase 2a: `LogCapability` is the first consumer; the other
/// capabilities continue with `claim_mailbox` + `Arc<AtomicBool>`
/// polling until their own migration PRs land.
#[derive(Debug)]
pub struct DropOnShutdownClaim {
    pub id: MailboxId,
    /// ADR-0106: the standard-dispatcher feed. Narrowed to `pub(crate)` —
    /// the raw-receiver shape survives only inside `aether-substrate` (the
    /// builder hands it straight to the pooled dispatcher), so no
    /// out-of-crate consumer can obtain an armed [`Envelope`] from a
    /// drop-on-shutdown claim either.
    pub(crate) receiver: mpsc::Receiver<Envelope>,
    pub mailbox_sender: MailboxSender,
    /// Issue 635 PR C: the pool wake hook. The mailbox closure invokes
    /// it after a successful inbox push so the chassis worker pool
    /// re-queues the actor's [`crate::scheduler::Drainable`] slot. Every
    /// actor is pool-dispatched (issue 1187), so this is always
    /// populated post-slot-construction; the [`OnceLock`] shape lets the
    /// closure's `get()` stay a single relaxed atomic load while the
    /// slot is still being built.
    ///
    /// Populated post-claim by `make_native_actor_boot` /
    /// `Spawner::spawn_actor` after the slot exists.
    pub wake_slot: Arc<MailboxWakeSlot>,
}

/// Cell holding the optional wake hook a `Pooled` mailbox fires after
/// each accepted send. The mailbox closure captures `Arc<MailboxWakeSlot>`
/// at registration time; the spawn path populates it once the
/// [`crate::scheduler::Drainable`] slot exists.
#[derive(Default)]
pub struct MailboxWakeSlot {
    inner: OnceLock<MailboxWakeFn>,
}

impl fmt::Debug for MailboxWakeSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxWakeSlot")
            .field("installed", &self.inner.get().is_some())
            .finish()
    }
}

/// Type-erased wake hook stamped into [`MailboxWakeSlot`].
pub type MailboxWakeFn = Arc<dyn Fn() + Send + Sync + 'static>;

impl MailboxWakeSlot {
    /// Install the wake hook. Idempotent on re-call (silently ignores
    /// the second set), but in production every claim is paired with
    /// a single set.
    pub fn set(&self, fn_: MailboxWakeFn) {
        let _ = self.inner.set(fn_);
    }

    /// Borrow the installed hook. Returns `None` only during the boot
    /// window before the slot is constructed and the hook is set; every
    /// actor is pool-dispatched (issue 1187), so a live actor always has
    /// one installed. Hot path — `OnceLock::get` is a single relaxed
    /// load.
    pub(crate) fn get(&self) -> Option<&MailboxWakeFn> {
        self.inner.get()
    }
}

/// Outcome of [`relay_or_transfer`]. Each abandonment variant carries
/// the discarded mail's `kind_name` (moved out of the transferred
/// dispatch) so the caller can render its own site-specific
/// `tracing::warn!` — the `target:` literal
/// and message differ per relay seam, which is why the log stays at the
/// call site while the obligation-settling core does not.
#[derive(Debug)]
pub(crate) enum RelayOutcome {
    /// The envelope was moved onto the actor's channel and the wake hook
    /// (if installed) fired. The obligation rides the value; the actor's
    /// dispatcher discharges it on drain.
    Delivered,
    /// The `Weak` sender could not be upgraded — the actor's strong
    /// sender is gone (it dropped its `MailboxSender` during shutdown).
    /// The mail was transferred (not dropped armed) before returning.
    SenderGone { kind_name: String },
    /// The sender upgraded but the receiver had disconnected, so the
    /// `mpsc::send` failed. The returned envelope was transferred before
    /// returning.
    ReceiverGone { kind_name: String },
}

/// The shared inbox-relay core (ADR-0094): upgrade the actor's `Weak`
/// sender, move the [`OwnedDispatch`] onto its channel, and fire the
/// wake hook — settling the settlement obligation at the two abandonment
/// seams in exactly one place.
///
/// Both the sender-gone and receiver-gone branches call
/// [`OwnedDispatch::mark_transferred`] before returning, so a send racing
/// teardown discards the mail at the relay seam rather than dropping an
/// armed dispatch and tripping the debug guard (#1564). The three
/// production inbox closures — the two `claim_mailbox*` variants here and
/// the instanced-actor closure in
/// [`crate::actor::native::spawn`] — route through this function so the
/// transfer contract has a single home. Per-site concerns (the instanced
/// `pending` bracket, each site's `tracing::warn!` target + message) stay
/// at the call site, driven by the returned [`RelayOutcome`].
pub(crate) fn relay_or_transfer(
    dispatch: OwnedDispatch,
    weak_tx: &Weak<mpsc::Sender<Envelope>>,
    wake: &MailboxWakeSlot,
) -> RelayOutcome {
    let Some(tx) = weak_tx.upgrade() else {
        // ADR-0094: the strong sender is gone — discard at this relay
        // seam, transferring the obligation. `mark_transferred` disarms
        // the guard, then `kind_name` moves out for the caller's log
        // (the rest of the dispatch, guard included, drops here).
        dispatch.mark_transferred();
        return RelayOutcome::SenderGone {
            kind_name: dispatch.kind_name,
        };
    };
    let env: Envelope = dispatch;
    if let Err(mpsc::SendError(env)) = tx.send(env) {
        // ADR-0094: receiver disconnected — discard at the seam, transfer.
        env.mark_transferred();
        return RelayOutcome::ReceiverGone {
            kind_name: env.kind_name,
        };
    }
    if let Some(wake) = wake.get() {
        wake();
    }
    RelayOutcome::Delivered
}

/// Strong handle to the inbound `Sender<Envelope>` for a mailbox
/// claimed via [`ChassisCtx::claim_mailbox_drop_on_shutdown`]. Held
/// by the capability for the lifetime of its dispatcher thread;
/// dropping it disconnects the channel and lets the dispatcher's
/// `recv()` return `Err(Disconnected)` immediately.
#[derive(Debug)]
pub struct MailboxSender {
    // Held purely for its `Drop` side effect. When this `Arc` drops
    // and refcount hits zero, the inner `Sender` drops, the channel
    // disconnects, and the dispatcher exits its `recv()` loop.
    _inner: Arc<mpsc::Sender<Envelope>>,
}

impl MailboxSender {
    /// Internal constructor — only
    /// [`ChassisCtx::claim_mailbox_drop_on_shutdown`] builds these.
    pub(crate) fn new(inner: Arc<mpsc::Sender<Envelope>>) -> Self {
        Self { _inner: inner }
    }
}

/// Generic fallback-router handler: invoked by substrate dispatch when a
/// local mailbox lookup misses. Phase 1 stores the handler but does
/// not call it; Phase 4 wires `Mailer::push` to consult the slot in
/// place of today's hub-specific bubble-up.
///
/// Returning `true` means "I handled this mail" (substrate does nothing
/// further); `false` means "not mine" (substrate falls through to its
/// warn-drop path). Today only `HubClientCapability` will claim the
/// slot; other implementations are possible (test routers, multi-hub
/// fan-out).
pub type FallbackRouter = Arc<dyn Fn(&Envelope) -> bool + Send + Sync + 'static>;

/// Kernel-side handle bundle exposed to a capability during its
/// `boot()` call. Shared (`&mut`) across every `boot()` in the
/// builder — one ctx per build, threaded through the capability list
/// in declaration order (ADR-0070 resolved decision 4).
pub struct ChassisCtx<'a> {
    registry: &'a Arc<Registry>,
    mailer: &'a Arc<Mailer>,
    fallback: &'a mut Option<FallbackRouter>,
    /// Indirection over [`crate::runtime::lifecycle::fatal_abort`]
    /// cloned into every [`crate::NativeBinding`] this ctx builds, so a
    /// wasm-guest trap can fatal-abort the substrate cleanly without
    /// each capability needing to plumb [`crate::HubOutbound`] itself.
    /// Defaults to [`crate::runtime::lifecycle::PanicAborter`] when the
    /// chassis builder doesn't override — production drivers swap in
    /// [`crate::runtime::lifecycle::OutboundFatalAborter`] via
    /// [`crate::chassis::builder::Builder::with_aborter`].
    aborter: &'a Arc<dyn FatalAborter>,
    /// Issue #601: every actor-mailbox claim appends its `MailboxId`
    /// here. The chassis builder reads the list after `boot_passives`.
    ///
    /// Synchronous-handler registrations (e.g. `AETHER_DIAGNOSTICS`)
    /// go through `Registry::register_inline` directly and do *not*
    /// land here — they're not actors.
    claimed_actor_mailboxes: &'a mut Vec<MailboxId>,
    /// Issue 607 Phase 3b (ADR-0079): the chassis's
    /// [`crate::Spawner`], cloned into every booted actor's
    /// [`crate::NativeBinding`] (via [`crate::NativeBinding::from_ctx`])
    /// so per-handler `NativeCtx::spawn_child` can reach the spawn
    /// machinery without separate plumbing. Built once at boot in
    /// `boot_passives`.
    spawner: &'a Arc<crate::Spawner>,
}

impl<'a> ChassisCtx<'a> {
    /// Internal constructor used by the ADR-0071
    /// [`crate::chassis::builder::Builder`].
    pub(crate) fn new(
        registry: &'a Arc<Registry>,
        mailer: &'a Arc<Mailer>,
        fallback: &'a mut Option<FallbackRouter>,
        aborter: &'a Arc<dyn FatalAborter>,
        claimed_actor_mailboxes: &'a mut Vec<MailboxId>,
        spawner: &'a Arc<crate::Spawner>,
    ) -> Self {
        Self {
            registry,
            mailer,
            fallback,
            aborter,
            claimed_actor_mailboxes,
            spawner,
        }
    }

    /// Register a `MailboxEntry::Inbox` under `C::NAMESPACE` and
    /// return both its derived [`MailboxId`] (ADR-0029 hash) and
    /// the receiver. The capability's own type is the single source
    /// of truth for the recipient name (issue 525 Phase 1).
    ///
    /// Tests that need a parameterized name (one fixture, many
    /// claims) reach for [`Self::claim_mailbox_with_override`].
    pub fn claim_mailbox<C: Addressable>(&mut self) -> Result<MailboxClaim, BootError> {
        self.claim_mailbox_with_override(C::NAMESPACE)
    }

    /// Register a `MailboxEntry::Inbox` under `name` and return
    /// both its derived [`MailboxId`] (ADR-0029 hash) and the
    /// receiver. Escape hatch for tests with parameterized names;
    /// production caps go through [`Self::claim_mailbox`] so the
    /// cap's own `NAMESPACE` is authoritative.
    ///
    /// The closure registered with the registry forwards every
    /// delivery into the sender side of the mpsc pair, so the
    /// capability's dispatcher loop is `while let Ok(env) =
    /// claim.receiver.recv() { ... }`. The receiver lives until the
    /// capability drops it; the matching sender lives in the sink
    /// closure stored on the registry until the registry itself is
    /// dropped.
    pub fn claim_mailbox_with_override(&mut self, name: &str) -> Result<MailboxClaim, BootError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let tx = Arc::new(tx);
        // iamacoffeepot/aether#1318: optional wake hook fired after each
        // accepted send. Unset by default (a single relaxed atomic load
        // on the hot path); the desktop window driver installs an
        // `EventLoopProxy` wake so `aether.window` mail nudges the winit
        // loop under `ControlFlow::Wait`. Mirrors the `MailboxWakeSlot`
        // the `claim_mailbox_drop_on_shutdown` variant carries.
        let wake_slot: Arc<MailboxWakeSlot> = Arc::new(MailboxWakeSlot::default());
        let wake_for_handler = Arc::clone(&wake_slot);
        let id = self.registry.try_register_inbox(
            name.to_owned(),
            // iamacoffeepot/aether#848: closure takes [`OwnedDispatch`]
            // and routes it through [`relay_or_transfer`], which owns the
            // upgrade → send → wake core and both ADR-0094 transfer seams.
            // This claim holds the *strong* `tx` for the registry's
            // lifetime (the channel must outlive every claimer), and
            // passes a derived `Weak` so it shares the same helper as the
            // drop-on-shutdown / instanced closures. The upgrade always
            // succeeds here, so `SenderGone` is unreachable.
            Arc::new(move |dispatch: OwnedDispatch| {
                // The strong `tx` is captured for liveness; this keeps the
                // move-closure holding it and documents that the derived
                // `Weak` below always upgrades.
                debug_assert!(Arc::strong_count(&tx) >= 1);
                match relay_or_transfer(dispatch, &Arc::downgrade(&tx), &wake_for_handler) {
                    RelayOutcome::Delivered => {}
                    RelayOutcome::ReceiverGone { kind_name } => {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = %kind_name,
                            "capability mailbox receiver dropped — mail discarded"
                        );
                    }
                    RelayOutcome::SenderGone { .. } => {
                        // Unreachable: the closure holds the strong `tx`,
                        // so the derived `Weak` always upgrades. Handled
                        // for match exhaustiveness.
                        debug_assert!(false, "claim_mailbox_with_override sender cannot be gone");
                    }
                }
            }),
        )?;
        self.claimed_actor_mailboxes.push(id);
        // iamacoffeepot/aether#1272: every claim returns its
        // per-actor [`ActorSlots`] wrapped in [`SharedActorSlots`]. The
        // `with_actor` boot path allocates its own [`ActorSlots`] (via
        // `Box<ActorSlots>` in `ClaimResources`) and ignores this one;
        // driver-as-actor capabilities that own the drain inline (the
        // desktop window driver) wrap their bespoke drain in
        // `local::with_stamped(slots.as_ref(), …)` so framework dispatch
        // arms reach the actor's per-actor `Local<T>` rings.
        Ok(MailboxClaim {
            id,
            inbox: SettlingInbox::new(id, rx, Arc::clone(self.mailer)),
            actor_slots: SharedActorSlots::new(),
            wake_slot,
        })
    }

    /// Variant of [`Self::claim_mailbox`] that returns a strong
    /// [`MailboxSender`] alongside the receiver. Claims under
    /// `C::NAMESPACE`. See
    /// [`Self::claim_mailbox_drop_on_shutdown_with_override`] for the
    /// arbitrary-name escape hatch.
    pub fn claim_mailbox_drop_on_shutdown<C: Addressable>(
        &mut self,
    ) -> Result<DropOnShutdownClaim, BootError> {
        self.claim_mailbox_drop_on_shutdown_with_override(C::NAMESPACE)
    }

    /// Variant of [`Self::claim_mailbox_with_override`] that returns
    /// a strong [`MailboxSender`] alongside the receiver. The registry
    /// holds only a [`Weak`] reference to the sender, so
    /// when the capability drops the `MailboxSender` (during shutdown),
    /// the channel disconnects and the dispatcher's `recv()` returns
    /// `Err(Disconnected)`.
    ///
    /// Use this when the capability wants the channel-drop + join
    /// shutdown lifecycle (ADR-0074 §Decision) instead of an
    /// `Arc<AtomicBool>` polling flag. Phase 2a wires
    /// `LogCapability` onto this; the other native capabilities
    /// migrate one PR at a time per the issue 509 plan.
    pub fn claim_mailbox_drop_on_shutdown_with_override(
        &mut self,
        name: &str,
    ) -> Result<DropOnShutdownClaim, BootError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        // Strong Arc rides on `DropOnShutdownClaim.mailbox_sender` and
        // lives for the capability's lifetime. The registry handler
        // only upgrades a `Weak` per call, so when the capability
        // drops its strong handle, the inner `Sender` also drops
        // and the dispatcher's `recv()` returns `Err(Disconnected)`.
        let tx = Arc::new(tx);
        let weak = Arc::downgrade(&tx);
        let wake_slot: Arc<MailboxWakeSlot> = Arc::new(MailboxWakeSlot::default());
        let wake_for_handler = Arc::clone(&wake_slot);
        let id = self.registry.try_register_inbox(
            name.to_owned(),
            // iamacoffeepot/aether#848 PR 3: routes through
            // [`relay_or_transfer`] (the shared upgrade → send → wake core
            // with both ADR-0094 transfer seams). The `Weak` upgrade can
            // fail here — the cap drops its strong `MailboxSender` during
            // shutdown — so both abandonment arms are reachable and warn.
            // #1564: settling the obligation in the helper is what keeps a
            // send racing teardown from dropping an armed dispatch.
            Arc::new(move |dispatch: OwnedDispatch| {
                match relay_or_transfer(dispatch, &weak, &wake_for_handler) {
                    RelayOutcome::Delivered => {}
                    RelayOutcome::SenderGone { kind_name } => {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = %kind_name,
                            "capability mailbox sender dropped — mail discarded"
                        );
                    }
                    RelayOutcome::ReceiverGone { kind_name } => {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = %kind_name,
                            "capability mailbox receiver dropped — mail discarded"
                        );
                    }
                }
            }),
        )?;
        self.claimed_actor_mailboxes.push(id);
        Ok(DropOnShutdownClaim {
            id,
            receiver: rx,
            mailbox_sender: MailboxSender::new(tx),
            wake_slot,
        })
    }

    /// Issue 607 Phase 7: undo a previous `claim_*_mailbox` call.
    /// Removes the sink from the chassis registry and the id from
    /// `claimed_actor_mailboxes`. Idempotent: calling on an id that
    /// wasn't claimed is a no-op.
    ///
    /// Used in the singleton-boot unwind path when `init` fails after
    /// the cap mailbox was claimed. Without this, the failed cap leaves
    /// a sink registered against its namespace.
    pub fn unclaim_mailbox(&mut self, id: MailboxId) {
        self.registry.remove_closure(id);
        self.claimed_actor_mailboxes.retain(|i| *i != id);
    }

    /// Clone-able mail-send handle. Capabilities stash this into
    /// their dispatcher state to send mail to other mailboxes
    /// (including other capabilities). Same `Arc<Mailer>` every
    /// capability sees, so an envelope sent here goes through the
    /// substrate's routing table the same way component-originated mail
    /// does.
    #[must_use]
    pub fn mail_send_handle(&self) -> Arc<Mailer> {
        Arc::clone(self.mailer)
    }

    /// Borrow the chassis's registry. Capabilities that resolve
    /// names or descriptors at boot (today: the hub client capability
    /// cloning the registry into its TCP reader thread) reach for
    /// this; most capabilities don't need it.
    #[must_use]
    pub fn registry(&self) -> &Arc<Registry> {
        self.registry
    }

    /// Borrow the chassis's mailer. Same shape as
    /// [`Self::mail_send_handle`] but returns a borrow instead of a
    /// clone — preferred when the capability is going to clone with
    /// `Arc::clone` itself.
    #[must_use]
    pub fn mailer(&self) -> &Arc<Mailer> {
        self.mailer
    }

    /// Clone the chassis's [`FatalAborter`]. Read by
    /// [`crate::NativeBinding::from_ctx`] so the wasm-trap abort
    /// path has somewhere to abort to without each
    /// transport plumbing [`crate::HubOutbound`] itself.
    #[must_use]
    pub fn fatal_aborter(&self) -> Arc<dyn FatalAborter> {
        Arc::clone(self.aborter)
    }

    /// Borrow the chassis's [`crate::Spawner`]. Used by
    /// [`crate::NativeBinding::from_ctx`] to clone an `Arc<Spawner>`
    /// into every booted actor's transport so per-handler
    /// `NativeCtx::spawn_child` can reach the spawn machinery.
    #[must_use]
    pub fn spawner_arc(&self) -> &Arc<crate::Spawner> {
        self.spawner
    }

    /// Issue 635 PR C: borrow the chassis worker pool's wake sink
    /// (ready-queue sender + spin/park coordinator). The `Pooled` branch
    /// of `make_native_actor_boot` clones this into the
    /// [`crate::scheduler::WakeHandle`] that fires when the actor's
    /// mailbox accepts a send.
    pub(crate) fn wake_sink(&self) -> &WakeSink {
        self.spawner.wake_sink()
    }

    /// Install the fallback-router handler. At most one capability
    /// may claim the slot; a second call returns
    /// [`BootError::FallbackRouterAlreadyClaimed`].
    ///
    /// Phase 1 stores the handler but does not consult it from substrate
    /// dispatch. Phase 4 wires `Mailer::push` against this slot and
    /// removes today's hub-specific `Mailer.outbound` field, at
    /// which point `HubClientCapability` (in
    /// `aether-substrate-bundle::hub`) claims the slot to forward
    /// unresolved mail over TCP.
    pub fn claim_fallback_router(&mut self, handler: FallbackRouter) -> Result<(), BootError> {
        if self.fallback.is_some() {
            return Err(BootError::FallbackRouterAlreadyClaimed);
        }
        *self.fallback = Some(handler);
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
mod tests {
    use super::*;

    use aether_actor::Local;

    use crate::actor::native::local::with_stamped;
    use aether_kinds::descriptors;

    use aether_kinds::trace::Nanos;

    use crate::actor::registry::ActorRegistry;
    use crate::config::RingCapacities;
    use crate::handle_store::HandleStore;
    use crate::mail::registry::MailboxEntry;
    use crate::mail::{KindId, MailId, MailRef, Source};
    use crate::runtime::lifecycle::PanicAborter;
    use crate::scheduler::{Pool, PoolConfig, PoolHandle};

    /// Per-actor scratch type for the iamacoffeepot/aether#1272 round-trip
    /// test. Mirrors the `ActorLogRing` shape (`Default + Local`) at the
    /// level the framework dispatch arm reaches it through.
    #[derive(Default)]
    struct Probe(u32);
    impl Local for Probe {}

    /// iamacoffeepot/aether#1272: a `MailboxClaim` returns its per-actor
    /// `ActorSlots`. Driver-as-actor capabilities (today only the desktop
    /// window driver) wrap their bespoke drain in
    /// `local::with_stamped(&claim.actor_slots, …)` so framework dispatch
    /// arms (`aether.log.tail` / `aether.trace.tail` / `aether.cost.tail`)
    /// reach the actor's per-actor `Local<T>` rings.
    #[test]
    fn claim_mailbox_returns_stampable_actor_slots() {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let actor_registry = Arc::new(ActorRegistry::new());
        let pool = Pool::start(PoolConfig::default(), Arc::clone(&aborter));
        let spawner = Arc::new(crate::Spawner::new(
            Arc::clone(&registry),
            actor_registry,
            Arc::clone(&mailer),
            Arc::clone(&aborter),
            pool.wake_sink(),
            RingCapacities::default(),
        ));
        let mut fallback: Option<FallbackRouter> = None;
        let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
        let mut ctx = ChassisCtx::new(
            &registry,
            &mailer,
            &mut fallback,
            &aborter,
            &mut claimed_actor_mailboxes,
            &spawner,
        );

        let claim = ctx
            .claim_mailbox_with_override("test.iamacoffeepot.1272.driver")
            .expect("first claim succeeds");

        // Stamp the claim's slots into TLS and write through a `Local<T>`;
        // a second `with_stamped` round-trips reads back through the same
        // slot — the property the framework dispatch arm relies on when
        // it calls `ActorLogRing::try_with(...)` inside the driver's
        // bespoke drain.
        with_stamped(claim.actor_slots.slots(), || {
            Probe::with_mut(|p| p.0 = 0x1272);
        });
        let read_back = with_stamped(claim.actor_slots.slots(), || {
            Probe::try_with(|p| p.0).expect("stamped slots carry Probe")
        });
        assert_eq!(read_back, 0x1272);

        // A fresh `ActorSlots` (not the one carried on the claim) must
        // see the Local at its default — confirms the round-trip above
        // actually went through the claim's slots and not a stale TLS
        // remnant.
        let other = SharedActorSlots::new();
        let other_read = with_stamped(other.slots(), || {
            Probe::try_with(|p| p.0).expect("any stamped slots carry Probe")
        });
        assert_eq!(other_read, 0, "fresh slots see the Local at its default");
    }

    /// The long-lived owned infra a `ChassisCtx` borrows from. Held by
    /// the test for the duration of the claim so the registered handler
    /// outlives the `ctx` that registered it.
    type TestInfra = (
        Arc<Registry>,
        Arc<Mailer>,
        Arc<crate::Spawner>,
        Arc<dyn FatalAborter>,
        PoolHandle,
    );

    fn test_infra() -> TestInfra {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let actor_registry = Arc::new(ActorRegistry::new());
        let pool = Pool::start(PoolConfig::default(), Arc::clone(&aborter));
        let spawner = Arc::new(crate::Spawner::new(
            Arc::clone(&registry),
            actor_registry,
            Arc::clone(&mailer),
            Arc::clone(&aborter),
            pool.wake_sink(),
            RingCapacities::default(),
        ));
        (registry, mailer, spawner, aborter, pool)
    }

    /// An armed `OwnedDispatch` addressed at `id`, shaped like the
    /// `subscribe_self` mail a loaded component sends from its `wire`
    /// hook. Armed, so dropping it without `discharge`/`mark_transferred`
    /// trips the ADR-0094 guard in a debug build.
    fn armed_subscribe_self(id: MailboxId) -> OwnedDispatch {
        OwnedDispatch::armed(
            KindId(7),
            "aether.lifecycle.subscribe_self".to_owned(),
            None,
            Source::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::new(id, 1),
            MailId::new(id, 1),
            None,
            Nanos(0),
            0,
            id,
        )
    }

    /// ADR-0094 / #1564: the `claim_mailbox_drop_on_shutdown` inbox
    /// closure registers under a `Weak` sender, so once the cap sheds its
    /// `MailboxSender` at shutdown the `weak.upgrade()` returns `None`. A
    /// mail arriving in that window (e.g. a loaded component's
    /// `subscribe_self` racing the lifecycle cap's teardown) must be
    /// `mark_transferred` at the seam, not dropped armed — which would
    /// trip the obligation guard and fatally abort the substrate.
    #[test]
    fn drop_on_shutdown_inbox_transfers_obligation_when_sender_gone() {
        let (registry, mailer, spawner, aborter, _pool) = test_infra();
        let claim_id;
        {
            let mut fallback: Option<FallbackRouter> = None;
            let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
            let mut ctx = ChassisCtx::new(
                &registry,
                &mailer,
                &mut fallback,
                &aborter,
                &mut claimed_actor_mailboxes,
                &spawner,
            );
            let claim = ctx
                .claim_mailbox_drop_on_shutdown_with_override("test.1564.sender_gone")
                .expect("claim succeeds");
            claim_id = claim.id;
            // Drop the only strong `Sender` — the registry's `Weak` now
            // fails to upgrade, exactly as it does once a cap shuts down.
            drop(claim.mailbox_sender);
            drop(claim.receiver);
        }
        let Some(MailboxEntry::Inbox { handler, .. }) = registry.entry(claim_id) else {
            panic!("claimed mailbox should be an Inbox entry");
        };
        // Pre-fix this dropped the armed dispatch and panicked the guard.
        handler.enqueue(armed_subscribe_self(claim_id));
    }

    /// ADR-0094 / #1564: the receiver-gone branch (`tx.send` returns
    /// `SendError` because the dispatcher's receiver dropped) must also
    /// `mark_transferred`, not drop the armed dispatch.
    #[test]
    fn drop_on_shutdown_inbox_transfers_obligation_when_receiver_gone() {
        let (registry, mailer, spawner, aborter, _pool) = test_infra();
        let claim_id;
        // Hold the `MailboxSender` so `weak.upgrade()` still succeeds; the
        // dropped receiver is what forces the `SendError` branch.
        let _keep_sender;
        {
            let mut fallback: Option<FallbackRouter> = None;
            let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
            let mut ctx = ChassisCtx::new(
                &registry,
                &mailer,
                &mut fallback,
                &aborter,
                &mut claimed_actor_mailboxes,
                &spawner,
            );
            let claim = ctx
                .claim_mailbox_drop_on_shutdown_with_override("test.1564.receiver_gone")
                .expect("claim succeeds");
            claim_id = claim.id;
            drop(claim.receiver);
            _keep_sender = claim.mailbox_sender;
        }
        let Some(MailboxEntry::Inbox { handler, .. }) = registry.entry(claim_id) else {
            panic!("claimed mailbox should be an Inbox entry");
        };
        // Pre-fix this dropped the armed dispatch and panicked the guard.
        handler.enqueue(armed_subscribe_self(claim_id));
    }

    /// ADR-0094 / #1565: [`relay_or_transfer`] owns both abandonment
    /// seams. Drive it directly through sender-gone (the `Weak`'s strong
    /// was dropped) and receiver-gone (the `Receiver` was dropped) and
    /// assert the returned outcome plus that the armed dispatch is
    /// transferred — dropping it armed would trip the debug guard. This
    /// is the helper-level mirror of the `drop_on_shutdown_inbox_*` tests.
    #[test]
    fn relay_or_transfer_settles_obligation_at_both_seams() {
        let id = MailboxId(0x1565);
        let wake = MailboxWakeSlot::default();

        // Sender gone: the only strong `Sender` dropped, so the `Weak`
        // fails to upgrade.
        let (tx, _rx) = mpsc::channel::<Envelope>();
        let tx = Arc::new(tx);
        let weak = Arc::downgrade(&tx);
        drop(tx);
        match relay_or_transfer(armed_subscribe_self(id), &weak, &wake) {
            RelayOutcome::SenderGone { kind_name } => {
                assert_eq!(kind_name, "aether.lifecycle.subscribe_self");
            }
            other => panic!("expected SenderGone, got {other:?}"),
        }

        // Receiver gone: the `Sender` upgrades but the `Receiver` dropped,
        // so `mpsc::send` returns `SendError`.
        let (tx, rx) = mpsc::channel::<Envelope>();
        let tx = Arc::new(tx);
        let weak = Arc::downgrade(&tx);
        drop(rx);
        match relay_or_transfer(armed_subscribe_self(id), &weak, &wake) {
            RelayOutcome::ReceiverGone { kind_name } => {
                assert_eq!(kind_name, "aether.lifecycle.subscribe_self");
            }
            other => panic!("expected ReceiverGone, got {other:?}"),
        }
        drop(tx);
    }

    /// The happy path: [`relay_or_transfer`] moves the envelope onto the
    /// channel and fires the wake hook. The delivered (armed) dispatch is
    /// discharged by draining the receiver — the dispatcher's job in
    /// production — so the obligation guard is satisfied.
    #[test]
    fn relay_or_transfer_delivers_and_wakes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let id = MailboxId(0x1565);
        let (tx, rx) = mpsc::channel::<Envelope>();
        let tx = Arc::new(tx);
        let weak = Arc::downgrade(&tx);

        let fired = Arc::new(AtomicBool::new(false));
        let fired_for_hook = Arc::clone(&fired);
        let wake = MailboxWakeSlot::default();
        wake.set(Arc::new(move || {
            fired_for_hook.store(true, Ordering::SeqCst);
        }));

        match relay_or_transfer(armed_subscribe_self(id), &weak, &wake) {
            RelayOutcome::Delivered => {}
            other => panic!("expected Delivered, got {other:?}"),
        }
        assert!(fired.load(Ordering::SeqCst), "wake hook fired on delivery");
        // Drain + discharge the delivered envelope (the dispatcher does
        // this in production); otherwise the armed guard panics on drop.
        let env = rx.recv().expect("delivered envelope is on the channel");
        env.discharge();
        drop(tx);
    }
}
