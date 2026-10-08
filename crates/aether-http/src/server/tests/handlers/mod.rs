//! Minimal native handler actors behind the server in the integration
//! tests: one that replies `200` echoing the request, one that closes
//! before answering its held reply (the close-time `502` path), one that
//! holds its reply and forwards to a peer that never answers (the
//! request-timeout `504` path), two
//! response-streaming handlers (ADR-0128) — a well-behaved one that
//! paces chunks against credit, and a flooder that ignores credit —
//! plus the routed handlers and one that answers `Stream` or `WebSocket`
//! without covering the data phase that follows. Most route handlers author
//! their routes through the typed `#[http::router]` / `#[http::route]` surface
//! (ADR-0131) — the macro emits the router's one request handler and
//! injects its `register_route_self` registration — so the tests exercise what a
//! component author writes. The conflict-`Err` and idempotent
//! double-claim handlers stay on the raw registration surface, so a
//! macro regression cannot mask a registration-semantics one, and the
//! self-releasing `/tmp` router is hand-written so it can hold its reply
//! until the server confirms the release (ADR-0243 §4).

use std::sync::Arc;

use aether_actor::{DependsOn, ProtocolRef};
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::testing::{TestChassis, fresh_substrate};

use crate::kinds::{HttpRouter, MethodFilter, RegisterRouteSelf};
use crate::server::{HttpServerCapability, HttpServerConfig};

mod basic;
mod routed;
mod shared;
mod streaming;
mod uncovered;

// `EchoHttpHandler` is also the `HttpRouter`-covering fixture the runtime's
// own unit tests narrow a handler path from, so the basic fixtures reach the
// whole `server` module rather than this test tree alone.
pub(in crate::server) use basic::{ClosingHttpHandler, EchoHttpHandler, FixedBodyHttpHandler};
pub(super) use basic::{HeldForwardHttpHandler, SilentPeer};
pub(super) use routed::{
    ApiRouteHandler, ApiV2Handler, BookRouteHandler, ExtractRouteHandler, MethodAnyHandler, MethodPostHandler,
    NestedRouteHandler, TmpRouteHandler, WiredRouteHandler,
};
pub(super) use shared::{ExclusiveMacroPoolHandler, SharedAlphaHandler, SharedBetaHandler, SharedMacroPoolHandler};
pub(super) use streaming::{
    FloodHttpHandler, STREAM_CHUNK_COUNT, StreamHttpHandler, StreamIdEchoHandler, StreamingUploadHandler,
    stream_chunk_body,
};
pub(super) use uncovered::UncoveredStreamRouter;

/// Two proven route holders, each an `HttpRouter` covered by a live
/// handler, beside the chassis that keeps them live: the route table's unit
/// tests claim and release routes for them. The server is composed disabled,
/// so the handlers' own `wire` registrations are refused and never touch the
/// table under test. The boot mail those registrations and their refusals
/// make has settled before the caller sees the chassis.
pub(in crate::server) fn router_holders()
-> (PassiveChassis<TestChassis>, ProtocolRef<HttpRouter>, ProtocolRef<HttpRouter>) {
    let (registry, mailer) = fresh_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<HttpServerCapability>((), HttpServerConfig::default())
        .with_actor::<EchoHttpHandler>(())
        .with_actor::<FixedBodyHttpHandler>(())
        .build_passive()
        .expect("caps boot");
    chassis.await_boot_settled();

    let a = chassis.actor_ref::<EchoHttpHandler>().narrow::<HttpRouter>();
    let b = chassis.actor_ref::<FixedBodyHttpHandler>().narrow::<HttpRouter>();

    (chassis, a, b)
}

/// Bind the calling handler as the `/` catch-all (ADR-0130) — the
/// shared replacement for the retired `handler_mailbox` default, so a
/// route-unmatched request reaches that handler.
fn bind_catch_all<A: DependsOn<HttpServerCapability>>(ctx: &mut NativeCtx<'_, A>) {
    ctx.send::<HttpServerCapability>(&RegisterRouteSelf {
        prefix: "/".to_string(),
        method: MethodFilter::Any,
        shared: false,
    });
}
