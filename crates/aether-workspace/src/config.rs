//! Resolved configuration for the `aether.workspace` actor (ADR-0237 decision
//! 8, ADR-0090). The Engine API endpoint and its TLS files, the import bounds,
//! and the fixed run allotment are configuration resolved at chassis boot
//! (argv > env > file > default) and handed to `init`; the actor reads no
//! environment variable of its own, and never `DOCKER_HOST`.
//!
//! The run allotment is one fixed set of amounts every run gets until
//! executor provisioning (#6710) replaces it with a host budget, per-program
//! estimates, and FIFO admission.

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
    /// Unset means `unix:///var/run/docker.sock`. `unix://<absolute path>`
    /// dials a socket, on Unix only. `tcp://<host>:<port>` dials a daemon
    /// anywhere, always over mutual TLS, and needs all three TLS files below;
    /// the port is required and there is no plaintext TCP. The host is a DNS
    /// name or an IP literal, an IPv6 one in brackets. Any other value refuses
    /// boot naming this key. `DOCKER_HOST` is never read.
    pub endpoint: Option<String>,
    /// The PEM file of the CA a `tcp://` daemon's certificate must chain to
    /// (`AETHER_WORKSPACE_TLS_CA_FILE`), the only trust root. Required for a
    /// `tcp://` endpoint and refused beside any other.
    pub tls_ca_file: Option<String>,
    /// The PEM file of the client certificate chain presented to a `tcp://`
    /// daemon (`AETHER_WORKSPACE_TLS_CERT_FILE`). Required for a `tcp://`
    /// endpoint and refused beside any other.
    pub tls_cert_file: Option<String>,
    /// The PEM file of the client certificate's private key
    /// (`AETHER_WORKSPACE_TLS_KEY_FILE`). Required for a `tcp://` endpoint
    /// and refused beside any other.
    pub tls_key_file: Option<String>,
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
    /// The wall-clock time, in milliseconds, one run's steps may take in
    /// total, from the first step's container create to the last step's
    /// exit. A step still running when it passes is killed and the run
    /// answers `Exhausted(Time)`. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 1_800_000u64))]
    pub run_deadline_millis: u64,
    /// The memory, in bytes, each step's container may use, swap included
    /// (`Memory` = `MemorySwap`). A step the kernel kills for it answers
    /// `Exhausted(Memory)`. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 8_589_934_592u64))]
    pub memory_limit_bytes: u64,
    /// The most processes and threads each step's container may hold at
    /// once. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 4_096))]
    pub pids_limit: u32,
    /// The most tree entries a run's output `/work` may decode to, implicit
    /// parent directories included. An output over it answers `Failed`. `0`
    /// refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 1_000_000))]
    pub output_max_entries: u32,
    /// The most file content, in bytes, a run's output `/work` may decode.
    /// An output over it answers `Failed`. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 8_589_934_592u64))]
    pub output_max_bytes: u64,
}

impl Default for WorkspaceConfig {
    /// The unset resolution, stated rather than derived: a derived `Default`
    /// would give zero bounds, which boot refuses.
    fn default() -> Self {
        Self {
            endpoint: None,
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
            max_in_flight: 1,
            import_max_entries: 1_000_000,
            import_max_bytes: 8 << 30,
            run_deadline_millis: 1_800_000,
            memory_limit_bytes: 8 << 30,
            pids_limit: 4_096,
            output_max_entries: 1_000_000,
            output_max_bytes: 8 << 30,
        }
    }
}
