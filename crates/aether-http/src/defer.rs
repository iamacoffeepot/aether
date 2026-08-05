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
//! Peers are addressed by their marker type, so the `send_with_context`
//! `HandlesKind<K>` gate compile-checks the request against the peer:
//! [`Ctx::peer`] names the singleton and [`Peer::defer`] forwards to it. One
//! entry point serves both a native root singleton and an embedded/wasm
//! component, because `peer` resolves against the component host's carry — the
//! carry an embedded id folds in and the caller's own cannot supply. Native,
//! because the reply obligation and `reply_to` are native; behind the
//! `runtime` feature.
//!
//! The reply route recovers the requester the same way `take_context` does —
//! by the reply's `in_reply_to` correlation, no correlation in any signature —
//! and answers. A peer that settles without replying yields the server's own
//! `502` net; one that never settles, its request timeout. Neither needs
//! anything here.

use aether_actor::{Addressable, HandlesKind, Manual, OutboundReply, Singleton};
use aether_data::{Kind, Source};
use aether_substrate::actor::native::{NativeActorMailbox, NativeCtx};

use super::kinds::HttpServerResponse;
use super::typed::{Ctx, Outcome};
use aether_component::ComponentHostCapability;

/// The requester's reply target, carried from a deferred route's request
/// handler to its reply route through the ADR-0139 request-context table (a
/// serializable `Source`, unlike the native reply guard). [`answer_deferred`]
/// recovers it and answers the original request.
#[derive(Kind, aether_data::Schema, serde::Serialize, serde::Deserialize)]
#[kind(name = "aether.http.deferred_source")]
#[doc(hidden)]
pub struct DeferredSource {
    /// The original HTTP requester (the server), correlation included.
    pub source: Source,
}

impl Ctx<'_, NativeCtx<'_, Manual>> {
    /// Name the singleton peer `R` a deferred route will forward to, capturing
    /// the requester's reply target from this ctx; [`Peer::defer`] then
    /// forwards the request and holds the route open until `R`'s reply lands
    /// (ADR-0154 §2). Reads `ctx.peer::<R>().defer(&request)`.
    ///
    /// `peer` resolves `R` against the **component host's** carry rather than
    /// the caller's, so one call site addresses both a native root cap (whose
    /// [`One`](aether_actor::One) resolver ignores the carry) and an embedded
    /// component (whose [`Embedded`](aether_actor::Embedded) resolver folds it
    /// to the id [`resolve_embedded`](aether_component::resolve_embedded)
    /// gives, under the component's default load name). That is why it is not
    /// `ctx.actor::<R>()`, which supplies the caller's carry and therefore
    /// refuses an embedded target outright (ADR-0119 amendment).
    #[must_use = "a peer does nothing until `.defer(&request)` forwards to it"]
    pub fn peer<R: Singleton>(&self) -> Peer<'_, R> {
        let host_carry = <ComponentHostCapability as Addressable>::resolve(0, ()).0;
        Peer { mailbox: self.actor_at::<R>(R::resolve(host_carry, ())), source: self.reply_target() }
    }
}

/// A deferred route's named peer, produced by [`Ctx::peer`]: the resolved
/// singleton mailbox plus the requester's reply target. [`defer`](Self::defer)
/// forwards the request and holds the route open.
pub struct Peer<'a, R: Addressable> {
    mailbox: NativeActorMailbox<'a, R>,
    source: Source,
}

impl<R: Addressable> Peer<'_, R> {
    /// Forward `request` to this peer and hold the route open until its reply.
    /// The send is *inherited* (ADR-0080 §7), so the request's chain stays open
    /// and the HTTP server does not `502` it before the reply;
    /// `send_with_context` stashes the requester's reply target for the paired
    /// `#[http::reply]` route to answer through. `R: HandlesKind<K>`
    /// compile-checks the request kind against the peer; `K` is inferred.
    pub fn defer<K>(self, request: &K) -> Outcome
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.mailbox.send_with_context(request, &DeferredSource { source: self.source });
        Outcome::Deferred
    }
}

/// Answer a deferred route's held request from the downstream reply the reply
/// route just mapped. Recovers the requester's `Source` via `take_context`
/// (keyed by the reply's `in_reply_to`, no correlation exposed) and replies to
/// it. A miss (no stored context — an unmatched reply) is a no-op. Public for
/// the macro-generated `#[http::reply]` glue only.
#[doc(hidden)]
pub fn answer_deferred(ctx: &mut NativeCtx<'_, Manual>, response: &HttpServerResponse) {
    if let Some(deferred) = ctx.take_context::<DeferredSource>() {
        ctx.reply_to(deferred.source, response);
    }
}

/// Answer a deferred route's request inline — the synchronous arm of
/// [`Outcome`] (`Outcome::Reply`), for a route that decides its answer without
/// forwarding (e.g. a validation `400`). Replies to the current inbound.
/// Public for the macro-generated `#[http::route]` glue only.
#[doc(hidden)]
pub fn answer_now(ctx: &mut NativeCtx<'_, Manual>, response: &HttpServerResponse) {
    ctx.reply(response);
}
