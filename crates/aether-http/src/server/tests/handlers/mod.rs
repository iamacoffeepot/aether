//! Minimal native handler actors behind the server in the integration
//! tests: one that replies `200` echoing the request, one that drops
//! the request without replying (the `502` safety-net path), two
//! response-streaming handlers (ADR-0128) — a well-behaved one that
//! paces chunks against credit, and a flooder that ignores credit —
//! plus the routed handlers. Most route handlers author their routes
//! through the typed `#[http::router]` / `#[http::route]` surface
//! (ADR-0131) — the macro mints each route's kind and injects its
//! `register_route_self` registration — so the tests exercise what a
//! component author writes. The conflict-`Err` and idempotent
//! double-claim handlers stay on the raw registration surface, so a
//! macro regression cannot mask a registration-semantics one.

use aether_data::Kind;
use aether_substrate::actor::native::NativeCtx;

use crate::kinds::{HttpServerRequest, RegisterRouteSelf};
use crate::server::HttpServerCapability;

mod basic;
mod routed;
mod shared;
mod streaming;

pub(super) use basic::{EchoHttpHandler, FixedBodyHttpHandler, SilentHttpHandler};
pub(super) use routed::{
    ApiRouteHandler, ApiV2Handler, BookRouteHandler, DeferRouteHandler, EchoPeer, ExtractRouteHandler,
    MethodAnyHandler, MethodPostHandler, SilentPeer, TmpRouteHandler, WiredRouteHandler,
};
pub(super) use shared::{ExclusiveMacroPoolHandler, SharedAlphaHandler, SharedBetaHandler, SharedMacroPoolHandler};
pub(super) use streaming::{
    FloodHttpHandler, STREAM_CHUNK_COUNT, StreamHttpHandler, StreamIdEchoHandler, StreamingUploadHandler,
    stream_chunk_body,
};

/// Bind the calling handler as the `/` catch-all (ADR-0130) — the
/// shared replacement for the retired `handler_mailbox` default, so a
/// route-unmatched request reaches that handler.
fn bind_catch_all(ctx: &mut NativeCtx<'_>) {
    ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
        prefix: "/".to_string(),
        method: None,
        kind: <HttpServerRequest as Kind>::ID,
        shared: false,
    });
}
