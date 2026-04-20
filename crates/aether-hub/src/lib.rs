//! aether-hub: central broker between Claude MCP clients and engine
//! processes. Owns the engine-facing TCP listener, handshake, and
//! heartbeat/reaping per ADR-0006, plus the Claude-facing rmcp
//! transport and the MCP tool surface.

use std::net::SocketAddr;

use tokio::net::TcpListener;

mod decoder;
mod encoder;
mod engine;
mod log_store;
mod mcp;
mod registry;
mod session;
mod spawn;

pub use decoder::{DecodeError, decode_schema};
pub use encoder::{EncodeError, encode_schema};

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
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("aether-hub: engine listener bound on {bound}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let sessions = sessions.clone();
        let pending = pending.clone();
        let logs = logs.clone();
        tokio::spawn(async move {
            if let Err(e) =
                engine::handle_connection(stream, registry, sessions, pending, logs).await
            {
                eprintln!("aether-hub: engine {peer} dropped: {e}");
            }
        });
    }
}
