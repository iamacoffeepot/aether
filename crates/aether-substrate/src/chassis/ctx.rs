//! Boot machinery shared by the chassis builder (`chassis::builder::Builder`,
//! ADR-0071): mailbox claim helpers, the per-cap [`MailboxClaim`] /
//! [`DropOnShutdownClaim`] result shapes, and the [`ChassisCtx`]
//! threaded through every cap's boot. Sibling modules: error types
//! live in `chassis::error`; the cross-flavour [`Envelope`] shape lives
//! in `actor::native::envelope`.

use std::sync::Arc;
use std::sync::mpsc;

use aether_actor::Actor;

use crate::actor::native::envelope::Envelope;
use crate::chassis::error::BootError;
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
            Arc::new(move |dispatch: OwnedDispatch| {
                let env: Envelope = dispatch;
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
            Arc::new(move |dispatch: OwnedDispatch| {
                let Some(tx) = weak.upgrade() else {
                    tracing::warn!(
                        target: "aether_substrate::capability",
                        kind = %dispatch.kind_name,
                        "capability mailbox sender dropped — mail discarded"
                    );
                    return;
                };
                let env: Envelope = dispatch;
                if let Err(mpsc::SendError(env)) = tx.send(env) {
                    tracing::warn!(
                        target: "aether_substrate::capability",
                        kind = %env.kind_name,
                        "capability mailbox receiver dropped — mail discarded"
                    );
                    return;
                }
                // Issue 635 PR C: fire the pool wake hook (if installed).
                // `get()` returns `None` only during the boot window
                // before the slot is built, so this is a single relaxed
                // atomic load on the hot path.
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
