//! Hub chassis binary entry point. The hub chassis lives in
//! `aether-hub`; this binary just reads argv-then-env and runs.
//!
//! Parses argv with [`HubCli`] (ADR-0090 unit d, issue 1258);
//! `--rpc-port` shadows `AETHER_RPC_PORT`.

// CLI diagnostic before tracing subscriber is installed (issue 891).
// `--config` prints the discovery dump to stdout before boot
// (ADR-0090 §4 / e2).
#![allow(clippy::print_stderr)]
#![allow(clippy::print_stdout)]

use aether_substrate::config::{KnobKind, KnobRecord, dump_config};
use aether_substrate_bundle::cli::HubCli;
use aether_substrate_bundle::hub::{Chassis, HubChassis, HubEnv};
use clap::Parser as _;

/// The hub chassis is coordinator-only — the full-stack cap knobs
/// don't apply, so the hub dumps just its own (RPC bind port + the
/// engines-cap liveness heartbeat, issue 1339) rather than the shared
/// `chassis_config_dump`.
const HUB_KNOBS: &[KnobRecord] = &[
    KnobRecord {
        env_key: "AETHER_RPC_PORT",
        doc: "aether.rpc.server bind port (default 8901).",
        default: Some("8901"),
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_HUB_HEARTBEAT_INTERVAL_SECS",
        doc: "Engine liveness-heartbeat ping cadence in seconds; 0 disables (--hub-heartbeat-interval-secs).",
        default: Some("5"),
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_HUB_HEARTBEAT_MISS_LIMIT",
        doc: "Consecutive missed pings before an engine is evicted (--hub-heartbeat-miss-limit).",
        default: Some("3"),
        kind: KnobKind::HandRegistered,
    },
];

fn main() -> anyhow::Result<()> {
    let cli = HubCli::parse();
    if cli.config {
        print!("{}", dump_config(&[], HUB_KNOBS));
        return Ok(());
    }
    let chassis = HubChassis::build(HubEnv::from_env_with_argv(&cli))?;
    eprintln!(
        "aether-substrate-bundle: hub chassis initialised (profile={})",
        HubChassis::PROFILE,
    );
    chassis.run()?;
    Ok(())
}
