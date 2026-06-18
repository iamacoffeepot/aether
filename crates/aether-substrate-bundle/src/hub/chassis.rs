//! Hub chassis (post-issue-763 P5f). The hub is now a thin coordinator
//! between the out-of-process `aether-mcp` MCP server and the
//! substrates the engines cap forks: it stands up a `SubstrateBoot` to
//! host actors, wires `TraceDispatchCapability` + `EngineServer` +
//! `RpcServerCapability` (the inbound `aether-mcp` dials), and blocks
//! on a SIGINT/SIGTERM signal in `run`. The OLD `EngineToHub` TCP
//! listener, hub-side sessions, `ProcessCapability`, loopback drainers,
//! and embedded MCP server all retired with P5e/P5f.
//!
//! Signal handling is sync: there is no async runtime to host. On Unix
//! `signal-hook`'s iterator API blocks the driver thread until SIGINT
//! or SIGTERM arrives; on Windows the `ctrlc` fallback covers Ctrl-C.

use std::net::SocketAddr;
use std::sync::Arc;

use aether_actor::Actor;
use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{EngineConfig, EngineServer, trace::TraceDispatchCapability};
use aether_kinds::BinaryManifest;
use aether_substrate::chassis::builder::{
    Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError,
};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::chassis_common::ActorRingConfig;
use crate::cli::HubCli;
use crate::hub::DEFAULT_RPC_PORT;
use std::thread;

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

impl HubChassis {
    /// The `--describe` manifest (ADR-0115, issue 1953): the chassis
    /// profile, the mailbox namespaces this binary links, and the
    /// `build.rs` provenance. The hub is the minimal coordinator chassis —
    /// it links the trace dispatcher, the engines cap, and the RPC server,
    /// not the full-stack cap set — so it lists those three directly
    /// rather than through the full-stack
    /// [`common_cap_namespaces`](crate::common_cap_namespaces) base.
    #[must_use]
    pub fn describe_manifest() -> BinaryManifest {
        let caps = vec![
            <TraceDispatchCapability as Actor>::NAMESPACE,
            <EngineServer as Actor>::NAMESPACE,
            <RpcServerCapability as Actor>::NAMESPACE,
        ];
        crate::binary_manifest(Self::PROFILE, caps)
    }
}

/// Resolved configuration the hub chassis takes at build time.
/// `rpc_addr` is the `aether.rpc.server` bind — the target the
/// out-of-process `aether-mcp` coordinator dials. `AETHER_RPC_PORT`
/// overrides the port. `engine` is the engines-cap config — today the
/// liveness-heartbeat tuning (issue 1339), resolved argv-then-env.
#[derive(Clone)]
pub struct HubEnv {
    pub rpc_addr: SocketAddr,
    pub engine: EngineConfig,
}

impl HubEnv {
    /// Read `AETHER_RPC_PORT` from the environment; fall back to
    /// [`DEFAULT_RPC_PORT`] when unset or unparseable. Binds on
    /// `127.0.0.1` — intentional for the current single-host
    /// development story.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_env_with_argv(&HubCli::default())
    }

    /// ADR-0090 unit d (issue 1258): resolve from argv-then-env.
    /// `cli.rpc_port` shadows `AETHER_RPC_PORT`; falling through still
    /// lands on [`DEFAULT_RPC_PORT`] (the hub always binds an RPC
    /// server, unlike desktop / headless). The engines overlay
    /// (`--hub-heartbeat-*`, issue 1339) resolves through the
    /// derive-emitted `from_argv_then_env` (argv beats
    /// `AETHER_HUB_HEARTBEAT_*` env beats the literal default). Takes
    /// `&HubCli`, cloning the overlay rather than consuming `cli` so the
    /// bin keeps it for the `--config` dump.
    #[must_use]
    pub fn from_env_with_argv(cli: &HubCli) -> Self {
        use std::net::{IpAddr, Ipv4Addr};
        let rpc_port = cli
            .rpc_port
            .or_else(super::rpc_port_from_env)
            .unwrap_or(DEFAULT_RPC_PORT);
        Self {
            rpc_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port),
            engine: EngineConfig::from_argv_then_env(cli.engine.clone().into_layer()),
        }
    }
}

impl HubChassis {
    fn build_inner(env: HubEnv) -> Result<BuiltChassis<Self>, BootError> {
        let HubEnv { rpc_addr, engine } = env;
        let boot = SubstrateBoot::builder("aether-hub", env!("CARGO_PKG_VERSION")).build()?;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);

        let driver = HubServerDriverCapability { boot };

        // Issue 1990: resolve the per-actor ring capacities so the hub
        // chassis honours `AETHER_ACTOR_{LOG,TRACE}_RING_SIZE` like the
        // full-stack chassis (which thread it via `with_common_caps`).
        let ring_caps = ActorRingConfig::try_from_env()?.to_ring_capacities();

        Builder::<Self>::new(registry, mailer)
            .with_ring_caps(ring_caps)
            .with_actor::<TraceDispatchCapability>(())
            // Liveness-heartbeat tuning (issue 1339), resolved
            // argv-then-env in `HubEnv::from_env_with_argv`.
            .with_actor::<EngineServer>(engine)
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

/// ADR-0071 driver capability for the hub chassis. Owns the
/// `SubstrateBoot` whose registry hosts the chassis actors. `run`
/// blocks the calling thread on a SIGINT/SIGTERM signal, then drops
/// the boot so the actor registry tears down.
pub struct HubServerDriverCapability {
    boot: SubstrateBoot,
}

/// Post-boot handle for [`HubServerDriverCapability`].
pub struct HubServerDriverRunning {
    boot: SubstrateBoot,
}

impl DriverCapability for HubServerDriverCapability {
    type Running = HubServerDriverRunning;

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self { boot } = self;
        Ok(HubServerDriverRunning { boot })
    }
}

impl DriverRunning for HubServerDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let Self { boot } = *self;
        let sig = shutdown_signal();
        tracing::info!("aether-substrate-hub: {sig} received, shutting down");
        // `boot` drops here — actor registries shut down, dispatcher
        // threads see their inbox senders drop and exit.
        drop(boot);
        Ok(())
    }
}

/// Blocks the calling thread until SIGINT or SIGTERM arrives on Unix;
/// on Windows falls back to Ctrl-C only via `ctrlc`. Returns a short
/// label for the log line.
///
/// Why both signals on Unix: interactive shells deliver SIGINT, but
/// process supervisors (systemd, supervisord), shell utilities
/// (`pkill`, `kill` without `-9`), and CI cancellation all send
/// SIGTERM. Ignoring SIGTERM means `pkill -f aether-substrate-hub`
/// kills the hub without running drops.
#[cfg(unix)]
fn shutdown_signal() -> &'static str {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                "aether-substrate-hub: signal handler install failed: {e}; \
                 parking thread — SIGKILL is the only exit"
            );
            thread::park();
            return "park";
        }
    };
    // The iterator only returns `None` if the underlying file
    // descriptor closes — can't happen for the lifetime of `signals`,
    // but the explicit branch keeps coverage total.
    match signals.forever().next() {
        Some(SIGINT) => "SIGINT",
        Some(SIGTERM) => "SIGTERM",
        Some(_) => "unknown signal",
        None => "signal stream ended",
    }
}

#[cfg(not(unix))]
fn shutdown_signal() -> &'static str {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<()>();
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = tx.send(());
    }) {
        tracing::error!(
            "aether-substrate-hub: ctrl-c handler install failed: {e}; \
             parking thread — SIGKILL is the only exit"
        );
        std::thread::park();
        return "park";
    }
    let _ = rx.recv();
    "Ctrl-C"
}
