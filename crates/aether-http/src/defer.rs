//! Deferred HTTP routes, built on the ADR-0139 relay machinery — no bespoke
//! obligation table, nothing on the actor SDK.
//!
//! A deferred route forwards its request to a peer and answers only when that
//! reply lands. This is the exact pattern audio / text / aether-kit-commons already
//! use for fs replies: capture the requester's `Source`, `send_with_context`
//! to the peer, and answer later via `take_context` + `reply_to`. The send
//! *inherits* the request's causal chain (ADR-0080 §7), so the request stays
//! in flight across the round-trip and the HTTP server never `502`s it early
//! — the framework holds the chain open for free, no `take_inbound` guard.
//!
//! Deferred requests are addressed by their request kind, so the
//! `send_with_context` `HandlesKind<K>` gate compile-checks the request against
//! the recipient: [`Ctx::defer`] captures the request and
//! [`DeferredRequest::to`] forwards it. The recipient is a declared dependency
//! of the router actor (`A: DependsOn<R>`), mailed through the proof that
//! declaration mints (ADR-0232 §1), so a loaded embedded component is not a
//! deferral target (ADR-0154 §2, amended). Native, because the reply
//! obligation and `reply_to` are native; behind the `runtime` feature.
//!
//! The reply route recovers the requester the same way `take_context` does —
//! by the reply's `in_reply_to` correlation, no correlation in any signature —
//! and answers. A peer that settles without replying yields the server's own
//! `502` net; one that never settles, its request timeout. Neither needs
//! anything here.

use aether_actor::{CallerAddressable, DependencyResolver, DependsOn, HandlesKind, Manual, OutboundReply, Singleton};
use aether_data::{ActorMail, Source};
use aether_substrate::actor::native::NativeCtx;

use super::kinds::HttpServerResponse;
use super::typed::{Ctx, Outcome};

/// The requester's reply target, carried from a deferred route's request
/// handler to its reply route through the ADR-0139 request-context table (a
/// serializable `Source`, unlike the native reply guard). [`answer_deferred`]
/// recovers it and answers the original request.
#[aether_data::kind(name = "aether.http.deferred_source")]
#[doc(hidden)]
pub struct DeferredSource {
    /// The original HTTP requester (the server), correlation included.
    pub source: Source,
}

impl<'transport, A> Ctx<'_, NativeCtx<'transport, A, Manual>> {
    /// Capture `request` and the requester's reply target for deferred
    /// forwarding; [`DeferredRequest::to`] names the singleton recipient and
    /// holds the route open until its reply lands (ADR-0154 §2). Reads
    /// `ctx.defer(&request).to::<R>()`.
    ///
    /// The captured request borrows this ctx mutably until `to` forwards it,
    /// so a route that defers binds its ctx `mut`. It can still read
    /// `ctx.request()` while building the request it defers.
    #[must_use = "a deferred request does nothing until `.to::<R>()` forwards it"]
    pub fn defer<'ctx, 'request, K: ActorMail>(
        &'ctx mut self,
        request: &'request K,
    ) -> DeferredRequest<'ctx, 'request, NativeCtx<'transport, A, Manual>, K> {
        let source = self.reply_target();
        DeferredRequest { ctx: &mut **self, request, source }
    }
}

/// A deferred route's captured request, produced by [`Ctx::defer`]: the
/// router's transport ctx, the request, and the requester's reply target.
/// [`to`](Self::to) forwards the request to its recipient while holding the
/// route open.
pub struct DeferredRequest<'ctx, 'request, C, K> {
    ctx: &'ctx mut C,
    request: &'request K,
    source: Source,
}

impl<A, K: ActorMail> DeferredRequest<'_, '_, NativeCtx<'_, A, Manual>, K> {
    /// Forward this request to recipient `R` and hold the route open until its reply.
    /// `R` is a declared dependency of the router actor (`A: DependsOn<R>`),
    /// and `R: HandlesKind<K>` compile-checks this request kind against it.
    /// The send is *inherited* (ADR-0080 §7), so the request's chain stays open
    /// and the HTTP server does not `502` it before the reply;
    /// `send_with_context` stashes the requester's reply target for the paired
    /// `#[http::reply]` route to answer through.
    #[must_use]
    pub fn to<R>(self) -> Outcome
    where
        R: Singleton + CallerAddressable + HandlesKind<K>,
        R::Resolver: DependencyResolver,
        A: DependsOn<R>,
    {
        let _ = self.ctx.send_with_context::<R>(self.request, &DeferredSource { source: self.source });
        Outcome::Deferred
    }
}

/// Answer a deferred route's held request from the downstream reply the reply
/// route just mapped. Recovers the requester's `Source` via `take_context`
/// (keyed by the reply's `in_reply_to`, no correlation exposed) and replies to
/// it. A miss (no stored context — an unmatched reply) is a no-op. Public for
/// the macro-generated `#[http::reply]` glue only.
#[doc(hidden)]
pub fn answer_deferred<A>(ctx: &mut NativeCtx<'_, A, Manual>, response: &HttpServerResponse) {
    if let Some(deferred) = ctx.take_context::<DeferredSource>() {
        ctx.reply_to(deferred.source, response);
    }
}

/// Answer a deferred route's request inline — the synchronous arm of
/// [`Outcome`] (`Outcome::Reply`), for a route that decides its answer without
/// forwarding (e.g. a validation `400`). Replies to the current inbound.
/// Public for the macro-generated `#[http::route]` glue only.
#[doc(hidden)]
pub fn answer_now<A>(ctx: &mut NativeCtx<'_, A, Manual>, response: &HttpServerResponse) {
    ctx.reply(response);
}
