use std::sync::Arc;
use std::time::Duration;

use aether_kinds::trace::Settled;

use super::passive_boot::{DynShutdown, PassiveBoot};
use crate::actor::native::ExportedHandles;
use crate::chassis::ctx::{ChassisCtx, FallbackRouter, MailboxClaim};
use crate::chassis::error::BootError;
use crate::chassis::settlement::SettlementRegistry;
use crate::config::{RingCapacities, SchedulerTuning};
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::{Pool, PoolConfig, PoolHandle, install_tuning, log_handoff_calibration};

pub(super) struct BootedPassives {
    pub(super) shutdowns: Vec<Box<dyn DynShutdown>>,
    pub(super) fallback: Option<FallbackRouter>,
    /// Issue 629 / Phase A: cap-published handle bundles. Populated
    /// during each cap's `init` via [`NativeInitCtx::publish_handle`].
    /// Borrowed (read-only) into [`DriverCtx::handle`] so drivers
    /// retrieve a clone of the published bundle. Replaces the pre-629
    /// type-keyed actor map; the actor itself never escapes its
    /// dispatcher thread.
    pub(super) handles: ExportedHandles,
    /// Cloned into every `ChassisCtx` and onto every booted
    /// [`NativeBinding`] so a wasm-guest trap can fatal-abort the
    /// substrate cleanly. Inherited from the [`Builder`]'s configured
    /// aborter.
    pub(super) aborter: Arc<dyn FatalAborter>,
    /// Issue #601: every actor mailbox claimed during passive boot.
    /// `Builder::build` / `build_passive` reads this list to dispatch
    /// `ConfigureLogDrain` mail to each actor before the driver runs,
    /// installing every actor's `LogDrainSlot` to the chassis-declared
    /// drain.
    pub(super) claimed_actor_mailboxes: Vec<MailboxId>,
    /// ADR-0155 §4: driver-as-actor mailboxes the driver's Claim hook
    /// reserved during Pass 1, each a live [`MailboxClaim`] keyed by name.
    /// `Builder::build` threads this onto the [`ChassisCtx`] it hands the
    /// driver's Start-stage `boot`, which recovers the inbox / actor slots /
    /// wake slot via `DriverCtx::take_claimed_mailbox`. Empty for a chassis
    /// whose driver claims nothing (the default no-op hook) and for the
    /// no-driver `build_passive` path.
    pub(super) reserved_driver_mailboxes: Vec<(String, MailboxClaim)>,
    /// Issue 607 Phase 2 / Phase 3 (ADR-0079): per-chassis actor
    /// lifecycle registry, plus the spawn machinery that writes into
    /// it. Both built once at boot; `Spawner` carries `Arc` clones of
    /// the chassis-level handles (registry, `actor_registry`, mailer,
    /// aborter) so future per-handler `spawn_child` reaches them
    /// without separate plumbing.
    pub(super) actor_registry: Arc<crate::ActorRegistry>,
    pub(super) spawner: Arc<crate::Spawner>,
    /// Issue 635 PR C: chassis-owned worker pool. Boots empty in
    /// [`boot_passives`] before any cap, then drains every actor (all
    /// pool-dispatched since issue 635 Phase 3 / issue 1187). Drops
    /// *after* `shutdowns` (per `BootedPassives::Drop` + implicit
    /// field-drop ordering), so every dispatcher slot has signalled
    /// shutdown before pool workers join.
    _pool: PoolHandle,
    /// ADR-0080 §6 settlement registry. Cloned into the Mailer's
    /// chassis-router closure (which decodes `Settled { root }`
    /// mail addressed to `CHASSIS_MAILBOX_ID` and signals
    /// subscribers); reachable from `BootedPassives`-holders via
    /// [`Self::settlement_registry`] for PR 4 gate-site
    /// `subscribe_settlement` calls.
    settlement_registry: Arc<SettlementRegistry>,
    /// Issue #2509: cumulative patience the instanced-actor teardown
    /// close-done gate waits before declaring a slot wedged. Threaded from
    /// [`Builder::with_teardown_cap`] and handed to
    /// `Spawner::shutdown_instanced` in `Self::shutdown_in_place`.
    teardown_cap: Duration,
}

impl BootedPassives {
    /// ADR-0080 §6: borrow the chassis-owned settlement registry.
    /// PR 4 gate-site code (lifecycle drains, the per-frame Tick
    /// barrier, `replace_component` drain) reaches for this to call
    /// `subscribe_settlement(root)` and wait on the returned receiver.
    pub fn settlement_registry(&self) -> &Arc<SettlementRegistry> {
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
        // Issue #1305: escalating patience replaces the old 2s
        // wall-clock deadline that false-fired under `--workspace`
        // saturation (flake #1295). The per-round budget is the log
        // cadence; the cumulative cap is generous (a healthy close
        // cycle resolves well before it; a genuine wedge exhausts it
        // and aborts/panics).
        // Issue #2509: the cumulative cap is now the configured
        // `teardown_cap` (default 300s, retunable via the shared
        // `AETHER_SETTLEMENT_CAP_SECS` knob) rather than a hardcoded
        // 30s, so a healthy-but-slow close cycle on a saturated box is
        // never false-fired — the same starvation-vs-wedge fix #2062
        // gave the settlement gates, on the gate it scoped out. The 2s
        // round budget (the warn cadence) is unchanged.
        self.spawner.shutdown_instanced(Duration::from_secs(2), self.teardown_cap);
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
// Issue #2509 added the teardown-cap arg; boot_passives already carries
// the resolved chassis config values in argument position (mirroring the
// `Builder` fields), so an extra `Duration` is the same shape.
#[allow(clippy::too_many_arguments)]
pub(super) fn boot_passives(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    aborter: &Arc<dyn FatalAborter>,
    workers: Option<usize>,
    ring_caps: RingCapacities,
    scheduler_tuning: SchedulerTuning,
    teardown_cap: Duration,
    passives: Vec<Box<dyn PassiveBoot>>,
    // ADR-0155 §4: the driver type's value-free Claim hook, run in Pass 1
    // alongside the passives' claims (before any Init). `Builder::build`
    // passes `<C::Driver>::claim`; the no-driver `build_passive` passes the
    // same for `C::Driver = NeverDriver`, whose default hook reserves
    // nothing.
    driver_claim: impl FnOnce(&mut ChassisCtx<'_>) -> Result<(), BootError>,
) -> Result<BootedPassives, BootError> {
    let mut shutdowns: Vec<Box<dyn DynShutdown>> = Vec::with_capacity(passives.len());
    let mut fallback: Option<FallbackRouter> = None;
    let mut handles = ExportedHandles::new();
    let mut claimed_actor_mailboxes: Vec<MailboxId> = Vec::new();
    // ADR-0155 §4: the driver Claim hook stashes its reserved mailboxes here;
    // moved onto `BootedPassives` at return so the driver's Start-stage boot
    // recovers them.
    let mut reserved_driver_mailboxes: Vec<(String, MailboxClaim)> = Vec::new();
    let actor_registry: Arc<crate::ActorRegistry> = Arc::new(crate::ActorRegistry::new());
    // Issue 635 PR C: stand up the worker pool before any cap boots.
    // The pool's wake sink is cloned into the Spawner (for instanced
    // actors) and into the ChassisCtx (for singleton caps). Every actor
    // drains on this pool — issue 635 Phase 3 made `Pooled` the default
    // and issue 1187 removed the per-actor-thread opt-out entirely.
    //
    // Issue 745: `workers` is the `AETHER_WORKERS` override threaded
    // through `Builder::with_workers`. `None` keeps `PoolConfig::default`
    // (`available_parallelism() - 1`, min 1); `Some(n)` swaps the
    // worker count while preserving every other default field
    // (`budget_template`, etc.).
    let pool_config = workers.map_or_else(PoolConfig::default, |n| PoolConfig { workers: n, ..PoolConfig::default() });
    // Install the resolved scheduler tuning into the scheduler's
    // process-global *before* the pool starts: `Pool::start` reads the
    // spin window, and `log_handoff_calibration` below reads the handoff
    // pin / time budget, all through the installed value. Installing first
    // guarantees no getter reads a default before the real value lands (the
    // install-before-`Pool::start` ordering invariant).
    install_tuning(scheduler_tuning);
    let pool = Pool::start(pool_config, Arc::clone(aborter));

    // iamacoffeepot/aether#1182: calibrate this box's cross-worker handoff
    // cost once at boot and log the keep-local budget the adaptive valve
    // *would* pick (`k × cost`) next to the current fixed default. Dark —
    // measurement only, drives no scheduling decision yet (the wiring is a
    // follow-up); the calibrated cost is cached for the future valve and
    // iamacoffeepot/aether#1127's recruiter.
    log_handoff_calibration();

    // ADR-0086 Phase 3c: the central trace queue + drainer retired. The
    // `Mailer`'s per-chassis `TraceHandle` records trace events directly
    // into per-actor rings (queried via `aether.trace.tail`) and drives
    // settlement through its emit-time `SettlementCounter` — no batching
    // thread to spawn.

    // ADR-0080 §6 settlement registry + chassis-mail router. The registry
    // owns the gate-site notification map (`subscribe_settlement` /
    // `subscribe_settlement_mail`); the lifecycle driver and other gate
    // sites wait on it.
    //
    // ADR-0086 Phase 2: settlement is now fired by the emit-time
    // `SettlementCounter` on the trace handle — synchronously on the
    // producing thread's zero-transition — not by the observer's drained
    // fold. Install the registry into the trace handle so the counter can
    // reach `fire_settled`.
    let settlement_registry: Arc<SettlementRegistry> = Arc::new(SettlementRegistry::new());
    mailer.install_settlement_registry(Arc::clone(&settlement_registry));
    mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement_registry));
    let settled_kind = <Settled as aether_data::Kind>::ID;
    mailer.install_chassis_router(Box::new(move |mail| {
        // The observer still folds the trace stream and emits a `Settled`
        // per root, but the emit-time counter already fired that root
        // synchronously (~1ms earlier), so the observer's late copy is
        // superseded — swallow it (acting on it would be a redundant
        // idempotent no-op). The observer's settlement *emission* is
        // removed in Phase 4 alongside the drainer; until then this guard
        // keeps the late mail from warn-storming as an unhandled kind.
        // Future chassis-internal kinds (debugger / describe_tree replies)
        // add matching arms here without touching the Mailer's surface.
        if mail.kind != settled_kind {
            tracing::warn!(
                target: "aether_substrate::chassis",
                kind = %mail.kind,
                "unhandled chassis-addressed kind",
            );
        }
    }));
    // Issue 1990: the chassis-host trace ring (off-actor producers —
    // `Tick` / MCP sends / test injects) lives on the Mailer's
    // `TraceHandle`, outside the `Spawner`/builder slot path, so set its
    // floor + growth ceiling explicitly to the same configured trace caps
    // the per-actor rings get. The ring is empty at boot, so resizing it
    // now is safe.
    mailer.trace_handle().set_chassis_host_ring_capacity(ring_caps.trace, ring_caps.trace_max);
    let spawner: Arc<crate::Spawner> = Arc::new(crate::Spawner::new(
        Arc::clone(registry),
        Arc::clone(&actor_registry),
        Arc::clone(mailer),
        Arc::clone(aborter),
        pool.wake_sink(),
        ring_caps,
    ));
    // Issue 697: multi-pass boot — claim → init → wire → spawn,
    // synchronized across all passives. Each pass below walks every
    // passive that advanced through the prior pass; on failure,
    // `cleanup_after_failure` runs in reverse order on every advanced
    // passive (and any already-spawned dispatchers shut down via
    // their `DynShutdown` handles).

    // Helper: build a fresh `ChassisCtx` borrowing from the locals.
    // Each phase re-takes the borrow because methods may mutate the
    // borrowed slots (e.g., claim pushes into `claimed_actor_mailboxes`).
    macro_rules! build_ctx {
        () => {
            ChassisCtx::new(
                registry,
                mailer,
                &mut fallback,
                aborter,
                &mut claimed_actor_mailboxes,
                &spawner,
                &mut reserved_driver_mailboxes,
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
        aborter: &Arc<dyn FatalAborter>,
        claimed_actor_mailboxes: &mut Vec<MailboxId>,
        spawner: &Arc<crate::Spawner>,
        booted: Vec<Box<dyn PassiveBoot>>,
        already_spawned: Vec<Box<dyn DynShutdown>>,
    ) {
        for shutdown in already_spawned.into_iter().rev() {
            shutdown.shutdown_dyn();
        }
        // ADR-0155 §4: `cleanup_after_failure` never touches driver-reserved
        // mailboxes (they belong to the driver's Claim hook, not a passive),
        // so the ctx borrows a throwaway stash the rollback drops.
        let mut reserved_driver_mailboxes: Vec<(String, MailboxClaim)> = Vec::new();
        for boot in booted.into_iter().rev() {
            let mut ctx = ChassisCtx::new(
                registry,
                mailer,
                fallback,
                aborter,
                claimed_actor_mailboxes,
                spawner,
                &mut reserved_driver_mailboxes,
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

    // ADR-0155 §4: still the Claim stage — the driver-as-actor claim hook
    // reserves its mailboxes alongside the passives, before any Init. The
    // produced `MailboxClaim`s ride `reserved_driver_mailboxes` to the
    // driver's Start-stage `boot` (`DriverCtx::take_claimed_mailbox`). A
    // failure here rolls the already-claimed passives back, exactly as a
    // passive claim failure does.
    {
        let mut ctx = build_ctx!();
        if let Err(e) = driver_claim(&mut ctx) {
            rollback(
                registry,
                mailer,
                &mut fallback,
                aborter,
                &mut claimed_actor_mailboxes,
                &spawner,
                booted,
                Vec::new(),
            );
            return Err(e);
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
                let remaining: Vec<Box<dyn PassiveBoot>> = booted_opt.into_iter().flatten().collect();
                rollback(
                    registry,
                    mailer,
                    &mut fallback,
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
        aborter: Arc::clone(aborter),
        claimed_actor_mailboxes,
        reserved_driver_mailboxes,
        actor_registry,
        spawner,
        _pool: pool,
        settlement_registry,
        teardown_cap,
    })
}
