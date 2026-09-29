//! Deferred HTTP routes (ADR-0154 §2), built on typed held replies
//! (ADR-0243) and the ADR-0139 request-context table — no bespoke obligation
//! table, nothing on the actor SDK.
//!
//! A deferred route forwards its request to a peer and answers only when that
//! reply lands. [`DeferredRequest::to`] holds the router's
//! [`HttpRouterReply`], which keeps the request's chain open, and parks the
//! [`Held`] ticket in the request context `send_with_context` stores for the
//! forwarded request. The paired `#[http::reply]` route takes that context
//! back by the reply's `in_reply_to` correlation, no correlation in any
//! signature, and answers the held reply through [`answer_deferred`]. A
//! router that closes first answers `502` through
//! [`HttpRouterReply`]'s `unanswered` reply; a peer that never replies leaves
//! the request to the server's request timeout, `504`.
//!
//! Deferred requests are addressed by their request kind, so the
//! `send_with_context` `HandlesKind<K>` gate compile-checks the request against
//! the recipient: [`Ctx::defer`] captures the request and
//! [`DeferredRequest::to`] forwards it. The recipient is a declared dependency
//! of the router actor (`A: DependsOn<R>`), mailed through the proof that
//! declaration mints (ADR-0232 §1), so a loaded embedded component is not a
//! deferral target (ADR-0154 §2, amended). Native, because the held reply is
//! native; behind the `runtime` feature, and so is [`Outcome`], the only
//! return a deferred route has.

use aether_actor::{CallerAddressable, DependencyResolver, DependsOn, HandlesKind, Singleton};
use aether_data::ActorMail;
use aether_substrate::actor::native::{Held, NativeCtx, Pending};

use super::kinds::{HttpRouterReply, HttpServerResponse};
use super::typed::Ctx;

/// What a native route that may defer answers with (ADR-0154 §2). A
/// synchronous route returns [`HttpServerResponse`] directly; a route that
/// returns `Outcome` chooses between replying inline and forwarding the
/// request to a peer capability and answering only when that reply lands.
///
/// - [`Reply`](Outcome::Reply) answers the request now with the carried
///   response.
/// - [`Deferred`](Outcome::Deferred) is what `ctx.defer(&request).to::<R>()`
///   returns: the request was forwarded to a peer with the router's reply
///   held, so a later `#[http::reply]` route answers when the peer's reply
///   lands.
///
/// Native-only: a wasm-guest route returns `HttpServerResponse`.
pub enum Outcome {
    /// Answer the request now with this response.
    Reply(HttpServerResponse),
    /// The request was forwarded to a recipient by `ctx.defer(&request).to::<R>()`;
    /// a later `#[http::reply]` route answers when the peer replies.
    Deferred(Deferred),
}

/// The receipt of a deferred route's held reply (ADR-0243 §3). Only
/// [`DeferredRequest::to`] makes one, by arming the hold, so an
/// [`Outcome::Deferred`] always names a reply some `#[http::reply]` route
/// owes.
#[must_use = "return the Outcome from the route so the router glue returns its receipt"]
pub struct Deferred(Pending<HttpRouterReply>);

impl Deferred {
    /// The held reply's receipt, which the `#[http::router]` glue returns as
    /// its handler's `Pending<HttpRouterReply>`. Public for the
    /// macro-generated glue only.
    #[doc(hidden)]
    #[must_use]
    pub fn into_receipt(self) -> Pending<HttpRouterReply> {
        self.0
    }
}

/// The router's held reply, carried from a deferred route's request handler
/// to its reply route through the ADR-0139 request-context table
/// (ADR-0243 §4). [`answer_deferred`] takes it back and answers the original
/// request.
#[aether_data::kind(name = "aether.http.deferred_route")]
struct DeferredRoute {
    held: Held<HttpRouterReply>,
}

impl<'transport, A> Ctx<'_, NativeCtx<'transport, A>> {
    /// Capture `request` for deferred forwarding; [`DeferredRequest::to`]
    /// names the singleton recipient and holds the route's reply until its
    /// reply lands (ADR-0154 §2). Reads `ctx.defer(&request).to::<R>()`.
    ///
    /// The captured request borrows this ctx mutably until `to` forwards it,
    /// so a route that defers binds its ctx `mut`. It can still read
    /// `ctx.request()` while building the request it defers.
    #[must_use = "a deferred request does nothing until `.to::<R>()` forwards it"]
    pub fn defer<'ctx, 'request, K: ActorMail>(
        &'ctx mut self,
        request: &'request K,
    ) -> DeferredRequest<'ctx, 'request, NativeCtx<'transport, A>, K> {
        DeferredRequest { ctx: &mut **self, request }
    }
}

/// A deferred route's captured request, produced by [`Ctx::defer`]: the
/// router's transport ctx and the request. [`to`](Self::to) forwards the
/// request to its recipient while holding the route's reply.
pub struct DeferredRequest<'ctx, 'request, C, K> {
    ctx: &'ctx mut C,
    request: &'request K,
}

impl<A, K: ActorMail> DeferredRequest<'_, '_, NativeCtx<'_, A>, K> {
    /// Forward this request to recipient `R`, holding the router's reply until
    /// the paired `#[http::reply]` route answers it. `R` is a declared
    /// dependency of the router actor (`A: DependsOn<R>`), and
    /// `R: HandlesKind<K>` compile-checks this request kind against it.
    ///
    /// The hold keeps the request's chain open, so the HTTP server does not
    /// `502` it before the reply, and the held ticket rides the forwarded
    /// request's context for the reply route to take back.
    #[must_use]
    pub fn to<R>(self) -> Outcome
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        R::Resolver: DependencyResolver,
        A: DependsOn<R>,
    {
        let (pending, held) = self.ctx.hold::<HttpRouterReply>();
        let _ = self.ctx.send_with_context::<R>(self.request, DeferredRoute { held });
        Outcome::Deferred(Deferred(pending))
    }
}

/// Answer a synchronous arm of a router whose handler holds its reply
/// (`-> Pending<HttpRouterReply>`, because some route defers): hold the reply
/// and answer it with `response` at once (ADR-0243 §2). Public for the
/// macro-generated `#[http::router]` glue only.
#[doc(hidden)]
pub fn answer_now<A>(ctx: &mut NativeCtx<'_, A>, response: HttpServerResponse) -> Pending<HttpRouterReply> {
    let (pending, held) = ctx.hold::<HttpRouterReply>();
    held.answer(ctx, &HttpRouterReply::Response(response));
    pending
}

/// Answer a deferred route's held request from the downstream reply the reply
/// route just mapped. Takes the held reply back via `take_context` (keyed by
/// the reply's `in_reply_to`, no correlation exposed) and answers it. A miss
/// (no stored context — an unmatched reply) is a no-op. Public for the
/// macro-generated `#[http::reply]` glue only.
#[doc(hidden)]
pub fn answer_deferred<A>(ctx: &mut NativeCtx<'_, A>, response: HttpServerResponse) {
    if let Some(DeferredRoute { held }) = ctx.take_context::<DeferredRoute>() {
        held.answer(ctx, &HttpRouterReply::Response(response));
    }
}
