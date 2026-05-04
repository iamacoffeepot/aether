//! ADR-0070 Phase 1: capability trait, chassis builder, and ctx.
//!
//! This module is purely additive. Existing chassis boot paths
//! (`SubstrateBoot::builder`) keep working unchanged; nothing yet
//! consumes the new builder. Phases 2–5 migrate each native sink
//! (handle, log, io, net, audio, render+camera) into a submodule of
//! `crate::capabilities` that implements [`Capability`]; Phase 4
//! wires the dispatch path to consult [`ChassisCtx::claim_fallback_router`]
//! and removes the substrate-side bubble-up in `Mailer`.
//!
//! The shape mirrors a wasm component (kinds + dispatcher + state +
//! lifecycle) but compiled in: a native capability owns mailboxes,
//! a Rust dispatcher, Rust state, and a `boot`/`shutdown` lifecycle.
//! See ADR-0070 for the full rationale.
//!
//! # Phase 1 scope
//!
//! - Trait + builder + ctx wiring against an `Arc<Registry>` and
//!   `Arc<Mailer>` supplied by the chassis.
//! - [`ChassisCtx::claim_mailbox`] registers an mpsc-fed sink on the
//!   registry under the given name and hands the capability the
//!   receiver. The registered handler converts each borrowed sink
//!   call into an owned [`Envelope`] and forwards it.
//! - [`ChassisCtx::claim_fallback_router`] stores a single fallback
//!   handler; substrate dispatch does not consult it yet (Phase 4).
//! - No sinks are migrated yet — `crate::capabilities` is an empty
//!   submodule placeholder that future PRs populate.

use std::collections::HashSet;
use std::error::Error as StdError;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::lifecycle::{FatalAborter, PanicAborter};
use crate::mail::{KindId, MailboxId, ReplyTo};
use crate::mailer::Mailer;
use crate::registry::{NameConflict, Registry};

/// One mail delivered to a capability through its mpsc receiver.
///
/// Sinks today receive borrowed args (`&str`, `&[u8]`); routing across
/// an mpsc channel forces ownership. Capabilities that care about
/// ergonomics destructure this once at the top of their loop.
#[derive(Debug)]
pub struct Envelope {
    pub kind: KindId,
    pub kind_name: String,
    pub origin: Option<String>,
    pub sender: ReplyTo,
    pub payload: Vec<u8>,
    pub count: u32,
}

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
/// Same as [`MailboxClaim`] plus a strong [`SinkSender`] the
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
    pub sink_sender: SinkSender,
}

/// Strong handle to the inbound `Sender<Envelope>` for a mailbox
/// claimed via [`ChassisCtx::claim_mailbox_drop_on_shutdown`]. Held
/// by the capability for the lifetime of its dispatcher thread;
/// dropping it disconnects the channel and lets the dispatcher's
/// `recv()` return `Err(Disconnected)` immediately.
#[derive(Debug)]
pub struct SinkSender {
    // Held purely for its `Drop` side effect. When this `Arc` drops
    // and refcount hits zero, the inner `Sender` drops, the channel
    // disconnects, and the dispatcher exits its `recv()` loop.
    _inner: Arc<mpsc::Sender<Envelope>>,
}

impl SinkSender {
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
/// [`BootedChassis::drain_frame_bound`] can wait on it as part of
/// the per-frame drain barrier (ADR-0074 §Decision 5).
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
    pub sink_sender: SinkSender,
    /// Shared with the registry's sink handler. The handler increments
    /// before pushing into the mpsc; the capability's dispatcher must
    /// decrement after each `dispatch()` returns.
    pub pending: Arc<AtomicU64>,
}

/// Diagnostic returned from [`BootedChassis::drain_frame_bound`] when
/// a frame-bound capability's inbox didn't drain within the budget.
/// The chassis frame loop routes this through `lifecycle::fatal_abort`
/// the same way component-side wedges do — see
/// [`crate::frame_loop::drain_frame_bound_or_abort`].
#[derive(Debug, Clone, Copy)]
pub struct WedgedFrameBound {
    pub mailbox: MailboxId,
    pub pending: u64,
    pub waited: Duration,
}

impl fmt::Display for WedgedFrameBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "frame-bound dispatcher wedged: mailbox={} pending={} waited={:?}",
            self.mailbox, self.pending, self.waited,
        )
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

/// Failure modes capability boot can raise. Per ADR-0063, any boot
/// error aborts the chassis before user code runs — no partial boots.
#[derive(Debug)]
pub enum BootError {
    /// The mailbox name is already bound, either to another
    /// capability that claimed it earlier or to a legacy
    /// `Registry::register_sink` call from `SubstrateBoot::build`.
    /// Phase 2-5 expect this during the side-by-side period and
    /// remove the legacy registration in the same diff.
    MailboxAlreadyClaimed { name: String },
    /// A second capability tried to register a fallback router after
    /// one was already installed. The slot is single-claim by design.
    FallbackRouterAlreadyClaimed,
    /// Anything else a capability's boot wants to surface.
    Other(Box<dyn StdError + Send + Sync + 'static>),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootError::MailboxAlreadyClaimed { name } => {
                write!(f, "mailbox {name:?} already claimed")
            }
            BootError::FallbackRouterAlreadyClaimed => {
                f.write_str("fallback router slot already claimed")
            }
            BootError::Other(e) => write!(f, "capability boot failed: {e}"),
        }
    }
}

impl StdError for BootError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            BootError::Other(e) => Some(&**e),
            _ => None,
        }
    }
}

impl From<NameConflict> for BootError {
    fn from(e: NameConflict) -> Self {
        BootError::MailboxAlreadyClaimed { name: e.name }
    }
}

/// Forward wasmtime errors raised during chassis boot
/// (`SubstrateBoot::build`, `add_capability`, hub-client connect, etc.)
/// into [`BootError::Other`]. Any wasmtime error during boot is
/// definitionally a boot error — chassis trait impls can `?` the
/// wasmtime call directly without per-call `.map_err` glue.
impl From<wasmtime::Error> for BootError {
    fn from(e: wasmtime::Error) -> Self {
        BootError::Other(Box::new(std::io::Error::other(format!("{e}"))))
    }
}

/// A native capability: chassis-policy code that owns one or more
/// mailboxes plus the state behind them. Each capability is
/// `boot()`-ed once during chassis startup; teardown happens through
/// the cap's own [`Drop`] impl when the chassis-owned `Box<Self>`
/// drops (issue 525 Phase 2 collapsed the prior `Self` / `Self::Running`
/// pair into one struct, retiring `RunningCapability::shutdown` in
/// favour of `Drop`).
///
/// Implementors author one struct per cap. The struct typically holds
/// `Option<JoinHandle<()>>`, an `Option<SinkSender>` (drives channel-
/// drop shutdown), and the cap's `Arc<NativeTransport>` — `boot()`
/// fills these from `None` to `Some` and returns the same `self`. The
/// `Drop` impl drops the sender first to break the channel and then
/// joins the thread, mirroring the prior `RunningCapability::shutdown`
/// body.
pub trait Capability: Send + Sized + 'static {
    /// The recipient name this capability claims at boot — the same
    /// string components address with `Mailbox<K>::send` on the wire.
    /// Issue 525 Phase 1: declared on the trait so the cap's own type
    /// is the single source of truth, retiring the per-cap free
    /// `pub const X_MAILBOX_NAME` consts that mirrored this field.
    /// Each in-tree cap declares its `aether.<name>` namespace per the
    /// post-ADR-0074 Phase 5 convention; test fixtures with
    /// parameterized names declare a placeholder and bypass via
    /// [`ChassisCtx::claim_mailbox_with_override`] +
    /// friends.
    const NAMESPACE: &'static str;

    /// ADR-0074 §Decision 5 scheduling class. `true` means this
    /// capability participates in the per-frame drain barrier — the
    /// chassis frame loop waits for the dispatcher's inbox to quiesce
    /// before submitting the next render frame, so any mail a
    /// component sent this frame is integrated before submit.
    /// Defaults to `false` (free-running). Today only `RenderCapability`
    /// overrides; future drawing-side capabilities will too.
    ///
    /// The const is paired with [`ChassisCtx::claim_frame_bound_mailbox`]:
    /// frame-bound capabilities must claim their inbox through that
    /// method so the chassis collects the per-mailbox pending counter
    /// the barrier reads. The trait const itself is the static marker
    /// used by Phase 4's cross-class `wait_reply` guard.
    const FRAME_BARRIER: bool = false;

    /// Wire the capability into the chassis. The pre-boot `self`
    /// carries any constructor config (read by [`Self::new`]-style
    /// builders); `boot` claims mailboxes through `ctx`, spawns the
    /// dispatcher thread, and returns the same `self` with its
    /// runtime-state fields populated. On error the chassis aborts
    /// already-booted caps via their [`Drop`] impls.
    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError>;
}

/// Marker trait for type-erased capability storage in [`BootedChassis`]
/// and the [`crate::chassis_builder`] passive list. Implemented blanket
/// for every [`Capability`]; carries no methods of its own — teardown
/// runs through the cap's [`Drop`] impl when the `Box<dyn ActorErased>`
/// drops (issue 525 Phase 2).
pub trait ActorErased: Send {}
impl<C: Capability> ActorErased for C {}

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
    /// builder hands these to the resulting [`BootedChassis`] so the
    /// frame loop can wait on them via
    /// [`BootedChassis::drain_frame_bound`].
    frame_bound_pending: &'a mut Vec<(MailboxId, Arc<AtomicU64>)>,
    /// Membership view of the same set the `frame_bound_pending` Vec
    /// covers, shared with every [`crate::NativeTransport`] built
    /// against this chassis. Capabilities clone the [`Arc`] into their
    /// transport at boot; the transport's cross-class `wait_reply`
    /// guard reads it (with a brief read-lock) to classify the
    /// recipient of an outbound request as frame-bound or
    /// free-running. [`Self::claim_frame_bound_mailbox`] inserts the
    /// claimed mailbox id here in addition to pushing onto the
    /// pending-counter list.
    frame_bound_set: &'a Arc<RwLock<HashSet<MailboxId>>>,
    /// Indirection over [`crate::lifecycle::fatal_abort`] cloned into
    /// every [`crate::NativeTransport`] this ctx builds, so the
    /// cross-class `wait_reply` guard (ADR-0074 §Decision 5) can
    /// abort without each capability needing to plumb
    /// [`crate::HubOutbound`] itself. Defaults to
    /// [`PanicAborter`] when the chassis builder doesn't override —
    /// production drivers swap in
    /// [`crate::lifecycle::OutboundFatalAborter`] via
    /// [`ChassisBuilder::with_aborter`] /
    /// [`crate::chassis_builder::Builder::with_aborter`].
    aborter: &'a Arc<dyn FatalAborter>,
}

impl<'a> ChassisCtx<'a> {
    /// Internal constructor used by [`ChassisBuilder::build`] and the
    /// ADR-0071 [`crate::chassis_builder::Builder`].
    pub(crate) fn new(
        registry: &'a Arc<Registry>,
        mailer: &'a Arc<Mailer>,
        fallback: &'a mut Option<FallbackRouter>,
        frame_bound_pending: &'a mut Vec<(MailboxId, Arc<AtomicU64>)>,
        frame_bound_set: &'a Arc<RwLock<HashSet<MailboxId>>>,
        aborter: &'a Arc<dyn FatalAborter>,
    ) -> Self {
        Self {
            registry,
            mailer,
            fallback,
            frame_bound_pending,
            frame_bound_set,
            aborter,
        }
    }

    /// Register an mpsc-fed sink under `C::NAMESPACE` and return both
    /// its derived [`MailboxId`] (ADR-0029 hash) and the receiver.
    /// The capability's own type is the single source of truth for
    /// the recipient name (issue 525 Phase 1).
    ///
    /// Tests that need a parameterized name (one fixture, many
    /// claims) reach for [`Self::claim_mailbox_with_override`].
    pub fn claim_mailbox<C: Capability>(&mut self) -> Result<MailboxClaim, BootError> {
        self.claim_mailbox_with_override(C::NAMESPACE)
    }

    /// Register an mpsc-fed sink under `name` and return both its
    /// derived [`MailboxId`] (ADR-0029 hash) and the receiver.
    /// Escape hatch for tests with parameterized names; production
    /// caps go through [`Self::claim_mailbox`] so the cap's own
    /// `NAMESPACE` is authoritative.
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
        let id = self.registry.try_register_sink(
            name.to_owned(),
            Arc::new(
                move |kind: KindId,
                      kind_name: &str,
                      origin: Option<&str>,
                      sender: ReplyTo,
                      payload: &[u8],
                      count: u32| {
                    let env = Envelope {
                        kind,
                        kind_name: kind_name.to_owned(),
                        origin: origin.map(str::to_owned),
                        sender,
                        payload: payload.to_vec(),
                        count,
                    };
                    if tx.send(env).is_err() {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = kind_name,
                            "capability mailbox receiver dropped — mail discarded"
                        );
                    }
                },
            ),
        )?;
        Ok(MailboxClaim { id, receiver: rx })
    }

    /// Variant of [`Self::claim_mailbox`] that returns a strong
    /// [`SinkSender`] alongside the receiver. Claims under
    /// `C::NAMESPACE`. See
    /// [`Self::claim_mailbox_drop_on_shutdown_with_override`] for the
    /// arbitrary-name escape hatch.
    pub fn claim_mailbox_drop_on_shutdown<C: Capability>(
        &mut self,
    ) -> Result<DropOnShutdownClaim, BootError> {
        self.claim_mailbox_drop_on_shutdown_with_override(C::NAMESPACE)
    }

    /// Variant of [`Self::claim_mailbox_with_override`] that returns
    /// a strong [`SinkSender`] alongside the receiver. The registry
    /// holds only a [`std::sync::Weak`] reference to the sender, so
    /// when the capability drops the `SinkSender` (during shutdown),
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
        // Strong Arc rides on `DropOnShutdownClaim.sink_sender` and
        // lives for the capability's lifetime. The registry handler
        // only upgrades a `Weak` per call, so when the capability
        // drops its strong handle, the inner `Sender` also drops
        // and the dispatcher's `recv()` returns `Err(Disconnected)`.
        let tx = Arc::new(tx);
        let weak = Arc::downgrade(&tx);
        let id = self.registry.try_register_sink(
            name.to_owned(),
            Arc::new(
                move |kind: KindId,
                      kind_name: &str,
                      origin: Option<&str>,
                      sender: ReplyTo,
                      payload: &[u8],
                      count: u32| {
                    let Some(tx) = weak.upgrade() else {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = kind_name,
                            "capability mailbox sender dropped — mail discarded"
                        );
                        return;
                    };
                    let env = Envelope {
                        kind,
                        kind_name: kind_name.to_owned(),
                        origin: origin.map(str::to_owned),
                        sender,
                        payload: payload.to_vec(),
                        count,
                    };
                    if tx.send(env).is_err() {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = kind_name,
                            "capability mailbox receiver dropped — mail discarded"
                        );
                    }
                },
            ),
        )?;
        Ok(DropOnShutdownClaim {
            id,
            receiver: rx,
            sink_sender: SinkSender::new(tx),
        })
    }

    /// Variant of [`Self::claim_mailbox_drop_on_shutdown`] for
    /// frame-bound capabilities. Claims under `C::NAMESPACE`. See
    /// [`Self::claim_frame_bound_mailbox_with_override`] for the
    /// arbitrary-name escape hatch.
    pub fn claim_frame_bound_mailbox<C: Capability>(
        &mut self,
    ) -> Result<FrameBoundClaim, BootError> {
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
    /// [`BootedChassis::drain_frame_bound`] can wait for it to hit
    /// zero before render submit. Pair this with `FRAME_BARRIER = true`
    /// on the [`Capability`] impl. See
    /// [`crate::capabilities::render::RenderCapability`] for the
    /// reference shape.
    pub fn claim_frame_bound_mailbox_with_override(
        &mut self,
        name: &str,
    ) -> Result<FrameBoundClaim, BootError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let tx = Arc::new(tx);
        let weak = Arc::downgrade(&tx);
        let pending = Arc::new(AtomicU64::new(0));
        let pending_for_handler = Arc::clone(&pending);
        let id = self.registry.try_register_sink(
            name.to_owned(),
            Arc::new(
                move |kind: KindId,
                      kind_name: &str,
                      origin: Option<&str>,
                      sender: ReplyTo,
                      payload: &[u8],
                      count: u32| {
                    let Some(tx) = weak.upgrade() else {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = kind_name,
                            "frame-bound capability sender dropped — mail discarded"
                        );
                        return;
                    };
                    let env = Envelope {
                        kind,
                        kind_name: kind_name.to_owned(),
                        origin: origin.map(str::to_owned),
                        sender,
                        payload: payload.to_vec(),
                        count,
                    };
                    // Increment before send so the dispatcher's
                    // matching decrement-after-dispatch sees a count
                    // > 0 by the time it tries to decrement. If the
                    // send itself fails (receiver dropped between the
                    // upgrade and the send — shutdown race), undo
                    // the increment so the counter doesn't drift up.
                    pending_for_handler.fetch_add(1, Ordering::AcqRel);
                    if tx.send(env).is_err() {
                        pending_for_handler.fetch_sub(1, Ordering::AcqRel);
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = kind_name,
                            "frame-bound capability receiver dropped — mail discarded"
                        );
                    }
                },
            ),
        )?;
        self.frame_bound_pending.push((id, Arc::clone(&pending)));
        // Mirror the membership into the shared set so each
        // capability's [`crate::NativeTransport`] can resolve "is the
        // recipient of this `wait_reply` frame-bound?" with a single
        // read-lock — without each transport having to scan the
        // pending-counter Vec on every check.
        self.frame_bound_set.write().unwrap().insert(id);
        Ok(FrameBoundClaim {
            id,
            receiver: rx,
            sink_sender: SinkSender::new(tx),
            pending,
        })
    }

    /// Clone-able mail-send handle. Capabilities stash this into
    /// their dispatcher state to send mail to other mailboxes
    /// (including other capabilities). Same `Arc<Mailer>` every
    /// capability sees, so an envelope sent here goes through the
    /// substrate's routing table the same way component-originated mail
    /// does.
    pub fn mail_send_handle(&self) -> Arc<Mailer> {
        Arc::clone(self.mailer)
    }

    /// Borrow the chassis's registry. Capabilities that resolve
    /// names or descriptors at boot (today: the hub client capability
    /// cloning the registry into its TCP reader thread) reach for
    /// this; most capabilities don't need it.
    pub fn registry(&self) -> &Arc<Registry> {
        self.registry
    }

    /// Borrow the chassis's mailer. Same shape as
    /// [`Self::mail_send_handle`] but returns a borrow instead of a
    /// clone — preferred when the capability is going to clone with
    /// `Arc::clone` itself.
    pub fn mailer(&self) -> &Arc<Mailer> {
        self.mailer
    }

    /// Read the list of frame-bound pending counters collected so far
    /// from earlier `claim_frame_bound_mailbox` calls. Used by
    /// [`crate::chassis_builder::DriverCtx::frame_bound_pending`] to
    /// snapshot the list at driver-boot time; capabilities that just
    /// want their own counter should hold the `Arc<AtomicU64>` from
    /// their [`FrameBoundClaim`] directly instead.
    pub fn frame_bound_pending(&self) -> &[(MailboxId, Arc<AtomicU64>)] {
        self.frame_bound_pending
    }

    /// Clone the chassis's shared frame-bound membership set. Read by
    /// [`crate::NativeTransport::from_ctx`] so the cross-class
    /// `wait_reply` guard can classify each outbound recipient.
    /// Capabilities that just want to send mail don't need this.
    pub fn frame_bound_set(&self) -> Arc<RwLock<HashSet<MailboxId>>> {
        Arc::clone(self.frame_bound_set)
    }

    /// Clone the chassis's [`FatalAborter`]. Read by
    /// [`crate::NativeTransport::from_ctx`] so the cross-class
    /// `wait_reply` guard has somewhere to abort to without each
    /// transport plumbing [`crate::HubOutbound`] itself.
    pub fn fatal_aborter(&self) -> Arc<dyn FatalAborter> {
        Arc::clone(self.aborter)
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

/// Type-erased boot trampoline so [`ChassisBuilder`] can collect
/// heterogeneous capability types into one `Vec`. Each entry, when
/// invoked, takes the ctx, calls the underlying `Capability::boot`,
/// and boxes the resulting cap as [`ActorErased`] so the chassis can
/// store every cap in one homogeneous `Vec`. Teardown runs through
/// the cap's own [`Drop`] when the box drops (issue 525 Phase 2).
type BootFn =
    Box<dyn FnOnce(&mut ChassisCtx<'_>) -> Result<Box<dyn ActorErased>, BootError> + Send>;

/// Declarative chassis composition. Capabilities are added in
/// declaration order (ADR-0070 resolved decision 3); `build()` boots
/// them in the same order. The first failure aborts the build and
/// shuts down any capabilities that already booted, so no chassis
/// observes a partially-booted state.
pub struct ChassisBuilder {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    pending: Vec<BootFn>,
    aborter: Arc<dyn FatalAborter>,
}

impl ChassisBuilder {
    /// Construct a fresh builder against the given substrate handles.
    /// Phase 1 leaves it to the caller to supply these — Phase 6
    /// (TestBench rewrite) folds substrate construction into the
    /// builder; until then, the existing `SubstrateBoot::build` is
    /// the construction site.
    ///
    /// Defaults the cross-class `wait_reply` aborter to
    /// [`PanicAborter`] — appropriate for tests and embedder-driven
    /// chassis. Production drivers swap in
    /// [`crate::lifecycle::OutboundFatalAborter`] via
    /// [`Self::with_aborter`] before `build()`.
    pub fn new(registry: Arc<Registry>, mailer: Arc<Mailer>) -> Self {
        Self {
            registry,
            mailer,
            pending: Vec::new(),
            aborter: Arc::new(PanicAborter),
        }
    }

    /// Override the default [`PanicAborter`] with a chassis-supplied
    /// [`FatalAborter`]. Production drivers (desktop, headless) call
    /// this with an [`crate::lifecycle::OutboundFatalAborter`] cloned
    /// from their [`crate::HubOutbound`] so a cross-class
    /// `wait_reply` violation broadcasts `SubstrateDying` before
    /// process exit. Single-call: a second invocation overwrites the
    /// prior aborter.
    pub fn with_aborter(mut self, aborter: Arc<dyn FatalAborter>) -> Self {
        self.aborter = aborter;
        self
    }

    /// Append a capability. Boot order is declaration order.
    pub fn with<C>(mut self, cap: C) -> Self
    where
        C: Capability,
    {
        self.pending.push(Box::new(move |ctx| {
            let booted = cap.boot(ctx)?;
            Ok(Box::new(booted) as Box<dyn ActorErased>)
        }));
        self
    }

    /// Boot every capability. On the first error, already-booted
    /// capabilities are torn down in reverse order via their `Drop`
    /// impls before the error propagates — no partial-boot state is
    /// ever returned.
    pub fn build(self) -> Result<BootedChassis, BootError> {
        let ChassisBuilder {
            registry,
            mailer,
            pending,
            aborter,
        } = self;
        let mut fallback: Option<FallbackRouter> = None;
        let mut frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)> = Vec::new();
        let frame_bound_set: Arc<RwLock<HashSet<MailboxId>>> =
            Arc::new(RwLock::new(HashSet::new()));
        let mut booted: Vec<Box<dyn ActorErased>> = Vec::with_capacity(pending.len());
        for boot in pending {
            let mut ctx = ChassisCtx::new(
                &registry,
                &mailer,
                &mut fallback,
                &mut frame_bound_pending,
                &frame_bound_set,
                &aborter,
            );
            match boot(&mut ctx) {
                Ok(actor) => booted.push(actor),
                Err(e) => {
                    // Drop in reverse boot order: pop runs each cap's
                    // `Drop` (channel-drop + thread join) one at a
                    // time, so a panic in one teardown doesn't strand
                    // the others. Equivalent to letting the Vec drop,
                    // but explicit so the order is documented.
                    while let Some(actor) = booted.pop() {
                        drop(actor);
                    }
                    return Err(e);
                }
            }
        }
        Ok(BootedChassis {
            running: booted,
            _fallback: fallback,
            frame_bound_pending,
            frame_bound_set,
            aborter,
        })
    }
}

/// The output of [`ChassisBuilder::build`]. Holds every booted
/// capability (each merged Self-with-runtime-state per issue 525
/// Phase 2) plus the (optionally claimed) fallback router. On
/// `shutdown` the boxes are dropped in reverse boot order, running
/// each cap's [`Drop`] impl so later-booted state can rely on
/// earlier-booted state during its own teardown.
pub struct BootedChassis {
    running: Vec<Box<dyn ActorErased>>,
    /// Held for the lifetime of the chassis; Phase 4 will read this
    /// from `Mailer` dispatch. Today it's just owned-and-not-called.
    _fallback: Option<FallbackRouter>,
    /// Per-mailbox pending counters for frame-bound capabilities,
    /// collected at boot from [`ChassisCtx::claim_frame_bound_mailbox`].
    /// Read by [`Self::drain_frame_bound`] each frame; per ADR-0074
    /// §Decision 5 the frame loop waits on these alongside component
    /// drains so render submit sees fully-integrated state.
    frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)>,
    /// Membership view of the frame-bound mailbox set. Shared with
    /// every [`crate::NativeTransport`] booted under this chassis;
    /// also threaded into [`Self::add`] so post-build capabilities
    /// see (and contribute to) the same set.
    frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
    /// Aborter cloned into every [`crate::NativeTransport`] this
    /// chassis builds. Defaulted to [`PanicAborter`] by
    /// [`ChassisBuilder::new`]; production drivers swap in
    /// [`crate::lifecycle::OutboundFatalAborter`] via
    /// [`ChassisBuilder::with_aborter`].
    aborter: Arc<dyn FatalAborter>,
}

impl fmt::Debug for BootedChassis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootedChassis")
            .field("running", &self.running.len())
            .field("fallback_claimed", &self._fallback.is_some())
            .field("frame_bound_mailboxes", &self.frame_bound_pending.len())
            .finish()
    }
}

impl BootedChassis {
    /// Number of booted capabilities. Useful for tests and boot
    /// logs; not expected to vary at runtime.
    pub fn len(&self) -> usize {
        self.running.len()
    }

    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// Boot one more capability into an already-built chassis. The
    /// capability sees the same `ChassisCtx` shape as those booted
    /// through [`ChassisBuilder::with`] — the same registry, the
    /// same mail-send handle, and (crucially) the same fallback-
    /// router slot, so the single-claim invariant still holds across
    /// the build-time and post-build sets.
    ///
    /// Used by chassis mains to compose chassis-conditional
    /// capabilities (`LogCapability`, `IoCapability`, etc.) on top
    /// of the universal capabilities `SubstrateBoot::build` already
    /// installed. Boots run in call order; shutdown tears down in
    /// reverse, exactly like the build-time path.
    pub fn add<C>(
        &mut self,
        registry: &Arc<Registry>,
        mailer: &Arc<Mailer>,
        cap: C,
    ) -> Result<(), BootError>
    where
        C: Capability,
    {
        let mut ctx = ChassisCtx::new(
            registry,
            mailer,
            &mut self._fallback,
            &mut self.frame_bound_pending,
            &self.frame_bound_set,
            &self.aborter,
        );
        let booted = cap.boot(&mut ctx)?;
        self.running.push(Box::new(booted));
        Ok(())
    }

    /// Wait for every frame-bound capability's inbox to drain (the
    /// pending counter on each [`FrameBoundClaim`] hits zero) within
    /// `budget`. Returns `Ok(())` on clean drain, or
    /// `Err(WedgedFrameBound)` describing the first mailbox the
    /// barrier was still waiting on when the budget expired.
    ///
    /// Polls in a tight sleep loop because the dispatcher decrement
    /// and the chassis driver thread don't share a synchronization
    /// primitive — the pending counter is the only signal. 50 µs
    /// sleep keeps wakeups cheap on quiet frames without burning a
    /// core on busy ones.
    pub fn drain_frame_bound(&self, budget: Duration) -> Result<(), WedgedFrameBound> {
        if self.frame_bound_pending.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + budget;
        loop {
            let mut still_pending: Option<(MailboxId, u64)> = None;
            for (mbox, pending) in &self.frame_bound_pending {
                let v = pending.load(Ordering::Acquire);
                if v > 0 {
                    still_pending = Some((*mbox, v));
                    break;
                }
            }
            match still_pending {
                None => return Ok(()),
                Some((mbox, count)) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(WedgedFrameBound {
                            mailbox: mbox,
                            pending: count,
                            waited: budget,
                        });
                    }
                    // Brief sleep — long enough to avoid burning a
                    // core, short enough that a typical drain (sub-ms
                    // dispatcher hop) finishes in one wakeup.
                    std::thread::sleep(Duration::from_micros(50));
                }
            }
        }
    }

    /// Tear down every capability in reverse boot order by popping
    /// each box and dropping it — the cap's `Drop` impl runs the
    /// channel-drop + thread-join sequence the prior
    /// `RunningCapability::shutdown` did. Idempotent with the
    /// implicit `Drop` on `BootedChassis`.
    pub fn shutdown(mut self) {
        self.shutdown_in_place();
    }

    fn shutdown_in_place(&mut self) {
        while let Some(actor) = self.running.pop() {
            drop(actor);
        }
    }
}

impl Drop for BootedChassis {
    fn drop(&mut self) {
        // Forgotten-shutdown safety net: the chassis owner can drop
        // a `BootedChassis` without calling `shutdown` and still
        // get every cap's `Drop` impl run in reverse boot order.
        self.shutdown_in_place();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::ReplyTo;
    use crate::registry::MailboxEntry;
    use std::sync::Mutex;

    /// Test-only capability that claims one mailbox and records
    /// every envelope it receives plus whether shutdown ran.
    /// Post-issue-525-Phase-2 the cap is one struct: pre-boot fields
    /// (`name`, shared `log`/`shutdown_flag` handles) live alongside
    /// the runtime `Option<Receiver>` populated in `boot`. `Drop`
    /// runs the prior `shutdown` body.
    struct EchoCapability {
        name: &'static str,
        log: Arc<Mutex<Vec<Envelope>>>,
        shutdown_flag: Arc<Mutex<bool>>,
        receiver: Mutex<Option<mpsc::Receiver<Envelope>>>,
    }

    impl EchoCapability {
        fn new(
            name: &'static str,
            log: Arc<Mutex<Vec<Envelope>>>,
            shutdown_flag: Arc<Mutex<bool>>,
        ) -> Self {
            Self {
                name,
                log,
                shutdown_flag,
                receiver: Mutex::new(None),
            }
        }
    }

    impl Capability for EchoCapability {
        // Placeholder: parameterized fixtures bypass the type-level
        // namespace via `claim_mailbox_with_override` below, so no
        // production code observes this string.
        const NAMESPACE: &'static str = "test.echo.placeholder";
        fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError> {
            let claim = ctx.claim_mailbox_with_override(self.name)?;
            *self.receiver.lock().unwrap() = Some(claim.receiver);
            Ok(self)
        }
    }

    impl Drop for EchoCapability {
        fn drop(&mut self) {
            // Drain any pending envelopes synchronously so tests can
            // assert against `log` after the drop returns.
            if let Some(rx) = self.receiver.lock().unwrap().take() {
                while let Ok(env) = rx.try_recv() {
                    self.log.lock().unwrap().push(env);
                }
            }
            *self.shutdown_flag.lock().unwrap() = true;
        }
    }

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    fn deliver(registry: &Registry, name: &str, payload: &[u8]) {
        let id = registry.lookup(name).expect("mailbox registered");
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry exists") else {
            panic!("expected sink entry for {name}");
        };
        handler(KindId(42), "test.kind", None, ReplyTo::NONE, payload, 1);
    }

    #[test]
    fn capability_claims_mailbox_and_receives_mail() {
        let (registry, mailer) = fresh_substrate();
        let log = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(Mutex::new(false));

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(EchoCapability::new(
                "test.echo",
                Arc::clone(&log),
                Arc::clone(&flag),
            ))
            .build()
            .expect("build succeeds");
        assert_eq!(chassis.len(), 1);

        deliver(&registry, "test.echo", b"hello");
        deliver(&registry, "test.echo", b"world");

        chassis.shutdown();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].payload, b"hello");
        assert_eq!(log[1].payload, b"world");
        assert!(*flag.lock().unwrap());
    }

    #[test]
    fn duplicate_mailbox_claim_fails_with_loud_error() {
        let (registry, mailer) = fresh_substrate();
        // Pre-register the name to simulate the side-by-side period
        // where legacy `register_sink` and a new capability would
        // both target the same mailbox.
        registry.register_sink("test.collide", Arc::new(|_, _, _, _, _, _| {}));

        let log = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(Mutex::new(false));
        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(EchoCapability::new("test.collide", log, flag))
            .build()
            .expect_err("build must reject duplicate claim");
        assert!(
            matches!(err, BootError::MailboxAlreadyClaimed { ref name } if name == "test.collide")
        );
    }

    #[test]
    fn boot_failure_shuts_down_already_booted_capabilities() {
        let (registry, mailer) = fresh_substrate();
        // First capability claims a fresh name; the second is set up
        // to fail by pre-registering its target name.
        registry.register_sink("test.fail.second", Arc::new(|_, _, _, _, _, _| {}));

        let first_flag = Arc::new(Mutex::new(false));
        let second_flag = Arc::new(Mutex::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(EchoCapability::new(
                "test.fail.first",
                Arc::clone(&log),
                Arc::clone(&first_flag),
            ))
            .with(EchoCapability::new(
                "test.fail.second",
                Arc::clone(&log),
                Arc::clone(&second_flag),
            ))
            .build()
            .expect_err("second capability must fail");
        assert!(matches!(err, BootError::MailboxAlreadyClaimed { .. }));
        assert!(
            *first_flag.lock().unwrap(),
            "first capability dropped on boot abort"
        );
        // Post-issue-525-Phase-2: `second_flag` is set to `true` here
        // too — the second EchoCapability value drops on the failed
        // boot path, running its `Drop` impl. Pre-Phase-2 the flag
        // was a "ran shutdown" proxy on the post-boot Running, which
        // never existed for the second cap. The new semantic is "Drop
        // ran" (on every constructed value, regardless of boot
        // success), so we don't assert against `second_flag` here —
        // the boot-aborted-cleanly check is `first_flag` plus the
        // typed `BootError`.
        let _ = second_flag;
    }

    #[test]
    fn fallback_router_slot_is_single_claim() {
        let (registry, mailer) = fresh_substrate();

        struct FallbackCap {
            should_succeed: bool,
        }
        impl Capability for FallbackCap {
            const NAMESPACE: &'static str = "test.fallback.placeholder";
            fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError> {
                let handler: FallbackRouter = Arc::new(|_env: &Envelope| true);
                ctx.claim_fallback_router(handler)?;
                if self.should_succeed {
                    Ok(self)
                } else {
                    Err(BootError::Other("unreachable".into()))
                }
            }
        }

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(FallbackCap {
                should_succeed: true,
            })
            .with(FallbackCap {
                should_succeed: true,
            })
            .build()
            .expect_err("second fallback claim must fail");
        assert!(matches!(err, BootError::FallbackRouterAlreadyClaimed));
    }

    #[test]
    fn mail_send_handle_clones_to_same_mailer() {
        let (registry, mailer) = fresh_substrate();

        struct ProbeCap {
            captured: Arc<Mutex<Option<Arc<Mailer>>>>,
        }
        impl Capability for ProbeCap {
            const NAMESPACE: &'static str = "test.probe.placeholder";
            fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self, BootError> {
                *self.captured.lock().unwrap() = Some(ctx.mail_send_handle());
                Ok(self)
            }
        }

        let captured = Arc::new(Mutex::new(None));
        ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(ProbeCap {
                captured: Arc::clone(&captured),
            })
            .build()
            .expect("build succeeds")
            .shutdown();

        let captured = captured.lock().unwrap().take().expect("handle captured");
        assert!(Arc::ptr_eq(&captured, &mailer));
    }
}
