//! Resolved HTTP egress configuration (ADR-0090). The `#[derive(Config)]`
//! layer the chassis builds from argv/env and hands to
//! `with_actor::<HttpCapability>(config)`.

use std::collections::HashSet;
use std::time::Duration;

use super::{
    DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_IN_FLIGHT_PER_SENDER, DEFAULT_MAX_IN_FLIGHT_TOTAL, DEFAULT_TIMEOUT_MILLIS,
};

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
/// `from_argv_then_env` shims under `feature = "runtime"`. The
/// wasm-marker build carries only the domain struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(feature = "runtime", config(env_prefix = "AETHER_HTTP", cli_prefix = "http"))]
pub struct HttpConfig {
    /// Disable HTTP egress; every fetch replies with an error.
    ///
    /// Swaps the `UreqHttpAdapter` for a `DisabledHttpAdapter` that
    /// replies `HttpError::Disabled` to every fetch. `env` + `cli_long`
    /// overrides pin the wire shape (`AETHER_HTTP_DISABLE`,
    /// `--http-disable`) to the pre-derive names while the domain field
    /// stays `disabled` for read-site clarity.
    #[cfg_attr(feature = "runtime", config(env = "AETHER_HTTP_DISABLE", cli_long = "http-disable", default = false))]
    pub disabled: bool,
    /// Hostnames allowed for outbound requests; empty denies all.
    ///
    /// Each hostname is re-checked on every redirect hop within a bounded
    /// budget (issue #3463), not just on the initial URL. An empty set
    /// rejects every request (deny-by-default per ADR-0043). The
    /// `csv_set` hint auto-wires the shared comma-split parser on the env
    /// side.
    #[cfg_attr(feature = "runtime", config(default = [], csv_set))]
    pub allowlist: HashSet<String>,
    /// Reject plaintext HTTP URLs and allow only HTTPS.
    ///
    /// An `http://` URL is rejected with `HttpError::InvalidUrl`.
    #[cfg_attr(feature = "runtime", config(default = false))]
    pub require_https: bool,
    /// Maximum request and response body size in bytes.
    ///
    /// Caps both inbound and outbound body bytes. Defaults to
    /// [`DEFAULT_MAX_BODY_BYTES`] (16 MB).
    #[cfg_attr(feature = "runtime", config(default = 16_777_216))]
    pub max_body_bytes: usize,
    /// Default per-request timeout in milliseconds.
    ///
    /// Applied when a fetch request carries no explicit timeout. Defaults
    /// to [`DEFAULT_TIMEOUT_MILLIS`] (30 s). The derive's `ms_duration`
    /// hint stores the Layer field as `u32`-ms and bridges via
    /// `Duration::from_millis(u64::from(...))`;
    /// `layer_field = "timeout_ms"` pins the Layer / env / CLI shape to
    /// the pre-derive name (`AETHER_HTTP_TIMEOUT_MS`,
    /// `--http-timeout-ms`).
    #[cfg_attr(feature = "runtime", config(default = 30_000, ms_duration, layer_field = "timeout_ms"))]
    pub default_timeout: Duration,
    /// Maximum concurrent fetches one sender may have in flight (ADR-0158 §4).
    ///
    /// The per-sender fairness bound: one noisy sender cannot spend more
    /// than this share of the egress workers, so it cannot starve its
    /// peers. Defaults to [`DEFAULT_MAX_IN_FLIGHT_PER_SENDER`] (4), matching
    /// `TaskQueue::DEFAULT_MAX_IN_FLIGHT`. A resolved `0` clamps to 1 in the
    /// dispatcher (following `TaskQueue::new`'s clamp), never wedging a
    /// sender's queue.
    #[cfg_attr(feature = "runtime", config(default = 4))]
    pub max_in_flight_per_sender: usize,
    /// Maximum concurrent fetches across all senders (ADR-0158 §4).
    ///
    /// The host-protection ceiling: `N` senders each at their per-sender
    /// budget is otherwise an unbounded native worker-thread and socket
    /// count, so a fan-out of distinct senders cannot exhaust the host. A
    /// fetch dispatches only when it clears both this ceiling and its
    /// sender's budget. Defaults to [`DEFAULT_MAX_IN_FLIGHT_TOTAL`] (32) —
    /// eight senders at full per-sender budget before it engages. A
    /// resolved `0` clamps to 1 in the dispatcher.
    #[cfg_attr(feature = "runtime", config(default = 32))]
    pub max_in_flight_total: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            allowlist: HashSet::new(),
            require_https: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            default_timeout: Duration::from_millis(u64::from(DEFAULT_TIMEOUT_MILLIS)),
            max_in_flight_per_sender: DEFAULT_MAX_IN_FLIGHT_PER_SENDER,
            max_in_flight_total: DEFAULT_MAX_IN_FLIGHT_TOTAL,
        }
    }
}
