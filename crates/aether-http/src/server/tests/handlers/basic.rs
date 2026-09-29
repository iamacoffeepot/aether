//! The buffered `/` catch-all fixtures: one handler that replies `200`
//! echoing the request, one that replies a fixed non-empty body, and one that
//! holds the request's reply and closes before answering (the close-time
//! `502` path).

use aether_actor::actor;
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::chassis::error::BootError;

use crate::kinds::{HttpHeader, HttpRouterReply, HttpServerRequest, HttpServerResponse};
use crate::server::HttpServerCapability;

use super::bind_catch_all;

/// Replies `200` and echoes the request's method / path / query /
/// peer address (as headers) and body (verbatim), so a test can
/// assert the full request round-tripped to the handler.
pub struct EchoHttpHandler;

/// Empty runtime state for the stateless echo handler (ADR-0122: a
/// stateless cap still names a state type rather than `()` / `Self`).
pub struct EchoHttpHandlerState;

#[actor(singleton, root, depends(HttpServerCapability))]
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
    fn on_request(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, request: HttpServerRequest) -> HttpRouterReply {
        let headers = vec![
            HttpHeader { name: "x-aether-method".to_string(), value: format!("{:?}", request.method) },
            HttpHeader { name: "x-aether-path".to_string(), value: request.path.clone() },
            HttpHeader { name: "x-aether-query".to_string(), value: request.query.clone() },
            HttpHeader { name: "x-aether-peer-addr".to_string(), value: request.peer_addr.clone() },
            HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() },
        ];
        HttpRouterReply::Response(HttpServerResponse { status: 200, headers, body: request.body })
    }
}

/// Always replies `200` with a fixed non-empty body, regardless of
/// method — unlike [`EchoHttpHandler`] (which echoes the request
/// body, empty for HEAD by definition and so unable to prove body
/// suppression), this handler always has a body to suppress.
pub struct FixedBodyHttpHandler;

/// Empty runtime state for the stateless fixed-body handler (ADR-0122).
pub struct FixedBodyHttpHandlerState;

#[actor(singleton, root, depends(HttpServerCapability))]
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
    fn on_request(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _request: HttpServerRequest) -> HttpRouterReply {
        HttpRouterReply::Response(HttpServerResponse {
            status: 200,
            headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
            body: b"fixed body".to_vec(),
        })
    }
}

/// Holds the request's reply, parks the ticket in its state, and closes
/// itself before answering: the actor close that answers every live held
/// reply with its `unanswered` value (ADR-0243 §1), which for
/// `HttpRouterReply` is a `502`.
pub struct ClosingHttpHandler;

/// The held reply the closing handler never answers.
pub struct ClosingHttpHandlerState {
    parked: Option<Held<HttpRouterReply>>,
}

#[actor(singleton, root, depends(HttpServerCapability))]
impl NativeActor for ClosingHttpHandler {
    type State = ClosingHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_closing_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ClosingHttpHandlerState, BootError> {
        Ok(ClosingHttpHandlerState { parked: None })
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::single]
    fn on_request(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _request: HttpServerRequest,
    ) -> Pending<HttpRouterReply> {
        let (pending, held) = ctx.hold::<HttpRouterReply>();
        state.parked = Some(held);
        ctx.shutdown();
        pending
    }
}
