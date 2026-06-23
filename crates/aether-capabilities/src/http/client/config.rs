//! Resolved HTTP egress configuration (ADR-0090). The `#[derive(Config)]`
//! layer the chassis builds from argv/env and hands to
//! `with_actor::<HttpCapability>(config)`.

use std::collections::HashSet;
use std::time::Duration;

use super::{DEFAULT_MAX_BODY_BYTES, DEFAULT_TIMEOUT_MILLIS};

/// Resolved configuration for the substrate's HTTP adapter. Chassis
/// mains read env vars (`AETHER_HTTP_DISABLE`, `AETHER_HTTP_ALLOWLIST`,
/// `AETHER_HTTP_REQUIRE_HTTPS`, `AETHER_HTTP_MAX_BODY_BYTES`,
/// `AETHER_HTTP_TIMEOUT_MS`) into a `HttpConfig` and pass it to
/// `HttpCapability::new`. Tests build a `HttpConfig` directly,
/// never touching process env (issue 464).
///
/// ADR-0090 unit g (iamacoffeepot/aether#1264): the
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `HttpConfigLayer`, the clap-shaped `HttpOverlay`, the
/// `FromArgvThenEnv` impl, and the inherent `from_env` /
/// `from_argv_then_env` shims under `feature = "native"`. The
/// wasm-marker build carries only the domain struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "native", derive(aether_substrate::Config))]
#[cfg_attr(
    feature = "native",
    config(env_prefix = "AETHER_HTTP", cli_prefix = "http")
)]
pub struct HttpConfig {
    /// `AETHER_HTTP_DISABLE=1` swaps the `UreqHttpAdapter` for a
    /// `DisabledHttpAdapter` that replies `HttpError::Disabled` to
    /// every fetch. `env` + `cli_long` overrides pin the wire shape
    /// (`AETHER_HTTP_DISABLE`, `--http-disable`) to the pre-derive
    /// names while the domain field stays `disabled` for read-site
    /// clarity.
    #[cfg_attr(
        feature = "native",
        config(
            env = "AETHER_HTTP_DISABLE",
            cli_long = "http-disable",
            default = false
        )
    )]
    pub disabled: bool,
    /// Hostnames the adapter will dial. Empty = deny all
    /// (deny-by-default per ADR-0043). The `csv_set` hint auto-wires the
    /// shared comma-split parser on the env side.
    #[cfg_attr(feature = "native", config(default = [], csv_set))]
    pub allowlist: HashSet<String>,
    /// `AETHER_HTTP_REQUIRE_HTTPS=1` rejects `http://` URLs with
    /// `HttpError::InvalidUrl`.
    #[cfg_attr(feature = "native", config(default = false))]
    pub require_https: bool,
    /// Cap on inbound and outbound body bytes. Defaults to
    /// [`DEFAULT_MAX_BODY_BYTES`] (16 MB).
    #[cfg_attr(feature = "native", config(default = 16_777_216))]
    pub max_body_bytes: usize,
    /// Default per-request timeout when `Fetch.timeout_ms` is
    /// `None`. Defaults to [`DEFAULT_TIMEOUT_MILLIS`] (30 s). The derive's
    /// `ms_duration` hint stores the Layer field as `u32`-ms and
    /// bridges via `Duration::from_millis(u64::from(...))`;
    /// `layer_field = "timeout_ms"` pins the Layer / env / CLI shape to
    /// the pre-derive name (`AETHER_HTTP_TIMEOUT_MS`,
    /// `--http-timeout-ms`).
    #[cfg_attr(
        feature = "native",
        config(default = 30_000, ms_duration, layer_field = "timeout_ms")
    )]
    pub default_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            allowlist: HashSet::new(),
            require_https: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            default_timeout: Duration::from_millis(u64::from(DEFAULT_TIMEOUT_MILLIS)),
        }
    }
}
