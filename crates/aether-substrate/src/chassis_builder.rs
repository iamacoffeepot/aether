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
pub struct DriverCtx<'a> {
    inner: ChassisCtx<'a>,
}

impl<'a> DriverCtx<'a> {
    fn new(inner: ChassisCtx<'a>) -> Self {
        Self { inner }
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

type PassiveBoot = Box<dyn FnOnce(&mut ChassisCtx<'_>) -> Result<Box<dyn DynShutdown>, BootError>>;
type DriverBoot = Box<dyn FnOnce(&mut DriverCtx<'_>) -> Result<Box<dyn DriverRunning>, BootError>>;

fn make_passive_boot<C>(cap: C) -> PassiveBoot
where
    C: Actor + Dispatch + Send + 'static,
{
    Box::new(move |ctx| {
        let handle = ctx.spawn_actor_dispatcher(cap)?;
        Ok(Box::new(FacadeShutdown {
            handle: Some(handle),
        }) as Box<dyn DynShutdown>)
    })
}

fn make_fallback_router_boot(handler: FallbackRouter) -> PassiveBoot {
    Box::new(move |ctx| {
        ctx.claim_fallback_router(handler)?;
        Ok(Box::new(FallbackShutdown) as Box<dyn DynShutdown>)
    })
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
        let driver_running = {
            let chassis_ctx = ChassisCtx::new(
                &registry,
                &mailer,
                &mut booted.fallback,
                &mut booted.frame_bound_pending,
                &booted.frame_bound_set,
                &booted.aborter,
            );
            let mut driver_ctx = DriverCtx::new(chassis_ctx);
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
        match boot(&mut ctx) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::LogCapability;

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
            .with(LogCapability::new())
            .driver(RanDriver {
                ran: Arc::clone(&ran),
            })
            .build()
            .expect("build succeeds");

        chassis.run().expect("driver run succeeds");
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// Boot-time mailbox-claim collision aborts the build (and runs
    /// the prior cap's drop). Two `LogCapability` instances both
    /// claim `aether.log`; the second hits the duplicate-claim guard.
    #[test]
    fn duplicate_passive_mailbox_aborts_build_and_shuts_down_prior() {
        let (registry, mailer) = fresh_substrate();

        let err = Builder::<TestChassis>::new(registry, mailer)
            .with(LogCapability::new())
            .with(LogCapability::new())
            .build_passive()
            .expect_err("second passive must fail with duplicate claim");

        assert!(matches!(err, BootError::MailboxAlreadyClaimed { .. }));
    }
}
