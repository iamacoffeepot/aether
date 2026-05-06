//! Hub chassis (ADR-0034 Phase 1 / ADR-0035 / ADR-0071 phase 7d).
//!
//! Builder-shape adoption: [`HubChassis`] is a `Chassis` marker,
//! [`HubEnv`] carries resolved listener addresses, and
//! [`HubServerDriverCapability`] is the driver. [`HubChassis::build`]
//! returns a `BuiltChassis<HubChassis>` whose `run()` blocks the
//! calling thread on the tokio coordinator.
//!
//! The driver owns a multi-thread tokio runtime, the engine TCP
//! listener, the MCP HTTP server, the loopback drainers (inbound
//! tokio task + outbound std thread), and the SIGINT/SIGTERM →
//! children-cleanup → drop-substrate shutdown sequence — exactly the
//! coordinator behavior the previous `HubChassis::run_async` body
//! held inline. The Builder pattern adds no behavior change; it just
//! gives the hub the same compositional shape as desktop, headless,
//! and test-bench.
//!
//! ADR-0070 phase 5 / ADR-0071 phase 7d-2 will move
//! [`HubServerDriverCapability`] (and the underlying engine / mcp /
//! session / registry / log_store / loopback / spawn modules) out of
//! this binary crate into a new `aether-hub` library crate. This PR
//! lands the Capability shape in place; the relocation is mechanical
//! and tracked separately.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aether_capabilities::BroadcastCapability;
use aether_substrate::Chassis;
use aether_substrate::capability::BootError;
use aether_substrate::chassis_builder::{
    Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError,
};
use tokio::runtime::Runtime;

use crate::hub::process_capability::{ProcessCapability, ProcessCapabilityConfig};

use crate::hub::loopback::{
    LoopbackEngine, LoopbackHandle, run_inbound_drainer, spawn_outbound_drainer,
};
use crate::hub::{
    DEFAULT_ENGINE_PORT, DEFAULT_MCP_PORT, EngineRegistry, HubState, LogStore, PendingSpawns,
    SessionRegistry, run_engine_listener, run_mcp_server,
};

/// Grace window per child when the hub shuts down. Shorter than
/// `DEFAULT_TERMINATE_GRACE` (2s for individual MCP calls) because
/// shutdown is latency-sensitive: a user hitting Ctrl-C or a system
/// sending SIGTERM wants the hub gone promptly, and well-behaved
/// substrates exit on SIGTERM near-instantly. A child that ignores
/// SIGTERM gets SIGKILL'd after this window.
const SHUTDOWN_CHILD_GRACE: Duration = Duration::from_millis(1500);

/// ADR-0071 marker for the hub chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HubChassis>`] returned by
/// [`HubChassis::build`].
pub struct HubChassis;

impl Chassis for HubChassis {
    const PROFILE: &'static str = "hub";
    type Driver = HubServerDriverCapability;
    type Env = HubEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
    }
}

/// Resolved configuration the hub chassis takes at build time.
/// Embedders construct this and hand it to [`HubChassis::build`];
/// the binary `main()` reads `AETHER_ENGINE_PORT` / `AETHER_MCP_PORT`
/// via [`HubEnv::from_env`].
pub struct HubEnv {
    pub engine_addr: SocketAddr,
    pub mcp_addr: SocketAddr,
}

impl HubEnv {
    /// Read `AETHER_ENGINE_PORT` / `AETHER_MCP_PORT` from the
    /// environment; fall back to [`DEFAULT_ENGINE_PORT`] /
    /// [`DEFAULT_MCP_PORT`] when unset or unparseable. Binds both
    /// listeners on `127.0.0.1` — intentional for the current
    /// single-host development story.
    pub fn from_env() -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        let engine_port = env_port("AETHER_ENGINE_PORT").unwrap_or(DEFAULT_ENGINE_PORT);
        let mcp_port = env_port("AETHER_MCP_PORT").unwrap_or(DEFAULT_MCP_PORT);
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        Self {
            engine_addr: SocketAddr::new(loopback, engine_port),
            mcp_addr: SocketAddr::new(loopback, mcp_port),
        }
    }
}

impl HubChassis {
    /// Build a hub chassis: stand up the engine / session / spawn /
    /// log stores, boot the in-process [`LoopbackEngine`] (which
    /// constructs its own `SubstrateBoot` and registers it under
    /// `HUB_SELF_ENGINE_ID`), build the [`HubServerDriverCapability`]
    /// driver, and assemble a [`BuiltChassis<HubChassis>`] via the
    /// chassis_builder [`Builder`]. The hub chassis has no passive
    /// capabilities of its own today; future passives (an in-process
    /// log capability, etc.) compose via `Builder::with` between
    /// `new()` and `driver()`. The trait method [`Chassis::build`]
    /// forwards here.
    fn build_inner(env: HubEnv) -> Result<BuiltChassis<HubChassis>, BootError> {
        let HubEnv {
            engine_addr,
            mcp_addr,
        } = env;
        let registry = EngineRegistry::new();
        let sessions = SessionRegistry::new();
        let pending = PendingSpawns::new();
        let logs = LogStore::new();
        let loopback = LoopbackEngine::boot(&registry)?;
        let state = HubState::new(registry.clone(), sessions.clone(), logs.clone());

        // ADR-0078 Phase 1: ProcessCapability needs a tokio runtime
        // handle at cap-init time to drive its async spawn / wait /
        // terminate work from the dispatcher-thread sync handlers.
        // Build the runtime here (instead of in
        // `DriverCapability::boot`) so the Handle is available before
        // `Builder::with_actor::<ProcessCapability>(...)` runs `init`.
        // The driver later runs `block_on(coordinator)` against this
        // same runtime — no second runtime is constructed.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| BootError::Other(Box::new(e)))?;
        let rt_handle = rt.handle().clone();

        // The chassis_builder's `Builder::new` takes the substrate's
        // registry + mailer so passives have somewhere to claim
        // mailboxes against. The hub-chassis substrate lives inside
        // `LoopbackEngine.boot`; clone the handles before moving
        // `loopback` into the driver.
        let registry_arc = Arc::clone(&loopback.boot.registry);
        let mailer_arc = Arc::clone(&loopback.boot.queue);

        let driver = HubServerDriverCapability {
            engine_addr,
            mcp_addr,
            registry: registry.clone(),
            sessions,
            pending: pending.clone(),
            logs,
            state,
            loopback,
            rt,
        };

        Builder::<HubChassis>::new(registry_arc, mailer_arc)
            .with_actor::<BroadcastCapability>(())
            .with_actor::<ProcessCapability>(ProcessCapabilityConfig {
                engines: registry,
                pending,
                hub_engine_addr: engine_addr,
                runtime: rt_handle,
            })
            .driver(driver)
            .build()
    }
}

/// ADR-0071 driver capability for the hub chassis. Owns the tokio
/// runtime, the engine TCP listener, the MCP HTTP server, the
/// loopback drainers, and the SIGINT/SIGTERM coordinator. `boot`
/// constructs the multi-thread tokio runtime; `run` blocks on
/// `rt.block_on(coordinator)` and returns when either listener exits
/// or a shutdown signal arrives, after running the children-cleanup
/// + boot-drop sequence.
pub struct HubServerDriverCapability {
    engine_addr: SocketAddr,
    mcp_addr: SocketAddr,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
    logs: LogStore,
    state: Arc<HubState>,
    loopback: LoopbackEngine,
    /// Tokio runtime constructed in `HubChassis::build_inner` so
    /// `ProcessCapability::init` could grab a `Handle` at cap boot.
    /// `boot` moves this into [`HubServerDriverRunning`]; `run`
    /// blocks on `block_on(coordinator)` against it.
    rt: Runtime,
}

/// Post-boot handle for [`HubServerDriverCapability`]. Holds the constructed
/// tokio runtime + every state handle the coordinator needs. `run`
/// drains the runtime and returns once shutdown completes.
pub struct HubServerDriverRunning {
    rt: Runtime,
    engine_addr: SocketAddr,
    mcp_addr: SocketAddr,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
    logs: LogStore,
    state: Arc<HubState>,
    loopback: LoopbackEngine,
    /// Handle to the booted [`ProcessCapability`]. The shutdown
    /// coordinator calls [`ProcessCapability::shutdown_all`] on this
    /// to terminate every spawned child before dropping the loopback
    /// substrate. `None` is unreachable in production (the cap is
    /// always booted on the hub chassis); kept Optional only because
    /// `DriverCtx::actor` returns Option for type-system purity.
    process_cap: Option<Arc<ProcessCapability>>,
}

impl DriverCapability for HubServerDriverCapability {
    type Running = HubServerDriverRunning;

    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let HubServerDriverCapability {
            engine_addr,
            mcp_addr,
            registry,
            sessions,
            pending,
            logs,
            state,
            loopback,
            rt,
        } = self;
        let process_cap = ctx.actor::<ProcessCapability>();
        Ok(HubServerDriverRunning {
            rt,
            engine_addr,
            mcp_addr,
            registry,
            sessions,
            pending,
            logs,
            state,
            loopback,
            process_cap,
        })
    }
}

impl DriverRunning for HubServerDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let HubServerDriverRunning {
            rt,
            engine_addr,
            mcp_addr,
            registry,
            sessions,
            pending,
            logs,
            state,
            loopback,
            process_cap,
        } = *self;

        rt.block_on(coordinator(
            engine_addr,
            mcp_addr,
            registry,
            sessions,
            pending,
            logs,
            state,
            loopback,
            process_cap,
        ));

        Ok(())
    }
}

/// The body that pre-Builder lived inside `HubChassis::run_async`.
/// Spawns the inbound + outbound loopback drainers, the engine
/// listener, and the MCP server; `tokio::select!`s on all four +
/// the shutdown-signal future; then asks `ProcessCapability` to
/// terminate every spawned child and drops the substrate boot in
/// deterministic order.
#[allow(clippy::too_many_arguments)]
async fn coordinator(
    engine_addr: SocketAddr,
    mcp_addr: SocketAddr,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
    logs: LogStore,
    state: Arc<HubState>,
    loopback: LoopbackEngine,
    process_cap: Option<Arc<ProcessCapability>>,
) {
    let LoopbackEngine {
        boot,
        inbound_rx,
        outbound_rx,
    } = loopback;

    let loopback_handle = LoopbackHandle::from_boot(&boot);

    // Loopback drainers. The inbound task runs alongside the TCP +
    // MCP listeners; the outbound drainer runs on a dedicated
    // std::thread because `std::sync::mpsc::Receiver` blocks
    // synchronously. Both exit when their channel closes (at drop
    // time on process shutdown).
    let loopback_inbound_task = tokio::spawn(run_inbound_drainer(
        inbound_rx,
        Arc::clone(&boot.registry),
        Arc::clone(&boot.queue),
    ));
    let _loopback_outbound_thread = spawn_outbound_drainer(
        outbound_rx,
        registry.clone(),
        sessions.clone(),
        logs.clone(),
    );

    let engine_task = tokio::spawn(run_engine_listener(
        engine_addr,
        registry,
        sessions,
        pending,
        logs,
        loopback_handle,
    ));
    let mcp_task = tokio::spawn(run_mcp_server(mcp_addr, state));

    tokio::select! {
        r = engine_task => log_exit("engine listener", r),
        r = mcp_task => log_exit("mcp listener", r),
        r = loopback_inbound_task => log_exit("loopback inbound", r.map(Ok)),
        sig = shutdown_signal() => {
            eprintln!("aether-substrate-hub: {sig} received, shutting down");
        }
    }

    // Drain spawned children explicitly before dropping `boot` and
    // the tokio runtime. `kill_on_drop` would reap them on Arc
    // drop, but the drop ordering across tasks holding the cap's
    // children map isn't deterministic — an explicit pass guarantees
    // every child gets SIGTERM + a grace window (+ SIGKILL
    // escalation) regardless of which task drops its Arc last.
    // Skipping this is what orphaned children into init on hub
    // SIGTERM pre-fix.
    if let Some(cap) = process_cap.as_ref() {
        cap.shutdown_all(SHUTDOWN_CHILD_GRACE).await;
    }

    // `boot` drops here — scheduler workers join, HubOutbound's
    // Sender drops (closing the outbound channel), which lets
    // the outbound drainer thread exit naturally. We don't
    // explicitly join the thread: process shutdown handles any
    // still-draining frames.
    drop(boot);
}

/// Resolves when either SIGINT (Ctrl-C) or SIGTERM arrives on Unix;
/// on non-Unix falls back to `ctrl_c()` since tokio doesn't expose
/// named signals outside Unix. Returns a short label for the log line.
///
/// Why both signals: interactive shells deliver SIGINT, but process
/// supervisors (systemd, supervisord), shell utilities (`pkill`,
/// `kill` without `-9`), and CI cancellation all send SIGTERM.
/// Ignoring SIGTERM means `pkill -f aether-substrate-hub` kills the
/// hub without running drops, orphaning its spawned children.
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("aether-substrate-hub: SIGTERM handler install failed: {e}");
                // Fall through to ctrl_c-only — better than nothing.
                let _ = tokio::signal::ctrl_c().await;
                return "SIGINT";
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("aether-substrate-hub: SIGINT handler install failed: {e}");
                // Wait on SIGTERM only; SIGINT default action still kills.
                let _ = sigterm.recv().await;
                return "SIGTERM";
            }
        };
        tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}

fn env_port(name: &str) -> Option<u16> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn log_exit(label: &str, result: Result<std::io::Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => eprintln!("aether-substrate-hub: {label} exited"),
        Ok(Err(e)) => eprintln!("aether-substrate-hub: {label} error: {e}"),
        Err(e) => eprintln!("aether-substrate-hub: {label} join error: {e}"),
    }
}
