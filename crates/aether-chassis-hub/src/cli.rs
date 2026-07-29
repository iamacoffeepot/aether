//! The hub chassis CLI root (ADR-0090 unit d, issue 1258). [`HubCli`] is
//! coordinator-only — no full-stack caps — so it flattens the RPC-server and
//! engines-cap overlays plus the three tuning overlays the hub resolves off its
//! own source stack, alongside the source-selecting [`ChassisMeta`] flags. The
//! shared staging / flag-naming / help-forwarding machinery lives in
//! `aether_chassis::cli`.

use aether_chassis::boot::{ActorRingOverlay, RegistryQueueOverlay, SchedulerTuningOverlay, env_only_after_help};
use aether_chassis::cli::{ChassisCli, ChassisMeta};
use aether_fleet::FleetOverlay;
use aether_harness_substrate::SettlementOverlay;
use aether_rpc::RpcServerOverlay;
use clap::Parser;

/// Hub chassis CLI root — coordinator-only, no full-stack caps.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-substrate-hub",
    about = "Hub chassis — coordinator between aether-mcp + substrate fleet. ADR-0073.",
    long_about = "Hub chassis — coordinator between aether-mcp + substrate fleet. ADR-0073.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = env_only_after_help()
)]
pub struct HubCli {
    /// `--rpc-port` shadows `AETHER_RPC_PORT` — the `aether.rpc.server` bind
    /// port (the hub applies its `DEFAULT_RPC_PORT`, 8901, when unset).
    #[command(flatten)]
    pub rpc: RpcServerOverlay,

    /// Engines-cap knobs — the liveness-heartbeat tuning
    /// (`--hub-heartbeat-interval-secs` / `--hub-heartbeat-miss-limit`,
    /// issue 1339). Flattened from the derive-emitted overlay.
    #[command(flatten)]
    pub fleet: FleetOverlay,

    /// Per-actor ring-capacity knobs (issue 1990): `--actor-*`. The hub resolves
    /// `ActorRingConfig` off its own source stack for the actors its registry hosts
    /// (issue 3882 flattened the overlay here).
    #[command(flatten)]
    pub actor_ring: ActorRingOverlay,
    /// Scheduler hot-path tuning knobs (issue 2485): `--scheduler-*`. The hub
    /// resolves `SchedulerTuningConfig` off its own source stack (issue 3882).
    #[command(flatten)]
    pub scheduler: SchedulerTuningOverlay,
    /// ADR-0165 serialized-queue bounds (issue 4122): the hub resolves
    /// `RegistryQueueConfig` off its own source stack for the actors its
    /// registry hosts.
    #[command(flatten)]
    pub registry_queues: RegistryQueueOverlay,
    /// Settlement-patience backstop (issue 2062): `--settlement-cap-secs`. The hub
    /// resolves `SettlementConfig` for its own teardown budget (issue 3882).
    #[command(flatten)]
    pub settlement: SettlementOverlay,

    /// The source-selecting meta flags (`--config` / `--print-config` /
    /// `--describe`); see [`ChassisMeta`].
    #[command(flatten)]
    #[stage(skip)]
    pub meta: ChassisMeta,
}

impl ChassisCli for HubCli {
    fn meta(&self) -> &ChassisMeta {
        &self.meta
    }
}

#[cfg(test)]
mod tests {
    //! Hub root checkability (ADR-0156 §5): the hand-written root's long-flag set
    //! must equal the union of its composed overlays' flags plus the meta flags,
    //! so a dropped or stale flatten fails honestly.

    use super::HubCli;
    use aether_chassis::boot::{ActorRingOverlay, RegistryQueueOverlay, SchedulerTuningOverlay};
    use aether_chassis::cli::{long_flags, meta_flags, overlay_flags};
    use aether_fleet::FleetOverlay;
    use aether_harness_substrate::SettlementOverlay;
    use aether_rpc::RpcServerOverlay;
    use clap::CommandFactory;

    #[test]
    fn hub_root_flags_equal_composed_overlay_set() {
        // The hub composes the engines cap plus the RPC server; `--rpc-port`
        // now rides the derive-emitted `RpcServerOverlay` (#3849) like every
        // other flag, alongside the meta flags. Issue 3882 flattened the three
        // tuning overlays the hub resolves off its own source stack (actor ring /
        // scheduler / settlement).
        let mut expected = overlay_flags::<FleetOverlay>();
        expected.extend(overlay_flags::<RpcServerOverlay>());
        expected.extend(overlay_flags::<ActorRingOverlay>());
        expected.extend(overlay_flags::<SchedulerTuningOverlay>());
        expected.extend(overlay_flags::<RegistryQueueOverlay>());
        expected.extend(overlay_flags::<SettlementOverlay>());
        expected.extend(meta_flags());
        assert_eq!(long_flags(&HubCli::command()), expected);
    }
}
