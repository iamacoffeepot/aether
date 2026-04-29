//! aether-substrate-hub: hub chassis + the machinery it wraps. Owns
//! the engine-facing TCP listener, handshake, and heartbeat/reaping
//! per ADR-0006, plus the Claude-facing rmcp transport and the MCP
//! tool surface. ADR-0034 Phase 1 absorbed this code from the
//! retired `aether-hub` crate; `HubChassis` (below) is the `Chassis`
//! trait wrapper that owns the tokio runtime and drives both
//! listeners to termination.

use std::net::SocketAddr;

use tokio::net::TcpListener;

mod chassis;
mod decoder;
mod engine;
mod log_store;
mod loopback;
mod mcp;
mod registry;
mod session;
mod spawn;

// `encode_schema` lives in `aether-params-codec` so callers outside
// the hub (smoke runner, future tooling) can use the same JSON →
// wire-bytes path without depending on the hub binary crate.
pub use aether_params_codec::{EncodeError, encode_schema};
pub use chassis::HubChassis;
pub use decoder::{DecodeError, decode_schema};
pub use loopback::{HUB_SELF_ENGINE_ID, LoopbackEngine, LoopbackHandle};

pub use engine::HEARTBEAT_INTERVAL;
pub use engine::READ_TIMEOUT;
pub use log_store::{LogStore, ReadResult as LogReadResult};
pub use mcp::{DEFAULT_MCP_PORT, HubState, run_mcp_server};
pub use registry::{EngineRecord, EngineRegistry};
pub use session::{
    QueuedMail, SESSION_CHANNEL_CAPACITY, SessionHandle, SessionRecord, SessionRegistry,
};
pub use spawn::{
    DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_TERMINATE_GRACE, PendingSpawns, SpawnError, SpawnOpts,
    TerminateOutcome, spawn_substrate, terminate_substrate,
};

/// Default port the hub binds for engine TCP clients. ADR-0006 V0 fixes
/// this; `AETHER_ENGINE_PORT` overrides.
pub const DEFAULT_ENGINE_PORT: u16 = 8889;

/// Run the engine listener loop on `addr`, dispatching each accepted
/// connection to a per-connection task. Returns on listener error only;
/// individual connection failures are logged and isolated.
pub async fn run_engine_listener(
    addr: SocketAddr,
    registry: EngineRegistry,
    sessions: SessionRegistry,
    pending: PendingSpawns,
    logs: LogStore,
    loopback: loopback::LoopbackHandle,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("aether-substrate-hub: engine listener bound on {bound}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let sessions = sessions.clone();
        let pending = pending.clone();
        let logs = logs.clone();
        let loopback = loopback.clone();
        tokio::spawn(async move {
            if let Err(e) =
                engine::handle_connection(stream, registry, sessions, pending, logs, loopback).await
            {
                eprintln!("aether-substrate-hub: engine {peer} dropped: {e}");
            }
        });
    }
}
