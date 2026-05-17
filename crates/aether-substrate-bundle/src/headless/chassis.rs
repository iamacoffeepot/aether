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
//! handles `aether.test_bench`. `aether.control.platform_info` (now
//! a deleted kind name from a retired namespace) was
//! deleted as a kind in Phase 4 — no replacement, no MCP path until
//! issue 603 §F2 revives the per-domain shape.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{
    ComponentHostCapability, ComponentHostConfig, FsCapability, HandleCapability,
    HeadlessRenderCapability, HeadlessWindowCapability, HttpCapability, InputCapability,
    InputConfig, LogCapability, TcpCapability, UnsupportedTestBenchCapability, fs::NamespaceRoots,
    http::HttpConfig as HttpConf, trace::TraceObserverCapability,
};
use aether_data::Kind;
use aether_kinds::{SetMasterGain, SetMasterGainResult, Tick};
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
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
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConf,
    pub tick_period: Duration,
    /// Issue 763 P2: optional `aether.rpc.server` bind address.
    /// Populated from `AETHER_RPC_PORT`; `None` (default) skips booting
    /// `RpcServerCapability` so existing chassis behavior is unchanged.
    pub rpc_addr: Option<SocketAddr>,
}

impl HeadlessEnv {
    /// Read every chassis-relevant env var into a fresh `HeadlessEnv`.
    /// The single env-reading edge for the headless chassis (per
    /// issue 464). Tests bypass this by constructing `HeadlessEnv`
    /// directly.
    pub fn from_env() -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        let http = HttpConf::from_env();
        let namespace_roots = NamespaceRoots::from_env();
        let tick_hz = parse_tick_hz_env();
        let tick_period = Duration::from_nanos(1_000_000_000 / u64::from(tick_hz));
        // `AETHER_RPC_PORT` has no default — absent means RpcServer
        // doesn't boot. Binds `127.0.0.1`, matching the hub chassis.
        let rpc_addr = std::env::var("AETHER_RPC_PORT")
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .map(|p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p));
        HeadlessEnv {
            namespace_roots,
            http,
            tick_period,
            rpc_addr,
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
            namespace_roots,
            http,
            tick_period,
            rpc_addr,
        } = env;

        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION")).build()?;
        let _ = WORKERS;
        let component_host_config = ComponentHostConfig {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        let input_config = InputConfig::default();

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");

        // Audio nop sink — NoteOn/NoteOff fall through silently;
        // SetMasterGain replies Err so agents fail fast rather than
        // hang on a chassis with no audio device.
        //
        // Issue 838: registered as `Sink` (not `Closure`) so the
        // `Mailer::push` route brackets the inline handler with
        // `Received`/`Finished`. The handler does its work
        // synchronously (calls `send_reply` directly); there's no
        // actor dispatch loop behind it, so without the bracket
        // any chain that mails `aether.audio` from the headless
        // chassis leaks `in_flight` and never settles. Same shape
        // as the AETHER_DIAGNOSTICS sink in `boot.rs::register_sink`.
        let kind_set_master_gain = boot
            .registry
            .kind_id(SetMasterGain::NAME)
            .expect("SetMasterGain registered");
        let outbound_for_audio_sink = Arc::clone(&boot.outbound);
        boot.registry.register_sink(
            "aether.audio",
            Arc::new(
                move |dispatch: aether_substrate::mail::registry::MailDispatch<'_>| {
                    if dispatch.kind == kind_set_master_gain {
                        outbound_for_audio_sink.send_reply(
                            dispatch.sender,
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
            "componentless boot — load a component via aether.component.load",
        );

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0074 §Decision 5: production chassis configures the
        // cross-class `wait_reply` aborter so the substrate exits via
        // `lifecycle::fatal_abort` instead of unwinding.
        let aborter: Arc<dyn aether_substrate::runtime::lifecycle::FatalAborter> = Arc::new(
            aether_substrate::runtime::lifecycle::OutboundFatalAborter::new(Arc::clone(
                &boot.outbound,
            )),
        );

        let driver = HeadlessTimerCapability {
            boot,
            kind_tick,
            tick_period,
        };

        // ADR-0071 phase B: io / http / log compose through the
        // chassis_builder `.with()` chain. Boot order is declaration
        // order — log first so other capabilities' boot tracing routes
        // through the log capture.
        let mut builder = Builder::<HeadlessChassis>::new(registry, Arc::clone(&mailer))
            .with_aborter(aborter)
            .with_actor::<HandleCapability>(())
            .with_actor::<LogCapability>(())
            .with_actor::<TraceObserverCapability>(())
            .with_actor::<InputCapability>(input_config)
            .with_actor::<ComponentHostCapability>(component_host_config)
            .with_actor::<FsCapability>(namespace_roots)
            .with_actor::<HttpCapability>(http)
            .with_actor::<TcpCapability>(())
            .with_actor::<HeadlessRenderCapability>(())
            .with_actor::<HeadlessWindowCapability>(())
            .with_actor::<UnsupportedTestBenchCapability>(());
        // Issue 763 P2: boot the RPC server only when `AETHER_RPC_PORT`
        // is set, mirroring the hub chassis. The substrate becomes an
        // RPC server peer that a hub (or any client) connects out to.
        if let Some(rpc_addr) = rpc_addr {
            builder = builder.with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: rpc_addr.to_string(),
                peer_kind: PeerKind::Substrate {
                    engine_name: "aether-headless".into(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
            });
        }
        builder
            .with_log_drain::<LogCapability>()
            .driver(driver)
            .build()
    }
}
