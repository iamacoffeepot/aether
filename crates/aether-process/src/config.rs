//! Resolved configuration for the `aether.process` cap (ADR-0157
//! §Security, ADR-0090). The security posture is configuration, never a
//! handler-time environment read: the allowlist, the default timeout, and
//! the concurrency bound all resolve at chassis boot through the
//! `#[derive(Config)]` layer (argv > env > file > default) and land in
//! `init` as the cap's `Config`.

use std::collections::HashSet;
use std::time::Duration;

use super::{DEFAULT_MAX_IN_FLIGHT, DEFAULT_TIMEOUT_MILLIS};

/// Resolved `aether.process` configuration.
///
/// ADR-0090 unit g: the `#[derive(aether_substrate::Config)]` emits the
/// env-shaped `ProcessConfigLayer`, the clap-shaped `ProcessOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` shims under
/// `feature = "runtime"`. The wasm-marker build carries only this domain
/// struct.
///
/// The allowlist is the **deny-by-default** security boundary: empty by
/// default, so a freshly booted capability refuses every request until an
/// operator names the binaries it may run. Each entry is a
/// `"name=/absolute/path"` token — the request's `binary` field is a
/// logical *name* resolved against these, so a caller never supplies a
/// filesystem path and cannot reach an arbitrary executable (ADR-0157).
/// The tokens ride a `csv_set` `HashSet<String>` (the derive's only
/// collection shape); the runtime half splits each into
/// `(name, absolute path)` at `init`.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(feature = "runtime", config(env_prefix = "AETHER_PROCESS", cli_prefix = "process"))]
pub struct ProcessConfig {
    /// Deny-by-default allowlist of permitted binaries, each a
    /// `"name=/absolute/path"` token (`AETHER_PROCESS_ALLOWLIST`, a
    /// comma-separated list). Empty refuses every request.
    #[cfg_attr(feature = "runtime", config(default = [], csv_set))]
    pub allowlist: HashSet<String>,
    /// Maximum number of concurrent in-flight runs.
    ///
    /// A per-cap concurrency bound over the ADR-0093 dispatch queue; runs
    /// past the bound queue. The `nonzero` hint coerces a resolved `0`
    /// (which would queue forever) back to the default.
    #[cfg_attr(feature = "runtime", config(default = 8, nonzero))]
    pub max_in_flight: usize,
    /// Default per-run deadline in milliseconds, applied when a `run`
    /// request carries `timeout_millis == 0`.
    ///
    /// The derive's `ms_duration` hint + `layer_field = "timeout_ms"` pin
    /// the Layer / env / CLI shape (`AETHER_PROCESS_TIMEOUT_MS`,
    /// `--process-timeout-ms`).
    #[cfg_attr(feature = "runtime", config(default = 30_000, ms_duration, layer_field = "timeout_ms"))]
    pub default_timeout: Duration,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            allowlist: HashSet::new(),
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            default_timeout: Duration::from_millis(u64::from(DEFAULT_TIMEOUT_MILLIS)),
        }
    }
}
