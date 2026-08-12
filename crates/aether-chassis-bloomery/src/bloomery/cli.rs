//! The `bloomery` chassis CLI root (ADR-0090 unit d): argv overlays that
//! shadow `AETHER_*` env, mirroring the hub's `HubCli`. Each overlay's
//! `into_layer()` feeds the argv > env > default resolution in
//! [`BloomeryEnv::resolve`](super::BloomeryEnv::resolve);
//! an absent flag resolves `None` and falls through to env-only resolution, so
//! boot is byte-identical when argv is empty.

use clap::Parser;

use crate::artifacts::ArtifactsOverlay;
use crate::bloomery::chassis::{HttpPortOverlay, RpcPortOverlay};
use crate::bloomery::{CoordinatorOverlay, GithubConnectionOverlay};
use crate::session::SessionOverlay;
use crate::signing::SigningOverlay;
use crate::store::StoreOverlay;

/// The `bloomery` binary's clap root. The overlays carry the derive-emitted
/// `--rpc-port` / `--store-path` / `--artifacts-root` / `--github-*` flags;
/// `--describe` prints the binary manifest and exits before boot (ADR-0115).
#[derive(Parser, Debug, Default, Clone)]
#[command(name = "bloomery", about = "Bloomery coordinator chassis — SQLite journal store + RPC ingress. ADR-0149.")]
pub struct BloomeryCli {
    /// `--rpc-port` shadows `AETHER_RPC_PORT` — the RPC ingress bind port.
    #[command(flatten)]
    pub rpc: RpcPortOverlay,

    /// `--http-port` shadows `AETHER_HTTP_PORT` — the REST control-API bind port.
    #[command(flatten)]
    pub http: HttpPortOverlay,

    /// `--store-path` shadows `AETHER_STORE_PATH` — the `SQLite` journal file
    /// (`:memory:` for a non-durable store).
    #[command(flatten)]
    pub store: StoreOverlay,

    /// `--artifacts-root` shadows `AETHER_ARTIFACTS_ROOT` — the eviction-free
    /// artifacts content-store root (unset → the computed data-dir default).
    #[command(flatten)]
    pub artifacts: ArtifactsOverlay,

    /// `--github-*` shadow `AETHER_GITHUB_*` / `GITHUB_TOKEN` — the shared GitHub
    /// adapter connection, Actions, App-auth, and fixture knobs. Unset means
    /// unconfigured, so remote reactors mount disabled.
    #[command(flatten)]
    pub github: GithubConnectionOverlay,

    /// Backend-neutral coordinator cadence, persistence, routing, process, and identity knobs.
    #[command(flatten)]
    pub coordinator: CoordinatorOverlay,

    /// `--session-db-path` / `--session-cache-ttl-cutoff-mins` /
    /// `--session-lease-ttl-mins` / `--session-context-cap-tokens` shadow the
    /// `AETHER_SESSION_*` env — the executor session-reuse pool knobs.
    #[command(flatten)]
    pub session: SessionOverlay,

    /// `--signing-allowlist` shadows `AETHER_SIGNING_ALLOWLIST` — the host-local
    /// authorized-signer allowlist (`key-id:hex-public-key` entries) the
    /// `aether.signing` capability verifies answer signatures against.
    #[command(flatten)]
    pub signing: SigningOverlay,

    /// Print this binary's `BinaryManifest` (chassis kind, linked caps, build
    /// provenance) as JSON and exit before boot (ADR-0115). The hub's binary
    /// store forks `<binary> --describe` once at upload time to capture what a
    /// stored binary is.
    #[arg(long = "describe")]
    pub describe: bool,
}
