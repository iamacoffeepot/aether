use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use super::boot_passives::boot_passives;
use super::built::{BuiltChassis, PassiveChassis};
use super::driver::{DriverCapability, DriverCtx, DriverRunning};
use super::native_actor_boot::NativeActorBoot;
use super::passive_boot::{FallbackRouterBoot, PassiveBoot};
use crate::actor::native::NativeActor;
use crate::chassis::Chassis;
use crate::chassis::ctx::{ChassisCtx, FallbackRouter};
use crate::chassis::error::BootError;
use crate::config::{RingCapacities, SchedulerTuning};
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};

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

fn make_driver_boot<D: DriverCapability>(driver: D) -> DriverBoot {
    Box::new(move |ctx| {
        let running = driver.boot(ctx)?;
        Ok(Box::new(running) as Box<dyn DriverRunning>)
    })
}

/// Default worker-pool size for the passive ([`Builder::build_passive`])
/// build path when no explicit [`Builder::with_workers`] override is set.
/// Small on purpose: the passive path is the test / `SubstrateHarness` embedder
/// path (no production callers), and a near-full `available_parallelism()`
/// pool per test over-subscribes nextest's `num_cpus`-wide run. See
/// [`Builder::build_passive`] for the full rationale
/// (iamacoffeepot/aether#1295, iamacoffeepot/aether#1142).
const PASSIVE_DEFAULT_WORKERS: usize = 2;

/// Default cumulative patience for the instanced-actor teardown
/// close-done gate ([`Builder::with_teardown_cap`]) when no override is
/// set. Five minutes — matches the settlement gates'
/// `DEFAULT_SETTLEMENT_CAP_SECS` (300s): the generous backstop #2062
/// introduced so a healthy-but-slow close cycle on a saturated box is
/// never declared wedged (issue #2509). The constant survives only as
/// the default; the durable retune is the `AETHER_SETTLEMENT_CAP_SECS`
/// knob the production chassis thread in.
const DEFAULT_TEARDOWN_CAP: Duration = Duration::from_mins(5);

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
    /// Issue 745: override [`PoolConfig::workers`]. `None` means
    /// [`PoolConfig::default`](crate::scheduler::PoolConfig::default)
    /// (`available_parallelism() - 1`, min 1);
    /// `Some(n)` plumbs `n` into the pool at boot. Production chassis
    /// mains populate this from `AETHER_WORKERS`.
    pub(super) workers: Option<usize>,
    /// Issue 1990: per-actor ring capacities (`ActorLogRing` /
    /// `ActorTraceRing`). Production chassis mains populate this from the
    /// `ActorRingConfig` derive-`Config` knob (env `AETHER_ACTOR_*`);
    /// tests / `SubstrateHarness` leave it [`RingCapacities::default`]. Threaded
    /// into the `Spawner` (instanced spawns) + the cap-claim slot path
    /// (singleton caps) + the chassis-host trace ring at boot.
    ring_caps: RingCapacities,
    /// Scheduler hot-path tuning installed into the scheduler's
    /// process-global immediately before `Pool::start` in `boot_passives`.
    /// Production chassis mains populate this from the bundle-side
    /// `SchedulerTuningConfig` derive-`Config` knob (env `AETHER_*`);
    /// tests / `SubstrateHarness` leave it [`SchedulerTuning::default`].
    scheduler_tuning: SchedulerTuning,
    /// Issue #2509: cumulative patience the instanced-actor teardown
    /// close-done gate (`Spawner::shutdown_instanced`) waits before
    /// declaring a slot wedged. Defaults to the generous 300s backstop
    /// (`DEFAULT_TEARDOWN_CAP`) — the settlement-cap analogue #2062
    /// scoped out — so a healthy-but-slow close cycle on a saturated box
    /// is never false-fired. Production chassis mains populate this from
    /// the same `AETHER_SETTLEMENT_CAP_SECS` knob the settlement gates
    /// read (via `SettlementConfig::to_cap`, including its `0 → MAX`
    /// sentinel); tests / `SubstrateHarness` inherit the default.
    teardown_cap: Duration,
    _chassis: PhantomData<fn() -> C>,
    _state: PhantomData<fn() -> S>,
}

impl<C: Chassis> Builder<C, NoDriver> {
    /// Construct a fresh builder against the given substrate handles.
    /// Defaults the fatal-abort aborter to
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
            workers: None,
            ring_caps: RingCapacities::default(),
            scheduler_tuning: SchedulerTuning::default(),
            teardown_cap: DEFAULT_TEARDOWN_CAP,
            _chassis: PhantomData,
            _state: PhantomData,
        }
    }

    /// Override the default [`PanicAborter`] with a chassis-supplied
    /// [`FatalAborter`]. Production drivers (desktop, headless) call
    /// this before `build()` so a fatal abort (e.g. a wasm guest trap)
    /// exits the process cleanly. Single-call: a
    /// second invocation overwrites the prior aborter.
    #[must_use]
    pub fn with_aborter(mut self, aborter: Arc<dyn FatalAborter>) -> Self {
        self.aborter = aborter;
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
            workers: self.workers,
            ring_caps: self.ring_caps,
            scheduler_tuning: self.scheduler_tuning,
            teardown_cap: self.teardown_cap,
            _chassis: PhantomData,
            _state: PhantomData,
        }
    }

    /// No-driver build path. Boots every passive in declaration order
    /// and returns a [`PassiveChassis`] whose embedder is responsible
    /// for driving the loop manually (`SubstrateHarness`).
    pub fn build_passive(self) -> Result<PassiveChassis<C>, BootError> {
        // The passive (manually-driven) path has no production callers —
        // every chassis main goes through `.driver(_).build()`. It is the
        // build path for `SubstrateHarness` and the hundreds of substrate-booting
        // unit tests. Inheriting `PoolConfig::default()`'s
        // `available_parallelism() - 1` here means each such test spawns a
        // near-full worker pool; under nextest's `num_cpus`-wide run that is
        // ~`num_cpus^2` live threads, and the resulting multi-second
        // scheduling stalls starve settlement / teardown cycles past their
        // deadlines — the load-only flakes in iamacoffeepot/aether#1295 and
        // iamacoffeepot/aether#1142 (green in isolation and on 2-core CI,
        // red under a saturated local `--workspace` run). A small default
        // keeps the manual-drive path light; the perf benches that want a
        // real pool already pass `.with_workers(_)` explicitly, which wins.
        let workers = self.workers.or(Some(PASSIVE_DEFAULT_WORKERS));
        let booted = boot_passives(
            &self.registry,
            &self.mailer,
            &self.aborter,
            workers,
            self.ring_caps,
            self.scheduler_tuning,
            self.teardown_cap,
            self.passives,
        )?;
        // ADR-0081 retired the chassis-pushed `ConfigureLogDrain` mail
        // — each actor owns its own `ActorLogRing` and there is no
        // drain target to configure.
        Ok(PassiveChassis { booted, _chassis: PhantomData })
    }
}

/// State-independent declarations, available both before and after
/// `.driver(_)`: passives declared after the driver still boot before it
/// (declaration order never outranks the passives-before-driver
/// invariant), and the pool / ring / tuning / teardown overrides are
/// consumed only at `build()` / `build_passive()`.
impl<C: Chassis, S: BuilderState> Builder<C, S> {
    /// Register a fallback router — a single-shot handler the
    /// substrate consults for envelopes whose mailbox name doesn't
    /// resolve. Multiple calls collapse to a `BootError` at
    /// `build()` (single-claim invariant).
    #[must_use]
    pub fn with_fallback_router(mut self, handler: FallbackRouter) -> Self {
        self.passives.push(Box::new(FallbackRouterBoot::new(handler)));
        self
    }

    /// Declare a native actor to boot with the chassis, configured per
    /// ADR-0090's derive-`Config` path.
    #[must_use]
    pub fn with_actor<A>(mut self, config: A::Config) -> Self
    where
        A: NativeActor,
    {
        self.passives.push(Box::new(NativeActorBoot::<A>::new(config)));
        self
    }

    /// Issue 745: override the worker pool size. `None` keeps
    /// [`PoolConfig::default`](crate::scheduler::PoolConfig::default)
    /// (`available_parallelism() - 1`, min 1);
    /// `Some(n)` plumbs `n` into the pool at boot. `Some(0)` is
    /// clamped to 1 since the pool requires at least one worker.
    #[must_use]
    pub fn with_workers(mut self, workers: Option<usize>) -> Self {
        self.workers = workers.map(|n| n.max(1));
        self
    }

    /// Issue 1990: override the per-actor ring capacities (`ActorLogRing`
    /// / `ActorTraceRing`). Default is [`RingCapacities::default`] (the
    /// `aether-actor` const caps). Production chassis mains resolve the
    /// `ActorRingConfig` derive-`Config` knob (`AETHER_ACTOR_LOG_RING_SIZE`
    /// / `AETHER_ACTOR_TRACE_RING_SIZE`) and pass the lowered
    /// `RingCapacities` here; the caps thread into every spawned actor's
    /// rings and the chassis-host trace ring at boot.
    #[must_use]
    pub fn with_ring_caps(mut self, ring_caps: RingCapacities) -> Self {
        self.ring_caps = ring_caps;
        self
    }

    /// Override the scheduler hot-path tuning ([`SchedulerTuning`]).
    /// Default is [`SchedulerTuning::default`] (the built-in literals /
    /// adaptive knobs). Production chassis mains resolve the bundle-side
    /// `SchedulerTuningConfig` derive-`Config` knob (env `AETHER_*`) and
    /// pass the lowered `SchedulerTuning` here; `boot_passives` installs it
    /// into the scheduler's process-global before `Pool::start`.
    #[must_use]
    pub fn with_scheduler_tuning(mut self, scheduler_tuning: SchedulerTuning) -> Self {
        self.scheduler_tuning = scheduler_tuning;
        self
    }

    /// Issue #2509: override the instanced-actor teardown close-done
    /// gate's cumulative patience cap. Default is `DEFAULT_TEARDOWN_CAP`
    /// (the 300s backstop). Production chassis mains resolve the shared
    /// `AETHER_SETTLEMENT_CAP_SECS` knob (`SettlementConfig::to_cap`,
    /// including its `0 → Duration::MAX` "wait forever" sentinel) and pass
    /// the lowered `Duration` here, so one knob covers both the settlement
    /// gates and the teardown gate; `boot_passives` stores it on
    /// `BootedPassives`, whose `shutdown_in_place` hands it to
    /// `Spawner::shutdown_instanced`.
    #[must_use]
    pub fn with_teardown_cap(mut self, teardown_cap: Duration) -> Self {
        self.teardown_cap = teardown_cap;
        self
    }
}

impl<C: Chassis> Builder<C, HasDriver> {
    /// Boot every passive in declaration order, then boot the driver
    /// against a [`DriverCtx`]. Any failure aborts the build and
    /// shuts down the passives that already booted (via the
    /// crate-internal `BootedPassives` Drop) before propagating the
    /// error.
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
            ring_caps,
            scheduler_tuning,
            teardown_cap,
            ..
        } = self;
        let driver_boot = driver.expect("HasDriver state implies driver was supplied");

        let mut booted =
            boot_passives(&registry, &mailer, &aborter, workers, ring_caps, scheduler_tuning, teardown_cap, passives)?;
        // ADR-0081 retired the chassis-pushed `ConfigureLogDrain` mail
        // — each actor owns its own `ActorLogRing`.
        let driver_running = {
            let chassis_ctx = ChassisCtx::new(
                &registry,
                &mailer,
                &mut booted.fallback,
                &booted.aborter,
                &mut booted.claimed_actor_mailboxes,
                &booted.spawner,
            );
            let mut driver_ctx = DriverCtx::new(chassis_ctx, &booted.handles);
            driver_boot(&mut driver_ctx)?
        };

        Ok(BuiltChassis { booted, driver: driver_running, _chassis: PhantomData })
    }
}
