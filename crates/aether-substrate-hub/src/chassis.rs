//! Hub chassis (ADR-0034 Phase 1 / ADR-0035).
//!
//! This is the hub running as a `Chassis` implementation on top of
//! `aether-substrate-core`'s universal lifecycle trait. In this phase
//! the hub-chassis still does all of its work in native rust — the
//! TCP listener, session registry, spawn supervisor, and MCP surface
//! live in the `aether-hub` library crate and the chassis just owns
//! the tokio runtime that drives them. No components are hosted yet;
//! that lands in later phases of ADR-0034.
//!
//! The trait wrapper exists for two reasons: (1) every substrate
//! deployment is now a `Chassis` — desktop, headless, and hub —
//! which gives the codebase one mental model for "a running engine";
//! and (2) the capability flags (`has_tcp_listener = true`) let
//! future introspection tooling distinguish hub-chassis from the GPU
//! chassis without a runtime branch.

use std::net::SocketAddr;
use std::sync::Arc;

use aether_substrate_core::{Chassis, ChassisCapabilities};

use crate::loopback::{
    LoopbackEngine, LoopbackHandle, run_inbound_drainer, spawn_outbound_drainer,
};
use crate::{
    DEFAULT_ENGINE_PORT, DEFAULT_MCP_PORT, EngineRegistry, HubState, LogStore, PendingSpawns,
    SessionRegistry, run_engine_listener, run_mcp_server,
};

/// Hub chassis handle. Holds the bound addresses + shared state
/// handles and the in-process loopback engine (ADR-0034 Phase 2
/// sub-phase A); `run(self)` builds a tokio runtime and drives the
/// listeners + the loopback drainers to termination.
pub struct HubChassis {
    engine_addr: SocketAddr,
    mcp_addr: SocketAddr,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
    logs: LogStore,
    state: Arc<HubState>,
    loopback: LoopbackEngine,
}

impl HubChassis {
    /// Build a hub chassis with the given listener addresses. Fresh
    /// registry / session / spawn / log stores are created inline —
    /// they never outlive the chassis. Boots the in-process
    /// `SubstrateBoot` and registers it in the engine registry
    /// under `HUB_SELF_ENGINE_ID` before returning, so MCP tools
    /// see the hub-self engine from the moment `new()` completes.
    pub fn new(engine_addr: SocketAddr, mcp_addr: SocketAddr) -> wasmtime::Result<Self> {
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
        Ok(Self {
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

    /// Read `AETHER_ENGINE_PORT` / `AETHER_MCP_PORT` from the
    /// environment; fall back to `DEFAULT_ENGINE_PORT` /
    /// `DEFAULT_MCP_PORT` when unset or unparseable. Binds both
    /// listeners on `127.0.0.1` — intentional for the current
    /// single-host development story.
    pub fn from_env() -> wasmtime::Result<Self> {
        use std::net::{IpAddr, Ipv4Addr};
        let engine_port = env_port("AETHER_ENGINE_PORT").unwrap_or(DEFAULT_ENGINE_PORT);
        let mcp_port = env_port("AETHER_MCP_PORT").unwrap_or(DEFAULT_MCP_PORT);
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        Self::new(
            SocketAddr::new(loopback, engine_port),
            SocketAddr::new(loopback, mcp_port),
        )
    }

    async fn run_async(self) {
        let HubChassis {
            engine_addr,
            mcp_addr,
            registry,
            sessions,
            pending,
            logs,
            state,
            loopback,
        } = self;

        let LoopbackEngine {
            boot,
            inbound_rx,
            outbound_rx,
        } = loopback;

        let loopback_handle = LoopbackHandle::from_boot(&boot);

        // Loopback drainers. The inbound task runs alongside the
        // TCP + MCP listeners; the outbound drainer runs on a
        // dedicated std::thread because `std::sync::mpsc::Receiver`
        // blocks synchronously. Both exit when their channel closes
        // (at drop time on process shutdown).
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
            _ = tokio::signal::ctrl_c() => {
                eprintln!("aether-substrate-hub: shutting down");
            }
        }

        // `boot` drops here — scheduler workers join, HubOutbound's
        // Sender drops (closing the outbound channel), which lets
        // the outbound drainer thread exit naturally. We don't
        // explicitly join the thread: process shutdown handles any
        // still-draining frames.
        drop(boot);
    }
}

impl Chassis for HubChassis {
    const KIND: &'static str = "hub";
    const CAPABILITIES: ChassisCapabilities = ChassisCapabilities {
        has_gpu: false,
        has_window: false,
        has_tcp_listener: true,
    };

    fn run(self) -> wasmtime::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(self.run_async());
        Ok(())
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
