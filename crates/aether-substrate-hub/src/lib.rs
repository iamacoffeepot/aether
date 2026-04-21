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

use aether_hub::{
    DEFAULT_ENGINE_PORT, DEFAULT_MCP_PORT, EngineRegistry, HubState, LogStore, PendingSpawns,
    SessionRegistry, run_engine_listener, run_mcp_server,
};
use aether_substrate_core::{Chassis, ChassisCapabilities};

/// Hub chassis handle. Holds the bound addresses + shared state
/// handles; `run(self)` builds a tokio runtime and drives the
/// listeners to termination.
pub struct HubChassis {
    engine_addr: SocketAddr,
    mcp_addr: SocketAddr,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
    logs: LogStore,
    state: Arc<HubState>,
}

impl HubChassis {
    /// Build a hub chassis with the given listener addresses. Fresh
    /// registry / session / spawn / log stores are created inline —
    /// they never outlive the chassis.
    pub fn new(engine_addr: SocketAddr, mcp_addr: SocketAddr) -> Self {
        let registry = EngineRegistry::new();
        let sessions = SessionRegistry::new();
        let pending = PendingSpawns::new();
        let logs = LogStore::new();
        let state = HubState::new(
            registry.clone(),
            sessions.clone(),
            pending.clone(),
            logs.clone(),
            engine_addr,
        );
        Self {
            engine_addr,
            mcp_addr,
            registry,
            sessions,
            pending,
            logs,
            state,
        }
    }

    /// Read `AETHER_ENGINE_PORT` / `AETHER_MCP_PORT` from the
    /// environment; fall back to `DEFAULT_ENGINE_PORT` /
    /// `DEFAULT_MCP_PORT` when unset or unparseable. Binds both
    /// listeners on `127.0.0.1` — intentional for the current
    /// single-host development story.
    pub fn from_env() -> Self {
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
        } = self;

        let engine_task = tokio::spawn(run_engine_listener(
            engine_addr,
            registry,
            sessions,
            pending,
            logs,
        ));
        let mcp_task = tokio::spawn(run_mcp_server(mcp_addr, state));

        tokio::select! {
            r = engine_task => log_exit("engine listener", r),
            r = mcp_task => log_exit("mcp listener", r),
            _ = tokio::signal::ctrl_c() => {
                eprintln!("aether-substrate-hub: shutting down");
            }
        }
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
