//! Test-harness chassis (ADR-0067) — `SubstrateHarnessChassis` marker,
//! `SubstrateHarnessEnv` config, and the `SubstrateHarnessChassis::build_passive`
//! entry point.
//!
//! Issue 603 retired the `chassis_handler` closure: capture rides
//! `RenderCapability` (Phase 2), window-kind mail through
//! `SyntheticWindowCapability` (deterministic harness runtime), advance through
//! `SubstrateHarnessCapability` claiming `aether.substrate_harness` (Phase 4), and
//! `aether.control.platform_info` was deleted entirely (Phase 4).

use std::any::Any;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aether_component::{ComponentHostCapability, ComponentHostParams};
use aether_data::Kind;
use aether_data::KindId;
use aether_fs::{FsCapability, NamespaceRoots};
use aether_kinds::{FrameVerdict, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::ConfigSources;
use aether_substrate::mail::MailboxId;
use aether_substrate::{Chassis, Mailer, RingCapacities, SchedulerTuning, SubstrateBoot};
use aether_trace::TraceDispatchCapability;
use aether_window::SyntheticWindowCapability;

use super::cap::{SubstrateHarnessCapParams, SubstrateHarnessCapability};
use super::events::EventSender;
use aether_lifecycle::{LifecycleConfig, frame_lifecycle_params};
use aether_substrate::mail::registry::MailDispatch;
use std::io;

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility).
pub const WORKERS: usize = 2;

/// Test-harness observability mailbox. Scenarios that want to assert
/// on component-emitted kinds (the probe's
/// `aether.test_fixture.tick_observed`, for example) target this
/// name with `ctx.send_to_named`; the substrate-harness chassis registers
/// a synchronous-handler closure under this namespace via
/// `Registry::register_inline` (see `build_passive`) and the
/// closure records each kind name in `SubstrateHarnessEnv::observed_kinds`.
/// Only registered when `observed_kinds` is `Some` (binaries pass
/// `None` for zero overhead — mail to this mailbox warn-drops in
/// that mode).
///
/// Pre-iamacoffeepot/aether#838 this rode a full `NativeActor`
/// (`SubstrateHarnessObserverCapability`) specifically because synchronous
/// closures leaked `in_flight` and prevented chains from settling
/// — the harness's Tick settlement gate would otherwise wait the
/// full 5 s timeout per tick when a probe component routed
/// observation mail here. iamacoffeepot/aether#840 added the
/// `MailboxEntry::Inline` variant (renamed `MailboxEntry::Sink` ->
/// `Inline` in iamacoffeepot/aether#842) which brackets sync
/// handlers with `Received`/`Finished`, closing the gap and
/// letting us retire the actor-shaped workaround — one fewer
/// thread per `SubstrateHarness`.
pub const SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME: &str = "aether.substrate_harness.observer";

/// ADR-0071 marker type for the substrate-harness chassis. Carries no
/// fields — the chassis instance is the [`PassiveChassis<SubstrateHarnessChassis>`]
/// returned by [`Self::build_passive`]. Test-harness is the embedder-
/// driven (no-driver) chassis: the binary's `main()` and the
/// in-process [`super::SubstrateHarness`] both build through this and drive
/// their own event loops on top.
pub struct SubstrateHarnessChassis;

impl Chassis for SubstrateHarnessChassis {
    const PROFILE: &'static str = "substrate-harness";
    /// Phantom driver — substrate-harness is passive (the embedder is the
    /// driver). Declaring [`NeverDriver`] satisfies the trait bound;
    /// the value is never instantiated because `SubstrateHarness`'s build
    /// path goes through `Builder::<_>::build_passive`.
    type Driver = NeverDriver;
    type Env = SubstrateHarnessEnv;

    /// Inert by design — substrate-harness is a passive chassis. Callers
    /// that try to drive it through the trait method get an error
    /// pointing at [`SubstrateHarnessChassis::build_passive`], which is
    /// the actual entry point. The trait method exists so
    /// `Builder<SubstrateHarnessChassis, _>` can still parameterise over
    /// `Chassis` per ADR-0071.
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Err(BootError::Other(Box::new(io::Error::other(
            "SubstrateHarnessChassis has no driver; use SubstrateHarnessChassis::build_passive(env) instead \
             (the binary main() loops on events_rx; the in-process SubstrateHarness dispatches per-call)",
        ))))
    }
}

/// A deferred `Builder::with_actor` application: the embedder names the
/// cap type and captures its config; the chassis applies the closure
/// after the harness basics so composition order stays chassis-owned.
pub type ComposeFn = Box<dyn FnOnce(Builder<SubstrateHarnessChassis>) -> Builder<SubstrateHarnessChassis> + Send>;

/// PNG bytes, optional [`FrameVerdict`], optional similarity score, and
/// optional similarity pass a capture produces. The verdict is `Some`
/// iff the request carried `checks` (iamacoffeepot/aether#1777); the
/// similarity score / pass are `Some` iff the request carried a
/// `reference` (iamacoffeepot/aether#1780).
pub type CaptureOutcome = Result<(Vec<u8>, Option<FrameVerdict>, Option<f32>, Option<bool>), String>;

/// Frame-pump seam for the pumped GPU render runtime (ADR-0161 slice R4).
/// The core harness owns the advance / capture drive loop but no render
/// types; a hook (the `GpuFrameHook` in `aether-harness-substrate-capture`)
/// owns the [`PumpedSlot`](aether_substrate::PumpedSlot) for the pumped
/// `aether.render` actor and drains it at the harness's pump points, so
/// draw dispatch, capture readback, and present all run on the harness
/// thread that owns the offscreen GPU. A harness without a hook skips the
/// draw on advance and replies `Err` to captures.
///
/// The hook is `!Send` (it owns the `!Send` pumped slot); the harness is
/// single-threaded per instance, so it never crosses a thread boundary.
pub trait FrameHook {
    /// Mail one `aether.render.frame { replay_cache_when_idle }` to the
    /// pumped render actor and drain its slot so the frame records this
    /// call. `replay_cache_when_idle` is the issue 847 semantic: the advance
    /// path commits current (`false`); a capture-driving frame replays the
    /// last committed accumulators (`true`).
    fn send_frame(&mut self, replay_cache_when_idle: bool);
    /// Drain the pumped render slot without recording a frame — dispatches
    /// any queued render mail (advance draws, capture pre-mails, the
    /// `pre_settled` notices). Called each pump-loop iteration so a
    /// render-recipient chain settles while the harness blocks on a reply.
    fn pump(&mut self);
    /// Whether the pumped render actor holds a capture that is **ready to
    /// read back** — every pre-mail chain has settled (ADR-0161 R4 / issue
    /// 860), read from its state without mutating it (via
    /// `PumpedSlot::read_state`). This is the capture ordering barrier: the
    /// core drives exactly one capture frame once this is `true`, never
    /// while a pre-mail chain is still in flight, so the record can't run
    /// against an accumulator a draw has yet to land in (the stale-frame
    /// race the reverted #3923 hit). Settlement of the pre-mail chains — the
    /// slot being drained by [`Self::pump`] as the wait proceeds — is the
    /// deterministic barrier: a chain cannot settle until its terminal
    /// render handler dispatched the draw.
    fn capture_ready(&self) -> bool;
    /// The mailbox `capture_frame` / `frame` requests route to (the pumped
    /// render actor's namespace, resolved by the hook so the core stays
    /// render-free).
    fn render_mailbox(&self) -> MailboxId;
    /// Run the pumped slot's Closed-path teardown (`unwire`, cost-row drop,
    /// registry close + monitor fan-out). Called once on harness drop.
    fn shutdown(&mut self);
    /// Downcast surface for capture-crate extension methods (overlay
    /// snapshots) that need the hook's concrete type.
    fn as_any(&self) -> &dyn Any;
}

/// The render wiring the [`HookFactory`](crate::HookFactory) needs to boot
/// the pumped render slot after `build_passive` (ADR-0161). The pumped path
/// composes no build-time render cap — the hook claims the `aether.render`
/// slot post-boot via
/// [`PassiveChassis::boot_pumped_actor`](aether_substrate::chassis::builder::PassiveChassis::boot_pumped_actor)
/// — so the non-knob render wiring is handed straight to the hook factory,
/// which threads it into the pumped actor's `RenderParams`.
pub struct RenderHookWiring {
    /// The chassis mailer, so the hook can mail `frame` to the pumped slot.
    pub mailer: Arc<Mailer>,
    /// `SubstrateHarness` observation sink threaded into the render actor's
    /// `RenderParams`.
    pub observed_kinds: Option<Arc<Mutex<Vec<KindId>>>>,
    /// Resolved `"assets"` root for `capture_frame` similarity references.
    pub assets_dir: Option<PathBuf>,
}

/// Bag of resolved configs the substrate-harness chassis takes at build
/// time. Constructed by the embedder — the binary's `main()` reads
/// env vars; the in-process [`super::SubstrateHarness`] takes builder
/// args. `events_tx` is captured into the substrate-harness cap's config;
/// the matching `events_rx` rides on [`SubstrateHarnessBuild`] for the
/// embedder to drive.
pub struct SubstrateHarnessEnv {
    /// Number of workers for the wire-stable `EngineInfo.workers`
    /// field. Defaults to [`WORKERS`].
    pub workers: usize,
    /// Override for the scheduler worker-pool size (`PoolConfig::workers`).
    /// `None` keeps `PoolConfig::default` (`available_parallelism() - 1`,
    /// min 1) — the behaviour every `SubstrateHarness` had before
    /// iamacoffeepot/aether#1057.
    /// The mail-latency harness sets this to sweep pool size, since the
    /// pool-default dispatch model makes worker count the dominant
    /// latency variable for fan-out and under-load topologies.
    pub pool_workers: Option<usize>,
    /// Issue 1990: per-actor ring capacities (`ActorLogRing` /
    /// `ActorTraceRing`). `RingCapacities::default()` keeps the
    /// `aether-actor` const caps; a `SubstrateHarness` eviction test pins a small
    /// trace cap to observe `truncated_before`. Per-harness, no process env.
    pub ring_capacities: RingCapacities,
    /// Issue 2485: scheduler hot-path tuning. `SchedulerTuning::default()`
    /// keeps the built-in scheduler literals / adaptive knobs. Per-harness,
    /// no process env.
    pub scheduler_tuning: SchedulerTuning,
    /// Optional observation log: when `Some`, both render and
    /// camera dispatchers push every inbound mail's kind name to it.
    /// In-process API uses this to assert what the sinks have seen;
    /// binary passes `None` for zero overhead.
    pub observed_kinds: Option<Arc<Mutex<Vec<KindId>>>>,
    /// Sender side of the chassis event channel. Cloned into the
    /// `SubstrateHarnessCapability` config; the matching receiver rides on
    /// [`SubstrateHarnessBuild`].
    pub events_tx: EventSender,
    /// Optional `aether.fs` roots. When `Some`, the chassis
    /// pre-validates the roots via [`NamespaceRoots::ensure_dirs`]
    /// and chains `with_actor::<FsCapability>(roots)` into the
    /// builder. If pre-validation fails (e.g. a save root that
    /// points at a regular file), the chassis warns and skips the
    /// fs cap rather than aborting the whole boot — matches the
    /// pre-issue-673 silent-skip semantics. When `None`, fs is not
    /// booted at all.
    pub namespace_roots: Option<NamespaceRoots>,
    /// Compose the component host. Off, `aether.component.load` /
    /// `replace` / `drop` have no recipient — benches that never touch
    /// wasm skip the cap entirely.
    pub component_host: bool,
    /// Caller-supplied capability composition, applied to the chassis
    /// [`Builder`] after the harness basics (trace dispatch, the harness cap,
    /// lifecycle, headless window) in push order. The harness gives the
    /// basics; each embedder composes exactly the caps its scenario
    /// needs (issue #3764).
    pub compose: Vec<ComposeFn>,
    /// Issue #2509: cumulative patience for the instanced-actor teardown
    /// close-done gate. The in-process `SubstrateHarness` resolves this from the
    /// same `SettlementConfig` (`AETHER_SETTLEMENT_CAP_SECS`) knob its
    /// settlement-await loops read (honoring a programmatic
    /// `SubstrateHarness::settlement_cap` override), so a scenario's teardown
    /// gate uses the same patience as its settlement gates.
    pub teardown_budget: Duration,
}

/// Output of [`SubstrateHarnessChassis::build_passive`]. Bundles the
/// `PassiveChassis<SubstrateHarnessChassis>` (holding the booted Log +
/// Render passives via `chassis_builder` typed lookup) with the
/// substrate handles the embedder needs to drive its event loop —
/// queue, outbound, kind ids, render accumulator handles.
///
/// `boot` is exposed so the embedder can attach an egress backend
/// for reply correlation (the in-process `SubstrateHarness` wires a
/// `RecordingBackend` for this), read substrate-level handles
/// (`registry`, `queue`, `outbound`), and own the lifetime guard the
/// scheduler joins against on shutdown.
///
/// The embedder owns the matching `EventReceiver` for whichever
/// `EventSender` it passed into [`SubstrateHarnessEnv`]; the build does
/// not need to thread it through.
pub struct SubstrateHarnessBuild {
    pub passive: PassiveChassis<SubstrateHarnessChassis>,
    pub boot: SubstrateBoot,
    pub kind_tick: KindId,
}

impl SubstrateHarnessChassis {
    /// Build the substrate-harness chassis: stand up substrate-core
    /// internals via [`SubstrateBoot::build`], boot the standard
    /// passives + `SubstrateHarnessCapability` via the `chassis_builder`
    /// [`Builder`], and return a [`SubstrateHarnessBuild`] the embedder
    /// takes ownership of. The embedder is responsible for any
    /// further capability adds (io with whatever failure semantics
    /// it wants), GPU creation, egress-backend attach, and driving
    /// the event loop.
    ///
    /// # Panics
    /// Panics if the `Tick` kind isn't registered in the substrate boot
    /// — fail-fast per ADR-0063: `Tick` is part of the always-on kind
    /// vocabulary the substrate registers from
    /// `aether_kinds::descriptors::all()`, so a missing entry indicates
    /// a substrate-build bug.
    #[allow(clippy::too_many_lines)] // PR 3b growth from lifecycle graph + relay wiring.
    pub fn build_passive(env: SubstrateHarnessEnv) -> anyhow::Result<SubstrateHarnessBuild> {
        let SubstrateHarnessEnv {
            workers,
            pool_workers,
            ring_capacities,
            scheduler_tuning,
            observed_kinds,
            events_tx,
            namespace_roots,
            component_host,
            compose,
            teardown_budget,
        } = env;

        let mut boot = SubstrateBoot::build()?;
        let _ = workers;

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");

        // Phase 4: advance lands on `SubstrateHarnessCapability` claiming
        // `aether.substrate_harness`. The cap pushes `ChassisEvent::Advance`
        // onto the embedder loop just like the retired
        // `chassis_handler` closure did.
        let substrate_harness_cap_config = SubstrateHarnessCapParams { events: events_tx.clone() };

        // Pre-validate fs roots if supplied. Pre-validation
        // mirrors what `LocalFileAdapter::new` does inside
        // `FsCapability::init`: create_dir_all + canonicalize each
        // root. If validation succeeds, chain `with_actor`. If it
        // fails (e.g. save root pointing at a regular file on a CI
        // machine without writable defaults), warn and skip the fs cap —
        // the chassis still boots, components addressing
        // `aether.fs` see "unknown mailbox" mail-drops. Pre-issue-673
        // this was a post-build `boot.add_actor::<FsCapability>` call
        // with the same silent-skip semantics; the new shape moves
        // the validation up so all caps go through one boot path.
        // Nested match keeps the warn-log path readable; converting to
        // `map_or` buries the side-effect under closures.
        #[allow(clippy::option_if_let_else)]
        let io_roots = match namespace_roots {
            Some(roots) => match roots.ensure_dirs() {
                Ok(()) => Some(roots),
                Err(e) => {
                    tracing::warn!(
                        target: "aether_substrate::fs",
                        error = %e,
                        "io cap boot skipped in SubstrateHarness (root pre-validation failed; expected on systems without writable default roots)",
                    );
                    None
                }
            },
            None => None,
        };

        // Issue 775: scenarios that want to assert on component-
        // emitted kinds register a synchronous catch-all observer
        // closure under `aether.substrate_harness.observer`. The closure
        // body records each inbound mail's kind name into the shared
        // `observed_kinds` vec; the binary (`bin/substrate-harness.rs`)
        // passes `observed_kinds: None` and skips registration —
        // mail to the observer mailbox warn-drops in that mode.
        //
        // Registered via `register_inline` (issue 840 + iamacoffeepot/aether#841
        // follow-up): the closure runs inline on the pushing thread
        // and the mailer brackets it with `Received`/`Finished` so
        // chains touching this mailbox settle. Pre-iamacoffeepot/aether#840
        // this rode a full NativeActor specifically because closure
        // arms leaked settlement; now that `Inline` participates in
        // ADR-0080 §6 we get the same correctness with one fewer
        // thread per SubstrateHarness.
        //
        // iamacoffeepot/aether#4171: the harness composes its own chain rather
        // than routing through `aether_substrate::chassis::composed`, so it runs
        // that function's borrow-then-spend itself — the observer names the direct
        // mutator through a borrowed authority, and the token is then spent
        // unconditionally (the observer is optional, the spend is not), well
        // before `build_passive` installs the ADR-0165 seal. The `SubstrateBoot`
        // the embedder receives therefore carries no authority, which is what
        // keeps the registry's direct mutators unnameable once the owner has
        // taken over.
        let authority = boot.authority().ok_or(BootError::AlreadyComposed)?;
        if let Some(sink) = observed_kinds {
            let observed_for_handler = sink;
            boot.registry.register_inline(
                authority,
                SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
                // Records the kind *id*, not its name. The observer runs on
                // every observed dispatch, so resolving a name here would put a
                // registry lookup and a `String` allocation on that path for
                // data no assertion reads until the run is over
                // (iamacoffeepot/aether#4278). `count_observed` resolves the
                // one name it is asked about instead.
                Arc::new(move |dispatch: MailDispatch<'_>| {
                    observed_for_handler
                        .lock()
                        .expect("observed_kinds mutex is never poisoned (ADR-0063 fail-fast)")
                        .push(dispatch.kind);
                }),
            );
        }
        let _spent = boot.take_authority();

        // ADR-0082 §1 / PR 3b: substrate-harness uses the shared frame
        // lifecycle graph. The embedder pushes `LifecycleAdvance` via
        // SubstrateHarness's own pumping logic; the driver broadcasts Tick
        // to the stage subscriber set.
        //
        // Issue #3764: the fixed chain below is the harness basics — trace
        // dispatch, the harness cap, lifecycle, and the synthetic window.
        // Everything else is embedder-composed: the render cap
        // and component host ride env flags (they need boot-internal
        // wiring), fs rides pre-validated roots, and arbitrary caps
        // arrive as `compose` closures applied between the basics and
        // lifecycle.
        // ADR-0156 §5: the harness resolves off a HERMETIC source stack — no env
        // layer, no file layer — so a member it composes but forgets to stage
        // falls through to its compiled default rather than a stray process env
        // var. Before the compose-then-resolve inversion, harness-composed
        // members never read env (their values were constructed directly); this
        // keeps that property. Scenario overrides ride the programmatic layer via
        // `with_actor_configured` / the builder's `with_config`.
        // The hermetic in-process harness is a deliberate embedder: it does not
        // adopt the `composed` seam (no aborter, hermetic sources, per-scenario
        // compose), so it keeps its direct `Builder::new`.
        #[allow(clippy::disallowed_methods)]
        let mut builder = Builder::<Self>::new(Arc::clone(&boot.registry), Arc::clone(&boot.queue))
            .with_config_sources(ConfigSources::hermetic())
            .with_workers(pool_workers)
            .with_ring_capacities(ring_capacities)
            .with_scheduler_tuning(scheduler_tuning)
            .with_teardown_budget(teardown_budget)
            .with_actor::<TraceDispatchCapability>(());
        if component_host {
            builder = builder.with_actor::<ComponentHostCapability>(ComponentHostParams {
                engine: Arc::clone(&boot.engine),
                linker: Arc::clone(&boot.linker),
                hub_outbound: Arc::clone(&boot.outbound),
            });
        }
        // ADR-0161 R4/R5: the pumped render path composes no build-time
        // render cap — the frame hook claims the `aether.render` slot
        // post-`build_passive` via `PassiveChassis::boot_pumped_actor`.
        for apply in compose {
            builder = apply(builder);
        }
        builder = builder
            .with_actor::<SyntheticWindowCapability>(())
            .with_actor::<SubstrateHarnessCapability>(substrate_harness_cap_config)
            // ADR-0156 §5: compose + stage the lifecycle config in one paired call.
            .with_actor_configured::<LifecycleCapability>(frame_lifecycle_params(), LifecycleConfig::default());
        if let Some(roots) = io_roots {
            builder = builder.with_actor_configured::<FsCapability>((), roots);
        }
        let passive = builder.build_passive()?;

        // The cap config already cloned `events_tx`; dropping the
        // local copy lets the receiver hang up cleanly once every
        // sender is released.
        drop(events_tx);

        Ok(SubstrateHarnessBuild { passive, boot, kind_tick })
    }
}
