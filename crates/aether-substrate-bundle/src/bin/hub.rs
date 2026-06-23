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

use aether_capabilities::EngineConfigLayer;
use aether_substrate::config::{KnobKind, KnobRecord, dump_config};
use aether_substrate_bundle::cli::HubCli;
use aether_substrate_bundle::hub::{Chassis, HubChassis, HubEnv};
use clap::Parser as _;
use confique::Config as _;

/// `AETHER_RPC_PORT` is the hub's chassis-special bind-port knob —
/// resolved via `HubCli --rpc-port` rather than an `EngineConfig`
/// field, so it cannot ride the `EngineConfigLayer::META` walk and
/// stays a hand `KnobRecord`.
const RPC_PORT_RECORD: KnobRecord = KnobRecord {
    env_key: "AETHER_RPC_PORT",
    doc: "aether.rpc.server bind port (default 8901).",
    default: Some("8901"),
    kind: KnobKind::HandRegistered,
};

fn main() -> anyhow::Result<()> {
    let cli = HubCli::parse();
    if cli.config {
        print!(
            "{}",
            dump_config(&[&EngineConfigLayer::META], &[RPC_PORT_RECORD])
        );
        return Ok(());
    }
    // `--describe` (ADR-0115, issue 1953): print this binary's manifest —
    // chassis kind, linked caps, build provenance — as JSON, then exit
    // before boot.
    if cli.describe {
        println!(
            "{}",
            serde_json::to_string(&HubChassis::describe_manifest())?
        );
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
