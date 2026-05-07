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

use crate::hub::HubClient;
use aether_capabilities::{
    BroadcastCapability, CaptureBackend, HandleCapability, HeadlessWindowCapability, LogCapability,
    RenderCapability, RenderConfig, RenderHandles,
};
use aether_capabilities::{ControlPlaneCapability, ControlPlaneConfig};
use aether_data::Kind;
use aether_data::KindId;
use aether_kinds::{FrameStats, Tick};
use aether_substrate::capability::BootError;
use aether_substrate::chassis_builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::{
    Chassis, SubstrateBoot, capture::CaptureQueue, render::VERTEX_BUFFER_BYTES,
};

use super::cap::{TestBenchCapConfig, TestBenchCapability};
use super::events::{ChassisEvent, EventSender};

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility).
pub const WORKERS: usize = 2;

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
    /// Hub URL for `connect_hub`. Binary reads `AETHER_HUB_URL`;
    /// in-process API typically passes `None`.
    pub hub_url: Option<String>,
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
}

/// Output of [`TestBenchChassis::build_passive`]. Bundles the
/// `PassiveChassis<TestBenchChassis>` (holding the booted Log +
/// Render passives via chassis_builder typed lookup) with the
/// substrate handles the embedder needs to drive its event loop —
/// queue, outbound, kind ids, render accumulator handles, the hub
/// client.
///
/// `boot` is exposed so the embedder can call
/// `boot.add_capability(...)` for io / etc. with whatever failure
/// semantics it wants (binary uses `?`; in-process API silent-skips
/// io on systems without writable default roots).
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
    pub kind_frame_stats: KindId,
    pub hub: Option<HubClient>,
}

impl TestBenchChassis {
    /// Build the test-bench chassis: stand up substrate-core
    /// internals via [`SubstrateBoot::builder`], boot the standard
    /// passives + `TestBenchCapability` via the chassis_builder
    /// [`Builder`], connect the hub if `env.hub_url` is set, and
    /// return a [`TestBenchBuild`] the embedder takes ownership of.
    /// The embedder is responsible for any further capability adds
    /// (io with whatever failure semantics it wants), GPU creation,
    /// loopback attach, and driving the event loop.
    pub fn build_passive(env: TestBenchEnv) -> anyhow::Result<TestBenchBuild> {
        let TestBenchEnv {
            name,
            version,
            workers,
            hub_url,
            observed_kinds,
            events_tx,
            capture_queue,
        } = env;

        let boot = SubstrateBoot::builder(&name, &version).build()?;
        let _ = workers;
        let control_plane_config = ControlPlaneConfig {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
            input_subscribers: Arc::clone(&boot.input_subscribers),
        };

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let kind_frame_stats = boot
            .registry
            .kind_id(FrameStats::NAME)
            .expect("FrameStats registered");

        // Capture handoff lives on `RenderCapability` post-issue-603
        // Phase 2. The cap dispatcher parks the request on
        // `capture_queue`; the embedder loop sees `CaptureRequested`
        // and routes through `record_frame` + readback like before.
        let events_for_render = events_tx.clone();
        let render_config = RenderConfig {
            vertex_buffer_bytes: VERTEX_BUFFER_BYTES,
            observed_kinds,
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

        let passive =
            Builder::<TestBenchChassis>::new(Arc::clone(&boot.registry), Arc::clone(&boot.queue))
                .with_actor::<BroadcastCapability>(())
                .with_actor::<HandleCapability>(())
                .with_actor::<LogCapability>(())
                .with_actor::<ControlPlaneCapability>(control_plane_config)
                .with_actor::<RenderCapability>(render_config)
                .with_actor::<HeadlessWindowCapability>(())
                .with_actor::<TestBenchCapability>(test_bench_cap_config)
                .with_log_drain::<LogCapability>()
                .build_passive()?;

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

        let hub = crate::hub::connect_hub_client(&boot, hub_url.as_deref())?;

        // The cap config already cloned `events_tx`; dropping the
        // local copy lets the receiver hang up cleanly once every
        // sender is released.
        drop(events_tx);

        Ok(TestBenchBuild {
            passive,
            boot,
            render_handles,
            kind_tick,
            kind_frame_stats,
            hub,
        })
    }
}
