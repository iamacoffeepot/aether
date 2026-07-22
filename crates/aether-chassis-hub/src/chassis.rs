//! Hub chassis (post-issue-763 P5f). The hub is now a thin coordinator
//! between the out-of-process `aether-mcp` MCP server and the
//! substrates the engines cap forks: it stands up a `SubstrateBoot` to
//! host actors, wires `TraceDispatchCapability` + `FleetServer` +
//! `RpcServerCapability` (the inbound `aether-mcp` dials), and blocks
//! on a SIGINT/SIGTERM signal in `run`. The OLD `EngineToHub` TCP
//! listener, hub-side sessions, `ProcessCapability`, loopback drainers,
//! and embedded MCP server all retired with P5e/P5f.
//!
//! Signal handling is sync: there is no async runtime to host. On Unix
//! `signal-hook`'s iterator API blocks the driver thread until SIGINT
//! or SIGTERM arrives; on Windows the `ctrlc` fallback covers Ctrl-C.

use std::sync::Arc;
use std::time::Duration;

use aether_fleet::FleetServer;
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig, RpcServerParams};
use aether_substrate::chassis::BootableChassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::config::{
    ConfigError, ConfigSources, KnobRecord, RingCapacities, SchedulerTuning, StageArgv, validate_env,
};
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot};
use aether_trace::TraceDispatchCapability;

use crate::DEFAULT_RPC_PORT;
use aether_chassis::boot::{
    ActorRingConfig, RuntimeConfig, SchedulerTuningConfig, SettlementConfig, hub_residual_knobs, install_frame_size,
    load_chassis_config, with_hub_fleet_passthrough,
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
        let boot = SubstrateBoot::build()?;
        // #3849: re-apply the fully-resolved `AETHER_LOG_FILTER` directive now
        // the subscriber is installed (env > `[runtime]` file > `info`).
        apply_filter(&env.runtime.log_filter);
        let builder = Self::compose(&boot, env);
        // ADR-0156 §4 (was ADR-0090 §4 e1): warn on any unknown `AETHER_*` env
        // var, sweeping against the composition-derived known-key set (the
        // declared fleet pass-through) plus the hub residual hand records.
        validate_env(&builder.config_manifest().known_keys(&Self::residual_knobs()))?;
        let driver = HubServerDriverCapability { boot };
        builder.driver(driver).build()
    }
}

impl BootableChassis for HubChassis {
    fn resolve_env() -> Result<Self::Env, ConfigError> {
        HubEnv::from_env()
    }

    fn residual_knobs() -> Vec<KnobRecord> {
        hub_residual_knobs()
    }

    /// Compose the hub capability chain — the single claim/build path
    /// (ADR-0155) both [`Chassis::build`] and the describe / config helpers run,
    /// so the manifest roster can never drift from what boots: the trace
    /// dispatcher, the engines cap, and the RPC server. Returns the composed
    /// builder before the driver is installed — [`Chassis::build`] adds the
    /// signal-blocking driver and starts, while the describe / config helpers
    /// read the claim / config terminals off it. Takes the boot handle by
    /// reference so [`Chassis::build`] can move the same `boot` into the
    /// driver afterward.
    fn compose(boot: &SubstrateBoot, env: HubEnv) -> Builder<Self> {
        let HubEnv { sources, rpc_port, runtime: _, ring_capacities, scheduler_tuning, teardown_budget } = env;
        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);

        // ADR-0156 §4: declare the hub's fleet pass-through set — the full
        // fleet knobs a hub-spawned substrate inherits from the hub's env, the
        // one documented over-approximation in the aggregate. The engines cap
        // (`FleetConfig`) declares its own member via `with_actor` below.
        //
        // ADR-0156 §5: hand the builder the source stack so it resolves the
        // composed `FleetServer`'s `FleetConfig` (the liveness-heartbeat
        // tuning, issue 1339) off it. #3849: the RPC server's `Config` is now a
        // derive member resolved off the stack; the hub always binds, so it
        // composes the resolved-with-default port as an explicit override.
        with_hub_fleet_passthrough(Builder::<Self>::new(registry, mailer).with_config_sources(sources))
            .with_ring_capacities(ring_capacities)
            .with_scheduler_tuning(scheduler_tuning)
            .with_teardown_budget(teardown_budget)
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<FleetServer>(())
            .with_actor_configured::<RpcServerCapability>(
                RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: aether_substrate::engine_name::<Self>(),
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    kinds: vec![],
                },
                #[allow(clippy::disallowed_methods)] // hub wires both caps; resolve the engines-cap mailbox by its well-known depth-1 name
                route_target: Some(aether_data::mailbox_id_from_name("aether.fleet")),
            },
                RpcServerConfig { port: Some(rpc_port) },
            )
    }
}

/// Build-time inputs the hub chassis takes. `rpc_port` is the
/// `aether.rpc.server` bind port — the target the out-of-process `aether-mcp`
/// coordinator dials — resolved off the source stack (`--rpc-port` >
/// `AETHER_RPC_PORT` > `[rpc]` file) with the [`DEFAULT_RPC_PORT`] fallback.
///
/// ADR-0156 §5: the engines-cap `Config` (`FleetConfig`, the liveness
/// heartbeat tuning) no longer rides as a field — the builder resolves it off
/// [`Self::sources`]. What remains is the source stack plus the chassis-side
/// reads of the non-cap pool / ring / scheduler / teardown / runtime knobs and
/// the RPC port.
pub struct HubEnv {
    /// The config source stack (file + the engines-cap argv overlay) the
    /// builder resolves the composed `FleetServer`'s `Config` off (ADR-0156 §5).
    pub sources: ConfigSources,
    /// The resolved `aether.rpc.server` bind port. The hub always binds (unlike
    /// desktop / headless): `RpcServerConfig` is resolved off the source stack
    /// (argv `--rpc-port` > `AETHER_RPC_PORT` > `[rpc]` file) with the hub's
    /// [`DEFAULT_RPC_PORT`] fallback when unset (#3849), then composed as an
    /// explicit `with_actor_configured` override.
    pub rpc_port: u16,
    /// The substrate runtime knobs (#3849); [`Chassis::build`] re-applies
    /// [`RuntimeConfig::log_filter`] after the subscriber installs.
    pub runtime: RuntimeConfig,
    pub ring_capacities: RingCapacities,
    pub scheduler_tuning: SchedulerTuning,
    pub teardown_budget: Duration,
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
    /// `--rpc-port` (the flattened `RpcServerOverlay`, #3849) shadows
    /// `AETHER_RPC_PORT` through the source stack; falling through still
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
        // ADR-0156 §4: the unknown-`AETHER_*` sweep moved to `Chassis::build`,
        // where the composed builder's `config_manifest` (including the
        // declared fleet pass-through) supplies the hub's known-key set.
        let config_file = load_chassis_config(cli.config.clone())?;
        // ADR-0156 §5: assemble the source stack — the loaded config file plus
        // the engines-cap and RPC-server argv overlays. The builder resolves the
        // composed `FleetServer`'s `FleetConfig` off this; the chassis resolves
        // the non-cap ring / scheduler / teardown / runtime knobs and the
        // RPC-server port below off the same stack via their `ConfigMember`
        // sections.
        // ADR-0156 §5 (issue 3872): stage the engines-cap and RPC-server argv
        // overlays in one derived `StageArgv` call off the CLI declaration
        // (`cli` is borrowed so the bin keeps it for `--print-config`; clone to
        // stage by value). No hand-maintained per-cap `set_argv` block to forget.
        let mut sources = ConfigSources::new(config_file);
        cli.clone().stage(&mut sources);
        let ring_capacities = sources.resolve::<ActorRingConfig>()?.to_ring_capacities();
        let scheduler_tuning = sources.resolve::<SchedulerTuningConfig>()?.to_scheduler_tuning();
        let teardown_budget = sources.resolve::<SettlementConfig>()?.to_cap();
        let runtime = sources.resolve::<RuntimeConfig>()?;
        // ADR-0156 §6 (#3850): push the resolved wire-frame cap into the codec
        // here, before the RPC server binds and any framing runs — the codec
        // cannot pull the knob itself.
        install_frame_size(&mut sources)?;
        // #3849: the hub always binds (unlike desktop / headless) — resolve the
        // RPC port through the source stack and fall back to `DEFAULT_RPC_PORT`
        // when unset. Resolving it here consumes the staged `--rpc-port` overlay;
        // `compose` re-stages the resolved value as an explicit
        // `with_actor_configured` override so the builder binds it.
        let rpc_port = sources.resolve::<RpcServerConfig>()?.port.unwrap_or(DEFAULT_RPC_PORT);
        Ok(Self { sources, rpc_port, runtime, ring_capacities, scheduler_tuning, teardown_budget })
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

#[cfg(test)]
mod config_manifest_tests {
    use super::HubChassis;
    use aether_chassis::boot::hub_residual_knobs;
    use aether_substrate::chassis::config_manifest;

    #[test]
    fn hub_known_keys_carry_fleet_passthrough_engine_and_store_root() {
        // ADR-0156 §4 acceptance: the hub composes only trace+engine+rpc, but
        // a hub-spawned substrate inherits its env, so the hub declares the
        // full fleet knob set as its one documented over-approximation. Its own
        // engines-cap knob rides `with_actor`; the store-root override folds in
        // as a residual hand record.
        let manifest = config_manifest::<HubChassis>().expect("hub config manifest");
        let known = manifest.known_keys(&hub_residual_knobs());
        assert!(known.contains("AETHER_AUDIO_DISABLE"), "hub declares the fleet audio knob as pass-through");
        assert!(known.contains("AETHER_WINDOW_MODE"), "hub declares the fleet window knob as pass-through");
        assert!(known.contains("AETHER_TICK_HZ"), "hub declares the fleet tick knob as pass-through");
        assert!(known.contains("AETHER_HUB_HEARTBEAT_INTERVAL_SECS"), "hub claims its own composed engines-cap knob");
        assert!(known.contains("AETHER_FLEET_STORE_ROOT"), "hub folds in the store-root residual knob");
    }
}
