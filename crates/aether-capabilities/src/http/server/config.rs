//! Resolved HTTP server configuration (ADR-0090). The `#[derive(Config)]`
//! layer the chassis builds from argv/env and hands to
//! `with_actor::<HttpServerCapability>(config)`.

use super::{
    DEFAULT_BIND_ADDR, DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_REQUEST_BYTES,
    DEFAULT_REQUEST_TIMEOUT_MILLIS,
};

/// Init config for [`HttpServerCapability`](super::HttpServerCapability) (ADR-0108).
///
/// `bind_addr` is the address to bind (e.g. `"127.0.0.1:8080"`,
/// `"127.0.0.1:0"` to let the OS pick a port). `handler_mailbox` names the
/// single component mailbox every request is dispatched to — resolved by
/// name at dispatch time (late binding), so the handler component can load
/// or reload independently of the server. `max_request_bytes` caps the
/// request body, `max_header_bytes` caps the request line + headers, and
/// `request_timeout_millis` bounds both the per-read slow-loris timeout and
/// the handler response deadline.
///
/// `#[derive(aether_substrate::Config)]` (ADR-0090) emits the env-shaped
/// `HttpServerConfigLayer`, the clap-shaped `HttpServerOverlay`, and the
/// inherent `from_env` / `from_argv_then_env` shims under
/// `feature = "runtime"`; the wasm-marker build carries only this domain
/// struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(
    feature = "runtime",
    config(env_prefix = "AETHER_HTTP_SERVER", cli_prefix = "http-server")
)]
pub struct HttpServerConfig {
    /// Whether to bind the listening socket at all. Default `false` —
    /// the HTTP server is opt-in, so an unconfigured chassis binds no
    /// port. The remaining fields are consulted only when this is `true`.
    #[cfg_attr(feature = "runtime", config(default = false))]
    pub enabled: bool,
    /// Address to bind the listening socket. Defaults to loopback
    /// ([`DEFAULT_BIND_ADDR`]); a public interface is an explicit choice.
    /// A blank override (`AETHER_HTTP_SERVER_BIND_ADDR=`) falls back to
    /// the default — the derive treats an empty `String` as unset.
    #[cfg_attr(feature = "runtime", config(default = "127.0.0.1:8080"))]
    pub bind_addr: String,
    /// The single handler mailbox every request is dispatched to (e.g.
    /// `"aether.component/aether.embedded:web"`). Empty = every request is
    /// answered `503` (no handler resolves).
    #[cfg_attr(feature = "runtime", config(default = ""))]
    pub handler_mailbox: String,
    /// Cap on the request body in bytes ([`DEFAULT_MAX_REQUEST_BYTES`]);
    /// an announced `Content-Length` past this is answered `413`.
    #[cfg_attr(feature = "runtime", config(default = 1_048_576))]
    pub max_request_bytes: usize,
    /// Cap on the request line + header bytes ([`DEFAULT_MAX_HEADER_BYTES`]);
    /// a head that grows past this is answered `431`.
    #[cfg_attr(feature = "runtime", config(default = 65_536))]
    pub max_header_bytes: usize,
    /// Per-read socket timeout (slow-loris guard) and handler response
    /// deadline in milliseconds ([`DEFAULT_REQUEST_TIMEOUT_MILLIS`]); a
    /// handler that doesn't reply in time yields `504`.
    #[cfg_attr(feature = "runtime", config(default = 30_000))]
    pub request_timeout_millis: u64,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            handler_mailbox: String::new(),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            request_timeout_millis: DEFAULT_REQUEST_TIMEOUT_MILLIS,
        }
    }
}
