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
use aether_http::{HttpServerCapability, HttpServerConfig};
use aether_mcp::{McpServerCapability, McpServerConfiguration};
use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig, RpcServerParams};
use aether_substrate::chassis::builder::{Builder, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::{BootableChassis, ComposeBase, composed};
use aether_substrate::config::{ConfigError, ConfigSources, KnobRecord, validate_env};
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot};

use crate::mcp::HubToolProvider;
use crate::{DEFAULT_MCP_HTTP_PORT, DEFAULT_RPC_PORT};
use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, RegistryQueueConfig, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
    hub_residual_knobs, install_frame_size,
};
use aether_chassis::cli::ChassisCli;
use aether_codec::frame::max_frame_size;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;

use crate::cli::HubCli;

/// The hub's HTTP request deadline, in milliseconds.
///
/// Deliberately above `McpServerConfiguration::tool_timeout_millis` (600,000
/// by default) so the protocol layer — which can answer `isError: true` with
/// a diagnosis — expires a slow tool before the HTTP layer expires the whole
/// request with a bare status code. The same field also governs the
/// per-read slow-loris timeout, so a deadline this long is only defensible on
/// a loopback listener behind a fully buffered proxy; a directly exposed
/// listener must first split those two timeouts.
const MCP_HTTP_REQUEST_TIMEOUT_MILLIS: u64 = 610_000;

/// The port the hub's Model Context Protocol endpoint binds.
///
/// A hub-owned knob rather than a raw `HttpServerConfig.bind_addr` override,
/// for the reason Bloomery keeps its own `HttpPortConfig`: the chassis fixes
/// the interface (loopback, always) and the request deadline, and leaves the
/// operator exactly the one decision that is theirs. Whether the endpoint
/// binds at all is `AETHER_MCP_ENABLED`, not a field here — one switch, so
/// an enabled protocol server and an unbound listener cannot disagree.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_MCP_HTTP", cli_prefix = "mcp-http")]
pub struct McpEndpointConfig {
    /// Loopback port for `POST /mcp`.
    #[config(default = 8891)]
    pub port: u16,
}

impl Default for McpEndpointConfig {
    fn default() -> Self {
        Self { port: DEFAULT_MCP_HTTP_PORT }
    }
}

/// The listener configuration the hub gives its Model Context Protocol
/// endpoint.
///
/// Three decisions live here, and each is one an operator could otherwise
/// make wrongly: the endpoint binds loopback only, its request deadline
/// outlasts the protocol layer's own tool ceiling, and it is enabled exactly
/// when the protocol server is.
fn mcp_listener(mcp: &McpServerConfiguration, port: u16) -> HttpServerConfig {
    HttpServerConfig {
        enabled: mcp.enabled,
        bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port).to_string(),
        request_timeout_millis: MCP_HTTP_REQUEST_TIMEOUT_MILLIS,
        ..HttpServerConfig::default()
    }
}

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
        // The shadow Model Context Protocol endpoint (the design's migration
        // step 4). Both caps are composed unconditionally so the roster —
        // and the config manifest derived from it — is a property of the
        // chassis rather than of the environment it booted in; what the
        // environment decides is whether the endpoint is *enabled*. That one
        // resolved flag also gates the HTTP listener, so a running protocol
        // server without a listener, or a listener serving a disabled server,
        // are both unreachable states rather than diagnosable ones.
        let mcp = sources.resolve::<McpServerConfiguration>()?;
        // ADR-0156 §configuration-at-the-seam: the hub's provider can forward
        // a tool's work over `FleetServer`'s framed connections, so — unlike
        // a purely local provider — its provider-wire ceiling must not exceed
        // the frame ceiling this chassis installed. `install_frame_size` ran
        // in `build` before `composed`, so the installed value is final here.
        // The generic capability never reads the frame knob itself; this is
        // the composer that couples them.
        let mcp = McpServerConfiguration {
            provider_wire_maximum_bytes: mcp.provider_wire_maximum_bytes.min(max_frame_size()),
            ..mcp
        };

        let mcp_http = mcp_listener(&mcp, sources.resolve::<McpEndpointConfig>()?.port);
        // Install the shared base stratum by reusing `ChassisBase`'s own
        // `ComposeBase::install` — the drift-free single definition — even though
        // the hub's `Base` is the unit no-op.
        let builder =
            ChassisBase { sources, actor_ring, scheduler_tuning, registry_queues, settlement }.install(builder);
        let builder = builder
            // The endpoint's port is resolved above rather than carried by a
            // capability's own config, so it needs declaring here: an
            // undeclared member's key is absent from the composition-derived
            // known set, and `validate_env` would then refuse the very
            // variable this chassis reads.
            .declare_config_member::<McpEndpointConfig>()
            .with_actor_configured::<HttpServerCapability>((), mcp_http)
            .with_actor_configured::<McpServerCapability>((), mcp)
            .with_actor::<HubToolProvider>(());
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
mod endpoint_tests {
    use super::{DEFAULT_MCP_HTTP_PORT, McpServerConfiguration, mcp_listener};

    /// The endpoint's listener is off unless the protocol server is on.
    ///
    /// The two flags are separate knobs on separate capabilities, and the
    /// shadow posture depends on them agreeing: a listener bound while the
    /// protocol server is disabled answers every caller `503` from an
    /// unadvertised port, and a protocol server enabled behind no listener
    /// is unreachable with nothing to say so. Deriving one from the other is
    /// the decision this pins.
    #[test]
    fn the_listener_binds_exactly_when_the_protocol_server_is_enabled() {
        for enabled in [false, true] {
            let mcp = McpServerConfiguration { enabled, ..McpServerConfiguration::default() };

            let listener = mcp_listener(&mcp, DEFAULT_MCP_HTTP_PORT);

            assert_eq!(listener.enabled, enabled, "the listener tracks the protocol server's enabled flag");
            assert!(
                listener.bind_addr.starts_with("127.0.0.1:"),
                "the endpoint is loopback-only; a public listener needs encryption and authentication in front of \
                 it: {}",
                listener.bind_addr,
            );
        }
    }

    /// Tripwire: the HTTP request deadline must outlast the protocol layer's
    /// tool ceiling.
    ///
    /// The same `request_timeout_millis` governs the handler response
    /// deadline, so if it ever drops to or below `tool_timeout_millis` the
    /// HTTP layer expires a slow tool first — and it can only answer a bare
    /// status code, where the protocol layer would have answered
    /// `isError: true` naming what timed out. Raising the tool ceiling
    /// without raising this one is the edit that breaks it, and it breaks
    /// only for calls slow enough that nobody runs them in a test.
    #[test]
    fn the_request_deadline_outlasts_the_tool_ceiling() {
        let mcp = McpServerConfiguration::default();

        let listener = mcp_listener(&mcp, DEFAULT_MCP_HTTP_PORT);

        assert!(
            listener.request_timeout_millis > mcp.tool_timeout_millis,
            "the HTTP deadline ({}) must outlast the tool ceiling ({}) so a slow tool is expired by the layer that \
             can explain it",
            listener.request_timeout_millis,
            mcp.tool_timeout_millis,
        );
    }
}

#[cfg(test)]
mod config_manifest_tests {
    use super::HubChassis;
    use aether_chassis::boot::hub_residual_knobs;
    use aether_substrate::chassis::config_manifest;

    /// Every knob the shadow endpoint reads is a knob the hub admits.
    ///
    /// `validate_env` refuses an `AETHER_*` variable outside the
    /// composition-derived known set, so a member the chassis *resolves*
    /// but never *declares* produces the worst possible failure: setting
    /// the documented variable makes the hub refuse to boot, and the
    /// variable that turns the endpoint on is the one an operator reaches
    /// for first. The endpoint's own port knob is exactly that shape — it
    /// belongs to no capability's config — so it is declared by hand, and
    /// this is what holds the declaration to the resolve.
    #[test]
    fn the_shadow_endpoint_knobs_are_known_keys() {
        let manifest = config_manifest::<HubChassis>().expect("hub config manifest");
        let known = manifest.known_keys(&hub_residual_knobs());

        assert!(known.contains("AETHER_MCP_ENABLED"), "the switch that enables the endpoint must be settable");
        assert!(
            known.contains("AETHER_MCP_HTTP_PORT"),
            "the hand-declared endpoint port member must reach the known-key set",
        );
        assert!(
            known.contains("AETHER_HTTP_SERVER_MAX_REQUEST_BYTES"),
            "the endpoint's listener rides HttpServerConfig, so its knobs are the hub's too",
        );
    }

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
