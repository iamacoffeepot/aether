//! Headless chassis: `HeadlessChassis` (ADR-0035 / ADR-0071), the
//! `Err`-replying capability stubs that fail fast for kinds desktop
//! supports natively (capture/window) plus `Advance`, and the
//! [`HeadlessChassis::build`] entry point that assembles the substrate
//! + tick driver into a [`BuiltChassis`].
//!
//! Issue 603 retired the `chassis_handler` closure: each fail-fast
//! kind moved onto its own cap. `HeadlessRenderCapability` (Phase 2)
//! handles `aether.render`; `HeadlessWindowCapability` (Phase 3)
//! handles `aether.window`; `UnsupportedTestBenchCapability` (Phase 4)
//! handles `aether.test_bench`. `aether.control.platform_info` was
//! deleted as a kind in Phase 4 — no replacement, no MCP path until
//! issue 603 §F2 revives the per-domain shape.

use std::sync::Arc;
use std::time::Duration;

use aether_capabilities::{
    BroadcastCapability, ControlPlaneCapability, ControlPlaneConfig, HandleCapability,
    HeadlessRenderCapability, HeadlessWindowCapability, HttpCapability, IoCapability,
    LogCapability, TcpCapability, UnsupportedTestBenchCapability, http::HttpConfig as HttpConf,
    io::NamespaceRoots,
};
use aether_data::{Kind, KindId};
use aether_kinds::{FrameStats, SetMasterGain, SetMasterGainResult, Tick};
use aether_substrate::capability::BootError;
use aether_substrate::chassis_builder::{Builder, BuiltChassis};
use aether_substrate::{Chassis, SubstrateBoot};

use super::driver::{HeadlessTimerCapability, WORKERS, parse_tick_hz_env};

/// Marker type for the headless chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HeadlessChassis>`] returned
/// by [`Self::build`]. Same shape as [`crate::DesktopChassis`] post
/// ADR-0071 phase 3.
pub struct HeadlessChassis;

impl Chassis for HeadlessChassis {
    const PROFILE: &'static str = "headless";
    type Driver = super::driver::HeadlessTimerCapability;
    type Env = HeadlessEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
    }
}

/// Bag of resolved configs the headless chassis takes at build time.
/// `main()` populates it from env vars (per ADR-0070's "substrate-core
/// never reads env" invariant); tests construct one directly.
pub struct HeadlessEnv {
    pub hub_url: Option<String>,
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConf,
    pub tick_period: Duration,
}

impl HeadlessEnv {
    /// Read every chassis-relevant env var into a fresh `HeadlessEnv`.
    /// The single env-reading edge for the headless chassis (per
    /// issue 464). Tests bypass this by constructing `HeadlessEnv`
    /// directly.
    pub fn from_env() -> Self {
        let hub_url = std::env::var("AETHER_HUB_URL").ok();
        let http = HttpConf::from_env();
        let namespace_roots = NamespaceRoots::from_env();
        let tick_hz = parse_tick_hz_env();
        let tick_period = Duration::from_nanos(1_000_000_000 / u64::from(tick_hz));
        HeadlessEnv {
            hub_url,
            namespace_roots,
            http,
            tick_period,
        }
    }
}

impl HeadlessChassis {
    /// Build the headless chassis: stand up substrate-core internals,
    /// register the audio fail-fast sink, connect the hub, compose
    /// the native passives (broadcast/handle/log/control/io/http plus
    /// the headless render / window / test-bench fail-fast caps)
    /// through the chassis_builder `.with()` chain, then wrap the
    /// timer in a [`HeadlessTimerCapability`] and hand it to the
    /// builder.
    fn build_inner(env: HeadlessEnv) -> Result<BuiltChassis<HeadlessChassis>, BootError> {
        let HeadlessEnv {
            hub_url,
            namespace_roots,
            http,
            tick_period,
        } = env;

        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION")).build()?;
        let _ = WORKERS;
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

        // Audio nop sink — NoteOn/NoteOff fall through silently;
        // SetMasterGain replies Err so agents fail fast rather than
        // hang on a chassis with no audio device.
        let kind_set_master_gain = boot
            .registry
            .kind_id(SetMasterGain::NAME)
            .expect("SetMasterGain registered");
        let outbound_for_audio_sink = Arc::clone(&boot.outbound);
        boot.registry.register_sink(
            "aether.audio",
            Arc::new(
                move |kind: KindId,
                      _kind_name: &str,
                      _origin: Option<&str>,
                      sender,
                      _bytes: &[u8],
                      _count: u32| {
                    if kind == kind_set_master_gain {
                        outbound_for_audio_sink.send_reply(
                            sender,
                            &SetMasterGainResult::Err {
                                error: "unsupported on headless chassis — no audio device"
                                    .to_owned(),
                            },
                        );
                    }
                },
            ),
        );

        let tick_hz = (Duration::from_secs(1).as_nanos() / tick_period.as_nanos().max(1)) as u32;
        tracing::info!(
            target: "aether_substrate::boot",
            workers = WORKERS,
            tick_hz = tick_hz,
            "componentless boot — load a component via aether.control.load_component",
        );

        // Hub connect AFTER every chassis sink is registered (issue #262).
        // Post-ADR-0070 phase 4: hub client lives in `aether-hub`.
        let hub = crate::hub::connect_hub_client(&boot, hub_url.as_deref())?;

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0074 §Decision 5: production chassis configures the
        // cross-class `wait_reply` aborter to broadcast
        // `SubstrateDying` before exit.
        let aborter: Arc<dyn aether_substrate::lifecycle::FatalAborter> = Arc::new(
            aether_substrate::lifecycle::OutboundFatalAborter::new(Arc::clone(&boot.outbound)),
        );

        let driver = HeadlessTimerCapability {
            boot,
            kind_tick,
            kind_frame_stats,
            tick_period,
            hub,
        };

        // ADR-0071 phase B: io / http / log compose through the
        // chassis_builder `.with()` chain. Boot order is declaration
        // order — log first so other capabilities' boot tracing routes
        // through the log capture.
        Builder::<HeadlessChassis>::new(registry, Arc::clone(&mailer))
            .with_aborter(aborter)
            .with_actor::<BroadcastCapability>(())
            .with_actor::<HandleCapability>(())
            .with_actor::<LogCapability>(())
            .with_actor::<ControlPlaneCapability>(control_plane_config)
            .with_actor::<IoCapability>(namespace_roots)
            .with_actor::<HttpCapability>(http)
            .with_actor::<TcpCapability>(())
            .with_actor::<HeadlessRenderCapability>(())
            .with_actor::<HeadlessWindowCapability>(())
            .with_actor::<UnsupportedTestBenchCapability>(())
            .with_log_drain::<LogCapability>()
            .driver(driver)
            .build()
    }
}
