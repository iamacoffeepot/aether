//! Native deferred-reply surface for the typed route macro (ADR-0154 §2).
//! A deferred route forwards its request to a peer capability and answers
//! only when that reply lands. [`Ctx::defer`] holds the request's reply
//! obligation across the async boundary, arms the `504` settlement net, and
//! hands the obligation to the paired `#[http::reply]` route.
//!
//! Native-only: the reply obligation is an
//! [`InboundMail`](aether_substrate::InboundMail), a native construct (a
//! wasm guest's reply handle is one-shot and instance-local, ADR-0133), so
//! the whole module sits behind the `runtime` feature. The obligations are
//! parked in a per-actor table on the actor's binding (ADR-0154 §3, hardened
//! per iamacoffeepot/aether#3683): it drops with the actor (reclaiming
//! stranded sockets), its lock is scoped to one actor's traffic, and it is
//! bounded — `defer` answers `503` at the ceiling rather than growing
//! without bound at a slow or dead peer. The `504` reclamation stays here
//! because that status is HTTP-specific; the hold/take storage is generic
//! and lives on the SDK binding.

use std::sync::Arc;

use aether_actor::Manual;
use aether_data::Kind;
use aether_data::MailboxId;
use aether_kinds::trace::Settled;
use aether_substrate::actor::native::NativeCtx;

use super::kinds::HttpServerResponse;
use super::typed::{Ctx, Outcome};

impl Ctx<'_, NativeCtx<'_, Manual>> {
    /// Forward this route's request to `recipient` and answer only when
    /// that reply lands (ADR-0154 §2). Takes the request's reply
    /// obligation across the async boundary (so the request's chain does
    /// not settle and trip the HTTP server's `502` net), dispatches
    /// `request` as a fresh detached root, arms a settlement subscription
    /// that answers `504` if the downstream chain settles without a reply,
    /// and holds the obligation keyed by the dispatch's correlation. A
    /// paired `#[http::reply]` route recovers it via `in_reply_to` and
    /// answers.
    ///
    /// If the actor's per-actor obligation table is already at its ceiling,
    /// the request is refused with `503` and left un-forwarded rather than
    /// growing the table — the request's own reply obligation answers, so no
    /// obligation is taken and no downstream dispatch is made.
    pub fn defer<K: Kind>(mut self, recipient: MailboxId, request: &K) -> Outcome {
        if !self.deferred_reply_capacity_available() {
            return Outcome::Reply(HttpServerResponse {
                status: 503,
                headers: Vec::new(),
                body: Vec::from(&b"deferred-route obligation table full"[..]),
            });
        }
        let self_id = self.self_id();
        let inbound = self.take_inbound();
        let bytes = request.encode_into_bytes();
        let mail_id = self.send_envelope_detached(recipient, K::ID, &bytes);
        let mailer = self.mailer();
        if let Some(registry) = mailer.settlement_registry() {
            registry.subscribe_settlement_mail(mail_id, self_id, <Settled as Kind>::ID, Arc::clone(mailer));
        }
        self.hold_deferred_reply(mail_id.correlation_id, inbound);
        Outcome::Deferred
    }
}
