//! ADR-0071 Phase 2A: driver-capability traits + chassis builder
//! type-state.
//!
//! Sibling to ADR-0070's [`Capability`] family (post-issue-525-Phase-2:
//! one struct per cap, `Drop` replaces `RunningCapability::shutdown`).
//! A chassis composes passive capabilities (dispatcher-thread sinks
//! per ADR-0070) plus exactly one [`DriverCapability`] that owns the
//! chassis main thread. The type-state [`Builder`] enforces "exactly
//! one driver" structurally; embedders that drive manually (`TestBench`,
//! future embedded harnesses) build a [`PassiveChassis`] via the
//! no-driver path.
//!
//! # Phase 2A scope
//!
//! - Trait family + builder + ctx wiring.
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

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::{ExportedHandles, NativeActor, NativeDispatch, NativeInitCtx};
use crate::chassis::Chassis;
use crate::chassis::ctx::{ChassisCtx, FallbackRouter, MailboxClaim};
use crate::chassis::error::BootError;
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::scheduler::{Pool, PoolConfig, PoolHandle};
use aether_actor::Actor;
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
            Self::Other(e) => write!(f, "driver run failed: {e}"),
        }
    }
}

impl StdError for RunError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Other(e) => Some(&**e),
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
/// embedder-driven chassis kinds). The [`Chassis`]
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

    #[must_use]
    pub fn mail_send_handle(&self) -> Arc<Mailer> {
        self.inner.mail_send_handle()
    }

    pub fn claim_fallback_router(&mut self, handler: FallbackRouter) -> Result<(), BootError> {
        self.inner.claim_fallback_router(handler)
    }

    /// Snapshot of every frame-bound mailbox's pending counter
    /// collected during passive boot. Drivers stash this clone and
    /// hand it to [`crate::chassis::frame_loop::drain_frame_bound_or_abort`]
    /// each frame so render submit waits for inbound mail to drain
    /// alongside component drains (ADR-0074 §Decision 5).
    ///
    /// Returns an empty vec on chassis with no frame-bound
    /// capabilities (today: the headless chassis without render); in
    /// that case the per-frame call is a fast no-op.
    #[must_use]
    pub fn frame_bound_pending(&self) -> Vec<(MailboxId, Arc<AtomicU64>)> {
        self.inner.frame_bound_pending().to_vec()
    }

    /// Issue 629 / Phase A: retrieve a clone of a cap-published handle
    /// bundle of type `H`. `None` if no cap published one (typically
    /// because the cap that owns the handle wasn't booted on this
    /// chassis). Drivers use this to pull `RenderHandles` and similar
    /// driver-facing sub-handle bundles without reaching for the cap
    /// itself.
    #[must_use]
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

/// Builder state: no driver supplied yet. Accepts `.with_actor(_)`
/// / `.with_fallback_router(_)` and `.driver(_)` (which transitions
/// to [`HasDriver`]); also supports `.build_passive()` for the
/// embedder-driven path.
pub struct NoDriver;

/// Builder state: driver supplied. Accepts `.with_actor(_)` (passives
/// declared after the driver still boot before the driver per the
/// builder's invariant) and `.build()`.
pub struct HasDriver;

impl sealed::Sealed for NoDriver {}
impl sealed::Sealed for HasDriver {}
impl BuilderState for NoDriver {}
impl BuilderState for HasDriver {}

type DriverBoot = Box<dyn FnOnce(&mut DriverCtx<'_>) -> Result<Box<dyn DriverRunning>, BootError>>;

/// Issue 697: chassis boot is multi-pass. Every registered passive
/// walks `claim → init → wire → spawn` synchronized across all
/// passives — the chassis builder calls phase N on every passive
/// before any passive enters phase N+1. The boot ordering means:
///
/// - At `init` time, every peer mailbox is already claimed (claim
///   pass completed), so init's `Resolver::resolve_mailbox` reaches
///   every peer.
/// - At `wire` time, every actor has an `init`-built instance, so
///   wire-time mail to a peer queues in that peer's inbox; the
///   recipient's dispatcher hasn't started yet.
/// - The `spawn` pass starts dispatchers; queued wire mail processes
///   naturally as each comes up.
///
/// No drain barrier between spawn and steady state — issue 697 §"Why
/// no barrier" rejects waiting for inboxes to drain (breaks for
/// actors with async mail sources). Frame-bound actors that can't
/// tolerate a one-frame race against a peer's wire-emitted mail keep
/// load-bearing state in `init`, not `wire`.
///
/// Failure mode: any phase returning `Err` triggers
/// [`Self::cleanup_after_failure`] in reverse boot order on every
/// previously-advanced passive, then the error propagates. Already-
/// spawned dispatchers (only on a spawn-pass failure for a later
/// passive) shut down via the [`DynShutdown`] handles the spawn pass
/// produced.
trait PassiveBoot: Send {
    /// Phase 1 — claim namespace + mailbox; build per-cap transport
    /// + binding; stash claim resources for later phases.
    fn claim(&mut self, ctx: &mut ChassisCtx<'_>) -> Result<(), BootError>;

    /// Phase 2 — construct the actor instance via `A::init`. Default
    /// no-op for non-actor passives (e.g., the fallback router).
    fn init(
        &mut self,
        ctx: &mut ChassisCtx<'_>,
        handles: &mut ExportedHandles,
    ) -> Result<(), BootError> {
        let _ = ctx;
        let _ = handles;
        Ok(())
    }

    /// Phase 3 — post-init mail-allowed lifecycle hook
    /// ([`NativeActor::wire`], ADR-0079 amended). Default no-op.
    fn wire(&mut self) -> Result<(), BootError> {
        Ok(())
    }

    /// Phase 4 — spawn dispatcher; produce a shutdown handle.
    /// Consumes the impl.
    fn spawn(self: Box<Self>, ctx: &mut ChassisCtx<'_>) -> Result<Box<dyn DynShutdown>, BootError>;

    /// Roll back any acquired resources after a phase returned `Err`
    /// on this impl, or after a sibling passive's later phase failed
    /// while this impl had already advanced. Idempotent across the
    /// pre-spawn phases. Consumes the impl.
    fn cleanup_after_failure(self: Box<Self>, ctx: &mut ChassisCtx<'_>);
}

/// Single-phase passive: the fallback router lives entirely in the
/// claim step (it stashes its handler into `ChassisCtx::fallback`).
/// `init` / `wire` are no-ops; `spawn` returns the no-op
/// [`FallbackShutdown`].
struct FallbackRouterBoot {
    handler: Option<FallbackRouter>,
}

impl FallbackRouterBoot {
    fn new(handler: FallbackRouter) -> Self {
        Self {
            handler: Some(handler),
        }
    }
}

impl PassiveBoot for FallbackRouterBoot {
    fn claim(&mut self, ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        let handler = self
            .handler
            .take()
            .expect("FallbackRouterBoot::claim called twice");
        ctx.claim_fallback_router(handler)
    }

    fn spawn(
        self: Box<Self>,
        _ctx: &mut ChassisCtx<'_>,
    ) -> Result<Box<dyn DynShutdown>, BootError> {
        Ok(Box::new(FallbackShutdown))
    }

    fn cleanup_after_failure(self: Box<Self>, _ctx: &mut ChassisCtx<'_>) {
        // The router, once claimed, sits in `ctx.fallback` (an
        // `&mut Option<FallbackRouter>` borrowed from `BootedPassives`).
        // Boot failure unwinds the entire `BootedPassives`, so the
        // slot drops with it. Nothing to do here.
    }
}

/// Resources stashed during the [`PassiveBoot::claim`] pass and
/// threaded forward through `init` / `wire` / `spawn`. Composed into
/// [`BootState`]'s post-claim variants so the type system tracks
/// "what's been allocated" precisely.
struct ClaimResources {
    mailbox_id: MailboxId,
    transport: Arc<NativeBinding>,
    mailbox_sender: crate::chassis::ctx::MailboxSender,
    pending: Option<Arc<AtomicU64>>,
    wake_slot: Arc<crate::chassis::ctx::MailboxWakeSlot>,
    slots: Box<aether_actor::local::ActorSlots>,
}

/// Phase state of a [`NativeActorBoot`] — variants carry exactly the
/// resources that phase has acquired. Phase methods transition states
/// via `mem::replace(&mut self.state, Transitioning)` plus a final
/// state assignment, so each transition is atomic w.r.t. partial
/// moves.
enum BootState<A: NativeActor + NativeDispatch> {
    /// Pre-claim — only the cap config is held.
    Pending { config: A::Config },
    /// Post-claim, pre-init — mailbox + transport + slots claimed,
    /// config still pending consumption by `init`.
    Claimed {
        resources: ClaimResources,
        config: A::Config,
    },
    /// Post-init, pre-wire — actor instance constructed.
    Initialized {
        resources: ClaimResources,
        actor: Box<A>,
    },
    /// Post-wire, pre-spawn — wire ran. The dispatcher is next.
    Wired {
        resources: ClaimResources,
        actor: Box<A>,
    },
    /// Sentinel held only inside a phase method's body between
    /// `mem::replace` and the final state assignment. If the phase
    /// returns Err, it either restores a prior variant (so
    /// [`PassiveBoot::cleanup_after_failure`] sees the right state)
    /// or leaves `Transitioning` when no chassis-side resources are
    /// held (the failed body cleaned up inline).
    Transitioning,
}

/// Issue 552 stage 1 (multi-passed for issue 697): the [`NativeActor`]
/// boot. Claims the cap's mailbox under `A::NAMESPACE`, builds a fresh
/// per-cap [`NativeBinding`], constructs a [`NativeInitCtx`], calls
/// `A::init(config, &mut init_ctx)`, runs `A::wire`, and finally
/// spawns a dispatcher thread that pulls from the transport's inbox
/// and routes through [`NativeDispatch::__aether_dispatch_envelope`] —
/// the sum dispatch trait the `#[actor] impl NativeActor for A`
/// macro emits.
///
/// Stage 2d: `FRAME_BARRIER` caps go through this path too. The
/// frame-bound claim (`claim_frame_bound_mailbox_with_override`)
/// registers the per-mailbox `pending` counter into the chassis's
/// `frame_bound_pending` Vec; the dispatcher thread decrements after
/// each successful handler dispatch so
/// [`crate::chassis::frame_loop::drain_frame_bound_or_abort`]
/// (ADR-0074 §Decision 5) sees the counter drop to zero alongside
/// component drains.
struct NativeActorBoot<A: NativeActor + NativeDispatch> {
    state: BootState<A>,
}

impl<A: NativeActor + NativeDispatch> NativeActorBoot<A> {
    fn new(config: A::Config) -> Self {
        Self {
            state: BootState::Pending { config },
        }
    }
}

impl<A: NativeActor + NativeDispatch> PassiveBoot for NativeActorBoot<A> {
    fn claim(&mut self, ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        let BootState::Pending { config } =
            std::mem::replace(&mut self.state, BootState::Transitioning)
        else {
            panic!("PassiveBoot::claim called in non-Pending state");
        };

        // Issue 607 Phase 3b (ADR-0079): claim namespace ownership for
        // this singleton's `Actor::NAMESPACE`. The actor registry
        // tracks one TypeId per namespace across both cardinalities
        // (Singleton/Instanced), so a later `spawn_child::<X>` whose
        // `X::NAMESPACE` collides with this singleton's namespace
        // surfaces as `SpawnError::NamespaceOwnedByOtherType`. Same
        // TypeId re-claiming the same namespace is idempotent.
        if ctx
            .spawner_arc()
            .actor_registry()
            .try_claim_namespace(A::NAMESPACE, std::any::TypeId::of::<A>())
            .is_err()
        {
            // The other claim is on the same namespace by a different
            // TypeId — a chassis-build collision. State stays
            // `Transitioning` (no resources held); cleanup_after_failure
            // sees that and does nothing.
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
        let claim_result = if A::FRAME_BARRIER {
            ctx.claim_frame_bound_mailbox::<A>().map(|claim| {
                (
                    claim.id,
                    claim.receiver,
                    claim.mailbox_sender,
                    Some(claim.pending),
                    claim.wake_slot,
                )
            })
        } else {
            ctx.claim_mailbox_drop_on_shutdown::<A>().map(|claim| {
                (
                    claim.id,
                    claim.receiver,
                    claim.mailbox_sender,
                    None,
                    claim.wake_slot,
                )
            })
        };
        let (mailbox_id, receiver, mailbox_sender, pending, wake_slot) = match claim_result {
            Ok(c) => c,
            Err(e) => {
                // Release the namespace claim we just made — otherwise
                // a later cap with a different TypeId legitimately
                // claiming the same namespace can't (issue 607 Phase 7).
                ctx.spawner_arc()
                    .actor_registry()
                    .release_namespace(A::NAMESPACE, std::any::TypeId::of::<A>());
                // State stays `Transitioning` — no further cleanup
                // for the rollback loop to do.
                return Err(e);
            }
        };

        // Per-cap transport. `NativeBinding::from_ctx` pulls the
        // chassis's frame-bound set + aborter so the cross-class
        // wait_reply guard wires automatically.
        let transport = Arc::new(NativeBinding::from_ctx(ctx, mailbox_id, A::FRAME_BARRIER));
        transport.install_inbox(receiver);

        // Per-actor scratch storage (issue 582 / ADR-0074). Stamped
        // into TLS via `local::with_stamped` for the duration of
        // `init`, `wire`, and each handler dispatch so library code
        // inside the actor (e.g., the issue-581 log buffer) can reach
        // `Local::with_mut` without threading a ctx through.
        let slots = Box::new(aether_actor::local::ActorSlots::new());

        self.state = BootState::Claimed {
            resources: ClaimResources {
                mailbox_id,
                transport,
                mailbox_sender,
                pending,
                wake_slot,
                slots,
            },
            config,
        };
        Ok(())
    }

    fn init(
        &mut self,
        ctx: &mut ChassisCtx<'_>,
        handles: &mut ExportedHandles,
    ) -> Result<(), BootError> {
        let BootState::Claimed { resources, config } =
            std::mem::replace(&mut self.state, BootState::Transitioning)
        else {
            panic!("PassiveBoot::init called in non-Claimed state");
        };

        // Issue #581: stamp the actor's transport as the in-actor
        // `MailDispatch` around `init` so any `tracing::*` event the
        // cap fires during boot drains to LogCapability when init
        // returns.
        let init_result = {
            let mailer_clone = ctx.mail_send_handle();
            let mut init_ctx = NativeInitCtx::new(&resources.transport, handles, mailer_clone);
            aether_actor::local::with_stamped(&resources.slots, || {
                crate::runtime::log_install::with_actor_dispatch(
                    &*resources.transport as &dyn crate::runtime::log_install::MailDispatch,
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
                // A::init consumed `config`, so we can't restore the
                // Claimed variant. Inline the same cleanup
                // `cleanup_after_failure` would do for Claimed: release
                // the mailbox + namespace claim, then let `resources`
                // drop at end of scope (closing transport + sender).
                ctx.unclaim_mailbox(resources.mailbox_id);
                ctx.spawner_arc()
                    .actor_registry()
                    .release_namespace(A::NAMESPACE, std::any::TypeId::of::<A>());
                drop(resources);
                // State stays `Transitioning` — no further work for
                // the rollback loop to do.
                return Err(e);
            }
        };

        // Issue 629 / Phase A: dispatcher takes Box<A> ownership.
        self.state = BootState::Initialized {
            resources,
            actor: Box::new(actor),
        };
        Ok(())
    }

    fn wire(&mut self) -> Result<(), BootError> {
        let BootState::Initialized {
            resources,
            mut actor,
        } = std::mem::replace(&mut self.state, BootState::Transitioning)
        else {
            panic!("PassiveBoot::wire called in non-Initialized state");
        };

        // Issue 584 Phase 2a (ADR-0079 amended): post-init mail-allowed
        // hook. The wire pass runs after the chassis's claim + init
        // passes, so every peer mailbox is published and addressable;
        // wire-emitted mail queues in recipient inboxes (no dispatcher
        // is running yet — spawn pass is next). Wrapped in the same
        // `with_stamped` + `with_actor_dispatch` envelope as `init`
        // and per-envelope dispatch so `Local<T>` and `tracing::*`
        // route identically.
        aether_actor::local::with_stamped(&resources.slots, || {
            crate::runtime::log_install::with_actor_dispatch(
                &*resources.transport as &dyn crate::runtime::log_install::MailDispatch,
                || {
                    let mut wire_ctx = crate::actor::native::NativeCtx::new(
                        &resources.transport,
                        aether_data::ReplyTo::NONE,
                        aether_data::MailId::NONE,
                        aether_data::MailId::NONE,
                    );
                    actor.wire(&mut wire_ctx);
                    aether_actor::log::drain_buffer();
                },
            );
        });

        self.state = BootState::Wired { resources, actor };
        Ok(())
    }

    fn spawn(self: Box<Self>, ctx: &mut ChassisCtx<'_>) -> Result<Box<dyn DynShutdown>, BootError> {
        let BootState::Wired { resources, actor } = self.state else {
            panic!("PassiveBoot::spawn called in non-Wired state");
        };
        let ClaimResources {
            mailbox_id,
            transport,
            mailbox_sender,
            pending,
            wake_slot,
            slots,
        } = resources;

        match A::SCHEDULING {
            aether_actor::Scheduling::Dedicated => {
                // Today's path: spawn one OS thread per actor.
                let mut actor = actor;
                let transport_for_thread = Arc::clone(&transport);
                let actor_registry_for_thread = Arc::clone(ctx.spawner_arc().actor_registry());
                let mailer_for_thread = ctx.mail_send_handle();
                let self_id_for_thread = mailbox_id;
                let thread_name = alloc_native_actor_thread_name::<A>();
                let thread = std::thread::Builder::new()
                    .name(thread_name)
                    .spawn(move || {
                        crate::actor::native::dispatch::dispatch_loop_run::<A>(
                            &transport_for_thread,
                            &mut actor,
                            &slots,
                            pending.as_ref(),
                            &actor_registry_for_thread,
                            &mailer_for_thread,
                            self_id_for_thread,
                        );
                    })
                    .map_err(|e| BootError::Other(Box::new(e)))?;

                Ok(Box::new(NativeActorShutdown {
                    thread: Some(thread),
                    mailbox_sender: Some(mailbox_sender),
                }) as Box<dyn DynShutdown>)
            }
            aether_actor::Scheduling::Pooled => {
                // Issue 635 PR C path: register a `DispatcherSlot`
                // with the chassis worker pool. No per-actor thread.
                // The `wake_slot` in the mailbox closure fires the
                // pool wake hook on every accepted send.
                let actor_registry = Arc::clone(ctx.spawner_arc().actor_registry());
                let mailer_clone = ctx.mail_send_handle();
                let slot = crate::actor::native::dispatcher_slot::DispatcherSlot::<A>::new(
                    actor,
                    Arc::clone(&transport),
                    slots,
                    pending,
                    actor_registry,
                    mailer_clone,
                    mailbox_id,
                );
                let slot_dyn: Arc<dyn crate::scheduler::Drainable> = slot.clone();
                let weak: std::sync::Weak<dyn crate::scheduler::Drainable> =
                    Arc::downgrade(&slot_dyn);
                drop(slot_dyn);
                let wake = crate::scheduler::WakeHandle::new(
                    Arc::clone(slot.state()),
                    weak,
                    ctx.pool_ready_tx().clone(),
                );
                // Issue 697 multi-pass: mail addressed at this actor
                // during the wire pass landed in its inbox before the
                // wake hook was installed, so the closure-side wake
                // fired against an empty `wake_slot`. Fire one wake
                // here so a populated inbox enters the ready queue.
                // Mirrors the same fix `Spawner::spawn_actor`'s
                // Pooled branch carries (issue 635 Phase 3).
                let manual_wake = wake.clone();
                wake_slot.set(Arc::new(move || {
                    // Inbox-sender hook — same fire-and-forget shape
                    // as the spawn.rs analogue: scheduler deduplicates
                    // the CAS, so the bool is irrelevant here.
                    let _ = wake.wake();
                }));
                let _ = manual_wake.wake();
                Ok(Box::new(PooledActorShutdown::<A> {
                    slot: Some(slot),
                    mailbox_sender: Some(mailbox_sender),
                }) as Box<dyn DynShutdown>)
            }
        }
    }

    fn cleanup_after_failure(self: Box<Self>, ctx: &mut ChassisCtx<'_>) {
        match self.state {
            // Pre-claim or mid-method failure that already cleaned up
            // inline — no chassis-side state to release.
            BootState::Pending { .. } | BootState::Transitioning => {}
            // Any past-claim variant: release the mailbox + namespace
            // claims. `resources` (and any held actor) drop at the end
            // of this match arm — dropping `transport` closes the
            // installed receiver, dropping `mailbox_sender` closes the
            // channel.
            BootState::Claimed { resources, .. }
            | BootState::Initialized { resources, .. }
            | BootState::Wired { resources, .. } => {
                ctx.unclaim_mailbox(resources.mailbox_id);
                ctx.spawner_arc()
                    .actor_registry()
                    .release_namespace(A::NAMESPACE, std::any::TypeId::of::<A>());
            }
        }
    }
}

/// Shutdown adapter for a `Pooled` [`NativeActor`] (issue 635 PR C).
/// On chassis shutdown:
/// 1. Sets the binding's `should_shutdown` flag so the next
///    [`crate::scheduler::DispatcherSlot::run_cycle`] observes the
///    signal and runs `unwire` + registry finalize.
/// 2. Drops the [`crate::chassis::ctx::MailboxSender`] so subsequent
///    sends warn-and-discard.
/// 3. Drops the slot Arc — the chassis-held strong ref. The pool
///    worker's strong ref (via the ready queue) drops at end of the
///    final cycle. The pool's `Drop` joins workers, so any in-flight
///    cycle finishes before chassis shutdown returns.
///
/// Today no production cap is `Pooled`, so this struct is reachable
/// but unused at runtime.
struct PooledActorShutdown<A>
where
    A: NativeActor + NativeDispatch,
{
    slot: Option<Arc<crate::actor::native::dispatcher_slot::DispatcherSlot<A>>>,
    mailbox_sender: Option<crate::chassis::ctx::MailboxSender>,
}

impl<A> DynShutdown for PooledActorShutdown<A>
where
    A: NativeActor + NativeDispatch,
{
    fn shutdown_dyn(mut self: Box<Self>) {
        if let Some(slot) = &self.slot {
            slot.binding().signal_shutdown();
        }
        // Drop sender first so the inbox closes; subsequent wakes
        // silently no-op via WakeHandle's Weak failing to upgrade.
        self.mailbox_sender.take();
        drop(self.slot.take());
    }
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
/// [`Builder::with_actor`]. Drops the [`crate::chassis::ctx::MailboxSender`]
/// to disconnect the channel (the dispatcher's `recv_blocking` returns
/// `None` and the thread exits), then joins the thread.
struct NativeActorShutdown {
    thread: Option<std::thread::JoinHandle<()>>,
    mailbox_sender: Option<crate::chassis::ctx::MailboxSender>,
}

impl DynShutdown for NativeActorShutdown {
    fn shutdown_dyn(mut self: Box<Self>) {
        // Sender first — disconnects the channel and lets the
        // dispatcher's `recv_blocking` return None so the thread
        // exits. Joining a still-attached sender would hang.
        self.mailbox_sender.take();
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
/// `Builder<C, NoDriver>` accepts [`Self::with_actor`] /
/// [`Self::with_fallback_router`] and either [`Self::driver`] or
/// [`Self::build_passive`]; once `.driver(d)` runs the builder
/// transitions to `Builder<C, HasDriver>` which only accepts further
/// passives and [`Self::build`].
pub struct Builder<C: Chassis, S: BuilderState = NoDriver> {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    passives: Vec<Box<dyn PassiveBoot>>,
    driver: Option<DriverBoot>,
    aborter: Arc<dyn FatalAborter>,
    /// Issue #601: chassis-declared log-drain target. `None` until the
    /// chassis calls [`Self::with_log_drain`]; on `build()` the
    /// mailbox id is dispatched as `aether.log.configure_drain` mail
    /// to every booted actor so each actor's `LogDrainSlot` is
    /// installed. `ComponentHostCapability` snapshots the same drain
    /// for the runtime `load_component` path — runtime-loaded
    /// components receive `ConfigureLogDrain` themselves on
    /// registration. The chassis Builder declares the drain; the
    /// runtime state lives entirely in `ComponentHostCapability` and per-actor
    /// `LogDrainSlot`s, set the same way every actor's slot is set:
    /// via mail.
    log_drain: Option<MailboxId>,
    /// Issue 745: override [`PoolConfig::workers`]. `None` means
    /// [`PoolConfig::default`] (`available_parallelism() - 1`, min 1);
    /// `Some(n)` plumbs `n` into the pool at boot. Production chassis
    /// mains populate this from `AETHER_WORKERS`.
    workers: Option<usize>,
    _chassis: PhantomData<fn() -> C>,
    _state: PhantomData<fn() -> S>,
}

impl<C: Chassis> Builder<C, NoDriver> {
    /// Construct a fresh builder against the given substrate handles.
    /// Defaults the cross-class `wait_reply` aborter to
    /// [`PanicAborter`]; production drivers swap in
    /// [`crate::runtime::lifecycle::OutboundFatalAborter`] via
    /// [`Self::with_aborter`] before `build()` / `build_passive()`.
    pub fn new(registry: Arc<Registry>, mailer: Arc<Mailer>) -> Self {
        Self {
            registry,
            mailer,
            passives: Vec::new(),
            driver: None,
            aborter: Arc::new(PanicAborter),
            log_drain: None,
            workers: None,
            _chassis: PhantomData,
            _state: PhantomData,
        }
    }

    /// Override the default [`PanicAborter`] with a chassis-supplied
    /// [`FatalAborter`]. Production drivers (desktop, headless) call
    /// this before `build()` so a cross-class `wait_reply` violation
    /// broadcasts `SubstrateDying` before process exit. Single-call: a
    /// second invocation overwrites the prior aborter.
    #[must_use]
    pub fn with_aborter(mut self, aborter: Arc<dyn FatalAborter>) -> Self {
        self.aborter = aborter;
        self
    }

    /// Issue 745: override the worker pool size. `None` keeps
    /// [`PoolConfig::default`] (`available_parallelism() - 1`, min 1);
    /// `Some(n)` plumbs `n` into the pool at boot. `Some(0)` is
    /// clamped to 1 since the pool requires at least one worker. The
    /// override can be applied either before or after `.driver(_)`.
    #[must_use]
    pub fn with_workers(mut self, workers: Option<usize>) -> Self {
        self.workers = workers.map(|n| n.max(1));
        self
    }

    /// Register a fallback router — a single-shot handler the
    /// substrate consults for envelopes whose mailbox name doesn't
    /// resolve. Multiple calls collapse to a `BootError` at
    /// `build()` (single-claim invariant).
    #[must_use]
    pub fn with_fallback_router(mut self, handler: FallbackRouter) -> Self {
        self.passives
            .push(Box::new(FallbackRouterBoot::new(handler)));
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
    #[must_use]
    pub fn with_actor<A>(mut self, config: A::Config) -> Self
    where
        A: NativeActor + NativeDispatch,
    {
        self.passives
            .push(Box::new(NativeActorBoot::<A>::new(config)));
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
    #[must_use]
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
            workers: self.workers,
            _chassis: PhantomData,
            _state: PhantomData,
        }
    }

    /// No-driver build path. Boots every passive in declaration order
    /// and returns a [`PassiveChassis`] whose embedder is responsible
    /// for driving the loop manually (`TestBench`).
    pub fn build_passive(self) -> Result<PassiveChassis<C>, BootError> {
        let booted = boot_passives(
            &self.registry,
            &self.mailer,
            &self.aborter,
            self.workers,
            self.passives,
        )?;
        // Issue #601: push `ConfigureLogDrain` to every booted actor
        // and to `aether.component` so every per-actor `LogDrainSlot`
        // (auto-emitted handler) plus the `ComponentHostCapability`'s
        // drain slot (for the runtime load path) resolve to the
        // chassis-declared target. No-op if the chassis didn't call
        // `with_log_drain`.
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
    /// Register a fallback router after the driver was supplied.
    /// Booted before the driver in declaration order.
    #[must_use]
    pub fn with_fallback_router(mut self, handler: FallbackRouter) -> Self {
        self.passives
            .push(Box::new(FallbackRouterBoot::new(handler)));
        self
    }

    /// Mirror of [`Builder::with_actor`][Builder<C, NoDriver>::with_actor]
    /// for the post-driver state — same semantics, accepted because
    /// declaration-order before/after `.driver(_)` doesn't change
    /// boot order (passives boot before the driver regardless).
    #[must_use]
    pub fn with_actor<A>(mut self, config: A::Config) -> Self
    where
        A: NativeActor + NativeDispatch,
    {
        self.passives
            .push(Box::new(NativeActorBoot::<A>::new(config)));
        self
    }

    /// Mirror of [`Builder::with_log_drain`][Builder<C, NoDriver>::with_log_drain]
    /// for the post-driver state. Issue #601.
    #[must_use]
    pub fn with_log_drain<T>(mut self) -> Self
    where
        T: NativeActor + HandlesKind<LogBatch>,
    {
        self.log_drain = Some(mailbox_id_from_name(T::NAMESPACE));
        self
    }

    /// Mirror of [`Builder::with_workers`][Builder<C, NoDriver>::with_workers]
    /// for the post-driver state. Issue 745.
    #[must_use]
    pub fn with_workers(mut self, workers: Option<usize>) -> Self {
        self.workers = workers.map(|n| n.max(1));
        self
    }

    /// Boot every passive in declaration order, then boot the driver
    /// against a [`DriverCtx`]. Any failure aborts the build and
    /// shuts down the passives that already booted (via
    /// [`BootedPassives::Drop`]) before propagating the error.
    ///
    /// # Panics
    /// Panics if the `HasDriver` typestate is reached without a driver
    /// installed — fail-fast per ADR-0063: the typestate guarantees
    /// `with_driver` has run, so a missing driver is a builder API bug.
    pub fn build(self) -> Result<BuiltChassis<C>, BootError> {
        let Self {
            registry,
            mailer,
            passives,
            driver,
            aborter,
            workers,
            ..
        } = self;
        let driver_boot = driver.expect("HasDriver state implies driver was supplied");

        let mut booted = boot_passives(&registry, &mailer, &aborter, workers, passives)?;
        // Issue #601: push `ConfigureLogDrain` to every booted actor
        // and to `aether.component` so the runtime load path picks up
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
    /// [`crate::NativeBinding`] booted under this chassis so the
    /// cross-class `wait_reply` guard can classify recipients.
    /// Populated alongside `frame_bound_pending` by
    /// [`ChassisCtx::claim_frame_bound_mailbox`].
    frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
    /// Cloned into every `ChassisCtx` and onto every booted
    /// [`crate::NativeBinding`] so the cross-class `wait_reply`
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
    /// the chassis-level handles (registry, `actor_registry`, mailer,
    /// `frame_bound_set`, aborter) so future per-handler `spawn_child`
    /// reaches them without separate plumbing.
    actor_registry: Arc<crate::ActorRegistry>,
    spawner: Arc<crate::Spawner>,
    /// Issue 635 PR C: chassis-owned worker pool. Boots empty in
    /// [`boot_passives`] before any cap; today no cap is `Pooled` so
    /// the pool sits idle, but the infrastructure is live so PR D /
    /// Phase 2 can flip individual caps without touching this layer
    /// again. Drops *after* `shutdowns` (per `BootedPassives::Drop` +
    /// implicit field-drop ordering), so every Dedicated dispatcher
    /// thread is gone before pool workers join.
    _pool: PoolHandle,
    /// ADR-0080 §3 trace drainer thread. Boots alongside the worker
    /// pool; its `Drop` signals + joins the thread (final flush
    /// included). Dropped after `shutdowns` and `_pool` per field
    /// order, so the last batch may target an already-dead
    /// `TraceObserver` mailbox — that's acceptable: the observer cap
    /// drained its inbox during its own dispatcher's phase-2 drain.
    _trace_drainer: crate::runtime::trace::TraceDrainerHandle,
    /// ADR-0080 §6 settlement registry. Cloned into the Mailer's
    /// chassis-router closure (which decodes `Settled { root }`
    /// mail addressed to `CHASSIS_MAILBOX_ID` and signals
    /// subscribers); reachable from `BootedPassives`-holders via
    /// [`Self::settlement_registry`] for PR 4 gate-site
    /// `subscribe_settlement` calls.
    settlement_registry: Arc<crate::chassis::settlement::SettlementRegistry>,
}

/// Issue #601: dispatch a `ConfigureLogDrain { mailbox: drain }` mail
/// to every actor whose mailbox was claimed during boot. Called by
/// `Builder::build` / `build_passive` after `boot_passives` returns.
/// No-op if `drain` is `None` (chassis didn't declare a log target).
///
/// Sends through the same `Mailer` every other mail uses — each actor
/// mailbox routes the envelope to the auto-emitted `ConfigureLogDrain`
/// arm in `#[handlers]`, which installs the per-actor `LogDrainSlot`.
/// Issue 603: `ComponentHostCapability` is now a normal actor booted
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
    /// ADR-0080 §6: borrow the chassis-owned settlement registry.
    /// PR 4 gate-site code (lifecycle drains, the per-frame Tick
    /// barrier, `replace_component` drain) reaches for this to call
    /// `subscribe_settlement(root)` and wait on the returned receiver.
    pub fn settlement_registry(&self) -> &Arc<crate::chassis::settlement::SettlementRegistry> {
        &self.settlement_registry
    }

    fn shutdown_in_place(&mut self) {
        // Issue 685: spawned-instanced actors close BEFORE the
        // singleton shutdowns walk. Two reasons: (1) their close
        // path's `MonitorNotice` fan-out targets singleton watchers
        // that we want still alive, (2) the pool is still up at this
        // point (drops via `_pool` field order after this method
        // returns), so workers can drain the close cycles the
        // `shutdown_instanced` wakes queue.
        self.spawner
            .shutdown_instanced(std::time::Duration::from_secs(2));
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

// Linear boot pipeline: claim mailbox -> wire FFI exports -> spawn
// each passive in declared order, plus rollback bookkeeping. The
// pieces share enough state that splitting into helpers obscures the
// boot ordering — leaving it as one function keeps the chassis boot
// sequence readable in one place.
#[allow(clippy::too_many_lines)]
fn boot_passives(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    aborter: &Arc<dyn FatalAborter>,
    workers: Option<usize>,
    passives: Vec<Box<dyn PassiveBoot>>,
) -> Result<BootedPassives, BootError> {
    let mut shutdowns: Vec<Box<dyn DynShutdown>> = Vec::with_capacity(passives.len());
    let mut fallback: Option<FallbackRouter> = None;
    let mut handles = ExportedHandles::new();
    let mut frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)> = Vec::new();
    let frame_bound_set: Arc<RwLock<HashSet<MailboxId>>> = Arc::new(RwLock::new(HashSet::new()));
    let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
    let actor_registry: Arc<crate::ActorRegistry> = Arc::new(crate::ActorRegistry::new());
    // Issue 635 PR C: stand up the worker pool before any cap boots.
    // The pool's ready_tx is cloned into the Spawner (for instanced
    // Pooled actors) and into the ChassisCtx (for singleton Pooled
    // caps). Today every cap is Dedicated so the pool sits idle, but
    // booting it here means PR D / Phase 2 can flip a cap to Pooled
    // without re-plumbing the boot order.
    //
    // Issue 745: `workers` is the `AETHER_WORKERS` override threaded
    // through `Builder::with_workers`. `None` keeps `PoolConfig::default`
    // (`available_parallelism() - 1`, min 1); `Some(n)` swaps the
    // worker count while preserving every other default field
    // (`budget_template`, etc.).
    let pool_config = workers.map_or_else(PoolConfig::default, |n| PoolConfig {
        workers: n,
        ..PoolConfig::default()
    });
    let pool = Pool::start(pool_config, Arc::clone(aborter));

    // ADR-0080 §3 trace pipeline. `init_substrate_start` pins the
    // monotonic-since-boot reference; `install_trace_queue` makes the
    // global queue visible to producer-side hooks; `start_drainer`
    // owns the thread that batches events into mail addressed to the
    // `aether.trace` sink. The handle in `BootedPassives` joins the
    // thread on chassis tear down.
    crate::runtime::trace::init_substrate_start();
    let trace_queue: Arc<crossbeam_queue::SegQueue<aether_kinds::trace::TraceEvent>> =
        Arc::new(crossbeam_queue::SegQueue::new());
    crate::runtime::trace::install_trace_queue(Arc::clone(&trace_queue));
    let trace_drainer =
        crate::runtime::trace::start_drainer(Arc::clone(&trace_queue), Arc::clone(mailer));

    // ADR-0080 §6 settlement registry + chassis-mail router. The
    // registry owns the gate-site notification map; the router
    // closure decodes `Settled { root }` mail addressed to
    // `CHASSIS_MAILBOX_ID` and signals the matching subscribers.
    // Other chassis-internal kinds (none today; future debugger /
    // describe_tree replies could land here) add matching arms inside
    // the router closure without touching the Mailer's surface.
    let settlement_registry: Arc<crate::chassis::settlement::SettlementRegistry> =
        Arc::new(crate::chassis::settlement::SettlementRegistry::new());
    mailer.install_settlement_registry(Arc::clone(&settlement_registry));
    let registry_for_router = Arc::clone(&settlement_registry);
    let settled_kind = <aether_kinds::trace::Settled as aether_data::Kind>::ID;
    mailer.install_chassis_router(Box::new(move |mail| {
        if mail.kind == settled_kind {
            match postcard::from_bytes::<aether_kinds::trace::Settled>(&mail.payload) {
                Ok(notice) => registry_for_router.fire_settled(notice.root),
                Err(e) => tracing::warn!(
                    target: "aether_substrate::chassis::settlement",
                    error = %e,
                    "Settled mail decode failed; subscribers not woken",
                ),
            }
        } else {
            tracing::warn!(
                target: "aether_substrate::chassis",
                kind = %mail.kind,
                "unhandled chassis-addressed kind",
            );
        }
    }));
    let spawner: Arc<crate::Spawner> = Arc::new(crate::Spawner::new(
        Arc::clone(registry),
        Arc::clone(&actor_registry),
        Arc::clone(mailer),
        Arc::clone(&frame_bound_set),
        Arc::clone(aborter),
        pool.ready_tx(),
    ));
    // Issue 697: multi-pass boot — claim → init → wire → spawn,
    // synchronized across all passives. Each pass below walks every
    // passive that advanced through the prior pass; on failure,
    // `cleanup_after_failure` runs in reverse order on every advanced
    // passive (and any already-spawned dispatchers shut down via
    // their `DynShutdown` handles).

    // Helper: build a fresh `ChassisCtx` borrowing from the locals.
    // Each phase re-takes the borrow because methods may mutate the
    // borrowed slots (e.g., claim pushes into `frame_bound_pending`).
    macro_rules! build_ctx {
        () => {
            ChassisCtx::new(
                registry,
                mailer,
                &mut fallback,
                &mut frame_bound_pending,
                &frame_bound_set,
                aborter,
                &mut claimed_actor_mailboxes,
                &spawner,
            )
        };
    }

    // Helper: undo every advanced passive in `booted` in reverse,
    // then propagate `err`. Spawn-pass failures additionally pass
    // already-spawned shutdowns; this helper handles those too.
    //
    // Placed mid-block intentionally — sits next to the call sites in
    // the boot sequence rather than hoisted to the top of `boot_into`.
    #[allow(clippy::too_many_arguments, clippy::items_after_statements)]
    fn rollback(
        registry: &Arc<Registry>,
        mailer: &Arc<Mailer>,
        fallback: &mut Option<FallbackRouter>,
        frame_bound_pending: &mut Vec<(MailboxId, Arc<AtomicU64>)>,
        frame_bound_set: &Arc<RwLock<HashSet<MailboxId>>>,
        aborter: &Arc<dyn FatalAborter>,
        claimed_actor_mailboxes: &mut Vec<MailboxId>,
        spawner: &Arc<crate::Spawner>,
        booted: Vec<Box<dyn PassiveBoot>>,
        already_spawned: Vec<Box<dyn DynShutdown>>,
    ) {
        for shutdown in already_spawned.into_iter().rev() {
            shutdown.shutdown_dyn();
        }
        for boot in booted.into_iter().rev() {
            let mut ctx = ChassisCtx::new(
                registry,
                mailer,
                fallback,
                frame_bound_pending,
                frame_bound_set,
                aborter,
                claimed_actor_mailboxes,
                spawner,
            );
            boot.cleanup_after_failure(&mut ctx);
        }
    }

    let mut booted: Vec<Box<dyn PassiveBoot>> = Vec::with_capacity(passives.len());

    // Pass 1 — claim.
    for mut boot in passives {
        let mut ctx = build_ctx!();
        match boot.claim(&mut ctx) {
            Ok(()) => booted.push(boot),
            Err(e) => {
                drop(boot);
                rollback(
                    registry,
                    mailer,
                    &mut fallback,
                    &mut frame_bound_pending,
                    &frame_bound_set,
                    aborter,
                    &mut claimed_actor_mailboxes,
                    &spawner,
                    booted,
                    Vec::new(),
                );
                return Err(e);
            }
        }
    }

    // Pass 2 — init.
    for boot in &mut *booted {
        let mut ctx = build_ctx!();
        if let Err(e) = boot.init(&mut ctx, &mut handles) {
            rollback(
                registry,
                mailer,
                &mut fallback,
                &mut frame_bound_pending,
                &frame_bound_set,
                aborter,
                &mut claimed_actor_mailboxes,
                &spawner,
                booted,
                Vec::new(),
            );
            return Err(e);
        }
    }

    // Pass 3 — wire.
    for boot in &mut *booted {
        if let Err(e) = boot.wire() {
            rollback(
                registry,
                mailer,
                &mut fallback,
                &mut frame_bound_pending,
                &frame_bound_set,
                aborter,
                &mut claimed_actor_mailboxes,
                &spawner,
                booted,
                Vec::new(),
            );
            return Err(e);
        }
    }

    // Pass 4 — spawn. On failure, already-pushed shutdowns drain in
    // reverse and any not-yet-spawned passives in `booted` (residing
    // as `Some` in the slot) clean up in reverse via the rollback
    // helper.
    let mut booted_opt: Vec<Option<Box<dyn PassiveBoot>>> = booted.into_iter().map(Some).collect();
    for slot in &mut booted_opt {
        let boot = slot.take().expect("each slot drained exactly once");
        let mut ctx = build_ctx!();
        match boot.spawn(&mut ctx) {
            Ok(s) => shutdowns.push(s),
            Err(e) => {
                let remaining: Vec<Box<dyn PassiveBoot>> =
                    booted_opt.into_iter().flatten().collect();
                rollback(
                    registry,
                    mailer,
                    &mut fallback,
                    &mut frame_bound_pending,
                    &frame_bound_set,
                    aborter,
                    &mut claimed_actor_mailboxes,
                    &spawner,
                    remaining,
                    shutdowns,
                );
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
        _pool: pool,
        _trace_drainer: trace_drainer,
        settlement_registry,
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
            .finish_non_exhaustive()
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
        let id = MailboxId(mailbox_id_from_name(&full_name).0);
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
    /// driver / `TestBench` / scenario inspection step, not from
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
        let Self { booted, driver, .. } = self;
        let result = driver.run();
        // Passives drop here, triggering reverse-order shutdown via
        // BootedPassives::Drop. Holding `booted` until after `result`
        // is bound keeps shutdown ordering deterministic.
        drop(booted);
        result
    }
}

/// A chassis built without a driver. The embedder (`TestBench`, future
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
    /// that drive a `PassiveChassis` directly (`TestBench`, integration
    /// harnesses). `None` if no booted cap published a handle of that
    /// type.
    pub fn handle<H: std::any::Any + Send + Sync + Clone + 'static>(&self) -> Option<H> {
        self.booted.handles.get::<H>()
    }

    /// ADR-0080 §6: borrow the chassis-owned settlement registry.
    /// PR 4 lifecycle / frame / `replace_component` gate sites reach
    /// for this to call `subscribe_settlement(root)`; PR 3 surfaces
    /// the accessor for tests that pump synthetic events through the
    /// trace pipeline and wait on the resulting `Settled` signal.
    pub fn settlement_registry(&self) -> &Arc<crate::chassis::settlement::SettlementRegistry> {
        self.booted.settlement_registry()
    }

    /// Snapshot of every frame-bound mailbox's pending counter
    /// collected during passive boot. Embedders (`TestBench`, bin
    /// drivers) clone this once and feed it to
    /// [`crate::chassis::frame_loop::drain_frame_bound_or_abort`] each frame —
    /// same role as [`crate::chassis::builder::DriverCtx::frame_bound_pending`]
    /// on the driver-build path.
    pub fn frame_bound_pending(&self) -> Vec<(MailboxId, Arc<AtomicU64>)> {
        self.booted.frame_bound_pending.clone()
    }

    /// Issue 607 Phase 5 (ADR-0079): mirror of
    /// [`BuiltChassis::resolve_actor`] for embedders that drive
    /// passive chassis directly (`TestBench`, integration tests).
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
        let id = MailboxId(mailbox_id_from_name(&full_name).0);
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
// Chassis-level integration tests stage many caps, sender threads,
// and assertions in a single test function so the boot-and-route
// sequence reads top-to-bottom; extracting helpers would either lose
// the staging context or add fixtures that aren't reused elsewhere.
#[allow(clippy::too_many_lines)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests {
    use super::*;

    /// Lightweight passive-cap fixture for chassis-level boot tests.
    /// The chassis-builder tests don't care about handler dispatch
    /// (per-cap dispatch coverage lives in `aether-capabilities`); the
    /// real caps would force a circular dep, so this stub stands in.
    struct StubLog;
    impl Actor for StubLog {
        const NAMESPACE: &'static str = "test.chassis_builder.stub_log";
    }
    impl aether_actor::Singleton for StubLog {}

    impl NativeActor for StubLog {
        type Config = ();
        fn init((): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }

    impl NativeDispatch for StubLog {
        fn __aether_dispatch_envelope(
            &mut self,
            _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        let store = Arc::new(crate::handle_store::HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
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
        impl Actor for FailingCap {
            const NAMESPACE: &'static str = "test.phase7.failing_cap";
        }
        impl aether_actor::Singleton for FailingCap {}

        impl NativeActor for FailingCap {
            type Config = ();
            fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Err(BootError::Other(Box::new(std::io::Error::other(
                    "intentional init failure for Phase 7 cleanup test",
                ))))
            }
        }
        impl NativeDispatch for FailingCap {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        use crate::mail::registry::MailboxEntry;
        use aether_data::Kind;
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        // Fixture cap. State behind interior mutability so `&self`
        // dispatch can mutate it (the post-552 norm).
        struct ProbeCap {
            received: Arc<AtomicU32>,
        }
        impl Actor for ProbeCap {
            const NAMESPACE: &'static str = "test.with_actor.probe";
        }
        impl aether_actor::Singleton for ProbeCap {}
        impl HandlesKind<Ping> for ProbeCap {}

        impl NativeActor for ProbeCap {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }

        // Hand-rolled NativeDispatch — what the macro arm emits in
        // task #731. The if-arm decodes Ping bytes, calls the
        // handler, returns Some(()) on success.
        impl NativeDispatch for ProbeCap {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
            .lookup(<ProbeCap as Actor>::NAMESPACE)
            .expect("with_actor claimed the mailbox");
        let MailboxEntry::Inbox(handler) = registry.entry(mailbox_id).expect("sink registered")
        else {
            panic!("ProbeCap claim must be a sink entry");
        };

        let payload = Ping { tag: 0xDEAD_BEEF };
        let bytes = payload.encode_into_bytes();
        handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Ping as Kind>::ID,
            Ping::NAME,
            &bytes,
            1,
        ));

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

    /// Issue 552 stage 2d: `with_actor` accepts `FRAME_BARRIER` caps.
    /// The chassis claims through `claim_frame_bound_mailbox`, the
    /// pending counter feeds the chassis's `frame_bound_pending` Vec,
    /// and the dispatcher decrements after each handler dispatch so
    /// the per-frame drain barrier sees the counter drop in lock-step.
    /// Pre-2d the entry point hard-rejected; the prior reject-test
    /// retired alongside that branch.
    #[test]
    fn with_actor_supports_frame_barrier_caps() {
        struct FrameBoundProbe;
        impl Actor for FrameBoundProbe {
            const NAMESPACE: &'static str = "test.with_actor.frame_bound";
            const FRAME_BARRIER: bool = true;
        }
        impl aether_actor::Singleton for FrameBoundProbe {}

        impl NativeActor for FrameBoundProbe {
            type Config = ();
            fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }
        }

        impl NativeDispatch for FrameBoundProbe {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        use crate::mail::registry::MailboxEntry;
        use aether_actor::Local;
        use aether_data::Kind;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Tick {
            seq: u32,
        }
        impl Kind for Tick {
            const NAME: &'static str = "test.local.tick";
            const ID: aether_data::KindId = aether_data::KindId(0xA1B2_C3D4_E5F6_0002);
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        // The cap holds an Arc<AtomicU32> the test reads after each
        // dispatch. The actor-local counter is keyed by `TypeId<Counter>`
        // — the chassis stamp is what makes `with_mut` resolve at
        // all (outside a stamp it would `debug_assert!` panic).
        struct LocalProbe {
            observed: Arc<AtomicU32>,
        }
        impl Actor for LocalProbe {
            const NAMESPACE: &'static str = "test.local.probe";
        }
        impl aether_actor::Singleton for LocalProbe {}
        impl HandlesKind<Tick> for LocalProbe {}

        // Newtype-per-slot is the Local convention: each
        // logical storage gets its own type, so two probes that
        // both want a u32 don't alias under TypeId. The
        // `#[local]` attribute is the shorthand for the
        // marker impl.
        #[derive(Default)]
        #[aether_actor::local]
        struct Counter(u32);

        impl NativeActor for LocalProbe {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                // Init runs inside the chassis builder's stamp guard
                // — write a sentinel so the handler test below proves
                // the same slots are reused across init→dispatch.
                Counter::with_mut(|c| c.0 = 100);
                Ok(Self { observed: config })
            }
        }

        impl NativeDispatch for LocalProbe {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
            .lookup(<LocalProbe as Actor>::NAMESPACE)
            .expect("with_actor claimed the mailbox");
        let MailboxEntry::Inbox(handler) = registry.entry(mailbox_id).expect("sink registered")
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
            handler.enqueue(crate::mail::registry::test_owned_dispatch(
                <Tick as Kind>::ID,
                Tick::NAME,
                &bytes,
                1,
            ));
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
        use crate::actor::native::spawn::Subname;
        use crate::mail::registry::MailboxEntry;
        use aether_actor::{HandlesKind, Instanced};
        use aether_data::{Kind, KindId as DataKindId};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct Hatch {
            tag: u32,
        }
        impl Kind for Hatch {
            const NAME: &'static str = "test.spawn_child.hatch";
            const ID: DataKindId = DataKindId(0xC0C1_C2C3_C4C5_C6C7);
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        struct ChildCap {
            received: Arc<AtomicU32>,
        }
        impl Actor for ChildCap {
            const NAMESPACE: &'static str = "test.spawn_child.child";
        }
        impl Instanced for ChildCap {}
        impl HandlesKind<Ping> for ChildCap {}
        impl NativeActor for ChildCap {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl NativeDispatch for ChildCap {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        impl Actor for ParentCap {
            const NAMESPACE: &'static str = "test.spawn_child.parent";
        }
        impl aether_actor::Singleton for ParentCap {}
        impl HandlesKind<Hatch> for ParentCap {}
        impl NativeActor for ParentCap {
            type Config = (Arc<AtomicU32>, Arc<AtomicU32>);
            fn init(
                (spawn_count, child_received): Self::Config,
                _ctx: &mut NativeInitCtx<'_>,
            ) -> Result<Self, BootError> {
                Ok(Self {
                    spawn_count,
                    child_received,
                })
            }
        }
        impl NativeDispatch for ParentCap {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
            .lookup(<ParentCap as Actor>::NAMESPACE)
            .expect("ParentCap claimed");
        let MailboxEntry::Inbox(handler) = registry.entry(parent_id).expect("sink") else {
            panic!("expected mailbox entry");
        };
        let bytes = (Hatch { tag: 1 }).encode_into_bytes();
        handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Hatch as Kind>::ID,
            Hatch::NAME,
            &bytes,
            1,
        ));

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
        let child_id = MailboxId(mailbox_id_from_name("test.spawn_child.child:0").0);
        assert!(
            chassis.actor_registry().is_live(child_id),
            "spawned child should be Live in the actor registry under the deterministic full-name id"
        );

        drop(chassis);
    }

    /// Issue 607 Phase 4a verify: `ctx.shutdown()` from inside an
    /// instanced actor's handler triggers the drain → unwire → exit
    /// path, flips the `actor_registry` slot to `Dead`, and inserts the
    /// id into `tombstones`. A reused subname after retirement returns
    /// `SpawnError::SubnameRetired`.
    #[test]
    fn ctx_shutdown_marks_dead_runs_unwire_tombstones_id() {
        use crate::actor::native::spawn::{SpawnError, Subname};
        use crate::mail::registry::MailboxEntry;
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        struct Closer {
            close_observed: Arc<AtomicU32>,
        }
        impl Actor for Closer {
            const NAMESPACE: &'static str = "test.shutdown.closer";
        }
        impl Instanced for Closer {}
        impl HandlesKind<Quit> for Closer {}
        impl NativeActor for Closer {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self {
                    close_observed: config,
                })
            }
            fn unwire(&mut self, _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>) {
                self.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl NativeDispatch for Closer {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        // trampoline drains, runs `unwire`, marks Dead, tombstones.
        let MailboxEntry::Inbox(handler) = registry.entry(id).expect("sink registered") else {
            panic!("expected mailbox entry for instanced actor");
        };
        let bytes = (Quit { tag: 1 }).encode_into_bytes();
        handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Quit as Kind>::ID,
            Quit::NAME,
            &bytes,
            1,
        ));

        // Wait for unwire to run + the registry slot to flip Dead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while close_observed.load(AtomicOrdering::SeqCst) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            close_observed.load(AtomicOrdering::SeqCst),
            1,
            "unwire fired exactly once after the dispatcher drained"
        );
        // Spin until the slot transitions Dead — the dispatcher
        // thread runs `mark_dead` after `unwire`, so there's a
        // small window between the close-observed bump above and the
        // registry update.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while chassis.actor_registry().is_live(id) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !chassis.actor_registry().is_live(id),
            "registry slot should transition Live → Dead after unwire runs"
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

    /// Issue 685: chassis teardown drives `unwire` on every spawned
    /// instanced actor, even those that never received a self-shutdown
    /// trigger. Pre-685 the Pooled spawn path's slot was reachable
    /// from the chassis only through the wake's `Weak`, and nothing
    /// signaled shutdown at chassis exit — so spawned actors silently
    /// skipped their close path. The Spawner's `shutdown_instanced`
    /// step now signals + wakes every spawned slot before the pool
    /// drops, and the chassis waits for each `Drainable::is_closed`.
    #[test]
    fn chassis_teardown_runs_unwire_for_pooled_spawned_actors() {
        use crate::actor::native::spawn::Subname;
        use aether_actor::Instanced;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        struct Quiet {
            close_observed: Arc<AtomicU32>,
        }
        impl Actor for Quiet {
            const NAMESPACE: &'static str = "test.teardown.quiet";
        }
        impl Instanced for Quiet {}
        impl NativeActor for Quiet {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self {
                    close_observed: config,
                })
            }
            fn unwire(&mut self, _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>) {
                self.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl NativeDispatch for Quiet {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let close_observed = Arc::new(AtomicU32::new(0));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let id = chassis
            .spawn_actor::<Quiet>(Subname::Counter, Arc::clone(&close_observed))
            .finish()
            .expect("spawn instanced actor");

        // No mail at all — the actor sits idle from the moment it
        // spawns. Pre-685 chassis teardown skipped its close path
        // entirely; post-685 the teardown step signals + wakes it and
        // the worker runs the close cycle before the pool drops.
        assert_eq!(close_observed.load(AtomicOrdering::SeqCst), 0);

        drop(chassis);

        assert_eq!(
            close_observed.load(AtomicOrdering::SeqCst),
            1,
            "chassis teardown must drive unwire exactly once for a quiet spawned actor",
        );
        // Drop the unused id binding so clippy stays quiet — its
        // referent (the actor_registry's Live entry) drops with the
        // chassis above.
        let _ = id;
    }

    /// Issue 714: stress version of the chassis-teardown contract.
    /// Spawn N=64 instanced actors and assert all N `close_observed`
    /// counters tick to exactly 1 after `drop(chassis)`. Pre-714 the
    /// polling-based `shutdown_instanced` could lose individual wakes
    /// under contention; the channel-signal rewrite is deterministic
    /// — even one missed `unwire` here fails the test.
    #[test]
    fn chassis_teardown_runs_unwire_for_many_pooled_actors() {
        use crate::actor::native::spawn::Subname;
        use aether_actor::Instanced;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        struct Quiet {
            close_observed: Arc<AtomicU32>,
        }
        impl Actor for Quiet {
            const NAMESPACE: &'static str = "test.teardown.quiet_many";
        }
        impl Instanced for Quiet {}
        impl NativeActor for Quiet {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self {
                    close_observed: config,
                })
            }
            fn unwire(&mut self, _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>) {
                self.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl NativeDispatch for Quiet {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        const N: usize = 64;

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let counters: Vec<Arc<AtomicU32>> = (0..N).map(|_| Arc::new(AtomicU32::new(0))).collect();
        for (i, counter) in counters.iter().enumerate() {
            let name = format!("inst-{i}");
            chassis
                .spawn_actor::<Quiet>(Subname::Named(&name), Arc::clone(counter))
                .finish()
                .expect("spawn instanced actor");
        }

        for counter in &counters {
            assert_eq!(counter.load(AtomicOrdering::SeqCst), 0);
        }

        drop(chassis);

        for (i, counter) in counters.iter().enumerate() {
            assert_eq!(
                counter.load(AtomicOrdering::SeqCst),
                1,
                "actor {i} must have run unwire exactly once",
            );
        }
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
        use crate::actor::native::spawn::Subname;
        use crate::mail::registry::MailboxEntry;
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        // Target — handles Quit by self-shutting.
        struct Target;
        impl Actor for Target {
            const NAMESPACE: &'static str = "test.monitor.target";
        }
        impl Instanced for Target {}
        impl HandlesKind<Quit> for Target {}
        impl NativeActor for Target {
            type Config = ();
            fn init((): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl NativeDispatch for Target {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
            handle: Mutex<Option<crate::actor::monitor::MonitorHandle>>,
        }
        impl Actor for Watcher {
            const NAMESPACE: &'static str = "test.monitor.watcher";
        }
        impl Instanced for Watcher {}
        impl HandlesKind<WatchOrder> for Watcher {}
        impl HandlesKind<aether_kinds::MonitorNotice> for Watcher {}
        impl NativeActor for Watcher {
            type Config = (Arc<AtomicU32>, Arc<AtomicU64>);
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self {
                    notice_count: config.0,
                    last_target: config.1,
                    handle: Mutex::new(None),
                })
            }
        }
        impl NativeDispatch for Watcher {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == WatchOrder::ID.0 {
                    let order = WatchOrder::decode_from_bytes(payload)?;
                    let target = MailboxId(order.target_id);
                    let h = ctx
                        .monitor(target)
                        .expect("target must be Live at order time");
                    *self.handle.lock().unwrap() = Some(h);
                    return Some(());
                }
                if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
                    let notice = <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
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
        let MailboxEntry::Inbox(watcher_handler) =
            registry.entry(watcher_id).expect("watcher sink registered")
        else {
            panic!("expected mailbox entry for watcher");
        };
        let order = WatchOrder {
            target_id: target_id.0,
        };
        watcher_handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <WatchOrder as Kind>::ID,
            WatchOrder::NAME,
            &order.encode_into_bytes(),
            1,
        ));

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
        let MailboxEntry::Inbox(target_handler) =
            registry.entry(target_id).expect("target sink registered")
        else {
            panic!("expected mailbox entry for target");
        };
        target_handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Quit as Kind>::ID,
            Quit::NAME,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        ));

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
        use crate::actor::native::spawn::Subname;
        use crate::mail::registry::MailboxEntry;
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        struct Target;
        impl Actor for Target {
            const NAMESPACE: &'static str = "test.monitor.target2";
        }
        impl Instanced for Target {}
        impl NativeActor for Target {
            type Config = ();
            fn init((): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl NativeDispatch for Target {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        struct Watcher {
            handle: Mutex<Option<crate::actor::monitor::MonitorHandle>>,
            close_observed: Arc<AtomicU32>,
        }
        impl Actor for Watcher {
            const NAMESPACE: &'static str = "test.monitor.watcher2";
        }
        impl Instanced for Watcher {}
        impl HandlesKind<WatchOrder> for Watcher {}
        impl HandlesKind<Quit> for Watcher {}
        impl NativeActor for Watcher {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self {
                    handle: Mutex::new(None),
                    close_observed: config,
                })
            }
            fn unwire(&mut self, _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>) {
                self.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl NativeDispatch for Watcher {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == WatchOrder::ID.0 {
                    let order = WatchOrder::decode_from_bytes(payload)?;
                    let target = MailboxId(order.target_id);
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
        let MailboxEntry::Inbox(watcher_handler) =
            registry.entry(watcher_id).expect("watcher sink registered")
        else {
            panic!("expected mailbox entry for watcher");
        };
        let order = WatchOrder {
            target_id: target_id.0,
        };
        watcher_handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <WatchOrder as Kind>::ID,
            WatchOrder::NAME,
            &order.encode_into_bytes(),
            1,
        ));

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
        watcher_handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Quit as Kind>::ID,
            Quit::NAME,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while close_observed.load(AtomicOrdering::SeqCst) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            close_observed.load(AtomicOrdering::SeqCst),
            1,
            "watcher's unwire fired exactly once",
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
        use crate::actor::native::spawn::Subname;
        use crate::mail::registry::MailboxEntry;
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
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
        impl Actor for Member {
            const NAMESPACE: &'static str = "test.resolve.member";
        }
        impl Instanced for Member {}
        impl HandlesKind<Quit> for Member {}
        impl NativeActor for Member {
            type Config = u32;
            fn init(tag: u32, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { tag })
            }
        }
        impl NativeDispatch for Member {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        let MailboxEntry::Inbox(handler) = registry.entry(id_c).expect("c sink registered") else {
            panic!("expected mailbox entry for c");
        };
        handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Quit as Kind>::ID,
            Quit::NAME,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        ));

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
        use crate::actor::native::spawn::Subname;
        use aether_actor::Instanced;

        struct Foo;
        impl Actor for Foo {
            const NAMESPACE: &'static str = "test.resolve_mismatch.foo";
        }
        impl Instanced for Foo {}
        impl NativeActor for Foo {
            type Config = ();
            fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl NativeDispatch for Foo {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        struct Bar;
        impl Actor for Bar {
            const NAMESPACE: &'static str = "test.resolve_mismatch.bar";
        }
        impl Instanced for Bar {}
        impl NativeActor for Bar {
            type Config = ();
            fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }
        }
        impl NativeDispatch for Bar {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
    /// path. Phase 6b (`TcpListenerActor` → `TcpSessionActor`) structurally
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
        use crate::actor::native::spawn::Subname;
        use crate::mail::registry::MailboxEntry;
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
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
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        struct Grandchild {
            received: Arc<AtomicU32>,
        }
        impl Actor for Grandchild {
            const NAMESPACE: &'static str = "test.recursive.grandchild";
        }
        impl Instanced for Grandchild {}
        impl HandlesKind<Ping> for Grandchild {}
        impl NativeActor for Grandchild {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl NativeDispatch for Grandchild {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        impl Actor for Parent {
            const NAMESPACE: &'static str = "test.recursive.parent";
        }
        impl Instanced for Parent {}
        impl HandlesKind<Hatch> for Parent {}
        impl HandlesKind<Quit> for Parent {}
        impl NativeActor for Parent {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self {
                    grandchild_received: config,
                })
            }
        }
        impl NativeDispatch for Parent {
            fn __aether_dispatch_envelope(
                &mut self,
                ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
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
        let MailboxEntry::Inbox(parent_handler) =
            registry.entry(parent_id).expect("parent sink registered")
        else {
            panic!("expected mailbox entry for parent");
        };
        parent_handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Hatch as Kind>::ID,
            Hatch::NAME,
            &(Hatch { tag: 1 }).encode_into_bytes(),
            1,
        ));

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
        let grandchild_id = MailboxId(mailbox_id_from_name("test.recursive.grandchild:only").0);
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
        parent_handler.enqueue(crate::mail::registry::test_owned_dispatch(
            <Quit as Kind>::ID,
            Quit::NAME,
            &(Quit { tag: 1 }).encode_into_bytes(),
            1,
        ));

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

    /// Issue 584 Phase 2a / 697 wire pass: `wire` runs exactly once
    /// for a singleton actor at chassis boot, after `init` succeeds
    /// and before the dispatcher pulls the first envelope.
    #[test]
    fn with_actor_runs_wire_once_at_chassis_boot() {
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        struct WireProbe {
            wire_count: Arc<AtomicU32>,
        }
        impl Actor for WireProbe {
            const NAMESPACE: &'static str = "test.wire.singleton";
        }
        impl aether_actor::Singleton for WireProbe {}
        impl NativeActor for WireProbe {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { wire_count: config })
            }
            fn wire(&mut self, _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>) {
                self.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl NativeDispatch for WireProbe {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let wire_count = Arc::new(AtomicU32::new(0));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<WireProbe>(Arc::clone(&wire_count))
            .build_passive()
            .expect("with_actor boot succeeds");

        assert_eq!(
            wire_count.load(AtomicOrdering::SeqCst),
            1,
            "wire must fire exactly once during builder.with_actor boot",
        );

        drop(chassis);
    }

    /// Issue 697 multi-pass model: wire-time mail crosses actors
    /// regardless of declaration order. Pinger's `wire` mails Ponger;
    /// Ponger's handler increments a counter. With Pinger declared
    /// FIRST, a single-pass interleaved boot would have Pinger's wire
    /// fire before Ponger's claim — the mail would warn-drop. The
    /// multi-pass model (claim-all → init-all → wire-all → spawn-all)
    /// claims both mailboxes before any wire runs, so the mail queues
    /// in Ponger's inbox and processes once dispatchers come up.
    #[test]
    fn wire_pass_mail_crosses_actors_pinger_first() {
        wire_pass_mail_crosses_actors(/* pinger_first */ true);
    }

    /// Mirror of [`wire_pass_mail_crosses_actors_pinger_first`] with
    /// the registration order reversed. Multi-pass model means both
    /// orderings are valid; this test pins the symmetry.
    #[test]
    fn wire_pass_mail_crosses_actors_ponger_first() {
        wire_pass_mail_crosses_actors(/* pinger_first */ false);
    }

    fn wire_pass_mail_crosses_actors(pinger_first: bool) {
        use aether_actor::Sender;
        use aether_data::{Kind, KindId as DataKindId};
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct WireBarrierPing {
            tag: u32,
        }
        impl Kind for WireBarrierPing {
            const NAME: &'static str = "test.barrier.wire_ping";
            const ID: DataKindId = DataKindId(0xB0B1_B2B3_B4B5_B6B7);
            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }
            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }

        struct Pinger {
            wire_ran: Arc<AtomicU32>,
        }
        impl Actor for Pinger {
            const NAMESPACE: &'static str = "test.barrier.pinger";
        }
        impl aether_actor::Singleton for Pinger {}
        impl NativeActor for Pinger {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { wire_ran: config })
            }
            fn wire(&mut self, ctx: &mut crate::actor::native::ctx::NativeCtx<'_>) {
                ctx.send_to_named::<WireBarrierPing>(
                    Ponger::NAMESPACE,
                    &WireBarrierPing { tag: 1 },
                );
                self.wire_ran.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl NativeDispatch for Pinger {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        struct Ponger {
            received: Arc<AtomicU32>,
        }
        impl Actor for Ponger {
            const NAMESPACE: &'static str = "test.barrier.ponger";
        }
        impl aether_actor::Singleton for Ponger {}
        impl HandlesKind<WireBarrierPing> for Ponger {}
        impl NativeActor for Ponger {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { received: config })
            }
        }
        impl NativeDispatch for Ponger {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                kind: crate::mail::KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == WireBarrierPing::ID.0 {
                    let _ = WireBarrierPing::decode_from_bytes(payload)?;
                    self.received.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let received = Arc::new(AtomicU32::new(0));
        let wire_ran = Arc::new(AtomicU32::new(0));

        let builder = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer));
        let builder = if pinger_first {
            builder
                .with_actor::<Pinger>(Arc::clone(&wire_ran))
                .with_actor::<Ponger>(Arc::clone(&received))
        } else {
            builder
                .with_actor::<Ponger>(Arc::clone(&received))
                .with_actor::<Pinger>(Arc::clone(&wire_ran))
        };
        let chassis = builder.build_passive().expect("multi-pass boot succeeds");

        assert_eq!(
            wire_ran.load(AtomicOrdering::SeqCst),
            1,
            "pinger's wire must have run during the wire pass",
        );

        // Wait for Ponger's dispatcher to drain the wire-emitted ping.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while received.load(AtomicOrdering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            received.load(AtomicOrdering::SeqCst),
            1,
            "ponger must observe pinger's wire-emitted ping (multi-pass barrier)",
        );

        drop(chassis);
    }

    /// Issue 584 Phase 2a runtime sibling: `Spawner::spawn_actor` runs
    /// `wire` exactly once on a freshly-spawned instanced actor —
    /// after `init` Ok and after the mailbox is published, before
    /// pre-load mail or the dispatcher pull. Runtime spawn doesn't
    /// need the chassis-boot multi-pass barrier (the substrate is
    /// already steady-state).
    #[test]
    fn spawn_actor_runs_wire_once_after_init() {
        use crate::actor::native::spawn::Subname;
        use aether_actor::Instanced;
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        struct WireSpawnProbe {
            wire_count: Arc<AtomicU32>,
        }
        impl Actor for WireSpawnProbe {
            const NAMESPACE: &'static str = "test.spawn_wire.probe";
        }
        impl Instanced for WireSpawnProbe {}
        impl NativeActor for WireSpawnProbe {
            type Config = Arc<AtomicU32>;
            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { wire_count: config })
            }
            fn wire(&mut self, _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>) {
                self.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }
        impl NativeDispatch for WireSpawnProbe {
            fn __aether_dispatch_envelope(
                &mut self,
                _ctx: &mut crate::actor::native::ctx::NativeCtx<'_>,
                _kind: crate::mail::KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }

        let (registry, mailer) = fresh_substrate();
        let wire_count = Arc::new(AtomicU32::new(0));
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        let id = chassis
            .spawn_actor::<WireSpawnProbe>(Subname::Counter, Arc::clone(&wire_count))
            .finish()
            .expect("spawn instanced actor");

        assert_eq!(
            wire_count.load(AtomicOrdering::SeqCst),
            1,
            "wire must fire exactly once on Spawner::spawn_actor",
        );

        drop(chassis);
        let _ = id;
    }

    /// Issue 745: `Builder::with_workers(None)` leaves the override
    /// field unset so `boot_passives` falls back to
    /// `PoolConfig::default()`.
    #[test]
    fn with_workers_none_leaves_field_unset() {
        let (registry, mailer) = fresh_substrate();
        let builder = Builder::<TestChassis>::new(registry, mailer).with_workers(None);
        assert_eq!(builder.workers, None);
    }

    /// Issue 745: a positive worker count plumbs through verbatim.
    #[test]
    fn with_workers_some_passes_through() {
        let (registry, mailer) = fresh_substrate();
        let builder = Builder::<TestChassis>::new(registry, mailer).with_workers(Some(7));
        assert_eq!(builder.workers, Some(7));
    }

    /// Issue 745: `Some(0)` clamps to 1 since the pool requires at
    /// least one worker.
    #[test]
    fn with_workers_some_zero_clamps_to_one() {
        let (registry, mailer) = fresh_substrate();
        let builder = Builder::<TestChassis>::new(registry, mailer).with_workers(Some(0));
        assert_eq!(builder.workers, Some(1));
    }

    /// Issue 745: the override survives the type-state transition into
    /// [`HasDriver`] so chassis mains can call `.with_workers(...)`
    /// either before or after `.driver(_)`.
    #[test]
    fn with_workers_survives_driver_transition() {
        let (registry, mailer) = fresh_substrate();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let builder = Builder::<DrivenTestChassis<RanDriver>>::new(registry, mailer)
            .with_workers(Some(3))
            .driver(RanDriver {
                ran: Arc::clone(&ran),
            });
        assert_eq!(builder.workers, Some(3));
    }
}
