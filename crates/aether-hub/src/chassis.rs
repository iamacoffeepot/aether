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

use aether_substrate_core::Chassis;
use aether_substrate_core::capability::BootError;
use aether_substrate_core::chassis_builder::{
    Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, NoDriver, RunError,
};
use tokio::runtime::Runtime;

use crate::loopback::{
    LoopbackEngine, LoopbackHandle, run_inbound_drainer, spawn_outbound_drainer,
};
use crate::spawn::terminate_substrate;
use crate::{
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
        let state = HubState::new(
            registry.clone(),
            sessions.clone(),
            pending.clone(),
            logs.clone(),
            engine_addr,
        );

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
            registry,
            sessions,
            pending,
            logs,
            state,
            loopback,
        };

        Builder::<HubChassis, NoDriver>::new(registry_arc, mailer_arc)
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
}

impl DriverCapability for HubServerDriverCapability {
    type Running = HubServerDriverRunning;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| BootError::Other(Box::new(e)))?;
        let HubServerDriverCapability {
            engine_addr,
            mcp_addr,
            registry,
            sessions,
            pending,
            logs,
            state,
            loopback,
        } = self;
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
        ));

        Ok(())
    }
}

/// The body that pre-Builder lived inside `HubChassis::run_async`.
/// Spawns the inbound + outbound loopback drainers, the engine
/// listener, and the MCP server; `tokio::select!`s on all four +
/// the shutdown-signal future; then runs `terminate_all_children` and
/// drops the substrate boot in deterministic order.
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

    // Hold a separate clone for the shutdown handler — the
    // `registry` binding is moved into `run_engine_listener`.
    let registry_for_shutdown = registry.clone();

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
    // drop, but the drop ordering across tasks holding registry
    // clones isn't deterministic — an explicit pass guarantees
    // every child gets SIGTERM + a grace window (+ SIGKILL
    // escalation) regardless of which task drops its Arc last.
    // Skipping this is what orphaned children into init on hub
    // SIGTERM pre-fix.
    terminate_all_children(&registry_for_shutdown).await;

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

/// Terminate every spawned substrate in parallel. Each call reuses
/// the same SIGTERM → grace → SIGKILL machinery that the
/// `terminate_substrate` MCP tool uses, so hub-shutdown cleanup and
/// per-engine cleanup share a code path. Errors are logged per child
/// but don't abort the loop — we want to reach every child.
async fn terminate_all_children(registry: &EngineRegistry) {
    let children = registry.drain_spawned_children();
    if children.is_empty() {
        return;
    }
    eprintln!(
        "aether-substrate-hub: terminating {} spawned child(ren)",
        children.len()
    );
    let handles: Vec<_> = children
        .into_iter()
        .map(|(id, child)| {
            tokio::spawn(async move {
                if let Err(e) = terminate_substrate(child, SHUTDOWN_CHILD_GRACE).await {
                    eprintln!("aether-substrate-hub: shutdown terminate {id:?}: {e}");
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.await;
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

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use aether_data::{EngineId, Uuid};
    use tokio::process::Command;

    use super::*;

    /// Spawn a `sleep 60` child so the terminate path has something
    /// real to signal + reap. Mirrors the pattern the
    /// `terminate_substrate` tests use in `spawn.rs`.
    fn spawn_sleep() -> tokio::process::Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh")
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn terminate_all_children_reaps_every_adopted_child() {
        let registry = EngineRegistry::new();
        let ids: Vec<EngineId> = (0..3)
            .map(|i| EngineId(Uuid::from_u128(0xC0FFEE + i as u128)))
            .collect();
        let children: Vec<_> = (0..3).map(|_| spawn_sleep()).collect();
        let pids: Vec<u32> = children
            .iter()
            .map(|c| c.id().expect("pid available"))
            .collect();
        for (id, child) in ids.iter().zip(children) {
            registry.adopt_child(*id, child);
        }

        terminate_all_children(&registry).await;

        // Registry is drained — `terminate_all_children` removed every
        // entry via `drain_spawned_children`.
        assert_eq!(
            registry.drain_spawned_children().len(),
            0,
            "terminate_all_children consumed everything"
        );
        // `libc::kill(pid, 0)` probes liveness: returns 0 if the
        // process exists and we can signal it, `-1` + `ESRCH` when
        // the pid is gone (reaped or never existed). `sh -c sleep 60`
        // handles SIGTERM immediately, so all three should be dead by
        // the time `terminate_all_children` returns.
        for pid in pids {
            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            let errno = std::io::Error::last_os_error().raw_os_error();
            assert!(
                rc != 0 && errno == Some(libc::ESRCH),
                "pid {pid} still signalable after shutdown (rc={rc}, errno={errno:?})"
            );
        }
    }
}
