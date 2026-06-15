//! Test-bench chassis (ADR-0067) — `TestBenchChassis` marker,
//! `TestBenchEnv` config, and the `TestBenchChassis::build_passive`
//! entry point.
//!
//! Issue 603 retired the `chassis_handler` closure: capture rides
//! `RenderCapability` (Phase 2), window-kind mail through
//! `HeadlessWindowCapability` (Phase 3, fail-fast), advance through
//! `TestBenchCapability` claiming `aether.test_bench` (Phase 4), and
//! `aether.control.platform_info` was deleted entirely (Phase 4).

use std::sync::{Arc, Mutex};

use aether_capabilities::LifecycleCapability;
use aether_capabilities::{
    CaptureBackend, FsCapability, HandleCapability, HeadlessWindowCapability, InputCapability,
    InputConfig, RenderCapability, RenderConfig, RenderHandles, TcpCapability, TextCapability,
    TrajectoryRecorderCapability, UiCapability, fs::NamespaceRoots, trace::TraceDispatchCapability,
};
use aether_capabilities::{ComponentHostCapability, ComponentHostConfig};
use aether_data::Kind;
use aether_data::KindId;
use aether_kinds::Tick;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{
    Chassis, SubstrateBoot, capture::CaptureQueue, render::VERTEX_BUFFER_BYTES,
};

use super::cap::{TestBenchCapConfig, TestBenchCapability};
use super::events::{ChassisEvent, EventSender};
use crate::chassis_common::frame_lifecycle_config;
use aether_substrate::mail::registry::MailDispatch;
use std::io;

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility).
pub const WORKERS: usize = 2;

/// Test-bench observability mailbox. Scenarios that want to assert
/// on component-emitted kinds (the probe's
/// `aether.test_fixture.tick_observed`, for example) target this
/// name with `ctx.send_to_named`; the test-bench chassis registers
/// a synchronous-handler closure under this namespace via
/// `Registry::register_inline` (see `build_passive`) and the
/// closure records each kind name in `TestBenchEnv::observed_kinds`.
/// Only registered when `observed_kinds` is `Some` (binaries pass
/// `None` for zero overhead — mail to this mailbox warn-drops in
/// that mode).
///
/// Pre-iamacoffeepot/aether#838 this rode a full `NativeActor`
/// (`TestBenchObserverCapability`) specifically because synchronous
/// closures leaked `in_flight` and prevented chains from settling
/// — the bench's Tick settlement gate would otherwise wait the
/// full 5 s timeout per tick when a probe component routed
/// observation mail here. iamacoffeepot/aether#840 added the
/// `MailboxEntry::Inline` variant (renamed `MailboxEntry::Sink` ->
/// `Inline` in iamacoffeepot/aether#842) which brackets sync
/// handlers with `Received`/`Finished`, closing the gap and
/// letting us retire the actor-shaped workaround — one fewer
/// thread per `TestBench`.
pub const TEST_BENCH_OBSERVER_MAILBOX_NAME: &str = "aether.test_bench.observer";

/// ADR-0071 marker type for the test-bench chassis. Carries no
/// fields — the chassis instance is the [`PassiveChassis<TestBenchChassis>`]
/// returned by [`Self::build_passive`]. Test-bench is the embedder-
/// driven (no-driver) chassis: the binary's `main()` and the
/// in-process [`super::TestBench`] both build through this and drive
/// their own event loops on top.
pub struct TestBenchChassis;

impl Chassis for TestBenchChassis {
    const PROFILE: &'static str = "test-bench";
    /// Phantom driver — test-bench is passive (the embedder is the
    /// driver). Declaring [`NeverDriver`] satisfies the trait bound;
    /// the value is never instantiated because `TestBench`'s build
    /// path goes through `Builder::<_>::build_passive`.
    type Driver = NeverDriver;
    type Env = TestBenchEnv;

    /// Inert by design — test-bench is a passive chassis. Callers
    /// that try to drive it through the trait method get an error
    /// pointing at [`TestBenchChassis::build_passive`], which is
    /// the actual entry point. The trait method exists so
    /// `Builder<TestBenchChassis, _>` can still parameterise over
    /// `Chassis` per ADR-0071.
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Err(BootError::Other(Box::new(io::Error::other(
            "TestBenchChassis has no driver; use TestBenchChassis::build_passive(env) instead \
             (the binary main() loops on events_rx; the in-process TestBench dispatches per-call)",
        ))))
    }
}

/// Bag of resolved configs the test-bench chassis takes at build
/// time. Constructed by the embedder — the binary's `main()` reads
/// env vars; the in-process [`super::TestBench`] takes builder
/// args. `events_tx` is captured into the test-bench cap's config;
/// the matching `events_rx` rides on [`TestBenchBuild`] for the
/// embedder to drive.
pub struct TestBenchEnv {
    /// Substrate identity for the hub `Hello` handshake (e.g.
    /// `"test-bench"`). Used by both binary and in-process API.
    pub name: String,
    /// Substrate version for the hub `Hello`. Typically
    /// `env!("CARGO_PKG_VERSION")` from the binary; in-process API
    /// supplies the same.
    pub version: String,
    /// Number of workers for the wire-stable `EngineInfo.workers`
    /// field. Defaults to [`WORKERS`].
    pub workers: usize,
    /// Override for the scheduler worker-pool size (`PoolConfig::workers`).
    /// `None` keeps `PoolConfig::default` (`available_parallelism() - 1`,
    /// min 1) — the behaviour every `TestBench` had before
    /// iamacoffeepot/aether#1057.
    /// The mail-latency harness sets this to sweep pool size, since the
    /// pool-default dispatch model makes worker count the dominant
    /// latency variable for fan-out and under-load topologies.
    pub pool_workers: Option<usize>,
    /// Optional observation log: when `Some`, both render and
    /// camera dispatchers push every inbound mail's kind name to it.
    /// In-process API uses this to assert what the sinks have seen;
    /// binary passes `None` for zero overhead.
    pub observed_kinds: Option<Arc<Mutex<Vec<String>>>>,
    /// Sender side of the chassis event channel. Cloned into the
    /// `TestBenchCapability` config + render's capture-wake closure;
    /// the matching receiver rides on [`TestBenchBuild`].
    pub events_tx: EventSender,
    /// Capture-handoff slot the render cap writes into; the
    /// embedder's frame loop drains it on each `RedrawRequested`-
    /// equivalent step.
    pub capture_queue: CaptureQueue,
    /// Optional `aether.fs` roots. When `Some`, the chassis
    /// pre-validates the roots via [`NamespaceRoots::ensure_dirs`]
    /// and chains `with_actor::<FsCapability>(roots)` into the
    /// builder. If pre-validation fails (e.g. a save root that
    /// points at a regular file), the chassis warns and skips the
    /// fs cap rather than aborting the whole boot — matches the
    /// pre-issue-673 silent-skip semantics. When `None`, fs is not
    /// booted at all.
    pub namespace_roots: Option<NamespaceRoots>,
}

/// Output of [`TestBenchChassis::build_passive`]. Bundles the
/// `PassiveChassis<TestBenchChassis>` (holding the booted Log +
/// Render passives via `chassis_builder` typed lookup) with the
/// substrate handles the embedder needs to drive its event loop —
/// queue, outbound, kind ids, render accumulator handles.
///
/// `boot` is exposed so the embedder can attach an egress backend
/// for reply correlation (the in-process `TestBench` wires a
/// `RecordingBackend` for this), read substrate-level handles
/// (`registry`, `queue`, `outbound`), and own the lifetime guard the
/// scheduler joins against on shutdown.
///
/// The embedder owns the matching `EventReceiver` for whichever
/// `EventSender` it passed into [`TestBenchEnv`]; the build does
/// not need to thread it through.
pub struct TestBenchBuild {
    pub passive: PassiveChassis<TestBenchChassis>,
    pub boot: SubstrateBoot,
    /// Driver-facing accumulator + GPU bundle. Pre-PR-E2 the embedder
    /// also got `Arc<RenderCapability>` via `passive.capability()` to
    /// call `install_gpu` / `record_frame` / etc. on it; post-E2
    /// render is a facade cap (dispatcher owns the cap) and the
    /// encoder-level methods live on [`RenderHandles`].
    pub render_handles: RenderHandles,
    pub kind_tick: KindId,
}

impl TestBenchChassis {
    /// Build the test-bench chassis: stand up substrate-core
    /// internals via [`SubstrateBoot::builder`], boot the standard
    /// passives + `TestBenchCapability` via the `chassis_builder`
    /// [`Builder`], and return a [`TestBenchBuild`] the embedder
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
    pub fn build_passive(env: TestBenchEnv) -> anyhow::Result<TestBenchBuild> {
        let TestBenchEnv {
            name,
            version,
            workers,
            pool_workers,
            observed_kinds,
            events_tx,
            capture_queue,
            namespace_roots,
        } = env;

        let boot = SubstrateBoot::builder(&name, &version).build()?;
        let _ = workers;
        let component_host_config = ComponentHostConfig {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");

        // Capture handoff lives on `RenderCapability` post-issue-603
        // Phase 2. The cap dispatcher parks the request on
        // `capture_queue`; the embedder loop sees `CaptureRequested`
        // and routes through `record_frame` + readback like before.
        let events_for_render = events_tx.clone();
        let render_config = RenderConfig {
            vertex_buffer_bytes: VERTEX_BUFFER_BYTES,
            observed_kinds: observed_kinds.clone(),
            assets_dir: None,
            capture_backend: Some(CaptureBackend {
                queue: capture_queue,
                wake: Arc::new(move || {
                    events_for_render
                        .send(ChassisEvent::CaptureRequested)
                        .map_err(|_| "test-bench chassis shutting down — capture aborted")
                }),
                outbound: Arc::clone(&boot.outbound),
            }),
        };

        // Phase 4: advance lands on `TestBenchCapability` claiming
        // `aether.test_bench`. The cap pushes `ChassisEvent::Advance`
        // onto the embedder loop just like the retired
        // `chassis_handler` closure did.
        let test_bench_cap_config = TestBenchCapConfig {
            events: events_tx.clone(),
        };

        let input_config = InputConfig::default();

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
                        "io cap boot skipped in TestBench (root pre-validation failed; expected on systems without writable default roots)",
                    );
                    None
                }
            },
            None => None,
        };

        // Issue 775: scenarios that want to assert on component-
        // emitted kinds register a synchronous catch-all observer
        // closure under `aether.test_bench.observer`. The closure
        // body records each inbound mail's kind name into the shared
        // `observed_kinds` vec; the binary (`bin/test-bench.rs`)
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
        // thread per TestBench.
        if let Some(sink) = observed_kinds {
            let observed_for_handler = sink;
            boot.registry.register_inline(
                TEST_BENCH_OBSERVER_MAILBOX_NAME,
                Arc::new(move |dispatch: MailDispatch<'_>| {
                    if dispatch.kind_name.is_empty() {
                        return;
                    }
                    observed_for_handler
                        .lock()
                        .expect("observed_kinds mutex is never poisoned (ADR-0063 fail-fast)")
                        .push(dispatch.kind_name.to_owned());
                }),
            );
        }

        // ADR-0082 §1 / PR 3b: test-bench uses the shared Tick-only
        // lifecycle graph. The embedder pushes `LifecycleAdvance` via
        // TestBench's own pumping logic; the driver broadcasts Tick to
        // `aether.input` via the relay subscriber.
        let mut builder = Builder::<Self>::new(Arc::clone(&boot.registry), Arc::clone(&boot.queue))
            .with_workers(pool_workers)
            .with_actor::<HandleCapability>(())
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<TrajectoryRecorderCapability>(())
            .with_actor::<InputCapability>(input_config)
            .with_actor::<ComponentHostCapability>(component_host_config)
            .with_actor::<TcpCapability>(())
            .with_actor::<RenderCapability>(render_config)
            .with_actor::<TextCapability>(())
            .with_actor::<UiCapability>(())
            .with_actor::<HeadlessWindowCapability>(())
            .with_actor::<TestBenchCapability>(test_bench_cap_config)
            .with_actor::<LifecycleCapability>(frame_lifecycle_config());
        if let Some(roots) = io_roots {
            builder = builder.with_actor::<FsCapability>(roots);
        }
        let passive = builder.build_passive()?;

        // Issue 629 / Phase A: render publishes its `RenderHandles`
        // bundle on the chassis's `ExportedHandles` map during `init`.
        // Embedders retrieve via `PassiveChassis::handle::<H>()` — no
        // `Arc<RenderCapability>` ever escapes the dispatcher thread.
        let render_handles: RenderHandles =
            passive.handle::<RenderHandles>().ok_or_else(|| {
                anyhow::anyhow!(
                    "TestBenchChassis::build: RenderHandles not published — RenderCapability must boot via with_actor before TestBench builds",
                )
            })?;

        // The cap config already cloned `events_tx`; dropping the
        // local copy lets the receiver hang up cleanly once every
        // sender is released.
        drop(events_tx);

        Ok(TestBenchBuild {
            passive,
            boot,
            render_handles,
            kind_tick,
        })
    }
}
