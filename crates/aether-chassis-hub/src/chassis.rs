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
use std::time::Duration;

use aether_engine::{EngineConfig, EngineServer};
use aether_kinds::BinaryManifest;
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_substrate::chassis::builder::{Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::{ConfigError, RingCapacities, SchedulerTuning, validate_env};
use aether_substrate::{Chassis, SubstrateBoot};
use aether_trace::TraceDispatchCapability;

use crate::DEFAULT_RPC_PORT;
use aether_chassis::boot::rpc_port_from_env;
use aether_chassis::boot::{
    ActorRingConfig, SchedulerTuningConfig, hub_known_keys, load_chassis_config, resolve_env_with_file,
    resolve_teardown_cap_with_file, resolve_with_file,
};
use aether_chassis::cli::HubCli;
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
    /// The `--describe` manifest (ADR-0115, amended by ADR-0155): the
    /// chassis profile, the mailbox namespaces this binary claims, and the
    /// `build.rs` provenance. Resolves the hub config the same argv/env/file
    /// way a real boot does, composes the exact capability chain
    /// `build_inner` runs (via `compose` — the trace dispatcher, the engines
    /// cap, and the RPC server), then runs the
    /// ADR-0155 claim-only terminal and reads the claimed namespaces off the
    /// registry. `--describe` stops before Init, so it starts no engine
    /// supervision and binds no socket.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when config resolution ([`HubEnv::from_env`]),
    /// substrate boot, or the claim pass fails.
    pub fn describe_manifest() -> Result<BinaryManifest, BootError> {
        let env = HubEnv::from_env().map_err(|e| BootError::Other(Box::new(e)))?;
        let boot = SubstrateBoot::builder("aether-hub", env!("CARGO_PKG_VERSION")).build()?;
        let caps = Self::compose(&boot, env).claim_namespaces()?;
        Ok(aether_chassis::binary_manifest(Self::PROFILE, caps))
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
    pub ring_caps: RingCapacities,
    pub scheduler_tuning: SchedulerTuning,
    pub teardown_cap: Duration,
}

impl HubEnv {
    /// Read `AETHER_RPC_PORT` from the environment; fall back to
    /// [`DEFAULT_RPC_PORT`] when unset or unparseable. Binds on
    /// `127.0.0.1` — intentional for the current single-host
    /// development story.
    ///
    /// # Errors
    ///
    /// See [`Self::from_env_with_argv`].
    pub fn from_env() -> Result<Self, ConfigError> {
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
    /// bin keeps it for the `--print-config` dump.
    ///
    /// # Errors
    ///
    /// Propagates [`ConfigError`] from the ADR-0090 §4 boot sweep
    /// ([`validate_env`]) — today the sweep only warns, but the
    /// `Result` keeps the hard-error half free to join without a
    /// call-site change, matching desktop / headless.
    pub fn from_env_with_argv(cli: &HubCli) -> Result<Self, ConfigError> {
        use std::net::{IpAddr, Ipv4Addr};
        // ADR-0090 §4 (e1): warn on any unknown AETHER_ env var before
        // resolving — a typo / stale export is loud but non-fatal.
        validate_env(&hub_known_keys())?;
        let config_file = load_chassis_config(cli.config.clone())?;
        let config_file = config_file.as_ref();
        let rpc_port = cli.rpc_port.or_else(rpc_port_from_env).unwrap_or(DEFAULT_RPC_PORT);
        Ok(Self {
            rpc_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), rpc_port),
            engine: resolve_with_file::<EngineConfig>(cli.engine.clone().into_layer(), config_file, "engine")?,
            ring_caps: resolve_env_with_file::<ActorRingConfig>(config_file, "actor")?.to_ring_capacities(),
            scheduler_tuning: resolve_env_with_file::<SchedulerTuningConfig>(config_file, "scheduler")?
                .to_scheduler_tuning(),
            teardown_cap: resolve_teardown_cap_with_file(config_file)?,
        })
    }
}

impl HubChassis {
    /// Compose the hub capability chain — the single claim/build path
    /// (ADR-0155) both [`Self::build_inner`] and [`Self::describe_manifest`]
    /// run, so the manifest roster can never drift from what boots: the trace
    /// dispatcher, the engines cap, and the RPC server. Returns the composed
    /// builder before the driver is installed — `build_inner` adds the
    /// signal-blocking driver and starts, while `describe_manifest` calls
    /// `claim_namespaces` on it. Takes the boot handle by reference so
    /// `build_inner` can move the same `boot` into the driver afterward.
    fn compose(boot: &SubstrateBoot, env: HubEnv) -> Builder<Self> {
        let HubEnv { rpc_addr, engine, ring_caps, scheduler_tuning, teardown_cap } = env;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);

        Builder::<Self>::new(registry, mailer)
            .with_ring_caps(ring_caps)
            .with_scheduler_tuning(scheduler_tuning)
            .with_teardown_cap(teardown_cap)
            .with_actor::<TraceDispatchCapability>((), ())
            // Liveness-heartbeat tuning (issue 1339), resolved
            // argv-then-env in `HubEnv::from_env_with_argv`.
            .with_actor::<EngineServer>(engine, ())
            .with_actor::<RpcServerCapability>(
                RpcServerConfig {
                bind_addr: Some(rpc_addr.to_string()),
                peer_kind: PeerKind::Substrate {
                    engine_name: "aether-hub".into(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
                #[allow(clippy::disallowed_methods)] // hub wires both caps; resolve the engines-cap mailbox by its well-known depth-1 name
                route_target: Some(aether_data::mailbox_id_from_name("aether.engine")),
            },
                (),
            )
    }

    fn build_inner(env: HubEnv) -> Result<BuiltChassis<Self>, BootError> {
        let boot = SubstrateBoot::builder("aether-hub", env!("CARGO_PKG_VERSION")).build()?;
        let builder = Self::compose(&boot, env);
        let driver = HubServerDriverCapability { boot };
        builder.driver(driver).build()
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
