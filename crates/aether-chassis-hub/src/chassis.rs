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

use aether_fleet::FleetServer;
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig, RpcServerParams};
use aether_substrate::chassis::builder::{Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::{BootableChassis, ComposeBase, composed};
use aether_substrate::config::{ConfigError, ConfigSources, KnobRecord, validate_env};
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::DEFAULT_RPC_PORT;
use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, RegistryQueueConfig, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
    hub_residual_knobs, install_frame_size,
};
use aether_chassis::cli::ChassisCli;
use std::thread;

use crate::cli::HubCli;

/// ADR-0071 marker for the hub chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HubChassis>`] returned by
/// [`HubChassis::build`].
pub struct HubChassis;

impl Chassis for HubChassis {
    const PROFILE: &'static str = "hub";
    type Driver = HubServerDriverCapability;
    type Env = ConfigSources;

    fn build(mut sources: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        let mut boot = SubstrateBoot::build()?;
        // #3849 / ADR-0162 §config-at-its-seam: re-apply the fully-resolved
        // `AETHER_LOG_FILTER` directive now the subscriber is installed (env >
        // `[runtime]` file > `info`). Resolved HERE, at the seam that applies it,
        // off the source stack `compose` then consumes.
        apply_filter(&sources.resolve::<RuntimeConfig>()?.log_filter);
        // ADR-0156 §6 (#3850): push the resolved wire-frame cap into the codec
        // here, before the RPC server binds and any framing runs — the codec
        // cannot pull the knob itself.
        install_frame_size(&mut sources)?;
        // `composed` mints the builder, installs the aborter (the hub's one
        // deliberate behavior change — `OutboundFatalAborter`, previously the
        // implicit `PanicAborter`) and the unit no-op base, then runs the hub's
        // fallible `compose` delta — which resolves the always-bind RPC port and
        // the base stratum off the source stack it now receives (ADR-0162).
        let builder = composed::<Self>(&mut boot, (), sources)?;
        // ADR-0162 (was ADR-0156 §4): warn on any unknown `AETHER_*` env var,
        // sweeping against the composition-derived known-key set plus the hub
        // residual hand records. Post-ADR-0162 the hub's env speaks to a single
        // audience (spawned children no longer inherit it — the fleet fork
        // scrubs `AETHER_*` and injects addressed config via argv), so the
        // known-key set is purely composition-derived like every other chassis.
        validate_env(&builder.config_manifest().known_keys(&Self::residual_knobs()))?;
        let driver = HubServerDriverCapability { boot };
        builder.driver(driver).build()
    }
}

impl BootableChassis for HubChassis {
    /// The hub's base is the unit no-op (like bloomery): its `compose` delta needs
    /// the source stack to resolve its always-bind RPC port, so the stack rides
    /// [`Chassis::Env`] as [`ConfigSources`] and reaches `compose`, rather than
    /// being consumed by a `ChassisBase` base ahead of it. The shared
    /// base stratum is still installed drift-free — `compose` reuses
    /// [`ChassisBase`]'s own [`ComposeBase::install`] (ADR-0162).
    type Base = ();

    /// Resolve the hub env off the source stack (ADR-0162): the lone per-chassis
    /// token is the `HubCli` type. The env IS the raw [`ConfigSources`] — every
    /// member the hub resolves (runtime, frame size, the base stratum, the RPC
    /// port) resolves at the seam that consumes it, so nothing is pre-resolved
    /// into an env bag.
    fn resolve_env() -> Result<(Self::Base, Self::Env), ConfigError> {
        Ok(((), HubCli::default().into_sources()?))
    }

    fn residual_knobs() -> Vec<KnobRecord> {
        hub_residual_knobs()
    }

    /// Compose the hub capability delta on top of the framework-minted builder
    /// [`composed`] hands it — the shared base stratum, the engines cap, and the
    /// port-overridden RPC server. `compose` is fallible (ADR-0162
    /// §config-at-its-seam): the hub resolves its always-bind RPC port off the
    /// source stack it receives right here, so the port is no longer pre-resolved
    /// into a per-chassis env bag.
    ///
    /// The base stratum (config sources, the fused ring / scheduler / settlement
    /// members, the two declare-only members, `TraceDispatchCapability`) is
    /// installed by reusing [`ChassisBase`]'s own [`ComposeBase::install`] rather
    /// than hand-copying it, so the hub can never drift from what desktop /
    /// headless install through their `ChassisBase` base (ADR-0162). The hub uses
    /// `Base = ()` — not `Base = ChassisBase` — because the RPC-port resolution
    /// needs the source stack to reach `compose`, which a source-consuming base
    /// ahead of it would forbid. This is the single claim/build path (ADR-0155)
    /// both [`Chassis::build`] and the describe / config helpers run, so the
    /// manifest roster can never drift from what boots.
    fn compose(
        builder: Builder<Self>,
        _boot: &SubstrateBoot,
        mut sources: ConfigSources,
    ) -> Result<Builder<Self>, BootError> {
        // #3930: resolve the three base non-cap members off the stack as structs.
        let actor_ring = sources.resolve::<ActorRingConfig>()?;
        let scheduler_tuning = sources.resolve::<SchedulerTuningConfig>()?;
        let registry_queues = sources.resolve::<RegistryQueueConfig>()?;
        let settlement = sources.resolve::<SettlementConfig>()?;
        // #3849: the hub always binds (unlike desktop / headless) — resolve the
        // RPC port off the source stack (argv `--rpc-port` > `AETHER_RPC_PORT` >
        // `[rpc]` file) and fall back to `DEFAULT_RPC_PORT` when unset. Resolved
        // before the stack is moved into the base, then composed as an explicit
        // `with_actor_configured` override so the builder binds it.
        let rpc_port = sources.resolve::<RpcServerConfig>()?.port.unwrap_or(DEFAULT_RPC_PORT);
        // Install the shared base stratum by reusing `ChassisBase`'s own
        // `ComposeBase::install` — the drift-free single definition — even though
        // the hub's `Base` is the unit no-op.
        let builder =
            ChassisBase { sources, actor_ring, scheduler_tuning, registry_queues, settlement }.install(builder);
        Ok(builder.with_actor::<FleetServer>(()).with_actor_configured::<RpcServerCapability>(
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
        ))
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
        tracing::info!("aether-hub: {sig} received, shutting down");
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
/// SIGTERM. Ignoring SIGTERM means `pkill -f aether-hub`
/// kills the hub without running drops.
#[cfg(unix)]
fn shutdown_signal() -> &'static str {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                "aether-hub: signal handler install failed: {e}; \
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
            "aether-hub: ctrl-c handler install failed: {e}; \
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
    fn hub_known_keys_are_composition_derived_and_exclude_fleet_knobs() {
        // ADR-0162 acceptance: the hub's known-key set is purely
        // composition-derived, like every other chassis. The fleet pass-through
        // over-approximation is deleted now that a hub-spawned substrate no
        // longer inherits the hub's env, so a fleet knob the hub does not itself
        // compose must be ABSENT from the hub's known set — this inverted
        // assertion is the drift tripwire against the pass-through returning.
        // The hub's own composed engines-cap knob rides `with_actor`; the
        // store-root override folds in as a residual hand record.
        let manifest = config_manifest::<HubChassis>().expect("hub config manifest");
        let known = manifest.known_keys(&hub_residual_knobs());
        assert!(
            !known.contains("AETHER_TICK_HZ"),
            "hub does not compose the headless tick cap, so the fleet tick knob must not be a known key"
        );
        assert!(
            !known.contains("AETHER_WINDOW_MODE"),
            "hub does not compose the desktop window cap, so the fleet window knob must not be a known key"
        );
        assert!(
            !known.contains("AETHER_AUDIO_DISABLE"),
            "hub does not compose the audio cap, so the fleet audio knob must not be a known key"
        );
        assert!(known.contains("AETHER_HUB_HEARTBEAT_INTERVAL_SECS"), "hub claims its own composed engines-cap knob");
        assert!(known.contains("AETHER_FLEET_STORE_ROOT"), "hub folds in the store-root residual knob");
        // The two process-global members the hub itself resolves stay known:
        // `AETHER_MAX_FRAME_SIZE` (pushed into the codec by `install_frame_size`)
        // and the runtime log/panic-hook knobs (`RuntimeConfig`), each declared at
        // the hub compose since their values apply off the builder.
        assert!(
            known.contains("AETHER_MAX_FRAME_SIZE"),
            "hub resolves the wire frame-size knob, so it stays a known key via the declared FrameSizeConfig member"
        );
        assert!(
            known.contains("AETHER_LOG_FILTER"),
            "hub resolves and re-applies the log-filter knob, so it stays known via the declared RuntimeConfig member"
        );
        // #3930: the ring / scheduler / settlement members moved out of the
        // pass-through and are now composed onto their builder seams through
        // `with_chassis_config_member`, which declares their membership as it
        // installs their value. Their env keys must stay in the hub's known-key
        // set — a dropped fuse line would drop them and start warning on a knob an
        // operator legitimately sets for the substrates the hub spawns.
        assert!(
            known.contains("AETHER_ACTOR_LOG_RING_SIZE"),
            "hub keeps the ring-capacity knob via the fused ActorRingConfig member"
        );
        assert!(
            known.contains("AETHER_SPIN_WINDOW_USEC"),
            "hub keeps the scheduler-tuning knob via the fused SchedulerTuningConfig member"
        );
        assert!(
            known.contains("AETHER_SETTLEMENT_CAP_SECS"),
            "hub keeps the settlement knob via the fused SettlementConfig member"
        );
    }
}
