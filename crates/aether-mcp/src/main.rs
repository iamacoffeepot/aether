//! `aether-mcp` — the out-of-process MCP coordinator (issue 763 P5).
//!
//! Claude Code points `.mcp.json` at this binary. It owns the `rmcp`
//! HTTP server and the tool surface, and reaches engines purely as an
//! RPC *client*: it dials the hub's `RpcServerCapability` once at
//! startup and relays every tool call as a wire `Call`. There is no
//! actor system in this process — just the wire.
//!
//! Until the P5d cutover the embedded `aether-substrate-bundle::hub::mcp`
//! server keeps running on its own port; this binary is additive and
//! defaults to a distinct port so the two coexist.

mod args;
mod rpc;
mod tools;

use std::net::SocketAddr;
use std::sync::Arc;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::rpc::RpcSession;
use crate::tools::Mcp;

/// Default port the MCP HTTP server binds. Distinct from the embedded
/// hub MCP's 8888 so the two coexist until the P5d cutover.
/// Overridable via `AETHER_MCP_PORT`.
const DEFAULT_MCP_PORT: u16 = 8890;

/// Default hub RPC address `aether-mcp` dials. The hub must be launched
/// with `AETHER_RPC_PORT` set to this port. Overridable via
/// `AETHER_HUB_RPC_ADDR`.
const DEFAULT_HUB_RPC_ADDR: &str = "127.0.0.1:8901";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AETHER_LOG_FILTER")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let hub_addr =
        std::env::var("AETHER_HUB_RPC_ADDR").unwrap_or_else(|_| DEFAULT_HUB_RPC_ADDR.to_owned());
    let mcp_port: u16 = std::env::var("AETHER_MCP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MCP_PORT);
    let mcp_addr = SocketAddr::from(([127, 0, 0, 1], mcp_port));

    // Dial the hub. The handshake is blocking, so run it on a
    // blocking-pool thread rather than stalling a runtime worker.
    tracing::info!(target: "aether_mcp", hub = %hub_addr, "dialing hub rpc server");
    let session = tokio::task::spawn_blocking({
        let hub_addr = hub_addr.clone();
        move || RpcSession::connect(&hub_addr)
    })
    .await
    .expect("connect task panicked")?;
    let session = Arc::new(session);
    tracing::info!(
        target: "aether_mcp",
        server = ?session.server(),
        "hub rpc connection established",
    );

    let factory_session = Arc::clone(&session);
    let service = StreamableHttpService::new(
        move || Ok(Mcp::new(Arc::clone(&factory_session))),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(mcp_addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(target: "aether_mcp", "mcp listener bound on http://{bound}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}
