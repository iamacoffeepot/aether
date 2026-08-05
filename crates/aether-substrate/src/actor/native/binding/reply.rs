//! The reply path native actors take (ADR-0080 §5) and the typed
//! request-context table replies are matched against (ADR-0139).

use super::NativeBinding;
use crate::mail::{MailId, Source};
use aether_data::{Kind, RequestId};

impl NativeBinding {
    /// Reply path for native actors (ADR-0080 §5 / #1695). Mints the
    /// reply's lineage `MailId` from this actor's disjoint
    /// `reply_lineage` allocator and routes through
    /// [`Mailer::send_reply`](crate::mail::mailer::Mailer::send_reply), so the reply joins the
    /// caller's causal chain: it inherits the handler's `root` and
    /// `parent`, and its `Sent` is recorded against that root (keeping
    /// the §6 hold contract exact — a synchronous reply's `Sent`
    /// precedes the replying handler's `Finished`). `root == MailId::NONE`
    /// (a reply from a ctx with no inbound chain) stamps the `NONE`
    /// triple and skips the producer hook.
    ///
    /// The per-handler [`super::ctx::NativeCtx`](crate::actor::native::ctx::NativeCtx) supplies `root` /
    /// `parent` from its in-flight context (`in_flight_root` /
    /// `outbound_parent`); the ADR-0093 deferred path supplies the
    /// `SettlementHold`'s root. Issue 665 retired the FFI-shaped
    /// `reply_mail` stub the prior `MailTransport` impl carried; this
    /// typed entry is the only reply API native actors reach for.
    pub fn send_reply_for_handler<K>(&self, sender: Source, payload: &K, root: MailId, parent: Option<MailId>)
    where
        K: Kind,
    {
        let correlation = self.reply_lineage.mint();
        let reply_id = MailId::new(self.self_mailbox(), correlation);
        self.mailer.send_reply(sender, payload, reply_id, root, parent);
    }

    /// Store request context for a just-minted outbound request.
    ///
    /// # Panics
    /// Panics if the request-context mutex is poisoned.
    pub fn store_request_context<C: Kind>(&self, request: RequestId, context: &C) {
        self.request_contexts
            .lock()
            .expect("request context table poisoned; fail-fast per ADR-0063")
            .insert(request, context);
    }

    /// Remove and decode request context for an inbound reply.
    ///
    /// # Panics
    /// Panics if the request-context mutex is poisoned.
    pub fn take_request_context<C: Kind>(&self, request: RequestId) -> Option<C> {
        self.request_contexts.lock().expect("request context table poisoned; fail-fast per ADR-0063").take(request)
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture::forward_to_envelope_sender;
    use super::*;
    use crate::actor::native::envelope::Envelope;
    use crate::chassis::inbox::ReplyLineage;
    use crate::mail::{MailboxId, SourceAddr};
    use crate::testing::{bare_substrate, boot_authority};
    use std::sync::Arc;
    use std::sync::mpsc;

    /// #1695 / ADR-0080 §5/§6: a synchronous `ctx.reply` from a handler
    /// with an in-flight chain stamps the reply mail with the caller's
    /// `root` + the handled mail as `parent`, mints the reply id in the
    /// replier's id space, and records the reply's `Sent` on that root —
    /// so the chain stays live until the reply's `Finished`. The reply
    /// joins the caller's chain instead of opening a lineage-less one.
    #[test]
    fn ctx_reply_joins_caller_chain() {
        use crate::actor::native::ctx::NativeCtx;
        use aether_actor::OutboundReply;
        use aether_kinds::Tick;

        let (registry, mailer) = bare_substrate();
        let counter = Arc::clone(mailer.trace_handle().settlement_counter());

        let (reply_tx, reply_rx) = mpsc::channel::<Envelope>();
        let caller =
            registry.register_inbox(&boot_authority(), "test.reply_chain.caller", forward_to_envelope_sender(reply_tx));

        let actor_mailbox = MailboxId(0x00BE_EF01);
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), actor_mailbox));

        let root = MailId::new(MailboxId(0xC0), 1);
        let request = MailId::new(MailboxId(0xC0), 1);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller), 55);

        {
            let mut ctx = NativeCtx::new_dispatching(&binding, caller_source, request, root);
            OutboundReply::reply(&mut ctx, &Tick::default());
            // ctx drops here; the reply already routed eagerly via the
            // Mailer (replies are not buffered), so the flush is a no-op.
        }

        let reply = reply_rx.try_recv().expect("reply routed to the caller");
        assert_eq!(reply.root, root, "reply inherits the caller's root");
        assert_eq!(reply.parent_mail, Some(request), "reply's parent is the handled request");
        assert_ne!(reply.mail_id, MailId::NONE, "reply carries a real mail id");
        assert_eq!(reply.mail_id.sender, actor_mailbox, "reply id is minted in the replier's id space");

        // The reply's Sent keeps the caller root live (the bare forwarding
        // sink records no Finished); the matching Finished reclaims it.
        assert_eq!(counter.live_roots(), 1, "the reply's Sent holds the caller chain open");
        mailer.record_finished(reply.mail_id, root);
        assert_eq!(counter.live_roots(), 0, "the reply's Finished balances its Sent exactly");
    }

    /// #1695: minting a reply's lineage id draws from the disjoint
    /// reply-lineage counter, so a reply never advances the `send`
    /// correlation `prev_correlation` reports (symmetric with the wasm
    /// trampoline's separate reply counter).
    #[test]
    fn reply_does_not_advance_send_correlation() {
        use crate::actor::native::ctx::NativeCtx;
        use aether_actor::OutboundReply;
        use aether_kinds::Tick;

        let (registry, mailer) = bare_substrate();
        let (reply_tx, _reply_rx) = mpsc::channel::<Envelope>();
        let caller =
            registry.register_inbox(&boot_authority(), "test.reply_corr.caller", forward_to_envelope_sender(reply_tx));

        let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF02)));
        assert_eq!(binding.prev_correlation(), 0);

        let root = MailId::new(MailboxId(0xC0), 1);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller), 7);
        {
            let mut ctx = NativeCtx::new_dispatching(&binding, caller_source, root, root);
            OutboundReply::reply(&mut ctx, &Tick::default());
            OutboundReply::reply(&mut ctx, &Tick::default());
        }
        assert_eq!(binding.prev_correlation(), 0, "replies must not advance the send correlation counter");
    }

    /// Step 3: reply ids minted via `send_reply_for_handler` still sit in
    /// the disjoint reply-lineage space ([`ReplyLineage::BASE`]), and minting
    /// a reply does not advance `prev_correlation` (the send counter).
    #[test]
    fn reply_mints_in_disjoint_space_and_does_not_advance_send_correlation() {
        use crate::actor::native::ctx::NativeCtx;
        use aether_actor::OutboundReply;
        use aether_kinds::Tick;

        let (registry, mailer) = bare_substrate();
        let (reply_tx, reply_rx) = mpsc::channel::<Envelope>();
        let caller = registry.register_inbox(
            &boot_authority(),
            "test.binding.reply_space.caller",
            forward_to_envelope_sender(reply_tx),
        );

        let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x00BE_EF03)));
        assert_eq!(binding.prev_correlation(), 0);

        let root = MailId::new(MailboxId(0xC0), 1);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller), 7);
        {
            let mut ctx = NativeCtx::new_dispatching(&binding, caller_source, root, root);
            OutboundReply::reply(&mut ctx, &Tick::default());
        }

        let reply_env = reply_rx.try_recv().expect("reply routed to the caller");
        assert!(
            reply_env.mail_id.correlation_id >= ReplyLineage::BASE,
            "reply id sits in the disjoint reply-lineage space",
        );
        assert_eq!(binding.prev_correlation(), 0, "minting a reply must not advance the send correlation counter");
        reply_env.discharge();
    }
}
