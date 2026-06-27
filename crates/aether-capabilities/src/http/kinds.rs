//! Wire kinds owned by the two HTTP capabilities (ADR-0121): the egress
//! client (`client.rs`) and the ingress server (`server.rs`). The
//! substrate core dispatches none of them, so they live with the
//! capabilities that own them rather than in `aether-kinds`. The module
//! is always-on and wasm-safe — the types depend only on
//! `aether_data::{Kind, Schema}` and serde, so the
//! `default-features = false` wasm consumers keep compiling.

use serde::{Deserialize, Serialize};

// ADR-0043 substrate HTTP egress. One request kind + one reply
// kind on the `"aether.http"` sink, plus supporting `HttpMethod`,
// `HttpHeader`, and `HttpError` shapes. All structured
// (Strings, Vecs, Option<u32>).
//
// Reply correlation follows the ADR-0041 pattern: the reply
// echoes the originating `url` so callers match reply-to-request
// without threading a pending-op queue. Request `body` is not
// echoed — correlation needs the identity of the request, not
// its contents, and a multi-MB upload should not round-trip its
// bytes. Components needing strict per-op correlation (same URL
// fired back-to-back, non-idempotent POST) lean on ADR-0042's
// per-Source correlation ids via `prev_correlation_p32` rather
// than a per-kind field.

/// HTTP method carried on `Fetch`. Enumerating at the schema
/// layer keeps `"get"` / `"GET"` / `"Get"` from disagreeing
/// across guests; the substrate maps each variant to its
/// canonical uppercase name when calling the HTTP backend.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

/// One HTTP header on a `Fetch` request or `FetchResult`
/// response. Expressed as a named-field struct because
/// `aether_data::Schema` has no blanket impl for tuples — if
/// that lands later the wire shape here is source-compatible
/// (same two fields in the same order).
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// Structured failure reason for an HTTP request (ADR-0043 §1).
/// Typed variants cover the branches agents routinely need to
/// match on — `Timeout` → retry, `AllowlistDenied` → config
/// issue, `BodyTooLarge` → chunk the response, `Disabled` →
/// surface to the operator. `InvalidUrl` carries the offending
/// URL text; `AdapterError` is the catchall preserving backend-
/// specific detail (DNS failure, TLS handshake, connection
/// refused, etc.) as free-form text.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    InvalidUrl(String),
    Timeout,
    BodyTooLarge,
    AllowlistDenied,
    Disabled,
    AdapterError(String),
}

/// `aether.http.fetch` — request the substrate perform an HTTP
/// request and reply with the response. Mailed to the
/// `"aether.http"` sink; reply lands via `reply_mail` as
/// `FetchResult`.
/// `timeout_ms` overrides the chassis default
/// (`AETHER_HTTP_TIMEOUT_MS`, default 30000) when set; `None`
/// uses the default.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.fetch")]
pub struct Fetch {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
    pub timeout_ms: Option<u32>,
}

/// Reply to `Fetch`. Both arms echo the originating `url` so the
/// caller correlates reply-to-request without threading a
/// pending-op queue — operation identity comes from the reply
/// kind itself (`aether.http.fetch_result`). Request `body` is
/// deliberately not echoed: correlation needs the identity of
/// the request, not its contents, and a multi-MB upload should
/// not round-trip. `Ok` carries the HTTP status, response
/// headers, and response body (bounded by
/// `AETHER_HTTP_MAX_BODY_BYTES`, default 16MB); `Err` carries an
/// `HttpError` variant.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.fetch_result")]
pub enum FetchResult {
    Ok {
        url: String,
        status: u16,
        headers: Vec<HttpHeader>,
        body: Vec<u8>,
    },
    Err {
        url: String,
        error: HttpError,
    },
}

// ADR-0108 HTTP server kinds. Two public kinds shared by the server
// capability (#1760) and the handler component (#1762): an inbound
// request delivered to the handler, and an outbound response returned
// by the handler. Both reuse `HttpMethod` / `HttpHeader` from ADR-0043
// so the inbound vocabulary is symmetric with the client.

/// Inbound HTTP request delivered to a handler component by the server
/// capability (ADR-0108). `query` is always present — empty string when
/// the URL carries no query component. `body` is raw bytes so binary
/// uploads round-trip without loss.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.request")]
pub struct HttpServerRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

/// Outbound HTTP response produced by a handler component and forwarded
/// to the waiting client by the server capability (ADR-0108). `status`
/// is the raw HTTP status code; `body` is raw bytes so binary responses
/// round-trip without loss.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.http.server.response")]
pub struct HttpServerResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

/// `aether.http.server.inbound_ready` — accept / reader sidecar →
/// `HttpServerCapability` dispatcher wake (ADR-0108, issue 1760).
/// The HTTP-server analog of `RpcInboundReady`: the sidecar pushes
/// the live work (an accepted `TcpStream`, a parsed request, a close
/// reason) over the cap's internal mpsc and fires this empty-payload
/// mail at the cap's own mailbox so the dispatcher handler drains the
/// queue. A `TcpStream` isn't wire-shaped and a request body may be
/// large, so the mail is only the wakeup signal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.http.server.inbound_ready")]
pub struct HttpInboundReady {}
