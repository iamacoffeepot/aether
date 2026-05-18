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

use super::driver::{HeadlessTimerCapability, parse_tick_hz_env};

/// Marker type for the headless chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HeadlessChassis>`] returned
/// by [`Self::build`]. Same shape as [`crate::DesktopChassis`] post
/// ADR-0071 phase 3.
pub struct HeadlessChassis;

impl Chassis for HeadlessChassis {
    const PROFILE: &'static str = "headless";
    type Driver = HeadlessTimerCapability;
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
    /// Issue 745: optional worker-pool size override. Populated from
    /// `AETHER_WORKERS`; `None` keeps `PoolConfig::default()` behavior
    /// (`available_parallelism() - 1`, min 1).
    pub workers: Option<usize>,
}

impl HeadlessEnv {
    /// Read every chassis-relevant env var into a fresh `HeadlessEnv`.
    /// The single env-reading edge for the headless chassis (per
    /// issue 464). Tests bypass this by constructing `HeadlessEnv`
    /// directly.
    #[must_use]
    pub fn from_env() -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        let http = HttpConf::from_env();
        let namespace_roots = NamespaceRoots::from_env();
        let tick_hz = parse_tick_hz_env();
        let tick_period = Duration::from_nanos(1_000_000_000 / u64::from(tick_hz));
        // `AETHER_RPC_PORT` has no default — absent means RpcServer
        // doesn't boot. Binds `127.0.0.1`, matching the hub chassis.
        let rpc_addr = crate::hub::rpc_port_from_env()
            .map(|p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p));
        let workers = parse_workers_env();
        Self {
            namespace_roots,
            http,
            tick_period,
            rpc_addr,
            workers,
        }
    }
}

/// Parse `AETHER_WORKERS`. Unset → `None` (chassis falls back to
/// [`aether_substrate::scheduler::PoolConfig::default`]); positive →
/// `Some(n)`; `0` → `Some(1)` with a warn (the pool requires at least
/// one worker); unparseable → `None` with a warn. Issue 745.
fn parse_workers_env() -> Option<usize> {
    let raw = std::env::var("AETHER_WORKERS").ok()?;
    match raw.trim().parse::<usize>() {
        Ok(0) => {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                "AETHER_WORKERS=0 — clamping to 1",
            );
            Some(1)
        }
        Ok(n) => Some(n),
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                error = %e,
                "AETHER_WORKERS unparseable — falling back to PoolConfig::default",
            );
            None
        }
    }
}

impl HeadlessChassis {
    /// Build the headless chassis: stand up substrate-core internals,
    /// register the audio fail-fast sink, connect the hub, compose
    /// the native passives (broadcast/handle/log/control/io/http plus
    /// the headless render / window / test-bench fail-fast caps)
    /// through the `chassis_builder` `.with()` chain, then wrap the
    /// timer in a [`HeadlessTimerCapability`] and hand it to the
    /// builder.
    fn build_inner(env: HeadlessEnv) -> Result<BuiltChassis<Self>, BootError> {
        let HeadlessEnv {
            namespace_roots,
            http,
            tick_period,
            rpc_addr,
            workers,
        } = env;

        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION")).build()?;
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
        // as the AETHER_DIAGNOSTICS sink in `boot.rs::register_inline`.
        let kind_set_master_gain = boot
            .registry
            .kind_id(SetMasterGain::NAME)
            .expect("SetMasterGain registered");
        let outbound_for_audio_sink = Arc::clone(&boot.outbound);
        boot.registry.register_inline(
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

        // Tick rates are bounded well below `u32::MAX` Hz (typically
        // 60-240 Hz); the `u128 → u32` narrowing is safe in practice.
        #[allow(clippy::cast_possible_truncation)]
        let tick_hz = (Duration::from_secs(1).as_nanos() / tick_period.as_nanos().max(1)) as u32;
        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
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
        let mut builder = Builder::<Self>::new(registry, Arc::clone(&mailer))
            .with_aborter(aborter)
            .with_workers(workers)
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

#[cfg(test)]
mod tests {
    use super::parse_workers_env;
    use std::sync::Mutex;

    /// Process-wide guard around `AETHER_WORKERS` env mutation —
    /// `cargo test` parallelises within a binary, so each parser test
    /// has to serialise its set/remove pair. Shared with the desktop
    /// chassis test would require a crate-level module; one per chassis
    /// is fine given there are four tests total.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Safety: this test owns the AETHER_WORKERS slot for the
        // duration of the closure via ENV_LOCK; no other thread inside
        // the same test binary mutates it concurrently. Edition-2024
        // marked the env mutators unsafe due to non-test signal-handler
        // races that don't apply here.
        unsafe {
            match value {
                Some(v) => std::env::set_var("AETHER_WORKERS", v),
                None => std::env::remove_var("AETHER_WORKERS"),
            }
        }
        let out = f();
        // SAFETY: same justification as the prior block — this test
        // still owns the `AETHER_WORKERS` slot via `ENV_LOCK`.
        unsafe {
            std::env::remove_var("AETHER_WORKERS");
        }
        out
    }

    #[test]
    fn parse_workers_unset_returns_none() {
        let parsed = with_env(None, parse_workers_env);
        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_workers_positive_returns_some() {
        let parsed = with_env(Some("4"), parse_workers_env);
        assert_eq!(parsed, Some(4));
    }

    #[test]
    fn parse_workers_zero_clamps_to_one() {
        let parsed = with_env(Some("0"), parse_workers_env);
        assert_eq!(parsed, Some(1));
    }

    #[test]
    fn parse_workers_unparseable_returns_none() {
        let parsed = with_env(Some("abc"), parse_workers_env);
        assert_eq!(parsed, None);
    }
}
