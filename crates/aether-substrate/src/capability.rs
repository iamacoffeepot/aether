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
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use aether_actor::Actor;
use aether_actor::Dispatch;

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

/// Marker trait for type-erased chassis-stored entries. Pre-PR-E3
/// the `BootedChassis` and `chassis_builder` passive lists held both
/// owned caps (legacy `Capability` path) and `FacadeHandle` wrappers
/// (facade caps). Post-E3 every cap is a facade; the only entries
/// in those lists are `FacadeHandle<C>` instances plus the
/// fallback-router marker. The trait survives so the boxed-dyn vec
/// keeps a stable type.
pub trait ActorErased: Send {}

/// Zero-sized stand-in for `ChassisBuilder::with_fallback_router`.
/// The fallback handler is owned by the chassis's `fallback` slot,
/// not by anything held in the type-erased entries vec; we still
/// push a marker so the boot order / shutdown drop semantics align
/// with the cap entries.
struct FallbackMarker;
impl ActorErased for FallbackMarker {}

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
    pub fn claim_mailbox<C: Actor>(&mut self) -> Result<MailboxClaim, BootError> {
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
    pub fn claim_mailbox_drop_on_shutdown<C: Actor>(
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

/// Chassis-side handle for a facade-cap dispatcher (ADR-0075).
///
/// The chassis owns this; the cap (with its [`Dispatch`] impl) lives
/// inside the dispatcher thread. Dropping the handle drops the
/// [`SinkSender`] (channel disconnects), the dispatcher's `recv()`
/// returns `Err(Disconnected)`, the thread exits, and the captured
/// cap drops with it — running its `Drop` impl on the dispatcher
/// thread (where any backend resource cleanup happens).
///
/// `PhantomData<C>` ties the handle to the cap type for storage
/// hygiene without holding the cap (the dispatcher thread does).
/// Boxed as `dyn ActorErased` in the chassis's `running` vec.
pub struct FacadeHandle<C: 'static> {
    thread: Option<JoinHandle<()>>,
    /// Drop-on-shutdown breaks the channel. `Option` so [`Drop`] can
    /// `take()` it before joining the thread; once gone, the
    /// registry's `Weak` upgrade fails on subsequent sends and the
    /// dispatcher's `recv()` returns `Err(Disconnected)`.
    sink_sender: Option<SinkSender>,
    _cap: PhantomData<fn() -> C>,
}

impl<C: 'static> Drop for FacadeHandle<C> {
    fn drop(&mut self) {
        // Sender first — that's what disconnects the channel and lets
        // the dispatcher thread exit. Joining a still-alive sender
        // would hang.
        self.sink_sender.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// `FacadeHandle<C>` is itself the chassis-stored entry — the cap C
// lives inside the dispatcher thread. `Send` is the only ActorErased
// requirement; the handle's fields (`Option<JoinHandle>`,
// `Option<SinkSender>`, PhantomData) are all `Send`.
impl<C: 'static> ActorErased for FacadeHandle<C> {}

impl<'a> ChassisCtx<'a> {
    /// Claim a mailbox for a facade-style chassis cap and spawn a
    /// dispatcher thread that owns the cap and routes inbound mail
    /// through its [`Dispatch`] impl. ADR-0075 §Decision 3.
    ///
    /// The cap is moved into the thread; the returned [`FacadeHandle`]
    /// owns the [`SinkSender`] + [`JoinHandle`]. On chassis shutdown
    /// the handle drops, the channel disconnects, the thread exits,
    /// and the cap's `Drop` runs on the dispatcher thread (so any
    /// backend cleanup that touches non-Send resources stays on the
    /// owning thread).
    ///
    /// Today this is the path PR C wires for [`aether_kinds::LogCapability`];
    /// PR D migrates the rest of the chassis caps onto it.
    pub fn spawn_actor_dispatcher<C>(&mut self, cap: C) -> Result<FacadeHandle<C>, BootError>
    where
        C: Actor + Dispatch + Send + 'static,
    {
        // FRAME_BARRIER caps (today: render) need the pending counter
        // the chassis frame loop drains against — claim through the
        // frame-bound path so the `pending` counter registers in the
        // chassis's `frame_bound_pending` Vec. Free-running caps go
        // through the regular drop-on-shutdown claim. The dispatch
        // loop is identical apart from the post-dispatch decrement.
        let (receiver, sink_sender, pending) = if C::FRAME_BARRIER {
            let claim = self.claim_frame_bound_mailbox_with_override(C::NAMESPACE)?;
            let FrameBoundClaim {
                id: _,
                receiver,
                sink_sender,
                pending,
            } = claim;
            (receiver, sink_sender, Some(pending))
        } else {
            let claim = self.claim_mailbox_drop_on_shutdown_with_override(C::NAMESPACE)?;
            let DropOnShutdownClaim {
                id: _,
                receiver,
                sink_sender,
            } = claim;
            (receiver, sink_sender, None)
        };

        let mut owned = cap;
        let thread = thread::Builder::new()
            .name(alloc_thread_name::<C>())
            .spawn(move || {
                // Channel-drop + join: pull until the sender side
                // disconnects. Worst-case shutdown latency is the OS
                // scheduler's wakeup, not a polling interval. The
                // strict-receiver miss path (None from __dispatch)
                // logs at the chassis-side dispatcher rather than the
                // SDK so the warning carries the cap's namespace.
                while let Ok(env) = receiver.recv() {
                    if owned
                        .__dispatch(env.sender, env.kind.0, &env.payload)
                        .is_none()
                    {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            actor = C::NAMESPACE,
                            kind = env.kind_name.as_str(),
                            "facade cap dispatch missed: kind not handled or decode failed"
                        );
                    }
                    // Decrement matches the sink-handler's increment —
                    // the chassis frame-bound drain barrier
                    // (`drain_frame_bound_or_abort`) reads this counter
                    // to know when the dispatcher is caught up.
                    if let Some(p) = &pending {
                        p.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                // `owned` drops here on thread exit, running the cap's
                // `Drop` impl (which in turn drops the backend, which
                // typically owns the resource cleanup).
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(FacadeHandle {
            thread: Some(thread),
            sink_sender: Some(sink_sender),
            _cap: PhantomData,
        })
    }
}

/// Build a stable `aether-actor-<namespace>` thread name for the
/// dispatcher. Namespaces are `&'static str`, so allocating once at
/// boot is fine — most chassis have ≤ 7 caps total.
fn alloc_thread_name<C: Actor>() -> String {
    let mut name = String::with_capacity("aether-actor-".len() + C::NAMESPACE.len());
    name.push_str("aether-actor-");
    name.push_str(C::NAMESPACE);
    name
}

/// Type-erased boot trampoline so [`ChassisBuilder`] can collect
/// heterogeneous capability types into one `Vec`. Each entry, when
/// invoked, takes the ctx, runs `spawn_actor_dispatcher` for the
/// capability, and boxes the returned [`FacadeHandle`] as
/// [`ActorErased`] so the chassis can store every cap in one
/// homogeneous `Vec`. Teardown runs through the handle's `Drop`
/// when the box drops.
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

    /// Append a chassis cap. The chassis claims the cap's mailbox
    /// and runs the dispatcher; the cap is an `Actor + Dispatch`
    /// value (typically built by `#[actor]` on an inherent impl).
    /// Boot order is declaration order. Pre-PR-E3 this method was
    /// named `with_facade`; the legacy `with`-takes-Capability
    /// variant retired alongside `Capability` itself.
    pub fn with<C>(mut self, cap: C) -> Self
    where
        C: Actor + Dispatch + Send + 'static,
    {
        self.pending.push(Box::new(move |ctx| {
            let handle = ctx.spawn_actor_dispatcher(cap)?;
            Ok(Box::new(handle) as Box<dyn ActorErased>)
        }));
        self
    }

    /// Register a fallback router — a single-shot handler the
    /// substrate consults for envelopes whose mailbox name doesn't
    /// resolve. Useful for tests that want to observe routing of
    /// unhandled mail; production chassis don't currently install
    /// one. Multiple calls collapse to a `BootError` at `build()`.
    pub fn with_fallback_router(mut self, handler: FallbackRouter) -> Self {
        self.pending.push(Box::new(move |ctx| {
            ctx.claim_fallback_router(handler)?;
            Ok(Box::new(FallbackMarker) as Box<dyn ActorErased>)
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
    /// cap sees the same `ChassisCtx` shape as those booted through
    /// [`ChassisBuilder::with`] — the same registry, the same
    /// mail-send handle, and (crucially) the same fallback-router
    /// slot, so the single-claim invariant still holds across the
    /// build-time and post-build sets.
    ///
    /// Used by chassis mains to compose chassis-conditional
    /// capabilities on top of the universal capabilities
    /// `SubstrateBoot::build` already installed (today: the bundle
    /// hooks `IoCapability` here in TestBench because adapter init
    /// can fail silently on systems without writable default roots).
    /// Boots run in call order; shutdown tears down in reverse,
    /// exactly like the build-time path.
    ///
    /// Pre-PR-E3 there was a separate `add_facade` for actor caps
    /// alongside `add` for legacy `Capability` caps; the legacy
    /// path retired alongside `Capability` itself.
    pub fn add<C>(
        &mut self,
        registry: &Arc<Registry>,
        mailer: &Arc<Mailer>,
        cap: C,
    ) -> Result<(), BootError>
    where
        C: Actor + Dispatch + Send + 'static,
    {
        let mut ctx = ChassisCtx::new(
            registry,
            mailer,
            &mut self._fallback,
            &mut self.frame_bound_pending,
            &self.frame_bound_set,
            &self.aborter,
        );
        let handle = ctx.spawn_actor_dispatcher(cap)?;
        self.running.push(Box::new(handle));
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
    use aether_data::ReplyTo;
    use aether_kinds::LogEvent;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    /// Boot-time mailbox-claim collision aborts the build and runs
    /// every already-booted cap's `Drop`. Two `LogCapability`
    /// instances both claim `aether.log`; the second hits the
    /// duplicate-claim guard and the chassis tears the first down.
    /// Per-cap mailbox-claim and mail-receive coverage lives in
    /// `crate::capabilities::log::tests` and the equivalents for
    /// the other facade caps.
    #[test]
    fn boot_failure_shuts_down_already_booted_capabilities() {
        use crate::capabilities::LogCapability;

        let (registry, mailer) = fresh_substrate();
        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .with(LogCapability::new())
            .build()
            .expect_err("second LogCapability must fail with duplicate claim");
        assert!(
            matches!(err, BootError::MailboxAlreadyClaimed { ref name } if name == LogCapability::NAMESPACE)
        );
    }

    /// Two `with_fallback_router` calls on one builder collapse to a
    /// `FallbackRouterAlreadyClaimed` at `build()` — single-slot
    /// invariant. Pre-PR-E3 a Capability::boot body called
    /// `ctx.claim_fallback_router`; post-E3 the builder method
    /// surfaces the same single-claim guard.
    #[test]
    fn fallback_router_slot_is_single_claim() {
        let (registry, mailer) = fresh_substrate();
        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_fallback_router(Arc::new(|_env: &Envelope| true))
            .with_fallback_router(Arc::new(|_env: &Envelope| true))
            .build()
            .expect_err("second fallback claim must fail");
        assert!(matches!(err, BootError::FallbackRouterAlreadyClaimed));
    }

    /// Smoke test: one cap, one envelope, clean shutdown. Validates
    /// the chassis builder boots through the facade path end-to-end.
    /// Per-cap routing semantics live in their own test modules; this
    /// test just exercises the chassis-level invariants
    /// (`with(cap).build()` succeeds, `shutdown()` joins).
    #[test]
    fn chassis_builder_boots_one_cap_and_shuts_down_clean() {
        use crate::capabilities::LogCapability;
        use aether_data::Kind;

        let (registry, mailer) = fresh_substrate();
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(LogCapability::new())
            .build()
            .expect("build succeeds");
        assert_eq!(chassis.len(), 1);

        let id = registry
            .lookup(LogCapability::NAMESPACE)
            .expect("log mailbox registered");
        let crate::registry::MailboxEntry::Sink(handler) =
            registry.entry(id).expect("entry exists")
        else {
            panic!("expected sink entry");
        };
        let event = LogEvent {
            level: 3,
            target: "aether_test".into(),
            message: "boot smoke".into(),
        };
        let bytes = postcard::to_allocvec(&event).expect("encode");
        handler(
            <LogEvent as Kind>::ID,
            "aether.log",
            None,
            ReplyTo::NONE,
            &bytes,
            1,
        );

        chassis.shutdown();
    }
}
