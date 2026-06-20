//! Reference HTTP handler fixture for the `serving-http` e2e test and
//! recipe (issue 1762, ADR-0108). Not a demo, not exemplary — its only
//! job is to prove the `aether.http.server` guest load path end to end:
//! `HttpServerCapability` dispatches an `HttpServerRequest` here; this
//! actor path-matches and replies `HttpServerResponse`; the cap formats
//! the HTTP/1.1 response and writes it to the client socket.
//!
//! Behaviour:
//!
//! - `GET /` → 200 `hello from aether`
//! - Anything else → 404 `not found`
//!
//! Registered at `aether.component/aether.embedded:web` after load.
//! The e2e test configures `HttpServerConfig.handler_mailbox` to that
//! address and then fires real `TcpStream` requests at the bound port.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references. A stateless handler that
// ignores `self` is correct but triggers `unused_self`.
#![allow(clippy::needless_pass_by_value, clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{HttpServerRequest, HttpServerResponse};

pub struct HttpHandler;

#[actor]
impl WasmActor for HttpHandler {
    const NAMESPACE: &'static str = "web";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HttpHandler)
    }

    /// Route an inbound HTTP request to a status + body and reply the
    /// formatted response. The HTTP server cap writes the reply to the
    /// waiting client socket.
    ///
    /// # Agent
    /// Not sent manually — the `aether.http.server` cap dispatches it
    /// on every inbound request. Configure `HttpServerConfig.handler_mailbox`
    /// to `"aether.component/aether.embedded:web"` to route here.
    #[handler]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, req: HttpServerRequest) -> HttpServerResponse {
        let (status, body): (u16, &[u8]) = match req.path.as_str() {
            "/" => (200, b"hello from aether"),
            _ => (404, b"not found"),
        };
        HttpServerResponse {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }
}
