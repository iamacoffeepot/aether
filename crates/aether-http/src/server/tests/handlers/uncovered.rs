//! A route holder that answers with a data phase it cannot take: it replies
//! `HttpRouterResult::Stream` or `HttpRouterResult::WebSocket` but covers
//! neither `StreamCreditRouter` nor `WebSocketRouter`, so the server refuses
//! the stream or the upgrade with `502` rather than seating it.

use aether_actor::actor;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::kinds::{HttpResponseStreamOpen, HttpRouterResult, HttpServerRequest, WebSocketAccept};
use crate::server::HttpServerCapability;

use super::bind_catch_all;

/// Binds the `/` catch-all and handles only `HttpServerRequest`: it accepts a
/// request carrying an `upgrade` header as a websocket and opens a response
/// stream for any other, with no handler for the credit, message, or close
/// that would follow.
pub struct UncoveredStreamRouter;

#[actor(singleton, root, depends(HttpServerCapability))]
impl NativeActor for UncoveredStreamRouter {
    type State = ();
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_uncovered_stream_router";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<(), BootError> {
        Ok(())
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        bind_catch_all(ctx);
    }

    #[handler::request]
    fn on_request(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, request: HttpServerRequest) -> HttpRouterResult {
        if request.headers.iter().any(|header| header.name.eq_ignore_ascii_case("upgrade")) {
            HttpRouterResult::WebSocket(WebSocketAccept { subprotocol: None, headers: Vec::new() })
        } else {
            HttpRouterResult::Stream(HttpResponseStreamOpen { status: 200, headers: Vec::new() })
        }
    }
}
