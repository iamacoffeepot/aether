// Thin binary entry point: parse ports from env, spin up the engine
// TCP listener and the MCP (streamable-HTTP) listener concurrently,
// wait for Ctrl-C.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use aether_hub::{
    DEFAULT_ENGINE_PORT, DEFAULT_MCP_PORT, EngineRegistry, HubState, PendingSpawns,
    SessionRegistry, run_engine_listener, run_mcp_server,
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let engine_port: u16 = env_port("AETHER_ENGINE_PORT").unwrap_or(DEFAULT_ENGINE_PORT);
    let mcp_port: u16 = env_port("AETHER_MCP_PORT").unwrap_or(DEFAULT_MCP_PORT);
    let engine_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), engine_port);
    let mcp_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), mcp_port);

    let registry = EngineRegistry::new();
    let sessions = SessionRegistry::new();
    let pending = PendingSpawns::new();
    let state = HubState::new(
        registry.clone(),
        sessions.clone(),
        pending.clone(),
        engine_addr,
    );

    let engine_task = tokio::spawn(run_engine_listener(
        engine_addr,
        registry,
        sessions,
        pending,
    ));
    let mcp_task = tokio::spawn(run_mcp_server(mcp_addr, state));

    tokio::select! {
        r = engine_task => log_exit("engine listener", r),
        r = mcp_task => log_exit("mcp listener", r),
        _ = tokio::signal::ctrl_c() => {
            eprintln!("aether-hub: shutting down");
        }
    }
    Ok(())
}

fn env_port(name: &str) -> Option<u16> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn log_exit(label: &str, result: Result<std::io::Result<()>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => eprintln!("aether-hub: {label} exited"),
        Ok(Err(e)) => eprintln!("aether-hub: {label} error: {e}"),
        Err(e) => eprintln!("aether-hub: {label} join error: {e}"),
    }
}
