//! The buffered `/` catch-all fixtures: one handler that replies `200`
//! echoing the request, one that replies a fixed non-empty body, one that
//! holds the request's reply and closes before answering (the close-time
//! `502` path), and one that holds its reply and forwards to a peer that never
//! answers (the request-timeout `504` path).

use aether_actor::{Unchecked, actor};
use aether_substrate::actor::native::{Erased, Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::chassis::error::BootError;

use crate::kinds::{HttpHeader, HttpRouterResult, HttpServerRequest, HttpServerResponse};
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

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        bind_catch_all(ctx);
        Ok(())
    }

    #[handler::request]
    fn on_request(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, request: HttpServerRequest) -> HttpRouterResult {
        let headers = vec![
            HttpHeader { name: "x-aether-method".to_string(), value: format!("{:?}", request.method) },
            HttpHeader { name: "x-aether-path".to_string(), value: request.path.clone() },
            HttpHeader { name: "x-aether-query".to_string(), value: request.query.clone() },
            HttpHeader { name: "x-aether-peer-addr".to_string(), value: request.peer_addr.clone() },
            HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() },
        ];
        HttpRouterResult::Response(HttpServerResponse { status: 200, headers, body: request.body })
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

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        bind_catch_all(ctx);
        Ok(())
    }

    #[handler::request]
    fn on_request(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _request: HttpServerRequest) -> HttpRouterResult {
        HttpRouterResult::Response(HttpServerResponse {
            status: 200,
            headers: vec![HttpHeader { name: "content-type".to_string(), value: "text/plain".to_string() }],
            body: b"fixed body".to_vec(),
        })
    }
}

/// Holds the request's reply, parks the ticket in its state, and closes
/// itself before answering: the actor close that answers every live held
/// reply with its `unanswered` value (ADR-0243 §1), which for
/// `HttpRouterResult` is a `502`.
pub struct ClosingHttpHandler;

/// The held reply the closing handler never answers.
pub struct ClosingHttpHandlerState {
    parked: Option<Held<HttpRouterResult>>,
}

#[actor(singleton, root, depends(HttpServerCapability))]
impl NativeActor for ClosingHttpHandler {
    type State = ClosingHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_closing_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ClosingHttpHandlerState, BootError> {
        Ok(ClosingHttpHandlerState { parked: None })
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        bind_catch_all(ctx);
        Ok(())
    }

    #[handler::request]
    fn on_request(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _request: HttpServerRequest,
    ) -> Pending<HttpRouterResult> {
        let (pending, held) = ctx.hold::<HttpRouterResult>();
        state.parked = Some(held);
        ctx.shutdown();
        pending
    }
}

/// What [`HeldForwardHttpHandler`] forwards to [`SilentPeer`].
#[aether_data::kind(name = "aether.http.test_ask")]
pub struct Ask;

/// The forwarding handler's held reply, parked in the forwarded [`Ask`]'s
/// request context until the peer's reply takes it back (ADR-0243 §4).
#[aether_data::kind(name = "aether.http.test_forward_context")]
struct ForwardContext {
    held: Held<HttpRouterResult>,
}

/// A peer that receives [`Ask`] and never replies.
pub struct SilentPeer;

/// Empty runtime state for the silent peer (ADR-0122).
pub struct SilentPeerState;

#[actor(singleton, root)]
impl NativeActor for SilentPeer {
    type State = SilentPeerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_silent_peer";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<SilentPeerState, BootError> {
        Ok(SilentPeerState)
    }

    // Unchecked and never answered: the forwarding handler's held reply keeps the
    // request's chain open, so the server answers `504` at its timeout.
    #[handler::unchecked(reason = "test: holds the reply unanswered so the server times out")]
    fn on_ask(_state: &mut SilentPeerState, _ctx: &mut NativeCtx<'_, Erased, Unchecked>, _ask: Ask) {}
}

/// Holds the request's reply and forwards an [`Ask`] to [`SilentPeer`] with
/// the held reply parked in the forwarded request's context: the hand-written
/// held-reply handler a router that waits on a peer is (ADR-0243 §4). The peer
/// never replies, so the request waits out the server's request timeout.
pub struct HeldForwardHttpHandler;

/// Empty runtime state: the held reply rides the request context, not the
/// actor.
pub struct HeldForwardHttpHandlerState;

#[actor(singleton, root, depends(HttpServerCapability, SilentPeer))]
impl NativeActor for HeldForwardHttpHandler {
    type State = HeldForwardHttpHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_held_forward_handler";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeldForwardHttpHandlerState, BootError> {
        Ok(HeldForwardHttpHandlerState)
    }

    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        bind_catch_all(ctx);
        Ok(())
    }

    #[handler::request]
    fn on_request(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _request: HttpServerRequest,
    ) -> Pending<HttpRouterResult> {
        let (pending, held) = ctx.hold::<HttpRouterResult>();
        let _ = ctx.send_with_context::<SilentPeer>(&Ask, ForwardContext { held });
        pending
    }
}
