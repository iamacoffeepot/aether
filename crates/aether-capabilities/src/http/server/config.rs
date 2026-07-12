//! Resolved HTTP server configuration (ADR-0090). The `#[derive(Config)]`
//! layer the chassis builds from argv/env and hands to
//! `with_actor::<HttpServerCapability>(config)`.

use super::{
    DEFAULT_BIND_ADDR, DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS, DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_HEADER_BYTES,
    DEFAULT_MAX_REQUEST_BYTES, DEFAULT_REQUEST_STREAM_WINDOW, DEFAULT_REQUEST_TIMEOUT_MILLIS,
    DEFAULT_RESPONSE_STREAM_WINDOW, DEFAULT_WS_IDLE_TIMEOUT_MILLIS,
};

/// Init config for [`HttpServerCapability`](super::HttpServerCapability) (ADR-0108).
///
/// `bind_addr` is the address to bind (e.g. `"127.0.0.1:8080"`,
/// `"127.0.0.1:0"` to let the OS pick a port). `handler_mailbox` is the
/// single fallback mailbox every unrouted request is dispatched to —
/// resolved once at config time to a `MailboxId`, its liveness checked per
/// request (late binding), so the handler component can load or reload
/// independently of the server. `max_request_bytes` caps the
/// request body, `max_header_bytes` caps the request line + headers,
/// `request_timeout_millis` bounds both the per-read slow-loris timeout and
/// the handler response deadline, and `max_connections` caps the live
/// connection table.
///
/// `#[derive(aether_substrate::Config)]` (ADR-0090) emits the env-shaped
/// `HttpServerConfigLayer`, the clap-shaped `HttpServerOverlay`, and the
/// inherent `from_env` / `from_argv_then_env` shims under
/// `feature = "runtime"`; the wasm-marker build carries only this domain
/// struct.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(feature = "runtime", config(env_prefix = "AETHER_HTTP_SERVER", cli_prefix = "http-server"))]
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
    /// The single fallback mailbox every request with no route-table match
    /// is dispatched to, resolved once at config time from its `/`-rendered
    /// lineage path (e.g. `"aether.component/aether.embedded:web"`) via the
    /// same ADR-0099 fold `Registry::lookup` uses. `None` (an empty
    /// `AETHER_HTTP_SERVER_HANDLER_MAILBOX`) = every unrouted request is
    /// answered `503` (no fallback handler). Liveness is still checked per
    /// request by id, so the handler component can load or reload
    /// independently of the server.
    #[cfg_attr(feature = "runtime", config(parse = parse_handler_mailbox))]
    pub handler_mailbox: Option<aether_data::MailboxId>,
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
    /// Idle timeout in milliseconds ([`DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS`])
    /// bounding a kept-alive connection sitting between requests (and a
    /// fresh connection that never sends its first byte); on expiry the
    /// idle connection is closed rather than pinning a reader thread.
    /// Distinct from `request_timeout_millis`, which stays the in-flight
    /// read (slow-loris) timeout and the handler response deadline.
    #[cfg_attr(feature = "runtime", config(default = 5_000))]
    pub keep_alive_timeout_millis: u64,
    /// Cap on live connections ([`DEFAULT_MAX_CONNECTIONS`]); once the
    /// connection table is at capacity a newly-accepted peer is refused
    /// `503` and closed rather than getting a reader thread.
    #[cfg_attr(feature = "runtime", config(default = 1024))]
    pub max_connections: usize,
    /// Response-stream credit-window depth ([`DEFAULT_RESPONSE_STREAM_WINDOW`],
    /// ADR-0128): the count of in-flight [`HttpResponseChunk`] mails a
    /// streaming handler may hold before the per-connection writer thread
    /// drains one and the cap replenishes credit. A count, not a byte or time
    /// quantity, so it carries no unit suffix.
    ///
    /// [`HttpResponseChunk`]: crate::http::kinds::HttpResponseChunk
    #[cfg_attr(feature = "runtime", config(default = 16))]
    pub response_stream_window: u32,
    /// Inbound request-stream credit-window depth ([`DEFAULT_REQUEST_STREAM_WINDOW`],
    /// ADR-0128): the count of in-flight [`HttpRequestChunk`] mails the cap
    /// delivers to a streaming handler before it parks the per-connection
    /// reader awaiting the handler's [`HttpRequestCredit`]. A full window backs
    /// pressure up into TCP so a fast peer cannot outrun a slow handler. A
    /// count, not a byte or time quantity, so it carries no unit suffix.
    ///
    /// [`HttpRequestChunk`]: crate::http::kinds::HttpRequestChunk
    /// [`HttpRequestCredit`]: crate::http::kinds::HttpRequestCredit
    #[cfg_attr(feature = "runtime", config(default = 16))]
    pub request_stream_window: u32,
    /// Dispatch-shard count (ADR-0135): how many instanced dispatch actors
    /// the supervisor spawns to run the per-connection request machinery in
    /// parallel. `0` (the default) sizes automatically to the scheduler
    /// pool's worker count. A count, not a byte or time quantity, so it
    /// carries no unit suffix.
    #[cfg_attr(feature = "runtime", config(default = 0))]
    pub dispatch_shards: usize,
    /// Websocket idle timeout in milliseconds ([`DEFAULT_WS_IDLE_TIMEOUT_MILLIS`],
    /// ADR-0129): the read deadline between frames on an upgraded websocket
    /// connection. Distinct from `request_timeout_millis` (the slow-loris
    /// in-flight read deadline) and much longer — an idle websocket between
    /// messages is normal, so it is bounded by a generous keepalive window, on
    /// expiry the idle socket is torn down rather than pinning a reader thread.
    #[cfg_attr(feature = "runtime", config(default = 300_000))]
    pub websocket_idle_timeout_millis: u64,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            handler_mailbox: None,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            request_timeout_millis: DEFAULT_REQUEST_TIMEOUT_MILLIS,
            keep_alive_timeout_millis: DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            response_stream_window: DEFAULT_RESPONSE_STREAM_WINDOW,
            request_stream_window: DEFAULT_REQUEST_STREAM_WINDOW,
            dispatch_shards: 0,
            websocket_idle_timeout_millis: DEFAULT_WS_IDLE_TIMEOUT_MILLIS,
        }
    }
}

/// Parse the `handler_mailbox` override. An empty string errors so confique
/// treats it as unset (mirroring `parse_dir` in the fs cap — `Err` + empty →
/// `None`); any non-empty value is a `/`-rendered lineage path folded to its
/// [`MailboxId`](aether_data::MailboxId) by the same ADR-0099
/// [`mailbox_id_from_path`](aether_data::mailbox_id_from_path) fold
/// `Registry::lookup` uses internally, so a nested-component path and a flat
/// root name resolve to exactly the id they do at dispatch today.
#[cfg(feature = "runtime")]
fn parse_handler_mailbox(s: &str) -> Result<aether_data::MailboxId, EmptyHandlerMailbox> {
    if s.is_empty() {
        Err(EmptyHandlerMailbox)
    } else {
        Ok(aether_data::mailbox_id_from_path(s))
    }
}

/// Sentinel error: an empty `AETHER_HTTP_SERVER_HANDLER_MAILBOX` value,
/// treated as unset by confique's parse path (`Err` + empty → `None`).
#[cfg(feature = "runtime")]
#[derive(Debug)]
struct EmptyHandlerMailbox;

#[cfg(feature = "runtime")]
impl std::fmt::Display for EmptyHandlerMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("empty handler-mailbox override")
    }
}

#[cfg(feature = "runtime")]
impl std::error::Error for EmptyHandlerMailbox {}

// ADR-0090: the parser is pure — it exercises the empty-is-unset guard and
// the non-empty lineage fold without touching process env. Native-only
// because `parse_handler_mailbox` only exists under the `runtime` feature.
#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::parse_handler_mailbox;

    #[test]
    fn parse_handler_mailbox_treats_empty_as_unset() {
        assert!(parse_handler_mailbox("").is_err(), "empty → unset (Err → None)");
        assert_eq!(
            parse_handler_mailbox("aether.component/aether.embedded:web").expect("non-empty folds to an id"),
            aether_data::mailbox_id_from_path("aether.component/aether.embedded:web")
        );
    }
}
