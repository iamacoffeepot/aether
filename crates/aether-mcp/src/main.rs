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

use std::env;
use tokio::net::TcpListener;
use tokio::task;

// Link `aether-labyrinth` for its `#[transform]` link-time inventory
// contribution alone (issue 1908) — `describe_transforms` reads the local
// `aether_data::transforms()` inventory, and this binary references no
// labyrinth symbol otherwise. Without this the `inventory` crate drops a
// fully-unreferenced dependency's submissions and the reachability
// certifier transforms silently vanish from the inventory. `as _` is the
// side-effect-linkage form, so the crate is never named or otherwise used.
extern crate aether_labyrinth as _;

mod args;
mod reverse;
mod rpc;
#[cfg(test)]
mod test_chassis;
mod tools;

use std::sync::Arc;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::rpc::RpcSession;
use crate::tools::{ComponentCache, KindsCache, Mcp, ReverseNameCache};

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

    // Top-level process config for the MCP binary (the hub address it dials and
    // its own bind port) — process wiring in main, not a capability reading config.
    #[allow(clippy::disallowed_methods)]
    let hub_addr =
        env::var("AETHER_HUB_RPC_ADDR").unwrap_or_else(|_| DEFAULT_HUB_RPC_ADDR.to_owned());
    #[allow(clippy::disallowed_methods)]
    let mcp_port: u16 = env::var("AETHER_MCP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MCP_PORT);
    let mcp_addr = format!("127.0.0.1:{mcp_port}");

    // Dial the hub. The handshake is blocking, so run it on a
    // blocking-pool thread rather than stalling a runtime worker.
    tracing::info!(target: "aether_mcp", hub = %hub_addr, "dialing hub rpc server");
    let session = task::spawn_blocking({
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

    // Process-wide component-capability cache, shared into every
    // per-session `Mcp` — `load_component` / `replace_component`
    // populate it, `describe_component` reads it.
    let components: Arc<ComponentCache> = Arc::new(ComponentCache::default());

    // Process-wide per-engine reverse-name cache (ADR-0088 §8), shared
    // into every per-session `Mcp` — built lazily from each engine's
    // served `aether.inventory` manifest the first time MCP renders an id
    // for that engine, then reused across tool calls and sessions.
    let names: Arc<ReverseNameCache> = Arc::new(ReverseNameCache::default());

    // Process-wide per-engine kind-encode cache (ADR-0091), shared into
    // every per-session `Mcp` — prefilled lazily from the substrate's
    // static `descriptors::all()` baseline on first touch, refreshed
    // on encode miss via `aether.inventory.kinds`, then reused across
    // tool calls and sessions.
    let kinds: Arc<KindsCache> = Arc::new(KindsCache::default());

    let factory_session = Arc::clone(&session);
    let factory_components = Arc::clone(&components);
    let factory_names = Arc::clone(&names);
    let factory_kinds = Arc::clone(&kinds);
    let service = StreamableHttpService::new(
        move || {
            Ok(Mcp::new(
                Arc::clone(&factory_session),
                Arc::clone(&factory_components),
                Arc::clone(&factory_names),
                Arc::clone(&factory_kinds),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = TcpListener::bind(&mcp_addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(target: "aether_mcp", "mcp listener bound on http://{bound}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}
