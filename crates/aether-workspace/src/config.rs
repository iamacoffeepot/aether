//! Resolved configuration for the `aether.workspace` actor (ADR-0237 decision
//! 8, ADR-0090). The Engine API endpoint and the import bounds are
//! configuration resolved at chassis boot (argv > env > file > default) and
//! handed to `init`; the actor reads no environment variable of its own, and
//! never `DOCKER_HOST`.

use alloc::string::String;

/// The endpoint an unset `endpoint` resolves to: the local daemon's socket.
pub const DEFAULT_ENDPOINT: &str = "unix:///var/run/docker.sock";

/// Resolved `aether.workspace` configuration.
///
/// Under `feature = "runtime"`, `#[derive(aether_substrate::Config)]` emits the
/// env-shaped `WorkspaceConfigLayer`, the clap-shaped `WorkspaceOverlay`, and
/// the `FromArgvThenEnv` impl. A wasm build carries only this domain struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(feature = "runtime", config(env_prefix = "AETHER_WORKSPACE", cli_prefix = "workspace"))]
pub struct WorkspaceConfig {
    /// The Docker Engine API endpoint (`AETHER_WORKSPACE_ENDPOINT`).
    ///
    /// Unset means `unix:///var/run/docker.sock`. Only `unix://` followed by
    /// an absolute socket path is accepted, and only on Unix; any other scheme
    /// refuses boot naming this key. `DOCKER_HOST` is never read.
    pub endpoint: Option<String>,
    /// The most imports that talk to the daemon at once; the rest queue, and
    /// none is dropped. A resolved `0` coerces back to the default.
    #[cfg_attr(feature = "runtime", config(default = 1, nonzero))]
    pub max_in_flight: usize,
    /// The most tree entries one import may decode, implicit parent
    /// directories included. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 1_000_000))]
    pub import_max_entries: u32,
    /// The most file content, in bytes, one import may decode. `0` refuses
    /// boot.
    #[cfg_attr(feature = "runtime", config(default = 8_589_934_592u64))]
    pub import_max_bytes: u64,
}

impl Default for WorkspaceConfig {
    /// The unset resolution, stated rather than derived: a derived `Default`
    /// would give zero bounds, which boot refuses.
    fn default() -> Self {
        Self { endpoint: None, max_in_flight: 1, import_max_entries: 1_000_000, import_max_bytes: 8 << 30 }
    }
}
