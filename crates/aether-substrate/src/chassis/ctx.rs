//! Boot machinery shared by the chassis builder (`chassis::builder::Builder`,
//! ADR-0071): mailbox claim helpers, the per-cap [`MailboxClaim`] /
//! [`FrameBoundClaim`] / [`DropOnShutdownClaim`] result shapes, and
//! the [`ChassisCtx`] threaded through every cap's boot. Sibling
//! modules: error types live in `chassis::error`; the cross-flavour
//! [`Envelope`](crate::actor::native::envelope::Envelope) shape lives
//! in `actor::native::envelope`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};

use aether_actor::Actor;

use crate::actor::native::envelope::Envelope;
use crate::chassis::error::BootError;
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::runtime::lifecycle::FatalAborter;

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
#[derive(Debug)]
pub struct MailboxClaim {
    pub id: MailboxId,
    pub receiver: mpsc::Receiver<Envelope>,
}

/// Result returned from [`ChassisCtx::claim_mailbox_drop_on_shutdown`].
///
/// Same as [`MailboxClaim`] plus a strong [`MailboxSender`] the
/// capability is expected to drop during shutdown to break the
/// channel — the channel-drop + join lifecycle ADR-0074 §Decision 5
/// settles on. The registry's sink-handler closure holds only a
/// [`std::sync::Weak`] back-reference, so once the strong handle
/// goes away, in-flight deliveries warn-drop and the dispatcher's
/// `recv()` returns `Err(Disconnected)`.
///
/// Phase 2a: `LogCapability` is the first consumer; the other
/// capabilities continue with `claim_mailbox` + `Arc<AtomicBool>`
/// polling until their own migration PRs land.
#[derive(Debug)]
pub struct DropOnShutdownClaim {
    pub id: MailboxId,
    pub receiver: mpsc::Receiver<Envelope>,
    pub mailbox_sender: MailboxSender,
    /// Issue 635 PR C: optional wake hook for `Pooled` actors. The
    /// mailbox closure invokes this after a successful inbox push so
    /// the chassis worker pool re-queues the actor's
    /// [`crate::scheduler::DispatcherSlot`]. `Dedicated` actors
    /// (today: every cap) leave this empty — the closure's `get()`
    /// is a single atomic load, ~free.
    ///
    /// Populated post-claim by the `Pooled` branch of
    /// `make_native_actor_boot` / `Spawner::spawn_actor` after the
    /// slot exists.
    pub wake_slot: Arc<MailboxWakeSlot>,
}

/// Cell holding the optional wake hook a `Pooled` mailbox fires after
/// each accepted send. The mailbox closure captures `Arc<MailboxWakeSlot>`
/// at registration time; the spawn path populates it once the
/// [`crate::scheduler::DispatcherSlot`] exists.
#[derive(Default)]
pub struct MailboxWakeSlot {
    inner: std::sync::OnceLock<MailboxWakeFn>,
}

impl std::fmt::Debug for MailboxWakeSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

    /// Borrow the installed hook. Returns `None` when the claim is
    /// for a `Dedicated` actor (no hook ever installed). Hot path —
    /// `OnceLock::get` is a single relaxed load.
    pub(crate) fn get(&self) -> Option<&MailboxWakeFn> {
        self.inner.get()
    }
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
    /// [`ChassisCtx::claim_mailbox_drop_on_shutdown`] /
    /// [`ChassisCtx::claim_frame_bound_mailbox`] build these.
    pub(crate) fn new(inner: Arc<mpsc::Sender<Envelope>>) -> Self {
        Self { _inner: inner }
    }
}

/// Result returned from [`ChassisCtx::claim_frame_bound_mailbox`].
///
/// Same as [`DropOnShutdownClaim`] plus a `pending` counter that the
/// sink registration handler increments on every accepted send and
/// the capability's dispatcher decrements after each processed
/// envelope. The chassis collects this counter so
/// [`crate::chassis::frame_loop::drain_frame_bound_or_abort`] can wait
/// on it as part of the per-frame drain barrier (ADR-0074 §Decision 5).
///
/// Capabilities authored with `Capability::FRAME_BARRIER = true`
/// must claim through this method instead of
/// [`ChassisCtx::claim_mailbox_drop_on_shutdown`] — otherwise the
/// chassis has no counter to wait on and the barrier degrades to
/// "components only", reintroducing the race the FRAME_BARRIER
/// classification exists to close.
#[derive(Debug)]
pub struct FrameBoundClaim {
    pub id: MailboxId,
    pub receiver: mpsc::Receiver<Envelope>,
    pub mailbox_sender: MailboxSender,
    /// Shared with the registry's sink handler. The handler increments
    /// before pushing into the mpsc; the capability's dispatcher must
    /// decrement after each `dispatch()` returns.
    pub pending: Arc<AtomicU64>,
    /// Issue 635 PR C: see [`DropOnShutdownClaim::wake_slot`].
    pub wake_slot: Arc<MailboxWakeSlot>,
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

/// Marker trait for type-erased chassis-stored entries. The chassis
/// builder's `BootedPassives` holds per-cap shutdown shims as
/// `Box<dyn ActorErased>` so the chassis can drop them in reverse
/// boot order regardless of cap type.
pub trait ActorErased: Send {}

/// Kernel-side handle bundle exposed to a capability during its
/// `boot()` call. Shared (`&mut`) across every `boot()` in the
/// builder — one ctx per build, threaded through the capability list
/// in declaration order (ADR-0070 resolved decision 4).
pub struct ChassisCtx<'a> {
    registry: &'a Arc<Registry>,
    mailer: &'a Arc<Mailer>,
    fallback: &'a mut Option<FallbackRouter>,
    /// Per-mailbox pending counters collected from
    /// [`ChassisCtx::claim_frame_bound_mailbox`] calls. The chassis
    /// builder hands these to the resulting `PassiveChassis` /
    /// `BuiltChassis` so the frame loop can wait on them via
    /// [`crate::chassis::frame_loop::drain_frame_bound_or_abort`].
    frame_bound_pending: &'a mut Vec<(MailboxId, Arc<AtomicU64>)>,
    /// Membership view of the same set the `frame_bound_pending` Vec
    /// covers, shared with every [`crate::NativeBinding`] built
    /// against this chassis. Capabilities clone the [`Arc`] into their
    /// transport at boot; the transport's cross-class `wait_reply`
    /// guard reads it (with a brief read-lock) to classify the
    /// recipient of an outbound request as frame-bound or
    /// free-running. [`Self::claim_frame_bound_mailbox`] inserts the
    /// claimed mailbox id here in addition to pushing onto the
    /// pending-counter list.
    frame_bound_set: &'a Arc<RwLock<HashSet<MailboxId>>>,
    /// Indirection over [`crate::runtime::lifecycle::fatal_abort`] cloned into
    /// every [`crate::NativeBinding`] this ctx builds, so the
    /// cross-class `wait_reply` guard (ADR-0074 §Decision 5) can
    /// abort without each capability needing to plumb
    /// [`crate::HubOutbound`] itself. Defaults to
    /// [`crate::runtime::lifecycle::PanicAborter`] when the chassis
    /// builder doesn't override — production drivers swap in
    /// [`crate::runtime::lifecycle::OutboundFatalAborter`] via
    /// [`crate::chassis::builder::Builder::with_aborter`].
    aborter: &'a Arc<dyn FatalAborter>,
    /// Issue #601: every actor-mailbox claim (`claim_mailbox_*`,
    /// `claim_frame_bound_mailbox_*`, `claim_mailbox_drop_on_shutdown_*`)
    /// appends its `MailboxId` here. The chassis builder reads the
    /// list after `boot_passives` to dispatch
    /// `aether.log.configure_drain` mail to each booted actor
    /// so its `LogDrainSlot` resolves to the chassis's declared drain
    /// (`Builder::with_log_drain<T>()`).
    ///
    /// Synchronous-handler registrations (e.g. `AETHER_DIAGNOSTICS`)
    /// go through `Registry::register_inline` directly and do *not*
    /// land here — they're not actors and have no `LogDrainSlot` to
    /// install.
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
    /// [`crate::chassis::builder::Builder`]. Eight refs is one
    /// over clippy's default; the alternative — a builder-of-the-builder
    /// — pays the same plumbing cost without adding clarity, since
    /// every chassis path that constructs a `ChassisCtx` already has
    /// every ref in scope.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        registry: &'a Arc<Registry>,
        mailer: &'a Arc<Mailer>,
        fallback: &'a mut Option<FallbackRouter>,
        frame_bound_pending: &'a mut Vec<(MailboxId, Arc<AtomicU64>)>,
        frame_bound_set: &'a Arc<RwLock<HashSet<MailboxId>>>,
        aborter: &'a Arc<dyn FatalAborter>,
        claimed_actor_mailboxes: &'a mut Vec<MailboxId>,
        spawner: &'a Arc<crate::Spawner>,
    ) -> Self {
        Self {
            registry,
            mailer,
            fallback,
            frame_bound_pending,
            frame_bound_set,
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
    pub fn claim_mailbox<C: Actor>(&mut self) -> Result<MailboxClaim, BootError> {
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
        let id = self.registry.try_register_inbox(
            name.to_owned(),
            // iamacoffeepot/aether#848: closure takes [`OwnedDispatch`]
            // and moves it into [`Envelope`] via `From`. Zero clones
            // on the cap dispatch hot path — one `Vec<u8>` + one
            // `String` saved per Inbox dispatch through this claim
            // path. `SendError` returns the envelope on failure so
            // the warn-log reads `env.kind_name` (the owned String)
            // without needing to clone ahead of the send.
            Arc::new(move |dispatch: crate::mail::registry::OwnedDispatch| {
                let env = Envelope::from(dispatch);
                if let Err(mpsc::SendError(env)) = tx.send(env) {
                    tracing::warn!(
                        target: "aether_substrate::capability",
                        kind = %env.kind_name,
                        "capability mailbox receiver dropped — mail discarded"
                    );
                }
            }),
        )?;
        self.claimed_actor_mailboxes.push(id);
        Ok(MailboxClaim { id, receiver: rx })
    }

    /// Variant of [`Self::claim_mailbox`] that returns a strong
    /// [`MailboxSender`] alongside the receiver. Claims under
    /// `C::NAMESPACE`. See
    /// [`Self::claim_mailbox_drop_on_shutdown_with_override`] for the
    /// arbitrary-name escape hatch.
    pub fn claim_mailbox_drop_on_shutdown<C: Actor>(
        &mut self,
    ) -> Result<DropOnShutdownClaim, BootError> {
        self.claim_mailbox_drop_on_shutdown_with_override(C::NAMESPACE)
    }

    /// Variant of [`Self::claim_mailbox_with_override`] that returns
    /// a strong [`MailboxSender`] alongside the receiver. The registry
    /// holds only a [`std::sync::Weak`] reference to the sender, so
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
            // iamacoffeepot/aether#848 PR 3: see `claim_mailbox_with_override`
            // for the rationale. The pre-send `weak.upgrade()` check
            // logs `dispatch.kind_name` (still owned by the closure
            // at that point); the post-send fail branch reads
            // `env.kind_name` out of the `SendError` payload.
            Arc::new(move |dispatch: crate::mail::registry::OwnedDispatch| {
                let Some(tx) = weak.upgrade() else {
                    tracing::warn!(
                        target: "aether_substrate::capability",
                        kind = %dispatch.kind_name,
                        "capability mailbox sender dropped — mail discarded"
                    );
                    return;
                };
                let env = Envelope::from(dispatch);
                if let Err(mpsc::SendError(env)) = tx.send(env) {
                    tracing::warn!(
                        target: "aether_substrate::capability",
                        kind = %env.kind_name,
                        "capability mailbox receiver dropped — mail discarded"
                    );
                    return;
                }
                // Issue 635 PR C: fire the `Pooled` wake hook (if
                // installed). `Dedicated` actors leave it empty,
                // so this is a single relaxed atomic load on the
                // hot path.
                if let Some(wake) = wake_for_handler.get() {
                    wake();
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

    /// Variant of [`Self::claim_mailbox_drop_on_shutdown`] for
    /// frame-bound capabilities. Claims under `C::NAMESPACE`. See
    /// [`Self::claim_frame_bound_mailbox_with_override`] for the
    /// arbitrary-name escape hatch.
    pub fn claim_frame_bound_mailbox<C: Actor>(&mut self) -> Result<FrameBoundClaim, BootError> {
        self.claim_frame_bound_mailbox_with_override(C::NAMESPACE)
    }

    /// Variant of [`Self::claim_mailbox_drop_on_shutdown_with_override`]
    /// for frame-bound capabilities (ADR-0074 §Decision 5). In addition
    /// to the channel-drop shutdown machinery, the registered sink
    /// handler increments a `pending` counter on every accepted send;
    /// the capability's dispatcher must decrement after each
    /// processed envelope so the counter reflects "envelopes accepted
    /// by the sink but not yet drained by the dispatcher."
    ///
    /// The chassis collects the counter so
    /// [`crate::chassis::frame_loop::drain_frame_bound_or_abort`] can
    /// wait for it to hit zero before render submit. Pair this with
    /// `FRAME_BARRIER = true` on the [`Actor`] impl. See
    /// `aether_capabilities::RenderCapability` for the reference shape.
    ///
    /// # Panics
    /// Panics if the `frame_bound_set` `RwLock` is poisoned — fail-fast
    /// per ADR-0063: a poisoned lock means a prior writer panicked
    /// under the guard, a substrate-level invariant violation.
    pub fn claim_frame_bound_mailbox_with_override(
        &mut self,
        name: &str,
    ) -> Result<FrameBoundClaim, BootError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let tx = Arc::new(tx);
        let weak = Arc::downgrade(&tx);
        let pending = Arc::new(AtomicU64::new(0));
        let pending_for_handler = Arc::clone(&pending);
        let wake_slot: Arc<MailboxWakeSlot> = Arc::new(MailboxWakeSlot::default());
        let wake_for_handler = Arc::clone(&wake_slot);
        let id = self.registry.try_register_inbox(
            name.to_owned(),
            // iamacoffeepot/aether#848 PR 3: see `claim_mailbox_with_override`
            // for the rationale. Pre-send increment + post-fail
            // decrement bracket the `tx.send` exactly as before;
            // the only change is that `dispatch` is `OwnedDispatch`
            // (moved into `env` via `From`) and the failure branch
            // reads `env.kind_name` out of the `SendError`.
            Arc::new(move |dispatch: crate::mail::registry::OwnedDispatch| {
                let Some(tx) = weak.upgrade() else {
                    tracing::warn!(
                        target: "aether_substrate::capability",
                        kind = %dispatch.kind_name,
                        "frame-bound capability sender dropped — mail discarded"
                    );
                    return;
                };
                let env = Envelope::from(dispatch);
                // Increment before send so the dispatcher's
                // matching decrement-after-dispatch sees a count
                // > 0 by the time it tries to decrement. If the
                // send itself fails (receiver dropped between the
                // upgrade and the send — shutdown race), undo
                // the increment so the counter doesn't drift up.
                pending_for_handler.fetch_add(1, Ordering::AcqRel);
                if let Err(mpsc::SendError(env)) = tx.send(env) {
                    pending_for_handler.fetch_sub(1, Ordering::AcqRel);
                    tracing::warn!(
                        target: "aether_substrate::capability",
                        kind = %env.kind_name,
                        "frame-bound capability receiver dropped — mail discarded"
                    );
                    return;
                }
                // Issue 635 PR C: fire the `Pooled` wake hook (if
                // installed). Frame-bound + pool-scheduled is a
                // valid combination per the issue's section 5
                // ("FRAME_BARRIER and SCHEDULING are orthogonal");
                // the chassis frame loop reads `pending` regardless
                // of where the dispatch happens.
                if let Some(wake) = wake_for_handler.get() {
                    wake();
                }
            }),
        )?;
        self.frame_bound_pending.push((id, Arc::clone(&pending)));
        // Mirror the membership into the shared set so each
        // capability's [`crate::NativeBinding`] can resolve "is the
        // recipient of this `wait_reply` frame-bound?" with a single
        // read-lock — without each transport having to scan the
        // pending-counter Vec on every check.
        self.frame_bound_set.write().unwrap().insert(id);
        self.claimed_actor_mailboxes.push(id);
        Ok(FrameBoundClaim {
            id,
            receiver: rx,
            mailbox_sender: MailboxSender::new(tx),
            pending,
            wake_slot,
        })
    }

    /// Issue 607 Phase 7: undo a previous `claim_*_mailbox` call.
    /// Removes the sink from the chassis registry, the (id, counter)
    /// entry from `frame_bound_pending`, the id from
    /// `frame_bound_set`, and the id from `claimed_actor_mailboxes`.
    /// Idempotent: calling on an id that wasn't claimed is a no-op.
    ///
    /// Used in the singleton-boot unwind path (chassis_builder /
    /// capability) when `init` fails after the cap mailbox was
    /// claimed. Without this, the failed cap leaves a sink registered
    /// against its namespace and a stuck counter in
    /// `frame_bound_pending` that the chassis frame loop would wait
    /// for forever.
    ///
    /// # Panics
    /// Panics if the `frame_bound_set` `RwLock` is poisoned — fail-fast
    /// per ADR-0063: a poisoned lock means a prior writer panicked
    /// under the guard, a substrate-level invariant violation.
    pub fn unclaim_mailbox(&mut self, id: MailboxId) {
        self.registry.remove_closure(id);
        self.frame_bound_pending.retain(|(i, _)| *i != id);
        self.frame_bound_set.write().unwrap().remove(&id);
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

    /// Read the list of frame-bound pending counters collected so far
    /// from earlier `claim_frame_bound_mailbox` calls. Used by
    /// [`crate::chassis::builder::DriverCtx::frame_bound_pending`] to
    /// snapshot the list at driver-boot time; capabilities that just
    /// want their own counter should hold the `Arc<AtomicU64>` from
    /// their [`FrameBoundClaim`] directly instead.
    #[must_use]
    pub fn frame_bound_pending(&self) -> &[(MailboxId, Arc<AtomicU64>)] {
        self.frame_bound_pending
    }

    /// Clone the chassis's shared frame-bound membership set. Read by
    /// [`crate::NativeBinding::from_ctx`] so the cross-class
    /// `wait_reply` guard can classify each outbound recipient.
    /// Capabilities that just want to send mail don't need this.
    #[must_use]
    pub fn frame_bound_set(&self) -> Arc<RwLock<HashSet<MailboxId>>> {
        Arc::clone(self.frame_bound_set)
    }

    /// Clone the chassis's [`FatalAborter`]. Read by
    /// [`crate::NativeBinding::from_ctx`] so the cross-class
    /// `wait_reply` guard has somewhere to abort to without each
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

    /// Issue 635 PR C: borrow the chassis worker pool's ready-queue
    /// sender. The `Pooled` branch of `make_native_actor_boot` clones
    /// this into the [`crate::scheduler::WakeHandle`] that fires when
    /// the actor's mailbox accepts a send.
    pub(crate) fn pool_ready_tx(
        &self,
    ) -> &crossbeam_channel::Sender<Arc<dyn crate::scheduler::Drainable>> {
        self.spawner.pool_ready_tx()
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
