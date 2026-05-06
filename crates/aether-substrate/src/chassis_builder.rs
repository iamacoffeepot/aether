//! ADR-0071 Phase 2A: driver-capability traits + chassis builder
//! type-state.
//!
//! Sibling to ADR-0070's [`Capability`] family (post-issue-525-Phase-2:
//! one struct per cap, `Drop` replaces `RunningCapability::shutdown`).
//! A chassis composes passive capabilities (dispatcher-thread sinks
//! per ADR-0070) plus exactly one [`DriverCapability`] that owns the
//! chassis main thread. The type-state [`Builder`] enforces "exactly
//! one driver" structurally; embedders that drive manually (TestBench,
//! future embedded harnesses) build a [`PassiveChassis`] via the
//! no-driver path.
//!
//! # Phase 2A scope
//!
//! - Trait family + builder + ctx wiring.
//! - The existing [`crate::capability::ChassisBuilder`] is unchanged
//!   and remains the construction site for current chassis. Phases
//!   3-7 (per ADR-0071) migrate each chassis to the new builder.
//! - [`Chassis::Driver`] / [`Chassis::Env`] / [`Chassis::build`] are
//!   not yet on the [`crate::chassis::Chassis`] trait — they land
//!   alongside the first real driver extraction (phase 3) so every
//!   chassis can nominate a real driver type rather than a stub.

use std::collections::HashSet;
use std::error::Error as StdError;
use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};

use crate::capability::{BootError, ChassisCtx, FacadeHandle, FallbackRouter, MailboxClaim};
use crate::chassis::Chassis;
use crate::lifecycle::{FatalAborter, PanicAborter};
use crate::mail::MailboxId;
use crate::mailer::Mailer;
use crate::native_actor::{Actors, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx};
use crate::native_transport::NativeTransport;
use crate::registry::Registry;
use aether_actor::Actor;
use aether_actor::Dispatch;

/// Failure mode raised by [`DriverRunning::run`].
#[derive(Debug)]
pub enum RunError {
    Other(Box<dyn StdError + Send + Sync + 'static>),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Other(e) => write!(f, "driver run failed: {e}"),
        }
    }
}

impl StdError for RunError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            RunError::Other(e) => Some(&**e),
        }
    }
}

/// A driver capability owns the chassis main thread. Each chassis
/// composes exactly one driver alongside its passive capabilities.
/// The driver's [`DriverRunning::run`] body holds whatever loop the
/// chassis needs — winit on desktop, std-timer on headless, TCP
/// accept on hub.
///
/// Not `Send`: the desktop driver's `winit::EventLoop` is `!Send` on
/// macOS, so the driver and its running stay on the chassis main
/// thread end-to-end. The `Builder` holds the driver capability and
/// the resulting `Running` on a single-threaded code path between
/// [`Builder::driver`] and [`BuiltChassis::run`], so neither needs
/// to cross threads.
pub trait DriverCapability: 'static {
    type Running: DriverRunning;
    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError>;
}

/// Post-boot driver handle. Built once at chassis boot, then handed
/// to [`BuiltChassis::run`], which calls [`DriverRunning::run`] on
/// the calling thread. Returns when the underlying loop drains
/// cleanly (window closed, accept loop done, shutdown signal).
pub trait DriverRunning: 'static {
    fn run(self: Box<Self>) -> Result<(), RunError>;
}

/// Phantom [`DriverCapability`] for passive chassis (test-bench, future
/// embedder-driven chassis kinds). The [`Chassis`](crate::chassis::Chassis)
/// trait requires `type Driver: DriverCapability`; passive chassis
/// declare this as their driver to satisfy the bound, but the value is
/// never instantiated (the `Builder<C, NoDriver>` path produces a
/// [`PassiveChassis<C>`] without ever resolving `C::Driver`). Its `boot`
/// is `unreachable!()` — reaching it implies someone tried to drive a
/// chassis that has no driver, which is a programmer error rather than
/// a runtime condition.
pub struct NeverDriver;

impl DriverCapability for NeverDriver {
    type Running = NeverDriverRunning;
    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        unreachable!(
            "NeverDriver is a phantom for passive chassis; it should never be booted. \
             Build the chassis via its inherent `build_passive(env)` instead."
        );
    }
}

/// Running-side of [`NeverDriver`]; same unreachability contract.
pub struct NeverDriverRunning;

impl DriverRunning for NeverDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        unreachable!("NeverDriverRunning::run is never called by design");
    }
}

/// Type-erased shutdown adapter for a passive [`Capability`] stored
/// in the builder's shutdown queue. Single-threaded path, so no
/// `Send` bound — the adapter never crosses threads (the chassis
/// runs on the main thread end-to-end).
trait DynShutdown {
    fn shutdown_dyn(self: Box<Self>);
}

/// Concrete adapter for a chassis cap. The chassis owns the
/// [`FacadeHandle`]; the cap itself lives in the dispatcher thread.
/// On shutdown the handle drops, severing the channel and joining
/// the thread; the captured cap drops there. Drivers that need to
/// talk to the cap reach for it through mail or — for caps that
/// expose driver-facing state, like render — through the cap's own
/// pre-build accessor (e.g. `RenderCapability::handles`).
struct FacadeShutdown<C: 'static> {
    handle: Option<FacadeHandle<C>>,
}

impl<C: 'static> DynShutdown for FacadeShutdown<C> {
    fn shutdown_dyn(mut self: Box<Self>) {
        // Drop the handle eagerly: drops SinkSender, channel
        // disconnects, dispatcher thread exits, cap drops. Equivalent
        // to letting `Box<Self>` drop, but explicit so the order is
        // documented.
        self.handle.take();
    }
}

/// Concrete adapter for the fallback-router slot. The handler itself
/// is owned by the chassis's `fallback` slot (claimed via
/// `ctx.claim_fallback_router`); this entry exists purely to keep
/// the boot-order / shutdown-order invariants aligned with cap
/// entries when `with_fallback_router` is mixed into a builder.
struct FallbackShutdown;

impl DynShutdown for FallbackShutdown {
    fn shutdown_dyn(self: Box<Self>) {
        // The fallback router doesn't own any threads or channels —
        // it's a single function pointer. Nothing to do here; the
        // chassis's `fallback` slot drops the `Arc` when the
        // `BootedPassives` drops.
    }
}

/// Boot-time context handed to a [`DriverCapability`]. Forwards the
/// passive [`ChassisCtx`] surface; pre-PR-E3 it also exposed typed
/// access to passive runnings via `expect` / `try_get`, but the
/// typed-runnings map retired alongside `Capability` so drivers
/// wanting cap state get it through pre-build accessors (today only
/// render via `RenderHandles`).
///
/// Issue 552 stage 1: also borrows the chassis's [`Actors`] map so
/// drivers can pull `Arc<A>` for caps booted via [`Builder::with_actor`].
/// `actor::<A>()` returns `None` if `A` wasn't booted.
pub struct DriverCtx<'a> {
    inner: ChassisCtx<'a>,
    actors: &'a Actors,
}

impl<'a> DriverCtx<'a> {
    fn new(inner: ChassisCtx<'a>, actors: &'a Actors) -> Self {
        Self { inner, actors }
    }

    /// Drivers have no `NAMESPACE` const to delegate against — claim
    /// by explicit name.
    pub fn claim_mailbox(&mut self, name: &str) -> Result<MailboxClaim, BootError> {
        self.inner.claim_mailbox_with_override(name)
    }

    pub fn mail_send_handle(&self) -> Arc<Mailer> {
        self.inner.mail_send_handle()
    }

    pub fn claim_fallback_router(&mut self, handler: FallbackRouter) -> Result<(), BootError> {
        self.inner.claim_fallback_router(handler)
    }

    /// Snapshot of every frame-bound mailbox's pending counter
    /// collected during passive boot. Drivers stash this clone and
    /// hand it to [`crate::frame_loop::drain_frame_bound_or_abort`]
    /// each frame so render submit waits for inbound mail to drain
    /// alongside component drains (ADR-0074 §Decision 5).
    ///
    /// Returns an empty vec on chassis with no frame-bound
    /// capabilities (today: the headless chassis without render); in
    /// that case the per-frame call is a fast no-op.
    pub fn frame_bound_pending(&self) -> Vec<(MailboxId, Arc<AtomicU64>)> {
        self.inner.frame_bound_pending().to_vec()
    }

    /// Issue 552 stage 1: look up an earlier-booted [`NativeActor`]
    /// by type and clone its `Arc`. `None` if the cap wasn't booted
    /// through [`Builder::with_actor`] (legacy `with(cap)` boots
    /// don't populate the actors map). Drivers reach for this when
    /// they need to clone a cap-owned handle (today: render's
    /// `RenderHandles` once `RenderCapability` migrates to
    /// `NativeActor` in stage 2).
    pub fn actor<A: NativeActor>(&self) -> Option<Arc<A>> {
        self.actors.get::<A>()
    }
}

mod sealed {
    pub trait Sealed {}
}

/// Type-state marker tracking whether a driver has been supplied.
/// Sealed: only [`NoDriver`] and [`HasDriver`] are valid.
pub trait BuilderState: sealed::Sealed {}

/// Builder state: no driver supplied yet. Accepts both `.with(_)`
/// and `.driver(_)` (which transitions to [`HasDriver`]); also
/// supports `.build_passive()` for the embedder-driven path.
pub struct NoDriver;

/// Builder state: driver supplied. Accepts `.with(_)` (passives
/// declared after the driver still boot before the driver per the
/// builder's invariant) and `.build()`.
pub struct HasDriver;

impl sealed::Sealed for NoDriver {}
impl sealed::Sealed for HasDriver {}
impl BuilderState for NoDriver {}
impl BuilderState for HasDriver {}

/// Issue 552 stage 1: every `PassiveBoot` closure now receives both a
/// [`ChassisCtx`] (registry / mailer / fallback / frame-bound state)
/// and a `&mut Actors` view so the new [`Builder::with_actor::<A>`]
/// path can insert booted `Arc<A>` instances. Closures from
/// [`make_passive_boot`] / [`make_fallback_router_boot`] ignore the
/// second arg; the new [`make_native_actor_boot`] uses it.
type PassiveBoot =
    Box<dyn FnOnce(&mut ChassisCtx<'_>, &mut Actors) -> Result<Box<dyn DynShutdown>, BootError>>;
type DriverBoot = Box<dyn FnOnce(&mut DriverCtx<'_>) -> Result<Box<dyn DriverRunning>, BootError>>;

fn make_passive_boot<C>(cap: C) -> PassiveBoot
where
    C: Actor + Dispatch + Send + 'static,
{
    Box::new(move |ctx, _actors| {
        let handle = ctx.spawn_actor_dispatcher(cap)?;
        Ok(Box::new(FacadeShutdown {
            handle: Some(handle),
        }) as Box<dyn DynShutdown>)
    })
}

fn make_fallback_router_boot(handler: FallbackRouter) -> PassiveBoot {
    Box::new(move |ctx, _actors| {
        ctx.claim_fallback_router(handler)?;
        Ok(Box::new(FallbackShutdown) as Box<dyn DynShutdown>)
    })
}

/// Issue 552 stage 1: factory for the new [`NativeActor`] boot path.
/// Claims the cap's mailbox under `A::NAMESPACE`, builds a fresh
/// per-cap [`NativeTransport`], constructs a [`NativeInitCtx`], calls
/// `A::init(config, &mut init_ctx)`, wraps the returned cap in an
/// `Arc<A>`, inserts into the chassis-side [`Actors`] map, and spawns
/// a dispatcher thread that pulls from the transport's inbox and
/// routes through [`NativeDispatch::__aether_dispatch_envelope`] —
/// the sum dispatch trait the `#[actor] impl NativeActor for A`
/// macro emits.
///
/// Stage 2d: FRAME_BARRIER caps now go through this path too. The
/// frame-bound claim (`claim_frame_bound_mailbox_with_override`)
/// registers the per-mailbox `pending` counter into the chassis's
/// `frame_bound_pending` Vec; the dispatcher thread decrements after
/// each successful handler dispatch so the chassis frame loop's
/// `drain_frame_bound_or_abort` (ADR-0074 §Decision 5) sees the
/// counter drop to zero alongside component drains.
fn make_native_actor_boot<A>(config: A::Config) -> PassiveBoot
where
    A: NativeActor + NativeDispatch,
{
    Box::new(move |ctx, actors| {
        // Frame-bound caps (today: render) claim through the
        // frame-bound path so the `pending` counter feeds the chassis
        // frame loop. Free-running caps take the regular drop-on-
        // shutdown claim. Both share the same dispatcher trampoline
        // shape apart from the post-dispatch decrement.
        let (mailbox_id, receiver, sink_sender, pending) = if A::FRAME_BARRIER {
            let claim = ctx.claim_frame_bound_mailbox::<A>()?;
            (
                claim.id,
                claim.receiver,
                claim.sink_sender,
                Some(claim.pending),
            )
        } else {
            let claim = ctx.claim_mailbox_drop_on_shutdown::<A>()?;
            (claim.id, claim.receiver, claim.sink_sender, None)
        };

        // Per-cap transport. `NativeTransport::from_ctx` pulls the
        // chassis's frame-bound set + aborter so the cross-class
        // wait_reply guard wires automatically.
        let transport = Arc::new(NativeTransport::from_ctx(ctx, mailbox_id, A::FRAME_BARRIER));
        transport.install_inbox(receiver);

        // Per-actor scratch storage (issue 582 / ADR-0074). Stamped
        // into TLS via `local::with_stamped` for the duration
        // of `init` and each handler dispatch so library code inside
        // the actor (e.g., the issue-581 log buffer) can reach
        // `Local::with_mut` without threading a ctx through.
        let slots = Box::new(aether_actor::local::ActorSlots::new());

        // Boot the cap through `init`. The NativeInitCtx borrows the
        // actors-so-far map (read-only) plus the transport (read-only)
        // plus a mailer clone for outbound hooks. Issue #581: also
        // stamp the actor's transport as the in-actor `MailDispatch`
        // around `init` so any `tracing::*` event the cap fires
        // during boot drains to LogCapability when init returns.
        let actor = {
            let mailer_clone = ctx.mail_send_handle();
            let mut init_ctx = NativeInitCtx::new(&transport, actors, mailer_clone);
            aether_actor::local::with_stamped(&slots, || {
                aether_actor::log::with_actor_dispatch(
                    &*transport as &dyn aether_actor::log::MailDispatch,
                    || {
                        let r = A::init(config, &mut init_ctx);
                        aether_actor::log::drain_buffer();
                        r
                    },
                )
            })?
        };
        let actor_arc: Arc<A> = Arc::new(actor);

        // Insert into the chassis lookup map. Earlier-booted caps
        // already inserted; later-booted caps will. Double-insert
        // can't happen because mailbox name is the dedup key (the
        // claim above would have failed first).
        actors.insert::<A>(Arc::clone(&actor_arc));

        // Spawn the dispatcher thread. The thread owns one Arc<A>,
        // one Arc<NativeTransport>, and the per-actor `ActorSlots`
        // (moved in by value); it loops `recv_blocking()` on the
        // transport (which pulls from the installed inbox and
        // disconnects when the chassis drops its `sink_sender`) and
        // routes each envelope through `__aether_dispatch_envelope`,
        // wrapped in `local::with_stamped` so handler bodies
        // see the per-actor storage.
        let actor_for_thread = Arc::clone(&actor_arc);
        let transport_for_thread = Arc::clone(&transport);
        let thread_name = alloc_native_actor_thread_name::<A>();
        let thread = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                while let Some(env) = transport_for_thread.recv_blocking() {
                    aether_actor::local::with_stamped(&slots, || {
                        // Issue #581: stamp the actor's transport as
                        // the in-actor `MailDispatch` so the actor-
                        // aware tracing layer's priority flush + the
                        // post-handler drain hook ship `LogBatch`
                        // mail with sender attribution.
                        aether_actor::log::with_actor_dispatch(
                            &*transport_for_thread as &dyn aether_actor::log::MailDispatch,
                            || {
                                let mut ctx =
                                    NativeCtx::new(&transport_for_thread, env.sender);
                                if actor_for_thread
                                    .__aether_dispatch_envelope(&mut ctx, env.kind, &env.payload)
                                    .is_none()
                                    && !actor_for_thread
                                        .__aether_dispatch_fallback(&mut ctx, &env)
                                {
                                    // Issue 576: catch-all caps override
                                    // `__aether_dispatch_fallback` and return
                                    // `true` after their fallback runs,
                                    // suppressing this warn. Strict receivers
                                    // keep the default (returns `false`) and
                                    // surface the miss.
                                    tracing::warn!(
                                        target: "aether_substrate::chassis_builder",
                                        actor = A::NAMESPACE,
                                        kind = env.kind_name.as_str(),
                                        "native actor dispatch missed: kind not handled or decode failed"
                                    );
                                }
                                aether_actor::log::drain_buffer();
                            },
                        );
                    });
                    // Decrement matches the sink-handler's increment —
                    // the chassis frame-bound drain barrier
                    // (`drain_frame_bound_or_abort`) reads this counter
                    // to know when the dispatcher is caught up.
                    if let Some(p) = &pending {
                        p.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    }
                }
            })
            .map_err(|e| BootError::Other(Box::new(e)))?;

        Ok(Box::new(NativeActorShutdown {
            thread: Some(thread),
            sink_sender: Some(sink_sender),
        }) as Box<dyn DynShutdown>)
    })
}

/// Build a stable `aether-actor-<namespace>` thread name for the
/// dispatcher. Mirrors the legacy capability path's helper but lives
/// in this module so `make_native_actor_boot` doesn't depend on a
/// `pub(crate)` shim from `capability.rs`.
fn alloc_native_actor_thread_name<A: Actor>() -> String {
    let mut name = String::with_capacity("aether-actor-".len() + A::NAMESPACE.len());
    name.push_str("aether-actor-");
    name.push_str(A::NAMESPACE);
    name
}

/// Shutdown adapter for a [`NativeActor`] booted through
/// [`Builder::with_actor`]. Drops the [`crate::capability::SinkSender`]
/// to disconnect the channel (the dispatcher's `recv_blocking` returns
/// `None` and the thread exits), then joins the thread. Mirrors
/// [`FacadeShutdown`] for the legacy facade path.
struct NativeActorShutdown {
    thread: Option<std::thread::JoinHandle<()>>,
    sink_sender: Option<crate::capability::SinkSender>,
}

impl DynShutdown for NativeActorShutdown {
    fn shutdown_dyn(mut self: Box<Self>) {
        // Sender first — disconnects the channel and lets the
        // dispatcher's `recv_blocking` return None so the thread
        // exits. Joining a still-attached sender would hang.
        self.sink_sender.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn make_driver_boot<D: DriverCapability>(driver: D) -> DriverBoot {
    Box::new(move |ctx| {
        let running = driver.boot(ctx)?;
        Ok(Box::new(running) as Box<dyn DriverRunning>)
    })
}

/// Declarative chassis builder, parametric over the chassis kind `C`
/// and a type-state `S` tracking whether a driver has been supplied.
/// `Builder<C, NoDriver>` accepts both [`Self::with`] and either
/// [`Self::driver`] or [`Self::build_passive`]; once `.driver(d)`
/// runs the builder transitions to `Builder<C, HasDriver>` which
/// only accepts further [`Self::with`] calls and [`Self::build`].
pub struct Builder<C: Chassis, S: BuilderState = NoDriver> {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    passives: Vec<PassiveBoot>,
    driver: Option<DriverBoot>,
    aborter: Arc<dyn FatalAborter>,
    _chassis: PhantomData<fn() -> C>,
    _state: PhantomData<fn() -> S>,
}

impl<C: Chassis> Builder<C, NoDriver> {
    /// Construct a fresh builder against the given substrate handles.
    /// Defaults the cross-class `wait_reply` aborter to
    /// [`PanicAborter`]; production drivers swap in
    /// [`crate::lifecycle::OutboundFatalAborter`] via
    /// [`Self::with_aborter`] before `build()` / `build_passive()`.
    pub fn new(registry: Arc<Registry>, mailer: Arc<Mailer>) -> Self {
        Self {
            registry,
            mailer,
            passives: Vec::new(),
            driver: None,
            aborter: Arc::new(PanicAborter),
            _chassis: PhantomData,
            _state: PhantomData,
        }
    }

    /// Override the default [`PanicAborter`] with a chassis-supplied
    /// [`FatalAborter`]. Mirrors
    /// [`crate::ChassisBuilder::with_aborter`]; production drivers
    /// (desktop, headless) call this before `build()` so a cross-class
    /// `wait_reply` violation broadcasts `SubstrateDying` before
    /// process exit. Single-call: a second invocation overwrites the
    /// prior aborter.
    pub fn with_aborter(mut self, aborter: Arc<dyn FatalAborter>) -> Self {
        self.aborter = aborter;
        self
    }

    /// Append a chassis cap. The chassis claims its mailbox and runs
    /// the dispatcher; the cap is an `Actor + Dispatch` value
    /// (typically built by `#[actor]` on an inherent impl). Boot
    /// order is declaration order; `.with` calls before and after
    /// `.driver(_)` boot together before the driver.
    ///
    /// Pre-PR-E3 this method was named `with_facade`; the legacy
    /// `with`-takes-Capability variant retired alongside `Capability`
    /// itself.
    pub fn with<P>(mut self, cap: P) -> Self
    where
        P: Actor + Dispatch + Send + 'static,
    {
        self.passives.push(make_passive_boot::<P>(cap));
        self
    }

    /// Register a fallback router — a single-shot handler the
    /// substrate consults for envelopes whose mailbox name doesn't
    /// resolve. Multiple calls collapse to a `BootError` at
    /// `build()` (single-claim invariant).
    pub fn with_fallback_router(mut self, handler: FallbackRouter) -> Self {
        self.passives.push(make_fallback_router_boot(handler));
        self
    }

    /// Issue 552 stage 1: boot a [`NativeActor`] with its associated
    /// `Config`. The chassis claims the cap's mailbox under
    /// `A::NAMESPACE`, runs `A::init(config, ctx)`, stores `Arc<A>`
    /// in the chassis-side [`Actors`] map, and spawns a dispatcher
    /// thread that drives the cap via [`NativeDispatch`].
    ///
    /// Boot order is declaration order; `.with_actor` calls before
    /// and after `.driver(_)` boot together before the driver runs.
    /// Init-time peer lookups via `ctx.actor::<EarlierCap>()` see
    /// every cap inserted earlier in the chain.
    ///
    /// FRAME_BARRIER caps aren't supported on this entry yet — see
    /// [`make_native_actor_boot`] for the fast-fail rationale. The
    /// legacy `with(cap)` path stays available for them.
    pub fn with_actor<A>(mut self, config: A::Config) -> Self
    where
        A: NativeActor + NativeDispatch,
    {
        self.passives.push(make_native_actor_boot::<A>(config));
        self
    }

    /// Supply the chassis's driver. Transitions to [`HasDriver`] —
    /// further `.driver(_)` calls are forbidden by the type system.
    /// Per ADR-0071 the driver type is fixed by `C::Driver`, so the
    /// builder rejects mismatched driver types at the call site
    /// rather than at boot.
    pub fn driver(mut self, driver: C::Driver) -> Builder<C, HasDriver> {
        self.driver = Some(make_driver_boot::<C::Driver>(driver));
        Builder {
            registry: self.registry,
            mailer: self.mailer,
            passives: self.passives,
            driver: self.driver,
            aborter: self.aborter,
            _chassis: PhantomData,
            _state: PhantomData,
        }
    }

    /// No-driver build path. Boots every passive in declaration order
    /// and returns a [`PassiveChassis`] whose embedder is responsible
    /// for driving the loop manually (TestBench).
    pub fn build_passive(self) -> Result<PassiveChassis<C>, BootError> {
        let booted = boot_passives(&self.registry, &self.mailer, &self.aborter, self.passives)?;
        // Issue #581: finalise actor-aware logging by wiring the
        // host-branch dispatch + log mailbox id once `LogCapability`
        // has claimed `"aether.log"`. No-op if the chassis skipped
        // the cap.
        crate::log_install::install_log_target_if_registered(
            Arc::clone(&self.mailer),
            &self.registry,
        );
        Ok(PassiveChassis {
            booted,
            _chassis: PhantomData,
        })
    }
}

impl<C: Chassis> Builder<C, HasDriver> {
    /// Append a chassis cap after the driver was supplied. Booted
    /// before the driver in declaration order.
    pub fn with<P>(mut self, cap: P) -> Self
    where
        P: Actor + Dispatch + Send + 'static,
    {
        self.passives.push(make_passive_boot::<P>(cap));
        self
    }

    /// Register a fallback router after the driver was supplied.
    /// Booted before the driver in declaration order.
    pub fn with_fallback_router(mut self, handler: FallbackRouter) -> Self {
        self.passives.push(make_fallback_router_boot(handler));
        self
    }

    /// Mirror of [`Builder::with_actor`][Builder<C, NoDriver>::with_actor]
    /// for the post-driver state — same semantics, accepted because
    /// declaration-order before/after `.driver(_)` doesn't change
    /// boot order (passives boot before the driver regardless).
    pub fn with_actor<A>(mut self, config: A::Config) -> Self
    where
        A: NativeActor + NativeDispatch,
    {
        self.passives.push(make_native_actor_boot::<A>(config));
        self
    }

    /// Boot every passive in declaration order, then boot the driver
    /// against a [`DriverCtx`]. Any failure aborts the build and
    /// shuts down the passives that already booted (via
    /// [`BootedPassives::Drop`]) before propagating the error.
    pub fn build(self) -> Result<BuiltChassis<C>, BootError> {
        let Builder {
            registry,
            mailer,
            passives,
            driver,
            aborter,
            ..
        } = self;
        let driver_boot = driver.expect("HasDriver state implies driver was supplied");

        let mut booted = boot_passives(&registry, &mailer, &aborter, passives)?;
        // Issue #581: finalise actor-aware logging once passives
        // have booted. Lookups `"aether.log"`; no-op if the cap
        // wasn't registered (chassis intentionally skipping logging).
        crate::log_install::install_log_target_if_registered(Arc::clone(&mailer), &registry);
        let driver_running = {
            let chassis_ctx = ChassisCtx::new(
                &registry,
                &mailer,
                &mut booted.fallback,
                &mut booted.frame_bound_pending,
                &booted.frame_bound_set,
                &booted.aborter,
            );
            let mut driver_ctx = DriverCtx::new(chassis_ctx, &booted.actors);
            driver_boot(&mut driver_ctx)?
        };

        Ok(BuiltChassis {
            booted,
            driver: driver_running,
            _chassis: PhantomData,
        })
    }
}

/// Internal carrier for the result of booting every passive.
struct BootedPassives {
    shutdowns: Vec<Box<dyn DynShutdown>>,
    fallback: Option<FallbackRouter>,
    /// Issue 552 stage 1: type-keyed map of booted [`NativeActor`]s.
    /// Populated by [`Builder::with_actor`] entries; the legacy
    /// `with(cap)` path leaves it empty. Borrowed (read-only) into
    /// [`DriverCtx`] and surfaced through [`PassiveChassis::actor`]
    /// / [`BuiltChassis`]'s lookup so drivers / embedders can pull
    /// `Arc<A>` for cap state without going through mail.
    actors: Actors,
    /// Per-mailbox pending counters from
    /// [`ChassisCtx::claim_frame_bound_mailbox`] calls — collected
    /// during passive boot, exposed to the driver via
    /// [`DriverCtx::frame_bound_pending`] (the driver stashes a clone
    /// for its frame loop).
    frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)>,
    /// Membership view of the same set; shared with every
    /// [`crate::NativeTransport`] booted under this chassis so the
    /// cross-class `wait_reply` guard can classify recipients.
    /// Populated alongside `frame_bound_pending` by
    /// [`ChassisCtx::claim_frame_bound_mailbox`].
    frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
    /// Cloned into every `ChassisCtx` and onto every booted
    /// [`crate::NativeTransport`] so the cross-class `wait_reply`
    /// guard has somewhere to abort to. Inherited from the
    /// [`Builder`]'s configured aborter.
    aborter: Arc<dyn FatalAborter>,
}

impl BootedPassives {
    fn shutdown_in_place(&mut self) {
        while let Some(s) = self.shutdowns.pop() {
            s.shutdown_dyn();
        }
    }
}

impl Drop for BootedPassives {
    fn drop(&mut self) {
        self.shutdown_in_place();
    }
}

fn boot_passives(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    aborter: &Arc<dyn FatalAborter>,
    passives: Vec<PassiveBoot>,
) -> Result<BootedPassives, BootError> {
    let mut shutdowns: Vec<Box<dyn DynShutdown>> = Vec::with_capacity(passives.len());
    let mut fallback: Option<FallbackRouter> = None;
    let mut actors = Actors::new();
    let mut frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)> = Vec::new();
    let frame_bound_set: Arc<RwLock<HashSet<MailboxId>>> = Arc::new(RwLock::new(HashSet::new()));
    for boot in passives {
        let mut ctx = ChassisCtx::new(
            registry,
            mailer,
            &mut fallback,
            &mut frame_bound_pending,
            &frame_bound_set,
            aborter,
        );
        match boot(&mut ctx, &mut actors) {
            Ok(shutdown) => shutdowns.push(shutdown),
            Err(e) => {
                while let Some(s) = shutdowns.pop() {
                    s.shutdown_dyn();
                }
                return Err(e);
            }
        }
    }
    Ok(BootedPassives {
        shutdowns,
        fallback,
        actors,
        frame_bound_pending,
        frame_bound_set,
        aborter: Arc::clone(aborter),
    })
}

/// A chassis built with a driver. [`Self::run`] delegates to the
/// driver's [`DriverRunning::run`] on the calling thread; when that
/// returns, every passive is shut down in reverse boot order.
pub struct BuiltChassis<C: Chassis> {
    booted: BootedPassives,
    driver: Box<dyn DriverRunning>,
    _chassis: PhantomData<fn() -> C>,
}

impl<C: Chassis> fmt::Debug for BuiltChassis<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltChassis")
            .field("profile", &C::PROFILE)
            .field("passives", &self.booted.shutdowns.len())
            .finish()
    }
}

impl<C: Chassis> BuiltChassis<C> {
    /// Issue 552 stage 1: look up a [`NativeActor`] booted via
    /// [`Builder::with_actor`]. `None` if `A` wasn't booted on this
    /// chassis. Useful for embedders that hold a [`BuiltChassis`]
    /// directly (today: hand-written test harnesses; bin chassis
    /// hand off to `run()` and don't keep the chassis around).
    pub fn actor<A: NativeActor>(&self) -> Option<Arc<A>> {
        self.booted.actors.get::<A>()
    }

    /// Block on the driver's run loop. On clean return, shut down
    /// every passive in reverse boot order. Driver errors propagate
    /// as [`RunError`]; passives still tear down before the error
    /// returns to the caller.
    pub fn run(self) -> Result<(), RunError> {
        let BuiltChassis { booted, driver, .. } = self;
        let result = driver.run();
        // Passives drop here, triggering reverse-order shutdown via
        // BootedPassives::Drop. Holding `booted` until after `result`
        // is bound keeps shutdown ordering deterministic.
        drop(booted);
        result
    }
}

/// A chassis built without a driver. The embedder (TestBench, future
/// embedded harnesses) drives any loop manually. Passives are booted
/// and accessible via [`Self::capability`]; they shut down when the
/// `PassiveChassis` is dropped.
pub struct PassiveChassis<C: Chassis> {
    booted: BootedPassives,
    _chassis: PhantomData<fn() -> C>,
}

impl<C: Chassis> fmt::Debug for PassiveChassis<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PassiveChassis")
            .field("profile", &C::PROFILE)
            .field("passives", &self.booted.shutdowns.len())
            .finish()
    }
}

impl<C: Chassis> PassiveChassis<C> {
    /// Number of booted passives. Useful for tests; not expected to
    /// vary at runtime.
    pub fn len(&self) -> usize {
        self.booted.shutdowns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.booted.shutdowns.is_empty()
    }

    /// Snapshot of every frame-bound mailbox's pending counter
    /// collected during passive boot. Embedders (TestBench, bin
    /// drivers) clone this once and feed it to
    /// [`crate::frame_loop::drain_frame_bound_or_abort`] each frame —
    /// same role as [`crate::chassis_builder::DriverCtx::frame_bound_pending`]
    /// on the driver-build path.
    pub fn frame_bound_pending(&self) -> Vec<(MailboxId, Arc<AtomicU64>)> {
        self.booted.frame_bound_pending.clone()
    }

    /// Issue 552 stage 1: look up a [`NativeActor`] booted via
    /// [`Builder::with_actor`]. `None` if `A` wasn't booted on this
    /// chassis. Embedders (TestBench, integration tests) reach for
    /// this when they need to peer at cap state without going
    /// through mail.
    pub fn actor<A: NativeActor>(&self) -> Option<Arc<A>> {
        self.booted.actors.get::<A>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lightweight passive-cap fixture for chassis-level boot tests.
    /// The chassis-builder tests don't care about handler dispatch
    /// (per-cap dispatch coverage lives in `aether-capabilities`); the
    /// real caps would force a circular dep, so this stub stands in.
    struct StubLog;
    impl aether_actor::Actor for StubLog {
        const NAMESPACE: &'static str = "test.chassis_builder.stub_log";
    }
    impl aether_actor::Singleton for StubLog {}

    impl crate::native_actor::NativeActor for StubLog {
        type Config = ();
        fn init(
            _: Self::Config,
            _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self)
        }
    }

    impl crate::native_actor::NativeDispatch for StubLog {
        fn __aether_dispatch_envelope(
            &self,
            _ctx: &mut crate::native_actor::NativeCtx<'_>,
            _kind: crate::mail::KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    /// Fixture chassis for passive-build tests.
    struct TestChassis;
    impl Chassis for TestChassis {
        const PROFILE: &'static str = "test";
        type Driver = NeverDriver;
        type Env = ();
        fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
            unreachable!("TestChassis is driven by Builder::new directly in unit tests");
        }
    }

    /// Fixture chassis for driver-build tests. Generic over the
    /// concrete `DriverCapability` so each test can pair the chassis
    /// type with whatever driver it's exercising.
    struct DrivenTestChassis<D: DriverCapability>(PhantomData<fn() -> D>);
    impl<D: DriverCapability + 'static> Chassis for DrivenTestChassis<D> {
        const PROFILE: &'static str = "test-driven";
        type Driver = D;
        type Env = ();
        fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
            unreachable!("DrivenTestChassis is driven by Builder::new directly in unit tests");
        }
    }

    /// Test driver: records that it ran, then exits.
    struct RanDriver {
        ran: Arc<std::sync::atomic::AtomicBool>,
    }

    struct RanDriverRunning {
        ran: Arc<std::sync::atomic::AtomicBool>,
    }

    impl DriverCapability for RanDriver {
        type Running = RanDriverRunning;
        fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
            Ok(RanDriverRunning { ran: self.ran })
        }
    }

    impl DriverRunning for RanDriverRunning {
        fn run(self: Box<Self>) -> Result<(), RunError> {
            self.ran.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    /// Driver build path: passives boot, driver runs, passives tear
    /// down on chassis drop. Per-cap dispatch coverage lives in the
    /// individual cap modules; this test exercises the chassis-level
    /// boot + run + teardown sequence.
    #[test]
    fn driver_build_runs_driver_and_tears_down_passives() {
        let (registry, mailer) = fresh_substrate();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let chassis = Builder::<DrivenTestChassis<RanDriver>>::new(registry, mailer)
            .with_actor::<StubLog>(())
            .driver(RanDriver {
                ran: Arc::clone(&ran),
            })
            .build()
            .expect("build succeeds");

        chassis.run().expect("driver run succeeds");
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Boot-time mailbox-claim collision aborts the build (and runs
    /// the prior cap's drop). Two `StubLog` instances both claim
    /// `test.chassis_builder.stub_log`; the second hits the
    /// duplicate-claim guard.
    #[test]
    fn duplicate_passive_mailbox_aborts_build_and_shuts_down_prior() {
        let (registry, mailer) = fresh_substrate();

        let err = Builder::<TestChassis>::new(registry, mailer)
            .with_actor::<StubLog>(())
            .with_actor::<StubLog>(())
            .build_passive()
            .expect_err("second passive must fail with duplicate claim");

        assert!(matches!(err, BootError::MailboxAlreadyClaimed { .. }));
    }

    /// Issue 552 stage 1: end-to-end smoke for the new
    /// [`Builder::with_actor`] boot path. Boots a hand-rolled
    /// `NativeActor + NativeDispatch` fixture, looks it up via
    /// [`PassiveChassis::actor`], pushes one envelope at the cap's
    /// mailbox, and asserts the dispatcher routed it to the right
    /// handler. Stage 1 lands the infrastructure; stage 2 migrates
    /// real caps onto it. This test is the load-bearing acceptance
    /// gate.
    #[test]
    fn with_actor_boots_dispatches_and_tears_down() {
        use crate::registry::MailboxEntry;
        use aether_data::{Kind, ReplyTo as DataReplyTo};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        // Fixture kind: a 4-byte cast-shape payload so encode_into_bytes
        // lands on the bytemuck path.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Ping {
            tag: u32,
        }
        impl Kind for Ping {
            const NAME: &'static str = "test.with_actor.ping";
            const ID: aether_data::KindId = aether_data::KindId(0xA1B2_C3D4_E5F6_0001);
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != core::mem::size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
        }

        // Fixture cap. State behind interior mutability so `&self`
        // dispatch can mutate it (the post-552 norm).
        struct ProbeCap {
            received: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for ProbeCap {
            const NAMESPACE: &'static str = "test.with_actor.probe";
        }
        impl aether_actor::Singleton for ProbeCap {}
        impl aether_actor::HandlesKind<Ping> for ProbeCap {}

        impl crate::native_actor::NativeActor for ProbeCap {
            type Config = Arc<AtomicU32>;
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }

        // Hand-rolled NativeDispatch — what the macro arm emits in
        // task #731. The if-arm decodes Ping bytes, calls the
        // handler, returns Some(()) on success.
        impl crate::native_actor::NativeDispatch for ProbeCap {
            fn __aether_dispatch_envelope(
                &self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Ping::ID.0 {
                    let _decoded = Ping::decode_from_bytes(payload)?;
                    self.received.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let received = Arc::new(AtomicU32::new(0));

        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<ProbeCap>(Arc::clone(&received))
            .build_passive()
            .expect("with_actor boot succeeds");

        // PassiveChassis lookup returns the booted Arc.
        let probe: Arc<ProbeCap> = chassis
            .actor::<ProbeCap>()
            .expect("ProbeCap registered in the actors map");
        assert!(Arc::ptr_eq(&probe.received, &received));

        // Push one envelope at the cap's mailbox via the registry's
        // sink handler. The dispatcher thread pulls from its inbox
        // and routes through __aether_dispatch_envelope → on_ping.
        let mailbox_id = registry
            .lookup(<ProbeCap as aether_actor::Actor>::NAMESPACE)
            .expect("with_actor claimed the mailbox");
        let MailboxEntry::Sink(handler) = registry.entry(mailbox_id).expect("sink registered")
        else {
            panic!("ProbeCap claim must be a sink entry");
        };

        let payload = Ping { tag: 0xDEAD_BEEF };
        let bytes = payload.encode_into_bytes();
        handler(
            <Ping as Kind>::ID,
            Ping::NAME,
            None,
            DataReplyTo::NONE,
            &bytes,
            1,
        );

        // Wait briefly for the dispatcher thread to dispatch.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while received.load(AtomicOrdering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            received.load(AtomicOrdering::SeqCst),
            1,
            "dispatcher should have routed Ping → on_ping within the wait budget"
        );

        drop(chassis);
    }

    /// Issue 552 stage 2d: with_actor accepts FRAME_BARRIER caps.
    /// The chassis claims through `claim_frame_bound_mailbox`, the
    /// pending counter feeds the chassis's `frame_bound_pending` Vec,
    /// and the dispatcher decrements after each handler dispatch so
    /// the per-frame drain barrier sees the counter drop in lock-step.
    /// Pre-2d the entry point hard-rejected; the prior reject-test
    /// retired alongside that branch.
    #[test]
    fn with_actor_supports_frame_barrier_caps() {
        struct FrameBoundProbe;
        impl aether_actor::Actor for FrameBoundProbe {
            const NAMESPACE: &'static str = "test.with_actor.frame_bound";
            const FRAME_BARRIER: bool = true;
        }
        impl aether_actor::Singleton for FrameBoundProbe {}

        impl crate::native_actor::NativeActor for FrameBoundProbe {
            type Config = ();
            fn init(
                _: (),
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self)
            }
        }

        impl crate::native_actor::NativeDispatch for FrameBoundProbe {
            fn __aether_dispatch_envelope(
                &self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(registry, mailer)
            .with_actor::<FrameBoundProbe>(())
            .build_passive()
            .expect("FRAME_BARRIER caps boot through with_actor");
        // Frame-bound claim populated the chassis's pending Vec.
        assert_eq!(
            chassis.frame_bound_pending().len(),
            1,
            "FRAME_BARRIER cap registered its pending counter for the drain barrier"
        );
        drop(chassis);
    }

    /// Issue 582: the chassis dispatcher trampoline stamps the
    /// per-actor [`aether_actor::local::ActorSlots`] into TLS
    /// for the duration of `init` and each handler call. A cap that
    /// reaches for `Local::with_mut` from inside both lifecycle
    /// stages must see its own state — verified end-to-end here so
    /// the stamping wiring can't silently regress.
    #[test]
    fn with_actor_stamps_local_for_init_and_handler() {
        use crate::registry::MailboxEntry;
        use aether_actor::Local;
        use aether_data::{Kind, ReplyTo as DataReplyTo};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Tick {
            seq: u32,
        }
        impl Kind for Tick {
            const NAME: &'static str = "test.local.tick";
            const ID: aether_data::KindId = aether_data::KindId(0xA1B2_C3D4_E5F6_0002);
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != core::mem::size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
        }

        // The cap holds an Arc<AtomicU32> the test reads after each
        // dispatch. The actor-local counter is keyed by `TypeId<Counter>`
        // — the chassis stamp is what makes `with_mut` resolve at
        // all (outside a stamp it would `debug_assert!` panic).
        struct LocalProbe {
            observed: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for LocalProbe {
            const NAMESPACE: &'static str = "test.local.probe";
        }
        impl aether_actor::Singleton for LocalProbe {}
        impl aether_actor::HandlesKind<Tick> for LocalProbe {}

        // Newtype-per-slot is the Local convention: each
        // logical storage gets its own type, so two probes that
        // both want a u32 don't alias under TypeId. The
        // `#[local]` attribute is the shorthand for the
        // marker impl.
        #[derive(Default)]
        #[aether_actor::local]
        struct Counter(u32);

        impl crate::native_actor::NativeActor for LocalProbe {
            type Config = Arc<AtomicU32>;
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                // Init runs inside the chassis builder's stamp guard
                // — write a sentinel so the handler test below proves
                // the same slots are reused across init→dispatch.
                Counter::with_mut(|c| c.0 = 100);
                Ok(Self { observed: config })
            }
        }

        impl crate::native_actor::NativeDispatch for LocalProbe {
            fn __aether_dispatch_envelope(
                &self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Tick::ID.0 {
                    let _decoded = Tick::decode_from_bytes(payload)?;
                    Counter::with_mut(|c| c.0 += 1);
                    let snapshot = Counter::with(|c| c.0);
                    self.observed.store(snapshot, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let observed = Arc::new(AtomicU32::new(0));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<LocalProbe>(Arc::clone(&observed))
            .build_passive()
            .expect("LocalProbe boots");

        let mailbox_id = registry
            .lookup(<LocalProbe as aether_actor::Actor>::NAMESPACE)
            .expect("with_actor claimed the mailbox");
        let MailboxEntry::Sink(handler) = registry.entry(mailbox_id).expect("sink registered")
        else {
            panic!("LocalProbe claim must be a sink entry");
        };

        // Three dispatches. Init seeded 100; the handler bumps once
        // per dispatch and snapshots — so observed should walk
        // 101, 102, 103 in order. We assert the final 103 with a
        // wait budget to cover dispatcher-thread scheduling.
        for seq in 0..3 {
            let payload = Tick { seq };
            let bytes = payload.encode_into_bytes();
            handler(
                <Tick as Kind>::ID,
                Tick::NAME,
                None,
                DataReplyTo::NONE,
                &bytes,
                1,
            );
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while observed.load(AtomicOrdering::SeqCst) != 103 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            observed.load(AtomicOrdering::SeqCst),
            103,
            "init seeded 100 + 3 handler bumps ⇒ Local at 103 (proves the same \
             ActorSlots is stamped across init and dispatch)"
        );

        drop(chassis);
    }
}
