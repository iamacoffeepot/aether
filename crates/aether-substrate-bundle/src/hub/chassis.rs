//! Hub chassis (post-issue-763 P5f). The hub is now a thin coordinator
//! between the out-of-process `aether-mcp` MCP server and the
//! substrates the engines cap forks: it stands up a `SubstrateBoot` to
//! host actors, wires `TraceObserverCapability` + `EngineServer` +
//! `RpcServerCapability` (the inbound `aether-mcp` dials), and blocks
//! on a SIGINT/SIGTERM signal in `run`. The OLD `EngineToHub` TCP
//! listener, hub-side sessions, `ProcessCapability`, loopback drainers,
//! and embedded MCP server all retired with P5e/P5f.

use std::net::SocketAddr;
use std::sync::Arc;

use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{EngineServer, trace::TraceObserverCapability};
use aether_substrate::chassis::builder::{
    Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError,
};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot};
use tokio::runtime::Runtime;

use crate::hub::DEFAULT_RPC_PORT;

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
/// `rpc_addr` is the `aether.rpc.server` bind — the target the
/// out-of-process `aether-mcp` coordinator dials. `AETHER_RPC_PORT`
/// overrides the port.
pub struct HubEnv {
    pub rpc_addr: SocketAddr,
}

impl HubEnv {
    /// Read `AETHER_RPC_PORT` from the environment; fall back to
    /// [`DEFAULT_RPC_PORT`] when unset or unparseable. Binds on
    /// `127.0.0.1` — intentional for the current single-host
    /// development story.
    pub fn from_env() -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        let rpc_port = std::env::var("AETHER_RPC_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RPC_PORT);
        Self {
            rpc_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port),
        }
    }
}

impl HubChassis {
    fn build_inner(env: HubEnv) -> Result<BuiltChassis<HubChassis>, BootError> {
        let HubEnv { rpc_addr } = env;
        let boot = SubstrateBoot::builder("aether-hub", env!("CARGO_PKG_VERSION")).build()?;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);

        // Current-thread runtime — only used to block on the shutdown
        // signal. No async tasks live here; the actors (RpcServer,
        // EngineServer, proxies) run on their own std threads.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| BootError::Other(Box::new(e)))?;

        let driver = HubServerDriverCapability { rt, boot };

        Builder::<HubChassis>::new(registry, mailer)
            .with_actor::<TraceObserverCapability>(())
            .with_actor::<EngineServer>(())
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: rpc_addr.to_string(),
                peer_kind: PeerKind::Substrate {
                    engine_name: "aether-hub".into(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
            })
            .driver(driver)
            .build()
    }
}

/// ADR-0071 driver capability for the hub chassis. Owns the tokio
/// runtime + the `SubstrateBoot` whose registry hosts the chassis
/// actors. `run` blocks on a SIGINT/SIGTERM signal, then drops the
/// boot so the actor registry tears down.
pub struct HubServerDriverCapability {
    rt: Runtime,
    boot: SubstrateBoot,
}

/// Post-boot handle for [`HubServerDriverCapability`].
pub struct HubServerDriverRunning {
    rt: Runtime,
    boot: SubstrateBoot,
}

impl DriverCapability for HubServerDriverCapability {
    type Running = HubServerDriverRunning;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let HubServerDriverCapability { rt, boot } = self;
        Ok(HubServerDriverRunning { rt, boot })
    }
}

impl DriverRunning for HubServerDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let HubServerDriverRunning { rt, boot } = *self;
        rt.block_on(async {
            let sig = shutdown_signal().await;
            eprintln!("aether-substrate-hub: {sig} received, shutting down");
        });
        // `boot` drops here — actor registries shut down, dispatcher
        // threads see their inbox senders drop and exit.
        drop(boot);
        Ok(())
    }
}

/// Resolves when either SIGINT (Ctrl-C) or SIGTERM arrives on Unix;
/// on non-Unix falls back to `ctrl_c()` since tokio doesn't expose
/// named signals outside Unix. Returns a short label for the log line.
///
/// Why both signals: interactive shells deliver SIGINT, but process
/// supervisors (systemd, supervisord), shell utilities (`pkill`,
/// `kill` without `-9`), and CI cancellation all send SIGTERM.
/// Ignoring SIGTERM means `pkill -f aether-substrate-hub` kills the
/// hub without running drops.
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
