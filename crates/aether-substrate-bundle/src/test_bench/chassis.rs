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

use aether_capabilities::{
    CaptureBackend, FsCapability, HandleCapability, HeadlessWindowCapability, InputCapability,
    InputConfig, LogCapability, RenderCapability, RenderConfig, RenderHandles, TcpCapability,
    fs::NamespaceRoots, trace::TraceObserverCapability,
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

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility).
pub const WORKERS: usize = 2;

/// Test-bench observability mailbox. Scenarios that want to assert on
/// component-emitted kinds (the probe's `aether.test_fixture.tick_observed`,
/// for example) target this name with `ctx.send_to_named`; the
/// test-bench chassis registers [`TestBenchObserverCapability`] under
/// this namespace and the cap's fallback handler records each kind
/// name in `TestBenchEnv::observed_kinds`. Only booted when
/// `observed_kinds` is `Some` (binaries pass `None` for zero
/// overhead).
///
/// Mirrored inline as the `NativeActor::NAMESPACE` literal inside
/// the observer module (the bridge macro can't see this const from
/// the lifted-impl scope); keep them in lockstep.
pub const TEST_BENCH_OBSERVER_MAILBOX_NAME: &str = "aether.test_bench.observer";

// `TestBenchObserverCapability` re-exports via the `#[bridge]` macro;
// `TestBenchObserverConfig` lives inside the `observer` mod and is
// surfaced for chassis builder calls.
pub use observer::TestBenchObserverConfig;

/// Catch-all observer cap that records every kind name addressed at
/// `aether.test_bench.observer`. Pre-issue-775 the same role lived on
/// `BroadcastCapability` (which also fanned the mail out to attached
/// MCP sessions); with that fan-out retired the observer survives as
/// a test-bench-private cap whose only job is recording kind names
/// for `count_observed` assertions. The cap is a real `NativeActor`
/// (not a `register_closure`) so its handler completes through the
/// framework's ADR-0080 settlement reporting — the bench's Tick
/// settlement gate would otherwise wait the full 5 s timeout per
/// tick when a probe component routes observation mail here.
#[aether_actor::bridge(singleton)]
mod observer {
    use std::sync::{Arc, Mutex};

    use aether_actor::actor;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    /// Config passed in via [`super::TestBenchEnv::observed_kinds`].
    /// The cap captures the `Arc<Mutex<Vec<String>>>` and pushes each
    /// handled mail's kind name into it.
    pub struct TestBenchObserverConfig {
        pub observed_kinds: Arc<Mutex<Vec<String>>>,
    }

    pub struct TestBenchObserverCapability {
        observed_kinds: Arc<Mutex<Vec<String>>>,
    }

    #[actor]
    impl NativeActor for TestBenchObserverCapability {
        type Config = TestBenchObserverConfig;
        // Mirror of `super::TEST_BENCH_OBSERVER_MAILBOX_NAME` (the
        // bridge macro lifts the impl outside the mod, so a `super::`
        // path no longer resolves in the rewritten location).
        const NAMESPACE: &'static str = "aether.test_bench.observer";

        fn init(
            cfg: TestBenchObserverConfig,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self {
                observed_kinds: cfg.observed_kinds,
            })
        }

        /// Catch-all: every kind sent to this mailbox records its
        /// name. The macro auto-emits a blanket `HandlesKind<K>` impl
        /// so callers using `ctx.send_to_named` against an arbitrary
        /// kind compile against the cap.
        #[fallback]
        fn on_any(&self, _ctx: &mut NativeCtx<'_>, env: &Envelope) {
            if env.kind_name.is_empty() {
                return;
            }
            self.observed_kinds
                .lock()
                .unwrap()
                .push(env.kind_name.clone());
        }
    }
}

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
    /// the value is never instantiated because TestBench's build
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
        Err(BootError::Other(Box::new(std::io::Error::other(
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
/// Render passives via chassis_builder typed lookup) with the
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
    /// passives + `TestBenchCapability` via the chassis_builder
    /// [`Builder`], and return a [`TestBenchBuild`] the embedder
    /// takes ownership of. The embedder is responsible for any
    /// further capability adds (io with whatever failure semantics
    /// it wants), GPU creation, egress-backend attach, and driving
    /// the event loop.
    pub fn build_passive(env: TestBenchEnv) -> anyhow::Result<TestBenchBuild> {
        let TestBenchEnv {
            name,
            version,
            workers,
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
            capture_backend: Some(CaptureBackend {
                queue: capture_queue.clone(),
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

        let mut builder =
            Builder::<TestBenchChassis>::new(Arc::clone(&boot.registry), Arc::clone(&boot.queue))
                .with_actor::<HandleCapability>(())
                .with_actor::<LogCapability>(())
                .with_actor::<TraceObserverCapability>(())
                .with_actor::<InputCapability>(input_config)
                .with_actor::<ComponentHostCapability>(component_host_config)
                .with_actor::<TcpCapability>(())
                .with_actor::<RenderCapability>(render_config)
                .with_actor::<HeadlessWindowCapability>(())
                .with_actor::<TestBenchCapability>(test_bench_cap_config);
        // Issue 775: scenarios that want to assert on component-emitted
        // kinds boot the catch-all observer cap. The binary
        // (`bin/test-bench.rs`) passes `observed_kinds: None` and skips
        // the cap entirely; mail to the observer mailbox warn-drops in
        // that mode.
        if let Some(sink) = observed_kinds.clone() {
            builder = builder.with_actor::<TestBenchObserverCapability>(TestBenchObserverConfig {
                observed_kinds: sink,
            });
        }
        if let Some(roots) = io_roots {
            builder = builder.with_actor::<FsCapability>(roots);
        }
        let passive = builder.with_log_drain::<LogCapability>().build_passive()?;

        // Issue 629 / Phase A: render publishes its `RenderHandles`
        // bundle on the chassis's `ExportedHandles` map during `init`.
        // Embedders retrieve via `PassiveChassis::handle::<H>()` — no
        // `Arc<RenderCapability>` ever escapes the dispatcher thread.
        let render_handles: aether_capabilities::RenderHandles =
            passive.handle::<aether_capabilities::RenderHandles>().ok_or_else(|| {
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
