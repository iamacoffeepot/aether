// Thin binary entry point: parse port from env, spin up the engine
// listener, wait for Ctrl-C. The Claude-facing rmcp transport lands in
// PR 4; until then the hub is engine-only.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use aether_hub::{DEFAULT_ENGINE_PORT, EngineRegistry, run_engine_listener};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("AETHER_ENGINE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ENGINE_PORT);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    let registry = EngineRegistry::new();
    let listener = tokio::spawn(run_engine_listener(addr, registry));

    tokio::select! {
        r = listener => {
            if let Ok(Err(e)) = r {
                eprintln!("aether-hub: listener error: {e}");
                return Err(e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("aether-hub: shutting down");
        }
    }
    Ok(())
}
