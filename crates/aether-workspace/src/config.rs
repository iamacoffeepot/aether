//! Resolved configuration for the `aether.workspace` actor (ADR-0237
//! decisions 8 and 9, ADR-0090). The Engine API endpoint and its TLS files,
//! the import bounds, the host budget runs are provisioned from, the default
//! allotment a run key never seen gets, and the fixed per-container limits are
//! configuration resolved at chassis boot (argv > env > file > default) and
//! handed to `init`; the actor reads no environment variable of its own, and
//! never `DOCKER_HOST`.
//!
//! The actor chooses each run's cores, memory, and deadline itself (decision
//! 9): the budget knobs state what it may hand out, and whatever the host
//! keeps back is the cores left out of `cpuset` and the memory left out of
//! `budget_memory_bytes`.

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
    /// The cores runs may be pinned to, in Docker's cpuset list syntax
    /// (`0-3,6`): indices from 0 to 1023, ranges low to high, overlaps
    /// merged. Default `"0"`. An empty list, a reversed range, a value that
    /// is not a number, or an index above 1023 refuses boot.
    #[cfg_attr(feature = "runtime", config(default = "0"))]
    pub cpuset: String,
    /// The memory, in bytes, the actor may reserve for runs at once. Every
    /// allotment's memory is clamped to it. Default 8 GiB. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 8_589_934_592u64))]
    pub budget_memory_bytes: u64,
    /// The cores each run is pinned to, clamped to the number of cores in
    /// `cpuset`. Default 4. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 4))]
    pub run_cores: u32,
    /// The memory, in bytes, each step's container gets, swap included,
    /// in a run whose key the actor has not seen. Clamped to
    /// `budget_memory_bytes`. Default 8 GiB. `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 8_589_934_592u64))]
    pub default_memory_bytes: u64,
    /// The wall-clock time, in milliseconds, a run whose key the actor has
    /// not seen may take across its steps, from the first step's container
    /// create to the last step's exit. Clamped to `max_deadline_millis`.
    /// Default 1,800,000 (30 minutes). `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 1_800_000u64))]
    pub default_deadline_millis: u64,
    /// The longest deadline, in milliseconds, any run is given, including
    /// after repeated `Exhausted(Time)` answers doubled its estimate: how
    /// long a hung step can hold its cores per attempt. Default 14,400,000
    /// (4 hours). `0` refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 14_400_000u64))]
    pub max_deadline_millis: u64,
    /// The percent a seen key's estimated peak memory and wall time are
    /// scaled by to give its allotment. Default 150. Below 100 refuses boot.
    #[cfg_attr(feature = "runtime", config(default = 150))]
    pub headroom_percent: u32,
    /// The most processes and threads each step's container may hold at
    /// once, the same for every run. `0` refuses boot.
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
            cpuset: String::from("0"),
            budget_memory_bytes: 8 << 30,
            run_cores: 4,
            default_memory_bytes: 8 << 30,
            default_deadline_millis: 1_800_000,
            max_deadline_millis: 14_400_000,
            headroom_percent: 150,
            pids_limit: 4_096,
            output_max_entries: 1_000_000,
            output_max_bytes: 8 << 30,
        }
    }
}
