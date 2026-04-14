// aether-hub: central broker between Claude MCP clients and engine
// processes. This crate owns the engine-facing TCP listener, handshake,
// and heartbeat/reaping per ADR-0006. The Claude-facing rmcp transport
// and MCP tool surface land in a follow-up PR.

use std::net::SocketAddr;

use tokio::net::TcpListener;

mod encoder;
mod engine;
mod mcp;
mod registry;
mod session;
mod spawn;

pub use encoder::{EncodeError, encode_pod};

pub use engine::HEARTBEAT_INTERVAL;
pub use engine::READ_TIMEOUT;
pub use mcp::{DEFAULT_MCP_PORT, HubState, run_mcp_server};
pub use registry::{EngineRecord, EngineRegistry};
pub use session::{
    QueuedMail, SESSION_CHANNEL_CAPACITY, SessionHandle, SessionRecord, SessionRegistry,
};
pub use spawn::{DEFAULT_HANDSHAKE_TIMEOUT, PendingSpawns, SpawnError, SpawnOpts, spawn_substrate};

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
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("aether-hub: engine listener bound on {bound}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let sessions = sessions.clone();
        let pending = pending.clone();
        tokio::spawn(async move {
            if let Err(e) = engine::handle_connection(stream, registry, sessions, pending).await {
                eprintln!("aether-hub: engine {peer} dropped: {e}");
            }
        });
    }
}
