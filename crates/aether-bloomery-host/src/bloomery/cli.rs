//! The `bloomery` chassis CLI root (ADR-0090 unit d): argv overlays that
//! shadow `AETHER_*` env, mirroring the hub's `HubCli`. Each overlay's
//! `into_layer()` feeds the argv > env > default resolution in
//! [`BloomeryEnv::from_env_with_argv`](super::BloomeryEnv::from_env_with_argv);
//! an absent flag resolves `None` and falls through to env-only resolution, so
//! boot is byte-identical when argv is empty.

use clap::Parser;

use crate::bloomery::chassis::RpcPortOverlay;
use crate::store::StoreOverlay;

/// The `bloomery` binary's clap root. The two overlays carry the derive-emitted
/// `--rpc-port` / `--store-path` flags; `--describe` prints the binary manifest
/// and exits before boot (ADR-0115).
#[derive(Parser, Debug, Default, Clone)]
#[command(name = "bloomery", about = "Bloomery coordinator chassis — SQLite journal store + RPC ingress. ADR-0149.")]
pub struct BloomeryCli {
    /// `--rpc-port` shadows `AETHER_RPC_PORT` — the RPC ingress bind port.
    #[command(flatten)]
    pub rpc: RpcPortOverlay,

    /// `--store-path` shadows `AETHER_STORE_PATH` — the `SQLite` journal file
    /// (`:memory:` for a non-durable store).
    #[command(flatten)]
    pub store: StoreOverlay,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps, build
    /// provenance) as JSON and exit before boot (ADR-0115). The hub's binary
    /// store forks `<binary> --describe` once at upload time to capture what a
    /// stored binary is.
    #[arg(long = "describe")]
    pub describe: bool,
}
