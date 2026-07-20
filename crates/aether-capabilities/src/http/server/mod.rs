//! `aether.http.server` — substrate HTTP server capability (ADR-0108,
//! issue 1760).
//!
//! Singleton actor modeled on `RpcServerCapability`. It binds a
//! `TcpListener` on the configured address at init, runs a sidecar accept
//! thread that hands each accepted socket to a per-connection reader
//! thread. A reader parses one HTTP/1.1 request (request line + headers +
//! a `Content-Length`-bounded body), pushes it over an internal mpsc, and
//! fires an [`HttpInboundReady`] wake mail at the cap's own mailbox so the
//! dispatcher drains the queue.
//!
//! On a parsed request the cap dispatches an
//! [`HttpServerRequest`](crate::http::kinds::HttpServerRequest) to the configured
//! handler mailbox as a fresh causal chain via
//! `NativeCtx::send_envelope_detached` (the wake mail is causally unrelated
//! to the inbound request), records the open response socket in an
//! in-flight table keyed by the dispatch's correlation id, and subscribes
//! to settlement of the dispatched root. The handler replies
//! [`HttpServerResponse`](crate::http::kinds::HttpServerResponse); the reply
//! routes back to the cap, the
//! reply-interception fallback formats the HTTP/1.1 response and writes it
//! to the held socket. A response-less chain settles into `502`, a
//! per-request timeout into `504`, and the trust caps reject oversize or
//! malformed input with `413` / `431` / `501` before any dispatch.
//!
//! ADR-0122 identity/runtime split: the addressing identity is the ZST
//! [`HttpServerCapability`]; the state-bearing runtime (the listener, the
//! accept thread, the connection table) lives in the `runtime` module behind the
//! one `feature = "runtime"` gate.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds resolve at file root through these imports —
// `#[actor]` emits the `HandlesKind<K>` markers always-on against the
// identity, and the `init` / handler bodies name these kinds. Post-ADR-0135
// the supervisor's surface is the sidecar wake plus route registration
// (ADR-0130); the per-request kinds (streaming, websocket, settlement,
// reply interception) live on the dispatch shard identity in `shard`.
use crate::http::kinds::HttpInboundReady;
use crate::http::kinds::{
    RegisterRoute, RegisterRouteResult, RegisterRouteSelf, UnregisterRoute, UnregisterRouteSelf, UnregisterRoutesAll,
};

// Default bind address. Loopback per ADR-0108 §6 — binding a public
// interface is an explicit operator choice.
/// Default `bind_addr` when unset: loopback, OS-assigned port (ADR-0108 §6).
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";
/// Default `max_request_bytes` (request body cap): 1 `MiB`.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1_048_576;
/// Default `max_header_bytes` (request line + headers cap): 64 `KiB`.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 65_536;
/// Default `request_timeout_millis` (slow-loris read + response deadline): 30 s.
pub const DEFAULT_REQUEST_TIMEOUT_MILLIS: u64 = 30_000;
/// Default `keep_alive_timeout_millis` (idle timeout between keep-alive
/// requests, and for a fresh connection that never sends): 5 s. Short by
/// design — a kept-alive connection sitting idle must not pin a reader
/// thread for the full request timeout.
pub const DEFAULT_KEEP_ALIVE_TIMEOUT_MILLIS: u64 = 5_000;
/// Default `max_connections` (live connection-table ceiling): a generous
/// bound that stops unbounded thread-per-connection resource exhaustion
/// without tripping legitimate loopback use. Operator-tunable; each unit
/// costs roughly one reader-thread stack.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;
/// Default `response_stream_window` (ADR-0128): the credit-window depth, a
/// count of in-flight response chunks a streaming handler may hold before the
/// cap's per-connection writer thread drains one and replenishes credit.
pub const DEFAULT_RESPONSE_STREAM_WINDOW: u32 = 16;
/// Default `request_stream_window` (ADR-0128): the inbound credit-window depth,
/// a count of in-flight [`HttpRequestChunk`] mails the cap delivers to a
/// streaming handler before it parks the per-connection reader awaiting the
/// handler's [`HttpRequestCredit`] replenishment.
///
/// [`HttpRequestChunk`]: crate::http::kinds::HttpRequestChunk
/// [`HttpRequestCredit`]: crate::http::kinds::HttpRequestCredit
pub const DEFAULT_REQUEST_STREAM_WINDOW: u32 = 16;
/// Default `websocket_idle_timeout_millis` (ADR-0129): the read deadline on an
/// upgraded websocket connection between frames, 5 minutes. Distinct from
/// `request_timeout_millis` (the slow-loris in-flight read deadline) and much
/// longer — an idle websocket sitting between messages is normal, not a
/// slow-loris attack, so it is bounded by a generous keepalive window rather
/// than the request deadline.
pub const DEFAULT_WS_IDLE_TIMEOUT_MILLIS: u64 = 300_000;

mod config;
mod shard;

pub use config::HttpServerConfig;
// The `Config` derive on `HttpServerConfig` emits these native-only sibling
// types in `config`; chassis CLI / boot wiring addresses them through the
// `server::` path, so re-export them here.
#[cfg(feature = "runtime")]
pub use config::{HttpServerConfigLayer, HttpServerOverlay};

/// Exported handle bundle published at boot. Reachable from the chassis
/// via `PassiveChassis::handle::<HttpServerHandle>()`; the load-bearing
/// field is `local_port` so embedders / tests can connect to the
/// OS-picked port when `bind_addr` requested port 0.
///
/// Plain data (no substrate type), so it stays at file root under the
/// existing `not(target_family = "wasm")` gate — the `pub use
/// server::HttpServerHandle` chain in `http/mod.rs` reads it from here.
#[cfg(not(target_family = "wasm"))]
#[derive(Clone)]
pub struct HttpServerHandle {
    pub local_port: u16,
}

/// `aether.http.server` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable`, the per-handler
/// `HandlesKind` markers, the `#[fallback]` reply-interception marker, and the
/// name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`HttpSupervisorState`, which owns the listener +
/// accept thread + shared route table + shard sinks, ADR-0135) lives behind
/// the one `feature = "runtime"` gate, so a transport-only build never names
/// the state type nor pulls `aether_substrate` through this cap.
#[actor(singleton)]
pub struct HttpServerCapability;

// The struct-hosted `#[actor(singleton)]` reads the sibling `runtime` module
// off disk, lifts the `NAMESPACE` + `#[handler]` kinds out of the
// `#[runtime] impl NativeActor` there, and emits the always-on identity
// markers (`Addressable`, one `HandlesKind<K>` per handler, the `#[fallback]`
// marker, the name-inventory entry) against this struct. The kind types those
// markers name (`HttpInboundReady` / `Settled`) are imported at file root
// above.
use aether_actor::actor;

// The runtime half — the whole `aether_substrate`-typed surface (the state,
// the sidecar threads, the parse/render machinery, the `#[runtime] impl
// NativeActor` with the handler bodies) — lives in the `runtime/` submodule,
// gated once here.
#[cfg(feature = "runtime")]
mod runtime;

#[cfg(all(test, feature = "runtime"))]
mod tests;
