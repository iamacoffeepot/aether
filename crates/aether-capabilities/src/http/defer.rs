//! Native deferred-reply machinery for the typed route surface (ADR-0154
//! §2/§3). A deferred route forwards its request to a peer capability and
//! answers only when that reply lands. This module holds the request's
//! reply obligation across the async boundary, arms the `504` settlement
//! net, and hands the obligation back to the paired `#[http::reply]` route.
//!
//! Native-only: the reply-obligation hold is [`InboundMail`], a native
//! construct (a wasm guest's reply handle is one-shot and instance-local,
//! ADR-0133), so the whole module sits behind the `runtime` feature. The
//! table lives in the HTTP surface, not the actor SDK (ADR-0154 §3): only
//! the correlation half (the echoed `correlation_id`) is generic, and the
//! `504` reclamation is HTTP-specific.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use aether_actor::Manual;
use aether_data::{Kind, MailboxId};
use aether_kinds::trace::Settled;
use aether_substrate::InboundMail;
use aether_substrate::actor::native::NativeCtx;

use super::typed::{Ctx, Outcome};

/// The process-global reply-obligation table (ADR-0154 §3): a deferred
/// request's held [`InboundMail`] keyed by `(routing actor, downstream
/// correlation)`. The actor half scopes entries per instance; the
/// correlation half is the detached dispatch's `MailId.correlation_id`,
/// which the downstream reply echoes (so the reply route recovers it via
/// `in_reply_to`) and the settlement notice carries as its root. Bounded
/// by in-flight deferred requests — an entry frees when its reply route
/// answers or the `504` settlement net fires.
fn deferrals() -> &'static Mutex<HashMap<(MailboxId, u64), InboundMail>> {
    static DEFERRALS: OnceLock<Mutex<HashMap<(MailboxId, u64), InboundMail>>> = OnceLock::new();
    DEFERRALS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remove and return the held reply obligation for `(self_id, correlation)`.
/// The generated `#[http::reply]` glue and the `504` settlement handler
/// both call this to answer (or fail-close) a held request. Public for the
/// macro-generated glue only — not an author-facing surface.
///
/// # Panics
/// Panics if the deferral-table mutex is poisoned — fail-fast per ADR-0063.
#[doc(hidden)]
#[must_use]
pub fn take_deferred(self_id: MailboxId, correlation: u64) -> Option<InboundMail> {
    deferrals().lock().expect("http deferral table poisoned; fail-fast per ADR-0063").remove(&(self_id, correlation))
}

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
    /// # Panics
    /// Panics if the deferral-table mutex is poisoned — fail-fast per
    /// ADR-0063 (a poisoned table means another handler panicked mid-op).
    pub fn defer<K: Kind>(mut self, recipient: MailboxId, request: &K) -> Outcome {
        let self_id = self.self_id();
        let inbound = self.take_inbound();
        let bytes = request.encode_into_bytes();
        let mail_id = self.send_envelope_detached(recipient, K::ID, &bytes);
        let mailer = self.mailer();
        if let Some(registry) = mailer.settlement_registry() {
            registry.subscribe_settlement_mail(mail_id, self_id, <Settled as Kind>::ID, Arc::clone(mailer));
        }
        deferrals()
            .lock()
            .expect("http deferral table poisoned; fail-fast per ADR-0063")
            .insert((self_id, mail_id.correlation_id), inbound);
        Outcome::Deferred
    }
}
