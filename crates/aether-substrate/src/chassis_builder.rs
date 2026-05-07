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
use crate::native_actor::{ExportedHandles, NativeActor, NativeCtx, NativeDispatch, NativeInitCtx};
use crate::native_transport::NativeTransport;
use crate::registry::Registry;
use aether_actor::Actor;
use aether_actor::Dispatch;
use aether_actor::HandlesKind;
use aether_data::mailbox_id_from_name;
use aether_kinds::{ConfigureLogDrain, LogBatch};

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
/// Issue 629 / Phase A: borrows the chassis's [`ExportedHandles`]
/// map. Drivers retrieve cap-published handle bundles via
/// [`Self::handle`]. The pre-629 `actor::<A>() -> Arc<A>` accessor
/// retired — the actor itself never escapes its dispatcher thread, so
/// drivers consume cap-exported handle clones instead.
pub struct DriverCtx<'a> {
    inner: ChassisCtx<'a>,
    handles: &'a ExportedHandles,
}

impl<'a> DriverCtx<'a> {
    fn new(inner: ChassisCtx<'a>, handles: &'a ExportedHandles) -> Self {
        Self { inner, handles }
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

    /// Issue 629 / Phase A: retrieve a clone of a cap-published handle
    /// bundle of type `H`. `None` if no cap published one (typically
    /// because the cap that owns the handle wasn't booted on this
    /// chassis). Drivers use this to pull `RenderHandles` and similar
    /// driver-facing sub-handle bundles without reaching for the cap
    /// itself.
    pub fn handle<H: std::any::Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.handles.get::<H>()
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

/// Every `PassiveBoot` closure receives a [`ChassisCtx`] (registry /
/// mailer / fallback / frame-bound state) and a `&mut ExportedHandles`
/// view. Issue 629 / Phase A: the second arg is the chassis's
/// handle-export map — caps publish driver-facing sub-handle bundles
/// during their `init` (via [`NativeInitCtx::publish_handle`]).
/// Closures from [`make_passive_boot`] / [`make_fallback_router_boot`]
/// ignore it; [`make_native_actor_boot`] threads it through to the
/// init ctx.
type PassiveBoot = Box<
    dyn FnOnce(
        &mut ChassisCtx<'_>,
        &mut ExportedHandles,
    ) -> Result<Box<dyn DynShutdown>, BootError>,
>;
type DriverBoot = Box<dyn FnOnce(&mut DriverCtx<'_>) -> Result<Box<dyn DriverRunning>, BootError>>;

fn make_passive_boot<C>(cap: C) -> PassiveBoot
where
    C: Actor + Dispatch + Send + 'static,
{
    Box::new(move |ctx, _handles| {
        let handle = ctx.spawn_actor_dispatcher(cap)?;
        Ok(Box::new(FacadeShutdown {
            handle: Some(handle),
        }) as Box<dyn DynShutdown>)
    })
}

fn make_fallback_router_boot(handler: FallbackRouter) -> PassiveBoot {
    Box::new(move |ctx, _handles| {
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
    Box::new(move |ctx, handles| {
        // Issue 607 Phase 3b (ADR-0079): claim namespace ownership for
        // this singleton's `Actor::NAMESPACE`. The actor registry
        // tracks one TypeId per namespace across both cardinalities
        // (Singleton/Instanced), so a later `spawn_child::<X>` whose
        // `X::NAMESPACE` collides with this singleton's namespace
        // surfaces as `SpawnError::NamespaceOwnedByOtherType`. Same
        // TypeId re-claiming the same namespace is idempotent.
        if let Err(_existing) = ctx
            .spawner_arc()
            .actor_registry()
            .try_claim_namespace(A::NAMESPACE, std::any::TypeId::of::<A>())
        {
            // The other claim is on the same namespace by a different
            // TypeId — a chassis-build collision. Surface as a typed
            // BootError; the chassis builder unwinds the partially
            // booted caps before propagating.
            return Err(BootError::Other(Box::new(std::io::Error::other(format!(
                "namespace {:?} already owned by a different TypeId — fix the conflicting actor's NAMESPACE const",
                A::NAMESPACE
            )))));
        }

        // Frame-bound caps (today: render) claim through the
        // frame-bound path so the `pending` counter feeds the chassis
        // frame loop. Free-running caps take the regular drop-on-
        // shutdown claim. Both share the same dispatcher trampoline
        // shape apart from the post-dispatch decrement.
        //
        // Issue 607 Phase 7: if the mailbox claim fails (name
        // collision against a peer cap claiming the same mailbox
        // through `register_sink`), release the namespace claim we
        // just made — otherwise a later cap with a different TypeId
        // legitimately claiming the same namespace can't.
        let claim_result = if A::FRAME_BARRIER {
            ctx.claim_frame_bound_mailbox::<A>().map(|claim| {
                (
                    claim.id,
                    claim.receiver,
                    claim.sink_sender,
                    Some(claim.pending),
                )
            })
        } else {
            ctx.claim_mailbox_drop_on_shutdown::<A>()
                .map(|claim| (claim.id, claim.receiver, claim.sink_sender, None))
        };
        let (mailbox_id, receiver, sink_sender, pending) = match claim_result {
            Ok(c) => c,
            Err(e) => {
                ctx.spawner_arc()
                    .actor_registry()
                    .release_namespace(A::NAMESPACE, std::any::TypeId::of::<A>());
                return Err(e);
            }
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
        // chassis's ExportedHandles (mutable, so the cap may publish
        // a driver-facing handle bundle) plus the transport (read-only)
        // plus a mailer clone for outbound hooks. Issue #581: also
        // stamp the actor's transport as the in-actor `MailDispatch`
        // around `init` so any `tracing::*` event the cap fires
        // during boot drains to LogCapability when init returns.
        //
        // Issue 607 Phase 7: failed init releases the slot before any
        // dispatcher thread is spawned — drop the transport
        // (including its installed inbox), unclaim the mailbox, and
        // release the namespace.
        let init_result = {
            let mailer_clone = ctx.mail_send_handle();
            let mut init_ctx = NativeInitCtx::new(&transport, handles, mailer_clone);
            aether_actor::local::with_stamped(&slots, || {
                aether_actor::log::with_actor_dispatch(
                    &*transport as &dyn aether_actor::log::MailDispatch,
                    || {
                        let r = A::init(config, &mut init_ctx);
                        aether_actor::log::drain_buffer();
                        r
                    },
                )
            })
        };
        let actor = match init_result {
            Ok(a) => a,
            Err(e) => {
                drop(transport);
                drop(sink_sender);
                ctx.unclaim_mailbox(mailbox_id);
                ctx.spawner_arc()
                    .actor_registry()
                    .release_namespace(A::NAMESPACE, std::any::TypeId::of::<A>());
                return Err(e);
            }
        };

        // Issue 629 / Phase A: dispatcher takes Box<A> ownership.
        // The actor lives exclusively on the dispatcher thread —
        // no chassis-side Arc share, no cross-thread access path.
        // Drivers / embedders consume cap-published handle bundles
        // from the chassis's `ExportedHandles` map instead.
        let mut actor: Box<A> = Box::new(actor);

        // Spawn the dispatcher thread. The thread owns the Box<A>,
        // an Arc<NativeTransport>, and the per-actor `ActorSlots`
        // (moved in by value); it loops `recv_blocking()` on the
        // transport (which pulls from the installed inbox and
        // disconnects when the chassis drops its `sink_sender`) and
        // routes each envelope through `__aether_dispatch_envelope`,
        // wrapped in `local::with_stamped` so handler bodies
        // see the per-actor storage.
        let transport_for_thread = Arc::clone(&transport);
        let actor_registry_for_thread = Arc::clone(ctx.spawner_arc().actor_registry());
        let mailer_for_thread = ctx.mail_send_handle();
        let self_id_for_thread = mailbox_id;
        let thread_name = alloc_native_actor_thread_name::<A>();
        let thread = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                // Issue 607 Phase 4a (ADR-0079): the singleton
                // dispatcher polls the self-shutdown flag the same
                // way the instanced dispatcher does in `spawn.rs`.
                // Channel-disconnect is the chassis-shutdown path;
                // both flow through the same drain → on_close → exit
                // sequence below. Singletons don't tombstone (their
                // mailbox slot stays in `Registry`); the chassis's
                // `BootedPassives::shutdown_in_place` reverse-drops
                // the SinkSender to fully disconnect the sink.
                loop {
                    if transport_for_thread.should_shutdown() {
                        break;
                    }
                    let env = match transport_for_thread.recv_blocking() {
                        Some(e) => e,
                        None => break,
                    };
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
                                if actor
                                    .__aether_dispatch_envelope(&mut ctx, env.kind, &env.payload)
                                    .is_none()
                                    && !actor
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
                // Drain remaining inbox synchronously so any in-flight
                // mail dispatched during the shutdown signal is
                // observed before `on_close` runs.
                while let Some(env) = transport_for_thread.try_recv() {
                    aether_actor::local::with_stamped(&slots, || {
                        aether_actor::log::with_actor_dispatch(
                            &*transport_for_thread as &dyn aether_actor::log::MailDispatch,
                            || {
                                let mut ctx =
                                    NativeCtx::new(&transport_for_thread, env.sender);
                                let _ = actor
                                    .__aether_dispatch_envelope(&mut ctx, env.kind, &env.payload);
                                aether_actor::log::drain_buffer();
                            },
                        );
                    });
                    if let Some(p) = &pending {
                        p.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    }
                }
                // Last-chance close hook. Runs whether the trigger was
                // self-shutdown (flag) or substrate shutdown (channel
                // disconnect). Default empty for singletons that don't
                // need it; opt-in for caps with cleanup state.
                aether_actor::local::with_stamped(&slots, || {
                    aether_actor::log::with_actor_dispatch(
                        &*transport_for_thread as &dyn aether_actor::log::MailDispatch,
                        || {
                            let mut close_ctx = NativeCtx::new(
                                &transport_for_thread,
                                crate::mail::ReplyTo::NONE,
                            );
                            actor.on_close(&mut close_ctx);
                            aether_actor::log::drain_buffer();
                        },
                    );
                });
                // Issue 607 Phase 4b (ADR-0079): close the actor in
                // the registry — drains monitors_of[id] for fan-out,
                // walks monitoring[id] to prune the singleton from
                // each watched target's forward index, then marks
                // Dead + tombstones the id. Singletons today don't
                // sit in `actors` as `Live`, so the slot transition
                // is purely sentinel; the reverse-prune is the
                // load-bearing step (a singleton that monitored
                // instanced actors must not leave dangling forward
                // refs after its dispatcher exits).
                let watchers = actor_registry_for_thread.close_actor(self_id_for_thread);
                if !watchers.is_empty() {
                    let notice = aether_kinds::MonitorNotice {
                        target: self_id_for_thread,
                    };
                    let payload =
                        <aether_kinds::MonitorNotice as aether_data::Kind>::encode_into_bytes(
                            &notice,
                        );
                    let kind = crate::mail::KindId(
                        <aether_kinds::MonitorNotice as aether_data::Kind>::ID.0,
                    );
                    for watcher in watchers {
                        mailer_for_thread.push(crate::mail::Mail::new(
                            watcher,
                            kind,
                            payload.clone(),
                            1,
                        ));
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
    /// Issue #601: chassis-declared log-drain target. `None` until the
    /// chassis calls [`Self::with_log_drain`]; on `build()` the
    /// mailbox id is dispatched as `aether.log.configure_drain` mail
    /// to every booted actor so each actor's `LogDrainSlot` is
    /// installed. `ControlPlaneCapability` snapshots the same drain
    /// for the runtime `load_component` path — runtime-loaded
    /// components receive `ConfigureLogDrain` themselves on
    /// registration. The chassis Builder declares the drain; the
    /// runtime state lives entirely in `ControlPlane` and per-actor
    /// `LogDrainSlot`s, set the same way every actor's slot is set:
    /// via mail.
    log_drain: Option<MailboxId>,
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
            log_drain: None,
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

    /// Issue #601: declare the chassis-wide log drain target. `T` must
    /// be a [`NativeActor`] that handles [`LogBatch`] (the cap's
    /// mailbox id is derived from `T::NAMESPACE` at compile time).
    /// On `build()` / `build_passive()` the chassis dispatches a
    /// `aether.log.configure_drain` mail to every booted actor so each
    /// actor's `LogDrainSlot` resolves to this mailbox; the
    /// auto-emitted handler in `#[actor]` does the install.
    ///
    /// No call → `log_drain` stays `None`, no `ConfigureLogDrain`
    /// dispatched, actors keep their default unset slot, and
    /// `drain_buffer` is a silent no-op (chassis intentionally
    /// skipping logging).
    pub fn with_log_drain<T>(mut self) -> Self
    where
        T: NativeActor + HandlesKind<LogBatch>,
    {
        self.log_drain = Some(mailbox_id_from_name(T::NAMESPACE));
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
            log_drain: self.log_drain,
            _chassis: PhantomData,
            _state: PhantomData,
        }
    }

    /// No-driver build path. Boots every passive in declaration order
    /// and returns a [`PassiveChassis`] whose embedder is responsible
    /// for driving the loop manually (TestBench).
    pub fn build_passive(self) -> Result<PassiveChassis<C>, BootError> {
        let booted = boot_passives(&self.registry, &self.mailer, &self.aborter, self.passives)?;
        // Issue #601: push `ConfigureLogDrain` to every booted actor
        // and to `aether.control` so every per-actor `LogDrainSlot`
        // (auto-emitted handler) plus the `ControlPlane`'s drain slot
        // (for the runtime load path) resolve to the chassis-declared
        // target. No-op if the chassis didn't call `with_log_drain`.
        dispatch_configure_log_drain(
            &self.registry,
            &self.mailer,
            &booted.claimed_actor_mailboxes,
            self.log_drain,
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

    /// Mirror of [`Builder::with_log_drain`][Builder<C, NoDriver>::with_log_drain]
    /// for the post-driver state. Issue #601.
    pub fn with_log_drain<T>(mut self) -> Self
    where
        T: NativeActor + HandlesKind<LogBatch>,
    {
        self.log_drain = Some(mailbox_id_from_name(T::NAMESPACE));
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
        // Issue #601: push `ConfigureLogDrain` to every booted actor
        // and to `aether.control` so the runtime load path picks up
        // the chassis-declared drain through the same mail every
        // actor receives.
        dispatch_configure_log_drain(
            &registry,
            &mailer,
            &booted.claimed_actor_mailboxes,
            self.log_drain,
        );
        let driver_running = {
            let chassis_ctx = ChassisCtx::new(
                &registry,
                &mailer,
                &mut booted.fallback,
                &mut booted.frame_bound_pending,
                &booted.frame_bound_set,
                &booted.aborter,
                &mut booted.claimed_actor_mailboxes,
                &booted.spawner,
            );
            let mut driver_ctx = DriverCtx::new(chassis_ctx, &booted.handles);
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
    /// Issue 629 / Phase A: cap-published handle bundles. Populated
    /// during each cap's `init` via [`NativeInitCtx::publish_handle`].
    /// Borrowed (read-only) into [`DriverCtx::handle`] so drivers
    /// retrieve a clone of the published bundle. Replaces the pre-629
    /// type-keyed actor map; the actor itself never escapes its
    /// dispatcher thread.
    handles: ExportedHandles,
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
    /// Issue #601: every actor mailbox claimed during passive boot.
    /// `Builder::build` / `build_passive` reads this list to dispatch
    /// `ConfigureLogDrain` mail to each actor before the driver runs,
    /// installing every actor's `LogDrainSlot` to the chassis-declared
    /// drain.
    claimed_actor_mailboxes: Vec<MailboxId>,
    /// Issue 607 Phase 2 / Phase 3 (ADR-0079): per-chassis actor
    /// lifecycle registry, plus the spawn machinery that writes into
    /// it. Both built once at boot; `Spawner` carries `Arc` clones of
    /// the chassis-level handles (registry, actor_registry, mailer,
    /// frame_bound_set, aborter) so future per-handler `spawn_child`
    /// reaches them without separate plumbing.
    actor_registry: Arc<crate::ActorRegistry>,
    spawner: Arc<crate::Spawner>,
}

/// Issue #601: dispatch a `ConfigureLogDrain { mailbox: drain }` mail
/// to every actor whose mailbox was claimed during boot. Called by
/// `Builder::build` / `build_passive` after `boot_passives` returns.
/// No-op if `drain` is `None` (chassis didn't declare a log target).
///
/// Sends through the same `Mailer` every other mail uses — each actor
/// mailbox routes the envelope to the auto-emitted `ConfigureLogDrain`
/// arm in `#[handlers]`, which installs the per-actor `LogDrainSlot`.
/// Issue 603: `ControlPlaneCapability` is now a normal actor booted
/// through this Builder, so its mailbox lands in
/// `claimed_actor_mailboxes` like every other cap and the pre-603
/// special-case lookup of `aether.control` retired.
fn dispatch_configure_log_drain(
    _registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    targets: &[MailboxId],
    drain: Option<MailboxId>,
) {
    let Some(drain) = drain else {
        return;
    };
    let kind = <ConfigureLogDrain as aether_data::Kind>::ID;
    for target in targets {
        let cfg = ConfigureLogDrain { mailbox: drain };
        let payload = bytemuck::bytes_of(&cfg).to_vec();
        let mail = crate::mail::Mail::new(*target, kind, payload, 1);
        mailer.push(mail);
    }
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
    let mut handles = ExportedHandles::new();
    let mut frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)> = Vec::new();
    let frame_bound_set: Arc<RwLock<HashSet<MailboxId>>> = Arc::new(RwLock::new(HashSet::new()));
    let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
    let actor_registry: Arc<crate::ActorRegistry> = Arc::new(crate::ActorRegistry::new());
    let spawner: Arc<crate::Spawner> = Arc::new(crate::Spawner::new(
        Arc::clone(registry),
        Arc::clone(&actor_registry),
        Arc::clone(mailer),
        Arc::clone(&frame_bound_set),
        Arc::clone(aborter),
    ));
    for boot in passives {
        let mut ctx = ChassisCtx::new(
            registry,
            mailer,
            &mut fallback,
            &mut frame_bound_pending,
            &frame_bound_set,
            aborter,
            &mut claimed_actor_mailboxes,
            &spawner,
        );
        match boot(&mut ctx, &mut handles) {
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
        handles,
        frame_bound_pending,
        frame_bound_set,
        aborter: Arc::clone(aborter),
        claimed_actor_mailboxes,
        actor_registry,
        spawner,
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
    /// Issue 607 Phase 5 (ADR-0079): look up a single instanced actor
    /// by its per-instance subname. Returns `Some(MailboxId)` only if
    /// `(A::NAMESPACE, subname)` resolves to a `Live` slot in the
    /// chassis's [`crate::ActorRegistry`] whose `TypeId` matches `A`.
    /// `None` for missing names, tombstoned names, or type
    /// mismatches.
    ///
    /// Issue 629 / Phase A: returns the address (`MailboxId`) instead
    /// of `Arc<A>`. The actor itself never escapes its dispatcher
    /// thread; callers that need to interact with the resolved
    /// instance mail it. Wrong-cardinality calls fail to compile via
    /// the `Instanced` bound.
    ///
    /// ```compile_fail
    /// # use aether_substrate::{BuiltChassis, NativeActor};
    /// # use aether_actor::Singleton;
    /// // Calling resolve_actor::<X>(...) on a Singleton fails to
    /// // compile — the Instanced bound is missing.
    /// fn _wrong<C: aether_substrate::Chassis, A: Singleton + NativeActor>(
    ///     chassis: &BuiltChassis<C>,
    /// ) {
    ///     let _ = chassis.resolve_actor::<A>("anything");
    /// }
    /// ```
    pub fn resolve_actor<A: aether_actor::Instanced + NativeActor>(
        &self,
        subname: &str,
    ) -> Option<MailboxId> {
        let full_name = format!("{}:{}", A::NAMESPACE, subname);
        let id = MailboxId(aether_data::mailbox_id_from_name(&full_name).0);
        let type_id = self.booted.actor_registry.type_id_at(id)?;
        if type_id != std::any::TypeId::of::<A>() {
            return None;
        }
        Some(id)
    }

    /// Issue 607 Phase 5 (ADR-0079): enumerate every `Live` instance
    /// of `A` along with its subname. Issue 629 / Phase A: returns
    /// `(subname, MailboxId)` pairs (not `Arc<A>`); the actor itself
    /// never escapes its dispatcher thread.
    ///
    /// **Diagnostic / embedder-test affordance.** Caps that supervise
    /// a fleet of instances (e.g. `TcpCapability` over
    /// `TcpListenerActor`) hold their own cap-local map of children
    /// and update it on `MonitorNotice` — they don't enumerate via
    /// the chassis registry from a handler. Reach for this from a
    /// driver / TestBench / scenario inspection step, not from
    /// production cap state. ADR-0079 supervisor-as-cap pattern.
    pub fn resolve_actors<A: aether_actor::Instanced + NativeActor>(
        &self,
    ) -> Vec<(String, MailboxId)> {
        self.booted.actor_registry.live_subnames_of_type::<A>()
    }

    /// Issue 607 Phase 3: chassis-level entry point for spawning an
    /// instanced actor (ADR-0079). Returns a [`crate::SpawnBuilder`]
    /// the caller chains `after_init` / `finish` against. The
    /// per-handler equivalent (`NativeCtx::spawn_child`) lands in a
    /// follow-up; callers in the chassis-builder scope (driver
    /// pre-build, embedders) reach for this.
    pub fn spawn_actor<'a, A>(
        &'a self,
        subname: crate::Subname<'a>,
        config: A::Config,
    ) -> crate::SpawnBuilder<'a, A>
    where
        A: aether_actor::Instanced + NativeActor + NativeDispatch,
    {
        crate::SpawnBuilder::new(
            Arc::clone(&self.booted.spawner),
            subname,
            config,
            crate::ReplyTo::NONE,
        )
    }

    /// Borrow the chassis's [`crate::ActorRegistry`]. Read-only;
    /// embedders that want to introspect live instanced actors
    /// (test assertions, diagnostics) reach for this.
    pub fn actor_registry(&self) -> &Arc<crate::ActorRegistry> {
        &self.booted.actor_registry
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

    /// Issue 629 / Phase A: retrieve a clone of a cap-published handle
    /// bundle of type `H`. Mirrors [`DriverCtx::handle`] for embedders
    /// that drive a `PassiveChassis` directly (TestBench, integration
    /// harnesses). `None` if no booted cap published a handle of that
    /// type.
    pub fn handle<H: std::any::Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.booted.handles.get::<H>()
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

    /// Issue 607 Phase 5 (ADR-0079): mirror of
    /// [`BuiltChassis::resolve_actor`] for embedders that drive
    /// passive chassis directly (TestBench, integration tests).
    /// Issue 629 / Phase A: returns the address (`MailboxId`); the
    /// actor itself never escapes its dispatcher thread.
    ///
    /// ```compile_fail
    /// # use aether_substrate::{PassiveChassis, NativeActor};
    /// # use aether_actor::Singleton;
    /// fn _wrong<C: aether_substrate::Chassis, A: Singleton + NativeActor>(
    ///     chassis: &PassiveChassis<C>,
    /// ) {
    ///     let _ = chassis.resolve_actor::<A>("anything");
    /// }
    /// ```
    pub fn resolve_actor<A: aether_actor::Instanced + NativeActor>(
        &self,
        subname: &str,
    ) -> Option<MailboxId> {
        let full_name = format!("{}:{}", A::NAMESPACE, subname);
        let id = MailboxId(aether_data::mailbox_id_from_name(&full_name).0);
        let type_id = self.booted.actor_registry.type_id_at(id)?;
        if type_id != std::any::TypeId::of::<A>() {
            return None;
        }
        Some(id)
    }

    /// Issue 607 Phase 5 (ADR-0079): mirror of
    /// [`BuiltChassis::resolve_actors`] for embedders that drive
    /// passive chassis directly. Issue 629 / Phase A: returns
    /// `(subname, MailboxId)` pairs. Diagnostic-only contract: caps
    /// that supervise a fleet hold their own cap-local map; this is
    /// for tests and chassis-level introspection only.
    pub fn resolve_actors<A: aether_actor::Instanced + NativeActor>(
        &self,
    ) -> Vec<(String, MailboxId)> {
        self.booted.actor_registry.live_subnames_of_type::<A>()
    }

    /// Issue 607 Phase 3: chassis-level entry point for spawning an
    /// instanced actor (ADR-0079). Returns a [`crate::SpawnBuilder`]
    /// the caller chains `after_init` / `finish` against. Mirrors
    /// [`BuiltChassis::spawn_actor`]; both build the same
    /// [`crate::SpawnBuilder`] over the chassis's [`crate::Spawner`].
    pub fn spawn_actor<'a, A>(
        &'a self,
        subname: crate::Subname<'a>,
        config: A::Config,
    ) -> crate::SpawnBuilder<'a, A>
    where
        A: aether_actor::Instanced + NativeActor + NativeDispatch,
    {
        crate::SpawnBuilder::new(
            Arc::clone(&self.booted.spawner),
            subname,
            config,
            crate::ReplyTo::NONE,
        )
    }

    /// Borrow the chassis's [`crate::ActorRegistry`]. Read-only;
    /// embedders that want to introspect live instanced actors
    /// (test assertions, diagnostics) reach for this.
    pub fn actor_registry(&self) -> &Arc<crate::ActorRegistry> {
        &self.booted.actor_registry
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
            &mut self,
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
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new());
        // Wire the mailer's registry so any test that pushes mail
        // (Phase 4b's close-time MonitorNotice fan-out, future
        // close-side mail) doesn't trip the "Mailer not wired" assert
        // in `Mailer::push`. Pre-Phase-4b tests that never reach
        // `mailer.push` are unaffected — wiring is one-shot and idle.
        mailer.wire(Arc::clone(&registry));
        (registry, mailer)
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

    /// Issue 607 Phase 7: a singleton whose `init` returns `Err`
    /// releases its slot before `with_actor` propagates the error.
    /// After the failed build, the chassis's `Registry` has no sink
    /// at the cap's namespace and the `ActorRegistry`'s `name_owners`
    /// no longer claims the namespace — so a fresh chassis can boot
    /// a different cap with the same namespace string (or the same
    /// cap with a different config) without colliding.
    #[test]
    fn failed_singleton_init_releases_namespace_and_sink() {
        struct FailingCap;
        impl aether_actor::Actor for FailingCap {
            const NAMESPACE: &'static str = "test.phase7.failing_cap";
        }
        impl aether_actor::Singleton for FailingCap {}

        impl crate::native_actor::NativeActor for FailingCap {
            type Config = ();
            fn init(
                _: (),
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Err(BootError::Other(Box::new(std::io::Error::other(
                    "intentional init failure for Phase 7 cleanup test",
                ))))
            }
        }
        impl crate::native_actor::NativeDispatch for FailingCap {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<FailingCap>(())
            .build_passive()
            .expect_err("init failure must propagate");
        // The error wraps init's std::io::Error message.
        assert!(
            format!("{err:?}").contains("intentional init failure"),
            "expected init error to propagate, got {err:?}",
        );

        // Sink at the cap's namespace must be gone — Registry::lookup
        // returns None for absent entries.
        assert!(
            registry.lookup(FailingCap::NAMESPACE).is_none(),
            "sink at {} should be removed after failed init",
            FailingCap::NAMESPACE,
        );
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
                &mut self,
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

        // Issue 629 / Phase A: chassis-level `actor::<X>()` retired.
        // The cap is owned by its dispatcher thread; the test verifies
        // the cap is alive via the mail dispatch round-trip below.

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
                &mut self,
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
                &mut self,
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

    /// Issue 607 Phase 3b verify: a singleton parent's handler calls
    /// `ctx.spawn_child::<Child>(...)` to launch an instanced actor.
    /// Asserts the child's `MailboxId` lands in the chassis's
    /// `ActorRegistry` as a Live entry, and that the parent-pre-loaded
    /// `after_init` mail dispatches as the child's first envelope.
    #[test]
    fn ctx_spawn_child_routes_through_handler() {
        use crate::registry::MailboxEntry;
        use crate::spawn::Subname;
        use aether_actor::{HandlesKind, Instanced};
        use aether_data::{Kind, KindId as DataKindId, ReplyTo as DataReplyTo};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Hatch {
            tag: u32,
        }
        impl Kind for Hatch {
            const NAME: &'static str = "test.spawn_child.hatch";
            const ID: DataKindId = DataKindId(0xC0C1_C2C3_C4C5_C6C7);
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

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Ping {
            tag: u32,
        }
        impl Kind for Ping {
            const NAME: &'static str = "test.spawn_child.ping";
            const ID: DataKindId = DataKindId(0xD0D1_D2D3_D4D5_D6D7);
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

        struct ChildCap {
            received: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for ChildCap {
            const NAMESPACE: &'static str = "test.spawn_child.child";
        }
        impl Instanced for ChildCap {}
        impl HandlesKind<Ping> for ChildCap {}
        impl crate::native_actor::NativeActor for ChildCap {
            type Config = Arc<AtomicU32>;
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl crate::native_actor::NativeDispatch for ChildCap {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Ping::ID.0 {
                    let _ = Ping::decode_from_bytes(payload)?;
                    self.received.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        struct ParentCap {
            spawn_count: Arc<AtomicU32>,
            child_received: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for ParentCap {
            const NAMESPACE: &'static str = "test.spawn_child.parent";
        }
        impl aether_actor::Singleton for ParentCap {}
        impl HandlesKind<Hatch> for ParentCap {}
        impl crate::native_actor::NativeActor for ParentCap {
            type Config = (Arc<AtomicU32>, Arc<AtomicU32>);
            fn init(
                (spawn_count, child_received): Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self {
                    spawn_count,
                    child_received,
                })
            }
        }
        impl crate::native_actor::NativeDispatch for ParentCap {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Hatch::ID.0 {
                    let _ = Hatch::decode_from_bytes(payload)?;
                    let _id = ctx
                        .spawn_child::<ChildCap>(Subname::Counter, Arc::clone(&self.child_received))
                        .after_init(Ping { tag: 42 })
                        .finish()
                        .expect("spawn_child must succeed");
                    self.spawn_count.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let spawn_count = Arc::new(AtomicU32::new(0));
        let child_received = Arc::new(AtomicU32::new(0));

        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<ParentCap>((Arc::clone(&spawn_count), Arc::clone(&child_received)))
            .build_passive()
            .expect("ParentCap boots");

        // Push Hatch at the parent's mailbox; the parent's handler
        // calls `ctx.spawn_child::<ChildCap>` which in turn pushes a
        // Ping at the new child via the after_init bootstrap.
        let parent_id = registry
            .lookup(<ParentCap as aether_actor::Actor>::NAMESPACE)
            .expect("ParentCap claimed");
        let MailboxEntry::Sink(handler) = registry.entry(parent_id).expect("sink") else {
            panic!("expected sink entry");
        };
        let bytes = (Hatch { tag: 1 }).encode_into_bytes();
        handler(
            <Hatch as Kind>::ID,
            Hatch::NAME,
            None,
            DataReplyTo::NONE,
            &bytes,
            1,
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while child_received.load(AtomicOrdering::SeqCst) < 1
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            spawn_count.load(AtomicOrdering::SeqCst),
            1,
            "parent's handler ran spawn_child exactly once"
        );
        assert_eq!(
            child_received.load(AtomicOrdering::SeqCst),
            1,
            "spawn_child's after_init mail dispatched as the child's first envelope"
        );

        // Child is Live in the chassis's actor registry.
        let child_id =
            crate::mail::MailboxId(aether_data::mailbox_id_from_name("test.spawn_child.child:0").0);
        assert!(
            chassis.actor_registry().is_live(child_id),
            "spawned child should be Live in the actor registry under the deterministic full-name id"
        );

        drop(chassis);
    }

    /// Issue 607 Phase 4a verify: `ctx.shutdown()` from inside an
    /// instanced actor's handler triggers the drain → on_close → exit
    /// path, flips the actor_registry slot to `Dead`, and inserts the
    /// id into `tombstones`. A reused subname after retirement returns
    /// `SpawnError::SubnameRetired`.
    #[test]
    fn ctx_shutdown_marks_dead_runs_on_close_tombstones_id() {
        use crate::registry::MailboxEntry;
        use crate::spawn::{SpawnError, Subname};
        use aether_actor::{HandlesKind, Instanced};
        use aether_data::{Kind, KindId as DataKindId};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Quit {
            tag: u32,
        }
        impl Kind for Quit {
            const NAME: &'static str = "test.shutdown.quit";
            const ID: DataKindId = DataKindId(0xE0E1_E2E3_E4E5_E6E7);
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

        struct Closer {
            close_observed: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for Closer {
            const NAMESPACE: &'static str = "test.shutdown.closer";
        }
        impl Instanced for Closer {}
        impl HandlesKind<Quit> for Closer {}
        impl crate::native_actor::NativeActor for Closer {
            type Config = Arc<AtomicU32>;
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self {
                    close_observed: config,
                })
            }
            fn on_close(&mut self, _ctx: &mut crate::native_actor::NativeCtx<'_>) {
                self.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl crate::native_actor::NativeDispatch for Closer {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Quit::ID.0 {
                    let _ = Quit::decode_from_bytes(payload)?;
                    ctx.shutdown();
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let close_observed = Arc::new(AtomicU32::new(0));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let id = chassis
            .spawn_actor::<Closer>(Subname::Counter, Arc::clone(&close_observed))
            .finish()
            .expect("spawn instanced actor");

        // Push a Quit envelope at the spawned mailbox via the
        // registered sink handler. The handler's `ctx.shutdown()`
        // flips the dispatcher's flag; after the handler returns the
        // trampoline drains, runs `on_close`, marks Dead, tombstones.
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("sink registered") else {
            panic!("expected sink entry for instanced actor");
        };
        let bytes = (Quit { tag: 1 }).encode_into_bytes();
        handler(
            <Quit as Kind>::ID,
            Quit::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &bytes,
            1,
        );

        // Wait for on_close to run + the registry slot to flip Dead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while close_observed.load(AtomicOrdering::SeqCst) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            close_observed.load(AtomicOrdering::SeqCst),
            1,
            "on_close fired exactly once after the dispatcher drained"
        );
        // Spin until the slot transitions Dead — the dispatcher
        // thread runs `mark_dead` after `on_close`, so there's a
        // small window between the close-observed bump above and the
        // registry update.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().is_live(id) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !chassis.actor_registry().is_live(id),
            "registry slot should transition Live → Dead after on_close runs"
        );
        assert!(
            chassis.actor_registry().is_tombstoned(id),
            "tombstone insertion forbids reuse of the retired full name"
        );

        // Spawning again under the same `Subname::Counter` would
        // increment the per-Spawner counter (so it'd target a fresh
        // id, not collide); reuse the same `Named` subname to land
        // back at the tombstoned id.
        let err = chassis
            .spawn_actor::<Closer>(Subname::Named("0"), Arc::clone(&close_observed))
            .finish()
            .expect_err("retired subname must reject");
        assert!(
            matches!(err, SpawnError::SubnameRetired { .. }),
            "expected SubnameRetired, got {err:?}"
        );

        drop(chassis);
    }

    /// Issue 607 Phase 4b verify: a `ctx.monitor(target)` registration
    /// fires exactly one `MonitorNotice` at the watcher when the
    /// target self-shuts. Two-actor scenario: Watcher (instanced)
    /// holds a `MonitorHandle` against Target (instanced) and counts
    /// the notices it receives; Target self-shuts on `Quit`. After
    /// the close fan-out we assert (1) the watcher saw the notice
    /// once with the right target id, (2) the target's slot is Dead +
    /// tombstoned, and (3) the registry's forward index drained.
    #[test]
    fn ctx_monitor_fires_notice_at_target_close() {
        use crate::registry::MailboxEntry;
        use crate::spawn::Subname;
        use aether_actor::{HandlesKind, Instanced};
        use aether_data::{Kind, KindId as DataKindId};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering};

        // Self-shutdown trigger for the target.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Quit {
            tag: u32,
        }
        impl Kind for Quit {
            const NAME: &'static str = "test.monitor.quit";
            const ID: DataKindId = DataKindId(0xC0DE_C0DE_4B4B_4B4B);
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

        // Tells the watcher which target to monitor. The watcher's
        // handler reads `target_id` and calls `ctx.monitor`.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct WatchOrder {
            target_id: u64,
        }
        impl Kind for WatchOrder {
            const NAME: &'static str = "test.monitor.watch_order";
            const ID: DataKindId = DataKindId(0x4B4B_C0DE_C0DE_C0DE);
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

        // Target — handles Quit by self-shutting.
        struct Target;
        impl aether_actor::Actor for Target {
            const NAMESPACE: &'static str = "test.monitor.target";
        }
        impl Instanced for Target {}
        impl HandlesKind<Quit> for Target {}
        impl crate::native_actor::NativeActor for Target {
            type Config = ();
            fn init(
                _: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl crate::native_actor::NativeDispatch for Target {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Quit::ID.0 {
                    let _ = Quit::decode_from_bytes(payload)?;
                    ctx.shutdown();
                    return Some(());
                }
                None
            }
        }

        // Watcher — handles WatchOrder by registering a monitor;
        // handles MonitorNotice by recording the target id and
        // bumping a counter.
        struct Watcher {
            notice_count: Arc<AtomicU32>,
            last_target: Arc<AtomicU64>,
            handle: Mutex<Option<crate::native_actor::MonitorHandle>>,
        }
        impl aether_actor::Actor for Watcher {
            const NAMESPACE: &'static str = "test.monitor.watcher";
        }
        impl Instanced for Watcher {}
        impl HandlesKind<WatchOrder> for Watcher {}
        impl HandlesKind<aether_kinds::MonitorNotice> for Watcher {}
        impl crate::native_actor::NativeActor for Watcher {
            type Config = (Arc<AtomicU32>, Arc<AtomicU64>);
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self {
                    notice_count: config.0,
                    last_target: config.1,
                    handle: Mutex::new(None),
                })
            }
        }
        impl crate::native_actor::NativeDispatch for Watcher {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == WatchOrder::ID.0 {
                    let order = WatchOrder::decode_from_bytes(payload)?;
                    let target = aether_data::MailboxId(order.target_id);
                    let h = ctx
                        .monitor(target)
                        .expect("target must be Live at order time");
                    *self.handle.lock().unwrap() = Some(h);
                    return Some(());
                }
                if kind.0 == <aether_kinds::MonitorNotice as aether_data::Kind>::ID.0 {
                    let notice =
                        <aether_kinds::MonitorNotice as aether_data::Kind>::decode_from_bytes(
                            payload,
                        )?;
                    self.last_target
                        .store(notice.target.0, AtomicOrdering::SeqCst);
                    self.notice_count.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        // Spawn target first so the watcher can register against a
        // Live id.
        let target_id = chassis
            .spawn_actor::<Target>(Subname::Counter, ())
            .finish()
            .expect("spawn target");

        let notice_count = Arc::new(AtomicU32::new(0));
        let last_target = Arc::new(AtomicU64::new(0));
        let watcher_id = chassis
            .spawn_actor::<Watcher>(
                Subname::Counter,
                (Arc::clone(&notice_count), Arc::clone(&last_target)),
            )
            .finish()
            .expect("spawn watcher");

        // Drive the watcher to register the monitor by pushing a
        // WatchOrder through its sink handler. After this returns
        // the watcher's handle is stored in `self.handle`.
        let MailboxEntry::Sink(watcher_handler) =
            registry.entry(watcher_id).expect("watcher sink registered")
        else {
            panic!("expected sink entry for watcher");
        };
        let order = WatchOrder {
            target_id: target_id.0,
        };
        watcher_handler(
            <WatchOrder as Kind>::ID,
            WatchOrder::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &order.encode_into_bytes(),
            1,
        );

        // Wait until the registry sees the monitor entry.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().monitor_count(target_id) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            chassis.actor_registry().monitor_count(target_id),
            1,
            "watcher's monitor should be registered against target",
        );
        assert_eq!(
            chassis.actor_registry().monitoring_count(watcher_id),
            1,
            "watcher should appear in the reverse index",
        );

        // Fire Quit at the target — its handler self-shuts; the
        // dispatcher's close path runs `close_actor`, which fans out
        // a MonitorNotice mail to watcher_id.
        let MailboxEntry::Sink(target_handler) =
            registry.entry(target_id).expect("target sink registered")
        else {
            panic!("expected sink entry for target");
        };
        target_handler(
            <Quit as Kind>::ID,
            Quit::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        );

        // Wait for the notice to land at the watcher.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while notice_count.load(AtomicOrdering::SeqCst) == 0 && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            notice_count.load(AtomicOrdering::SeqCst),
            1,
            "watcher should have received exactly one MonitorNotice",
        );
        assert_eq!(
            last_target.load(AtomicOrdering::SeqCst),
            target_id.0,
            "MonitorNotice.target should match the closed actor's id",
        );

        // Wait for target slot to flip Dead (the close path runs
        // close_actor → mark_dead after fan-out).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().is_live(target_id) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !chassis.actor_registry().is_live(target_id),
            "target slot should transition Live → Dead after close fan-out",
        );
        assert!(
            chassis.actor_registry().is_tombstoned(target_id),
            "target id should be tombstoned",
        );
        // Forward index for target was drained.
        assert_eq!(
            chassis.actor_registry().monitor_count(target_id),
            0,
            "monitors_of[target] must drain after fan-out",
        );

        drop(chassis);
    }

    /// Issue 607 Phase 4b verify: when the *watcher* dies first, the
    /// reverse-index walk prunes the watcher's entry from each
    /// monitored target's `monitors_of`. No `MonitorNotice` fires (the
    /// watcher is the one closing; targets are still alive).
    #[test]
    fn watcher_close_prunes_targets_forward_index() {
        use crate::registry::MailboxEntry;
        use crate::spawn::Subname;
        use aether_actor::{HandlesKind, Instanced};
        use aether_data::{Kind, KindId as DataKindId};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        // Re-use Quit + WatchOrder shape inline (test isolation).
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Quit {
            tag: u32,
        }
        impl Kind for Quit {
            const NAME: &'static str = "test.monitor.quit2";
            const ID: DataKindId = DataKindId(0xCAFE_BABE_DEAD_BEEF);
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
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct WatchOrder {
            target_id: u64,
        }
        impl Kind for WatchOrder {
            const NAME: &'static str = "test.monitor.watch_order2";
            const ID: DataKindId = DataKindId(0xBEEF_DEAD_BABE_CAFE);
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

        struct Target;
        impl aether_actor::Actor for Target {
            const NAMESPACE: &'static str = "test.monitor.target2";
        }
        impl Instanced for Target {}
        impl crate::native_actor::NativeActor for Target {
            type Config = ();
            fn init(
                _: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl crate::native_actor::NativeDispatch for Target {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        struct Watcher {
            handle: Mutex<Option<crate::native_actor::MonitorHandle>>,
            close_observed: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for Watcher {
            const NAMESPACE: &'static str = "test.monitor.watcher2";
        }
        impl Instanced for Watcher {}
        impl HandlesKind<WatchOrder> for Watcher {}
        impl HandlesKind<Quit> for Watcher {}
        impl crate::native_actor::NativeActor for Watcher {
            type Config = Arc<AtomicU32>;
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self {
                    handle: Mutex::new(None),
                    close_observed: config,
                })
            }
            fn on_close(&mut self, _ctx: &mut crate::native_actor::NativeCtx<'_>) {
                self.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl crate::native_actor::NativeDispatch for Watcher {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == WatchOrder::ID.0 {
                    let order = WatchOrder::decode_from_bytes(payload)?;
                    let target = aether_data::MailboxId(order.target_id);
                    let h = ctx.monitor(target).expect("target Live");
                    *self.handle.lock().unwrap() = Some(h);
                    return Some(());
                }
                if kind.0 == Quit::ID.0 {
                    let _ = Quit::decode_from_bytes(payload)?;
                    ctx.shutdown();
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let target_id = chassis
            .spawn_actor::<Target>(Subname::Counter, ())
            .finish()
            .expect("spawn target");
        let close_observed = Arc::new(AtomicU32::new(0));
        let watcher_id = chassis
            .spawn_actor::<Watcher>(Subname::Counter, Arc::clone(&close_observed))
            .finish()
            .expect("spawn watcher");

        // Watcher registers monitor against target.
        let MailboxEntry::Sink(watcher_handler) =
            registry.entry(watcher_id).expect("watcher sink registered")
        else {
            panic!("expected sink entry for watcher");
        };
        let order = WatchOrder {
            target_id: target_id.0,
        };
        watcher_handler(
            <WatchOrder as Kind>::ID,
            WatchOrder::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &order.encode_into_bytes(),
            1,
        );

        // Wait for register to land.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().monitor_count(target_id) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(chassis.actor_registry().monitor_count(target_id), 1);

        // Quit watcher — its close path walks `monitoring[watcher]` and
        // prunes watcher from `monitors_of[target]`.
        watcher_handler(
            <Quit as Kind>::ID,
            Quit::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while close_observed.load(AtomicOrdering::SeqCst) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            close_observed.load(AtomicOrdering::SeqCst),
            1,
            "watcher's on_close fired exactly once",
        );

        // Watcher slot tombstones; target slot still Live; target's
        // forward index drained of the dead watcher.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().is_live(watcher_id) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            chassis.actor_registry().is_tombstoned(watcher_id),
            "watcher tombstoned",
        );
        assert!(
            chassis.actor_registry().is_live(target_id),
            "target should still be Live (watcher closed, not target)",
        );
        assert_eq!(
            chassis.actor_registry().monitor_count(target_id),
            0,
            "target's monitors_of should drop the dead watcher",
        );

        drop(chassis);
    }

    /// Issue 607 Phase 5 verify: `resolve_actor` and `resolve_actors`
    /// against a multi-instance fixture. Spawns three instanced actors
    /// under one type, asserts:
    ///   - `resolve_actor::<A>("a")` finds the named instance.
    ///   - `resolve_actor::<A>("missing")` returns `None`.
    ///   - `resolve_actors::<A>()` enumerates all three (subname-keyed).
    ///   - After one closes, the iterator drops to two and the closed
    ///     name returns `None` from `resolve_actor`.
    #[test]
    fn resolve_actor_finds_named_instance_resolve_actors_enumerates() {
        use crate::registry::MailboxEntry;
        use crate::spawn::Subname;
        use aether_actor::{HandlesKind, Instanced};
        use aether_data::{Kind, KindId as DataKindId};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Quit {
            tag: u32,
        }
        impl Kind for Quit {
            const NAME: &'static str = "test.resolve.quit";
            const ID: DataKindId = DataKindId(0xF00D_F00D_F00D_F00D);
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

        // The `tag` field is set at init from the per-instance config
        // and would be read by handler code; Phase A's resolve_actor
        // returns MailboxId rather than `Arc<Member>` so the tag is no
        // longer externally observable. Kept as an init payload so the
        // spawn path covers the full Config-threaded shape.
        #[allow(dead_code)]
        struct Member {
            tag: u32,
        }
        impl aether_actor::Actor for Member {
            const NAMESPACE: &'static str = "test.resolve.member";
        }
        impl Instanced for Member {}
        impl HandlesKind<Quit> for Member {}
        impl crate::native_actor::NativeActor for Member {
            type Config = u32;
            fn init(
                tag: u32,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self { tag })
            }
        }
        impl crate::native_actor::NativeDispatch for Member {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Quit::ID.0 {
                    let _ = Quit::decode_from_bytes(payload)?;
                    ctx.shutdown();
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let id_a = chassis
            .spawn_actor::<Member>(Subname::Named("a"), 1)
            .finish()
            .expect("spawn a");
        let _id_b = chassis
            .spawn_actor::<Member>(Subname::Named("b"), 2)
            .finish()
            .expect("spawn b");
        let id_c = chassis
            .spawn_actor::<Member>(Subname::Named("c"), 3)
            .finish()
            .expect("spawn c");

        // Issue 629 / Phase A: resolve_actor returns the address
        // (`MailboxId`), not `Arc<A>`. Verify the address resolves and
        // matches the spawn-time id.
        let a_id = chassis.resolve_actor::<Member>("a").expect("a is live");
        assert_eq!(a_id, id_a, "resolve_actor returns the matching MailboxId");

        // Missing subname → None.
        assert!(
            chassis.resolve_actor::<Member>("missing").is_none(),
            "unknown subname should return None",
        );

        // resolve_actors enumerates all three. Order is registry-defined
        // (HashMap iteration), so collect into a sorted subname vec for
        // assertions. The Member's per-instance tag is dispatcher-thread
        // owned (Phase A) and not externally observable here; the
        // subname uniquely identifies the instance.
        let mut all: Vec<String> = chassis
            .resolve_actors::<Member>()
            .into_iter()
            .map(|(name, _id)| name)
            .collect();
        all.sort();
        assert_eq!(
            all,
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            "resolve_actors should enumerate every Live instance subname",
        );

        // Close c — Quit it through the sink handler. After close,
        // resolve_actors drops to two and resolve_actor::<Member>("c")
        // returns None.
        let MailboxEntry::Sink(handler) = registry.entry(id_c).expect("c sink registered") else {
            panic!("expected sink entry for c");
        };
        handler(
            <Quit as Kind>::ID,
            Quit::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        );

        // Wait for c's slot to flip Dead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().is_live(id_c) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(
            chassis.resolve_actor::<Member>("c").is_none(),
            "closed instance should disappear from resolve_actor",
        );
        let mut after: Vec<String> = chassis
            .resolve_actors::<Member>()
            .into_iter()
            .map(|(name, _id)| name)
            .collect();
        after.sort();
        assert_eq!(
            after,
            vec!["a".to_owned(), "b".to_owned()],
            "resolve_actors should drop the closed instance",
        );

        // Counter for unused warning. (`_id_a` / `_id_b` retain their
        // names elsewhere; this guard keeps the compiler happy.)
        let _ = AtomicU32::new(0).load(AtomicOrdering::SeqCst);

        drop(chassis);
    }

    /// Issue 607 Phase 5: type mismatch through `resolve_actor` returns
    /// `None` rather than a downcast that succeeds against the wrong
    /// type. Two instanced types live under different namespaces; a
    /// lookup with one type at the other's id mismatches and returns
    /// None.
    #[test]
    fn resolve_actor_returns_none_on_type_mismatch() {
        use crate::spawn::Subname;
        use aether_actor::Instanced;

        struct Foo;
        impl aether_actor::Actor for Foo {
            const NAMESPACE: &'static str = "test.resolve_mismatch.foo";
        }
        impl Instanced for Foo {}
        impl crate::native_actor::NativeActor for Foo {
            type Config = ();
            fn init(
                _: (),
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl crate::native_actor::NativeDispatch for Foo {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        struct Bar;
        impl aether_actor::Actor for Bar {
            const NAMESPACE: &'static str = "test.resolve_mismatch.bar";
        }
        impl Instanced for Bar {}
        impl crate::native_actor::NativeActor for Bar {
            type Config = ();
            fn init(
                _: (),
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl crate::native_actor::NativeDispatch for Bar {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let _ = chassis
            .spawn_actor::<Foo>(Subname::Named("only"), ())
            .finish()
            .expect("spawn foo");

        // Resolving with the same subname but the wrong type returns
        // None — the namespaces differ so the hashed full names differ
        // and Bar's "only" is just not present. (The TypeId guard
        // would catch a hash collision.)
        assert!(chassis.resolve_actor::<Bar>("only").is_none());

        // resolve_actors::<Bar>() is empty because no Bar instances
        // were spawned, even though a Foo with the same subname exists.
        assert_eq!(chassis.resolve_actors::<Bar>().len(), 0);
        assert_eq!(chassis.resolve_actors::<Foo>().len(), 1);

        drop(chassis);
    }

    /// Issue 607 Phase 5.5 verify: an instanced parent's handler calls
    /// `ctx.spawn_child::<Grandchild>(...)` to launch an instanced
    /// grandchild. Phase 3b shipped `Arc<Spawner>` threading through
    /// every spawned actor's transport precisely so this works; this
    /// test is the first end-to-end coverage of the instanced→instanced
    /// path. Phase 6b (TcpListenerActor → TcpSessionActor) structurally
    /// depends on this — listeners spawning sessions IS the recursive
    /// case.
    ///
    /// Asserts:
    ///   1. Grandchild's `MailboxId` is `Live` in the registry.
    ///   2. `chassis.resolve_actor::<Grandchild>(name)` resolves it.
    ///   3. Grandchild's `after_init` mail dispatches as its first
    ///      envelope (received counter bumps to 1).
    ///   4. Closing the parent does NOT cascade-close the grandchild —
    ///      no parent-child shutdown coupling is wired by default;
    ///      that's monitor-driven, opt-in.
    #[test]
    fn instanced_can_spawn_grandchild() {
        use crate::registry::MailboxEntry;
        use crate::spawn::Subname;
        use aether_actor::{HandlesKind, Instanced};
        use aether_data::{Kind, KindId as DataKindId};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        // Trigger to make the parent spawn its grandchild.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Hatch {
            tag: u32,
        }
        impl Kind for Hatch {
            const NAME: &'static str = "test.recursive.hatch";
            const ID: DataKindId = DataKindId(0xA00A_A00A_A00A_A00A);
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

        // Pre-loaded onto the grandchild via after_init.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Ping {
            tag: u32,
        }
        impl Kind for Ping {
            const NAME: &'static str = "test.recursive.ping";
            const ID: DataKindId = DataKindId(0xB00B_B00B_B00B_B00B);
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

        // Self-shutdown trigger for the parent.
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Quit {
            tag: u32,
        }
        impl Kind for Quit {
            const NAME: &'static str = "test.recursive.quit";
            const ID: DataKindId = DataKindId(0xC00C_C00C_C00C_C00C);
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

        struct Grandchild {
            received: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for Grandchild {
            const NAMESPACE: &'static str = "test.recursive.grandchild";
        }
        impl Instanced for Grandchild {}
        impl HandlesKind<Ping> for Grandchild {}
        impl crate::native_actor::NativeActor for Grandchild {
            type Config = Arc<AtomicU32>;
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl crate::native_actor::NativeDispatch for Grandchild {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Ping::ID.0 {
                    let _ = Ping::decode_from_bytes(payload)?;
                    self.received.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        struct Parent {
            grandchild_received: Arc<AtomicU32>,
        }
        impl aether_actor::Actor for Parent {
            const NAMESPACE: &'static str = "test.recursive.parent";
        }
        impl Instanced for Parent {}
        impl HandlesKind<Hatch> for Parent {}
        impl HandlesKind<Quit> for Parent {}
        impl crate::native_actor::NativeActor for Parent {
            type Config = Arc<AtomicU32>;
            fn init(
                config: Self::Config,
                _ctx: &mut crate::native_actor::NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self {
                    grandchild_received: config,
                })
            }
        }
        impl crate::native_actor::NativeDispatch for Parent {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::native_actor::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == Hatch::ID.0 {
                    let _ = Hatch::decode_from_bytes(payload)?;
                    // Recursive spawn: instanced parent → instanced
                    // grandchild. Pre-load a Ping so the grandchild's
                    // first envelope dispatches without an external
                    // mail step.
                    let _id = ctx
                        .spawn_child::<Grandchild>(
                            Subname::Named("only"),
                            Arc::clone(&self.grandchild_received),
                        )
                        .after_init(Ping { tag: 0xCAFE })
                        .finish()
                        .expect("recursive spawn must succeed");
                    return Some(());
                }
                if kind.0 == Quit::ID.0 {
                    let _ = Quit::decode_from_bytes(payload)?;
                    ctx.shutdown();
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let grandchild_received = Arc::new(AtomicU32::new(0));
        let parent_id = chassis
            .spawn_actor::<Parent>(Subname::Named("p1"), Arc::clone(&grandchild_received))
            .finish()
            .expect("spawn parent");

        // Trigger parent → grandchild spawn.
        let MailboxEntry::Sink(parent_handler) =
            registry.entry(parent_id).expect("parent sink registered")
        else {
            panic!("expected sink entry for parent");
        };
        parent_handler(
            <Hatch as Kind>::ID,
            Hatch::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &(Hatch { tag: 1 }).encode_into_bytes(),
            1,
        );

        // Wait for the grandchild's after_init Ping to dispatch (proves
        // the recursive spawn happened AND the after_init plumbing
        // works through it).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while grandchild_received.load(AtomicOrdering::SeqCst) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            grandchild_received.load(AtomicOrdering::SeqCst),
            1,
            "grandchild's after_init Ping should dispatch as its first envelope",
        );

        // Grandchild is Live in the registry under the deterministic
        // full-name id (NAMESPACE = "test.recursive.grandchild",
        // subname = "only").
        let grandchild_id = crate::mail::MailboxId(
            aether_data::mailbox_id_from_name("test.recursive.grandchild:only").0,
        );
        assert!(
            chassis.actor_registry().is_live(grandchild_id),
            "grandchild should be Live in the registry under the deterministic full-name id",
        );

        // Issue 629 / Phase A: resolve_actor returns the address.
        // Verify it resolves and matches the registry id.
        let resolved = chassis
            .resolve_actor::<Grandchild>("only")
            .expect("resolve_actor must find the grandchild");
        assert_eq!(
            resolved, grandchild_id,
            "resolve_actor returns the matching MailboxId",
        );
        // The grandchild is alive (verifies the dispatcher's Arc<AtomicU32>
        // is the same one passed in via config — the test's `received`
        // counter sees handler dispatches against the live instance).
        let _ = &grandchild_received;

        // Closing the parent does NOT cascade-close the grandchild.
        // Parent-child shutdown coupling is opt-in via monitor; without
        // it, the grandchild keeps running.
        parent_handler(
            <Quit as Kind>::ID,
            Quit::NAME,
            None,
            aether_data::ReplyTo::NONE,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        );

        // Wait for parent slot to flip Dead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().is_live(parent_id) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            chassis.actor_registry().is_tombstoned(parent_id),
            "parent should have tombstoned",
        );
        // Grandchild survives — no cascade.
        assert!(
            chassis.actor_registry().is_live(grandchild_id),
            "grandchild should outlive parent (no automatic cascade-close)",
        );
        assert!(
            chassis.resolve_actor::<Grandchild>("only").is_some(),
            "grandchild remains resolvable after parent's death",
        );

        drop(chassis);
    }
}
