//! The `bloomery` chassis CLI root (ADR-0090 unit d): argv overlays that
//! shadow `AETHER_*` env, mirroring the hub's `HubCli`. Each overlay's
//! `into_layer()` feeds the argv > env > default resolution in
//! [`BloomeryEnv::resolve`](super::BloomeryEnv::resolve);
//! an absent flag resolves `None` and falls through to env-only resolution, so
//! boot is byte-identical when argv is empty.

use clap::Parser;

use crate::artifacts::ArtifactsOverlay;
use crate::bloomery::CoordinatorOverlay;
#[cfg(feature = "github")]
use crate::bloomery::GithubConnectionOverlay;
#[cfg(feature = "github")]
use crate::bloomery::NotifyOverlay;
use crate::bloomery::chassis::{HttpPortOverlay, RpcPortOverlay};
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
    /// (`:memory:` for a non-durable store). `--github-store-path` is the same
    /// knob; both spellings resolve to one path.
    #[command(flatten)]
    pub store: StoreOverlay,

    /// `--artifacts-root` shadows `AETHER_ARTIFACTS_ROOT` — the eviction-free
    /// artifacts content-store root (unset → the computed data-dir default).
    #[command(flatten)]
    pub artifacts: ArtifactsOverlay,

    /// `--github-*` shadow `AETHER_GITHUB_*` / `GITHUB_TOKEN` — the shared GitHub
    /// adapter connection, Actions, App-auth, and fixture knobs. Unset means
    /// unconfigured, so remote reactors mount disabled.
    #[cfg(feature = "github")]
    #[command(flatten)]
    pub github: GithubConnectionOverlay,

    /// `--notify-webhook-file` shadows `AETHER_BLOOMERY_NOTIFY_WEBHOOK_FILE` —
    /// the host-local file holding the operator webhook URL. Unset means the
    /// notification reactor mounts disabled. A path, never the URL: the URL is
    /// a credential and argv is public.
    #[cfg(feature = "github")]
    #[command(flatten)]
    pub notify: NotifyOverlay,

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

    /// Inspect the lane-host tool kit against the process PATH and exit
    /// without booting (#5035). Prints every required tool's resolved path and
    /// version, or its install line when missing. Exit `0` when the kit is
    /// complete, `1` when anything is missing — the same inspect boot logs and
    /// the admission gate refuses against.
    #[arg(long = "doctor")]
    pub doctor: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::BloomeryCli;

    #[test]
    fn a_bare_invocation_is_the_daemon_not_a_subcommand() {
        // Commission authoring is a sibling binary. A required subcommand
        // would refuse argv that is only the binary name, and a `commission`
        // verb would steal what today boots the chassis.
        let cli = match BloomeryCli::try_parse_from(["bloomery"]) {
            Ok(cli) => cli,
            Err(error) => panic!("bare bloomery must still parse as the daemon: {error}"),
        };
        assert!(!cli.describe, "a bare invocation must not be --describe");
        assert!(!cli.doctor, "a bare invocation must not be --doctor");
    }
}
