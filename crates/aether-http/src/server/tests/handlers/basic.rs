//! The buffered `/` catch-all fixtures: one handler that replies `200`
//! echoing the request, one that replies a fixed non-empty body, and one that
//! drops the request without replying (the `502` safety-net path).

use aether_actor::actor;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::kinds::{HttpHeader, HttpServerRequest, HttpServerResponse};

use super::bind_catch_all;

/// Replies `200` and echoes the request's method / path / query /
/// peer address (as headers) and body (verbatim), so a test can
/// assert the full request round-tripped to the handler.
pub struct EchoHttpHandler;

/// Empty runtime state for the stateless echo handler (ADR-0122: a
/// stateless cap still names a state type rather than `()` / `Self`).
pub struct EchoHttpHandlerState;

#[actor(singleton, root)]
impl NativeActor for EchoHttpHandler {
    type State = EchoHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_echo_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<EchoHttpHandlerState, BootError> {
        Ok(EchoHttpHandlerState)
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::single]
    fn on_request(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        request: HttpServerRequest,
    ) -> HttpServerResponse {
        let headers = vec![
            HttpHeader { name: "x-aether-method".to_string(), value: format!("{:?}", request.method) },
            HttpHeader { name: "x-aether-path".to_string(), value: request.path.clone() },
            HttpHeader { name: "x-aether-query".to_string(), value: request.query.clone() },
            HttpHeader { name: "x-aether-peer-addr".to_string(), value: request.peer_addr.clone() },
            HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() },
        ];
        HttpServerResponse { status: 200, headers, body: request.body }
    }
}

/// Always replies `200` with a fixed non-empty body, regardless of
/// method — unlike [`EchoHttpHandler`] (which echoes the request
/// body, empty for HEAD by definition and so unable to prove body
/// suppression), this handler always has a body to suppress.
pub struct FixedBodyHttpHandler;

/// Empty runtime state for the stateless fixed-body handler (ADR-0122).
pub struct FixedBodyHttpHandlerState;

#[actor(singleton, root)]
impl NativeActor for FixedBodyHttpHandler {
    type State = FixedBodyHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_fixed_body_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<FixedBodyHttpHandlerState, BootError> {
        Ok(FixedBodyHttpHandlerState)
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::single]
    fn on_request(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _request: HttpServerRequest,
    ) -> HttpServerResponse {
        HttpServerResponse {
            status: 200,
            headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
            body: b"fixed body".to_vec(),
        }
    }
}

/// Receives the request and returns without replying — the response-less
/// chain the `502` settlement safety net covers.
pub struct SilentHttpHandler;

/// Empty runtime state for the stateless silent handler (ADR-0122).
pub struct SilentHttpHandlerState;

#[actor(singleton, root)]
impl NativeActor for SilentHttpHandler {
    type State = SilentHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_silent_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<SilentHttpHandlerState, BootError> {
        Ok(SilentHttpHandlerState)
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::single]
    fn on_request(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _request: HttpServerRequest) {
        // Intentionally drops the request without replying.
    }
}
