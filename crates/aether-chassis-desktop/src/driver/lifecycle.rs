use aether_data::Kind;
use aether_kinds::LifecycleAdvanceComplete;
use aether_substrate::InboundMail;

/// The disposition of one consumed `aether.lifecycle.advance_reply`
/// envelope, returned by [`consume_lifecycle_reply`] to
/// `App::recv_lifecycle_advance_next`.
pub(super) enum LifecycleReplyOutcome {
    /// The envelope was the expected [`LifecycleAdvanceComplete`]. Carries
    /// the decoded `next` stage kind id, or `None` when the payload failed
    /// to decode — the caller fail-fasts that the same as a missing reply.
    Complete(Option<u64>),
    /// An unexpected kind on the dedicated reply inbox (nothing else
    /// targets it). Discharged like the matched arm; the caller keeps
    /// waiting for the advance reply rather than mis-gating the cycle.
    Unexpected,
}

/// Consume one [`InboundMail`] off the lifecycle reply inbox. The mail's
/// ADR-0094 obligation guard + ADR-0080 §2 settlement bracket discharge
/// when the guard falls out of scope (ADR-0106) — the same scope-exit
/// settle the shared `dispatch_envelope` body runs for the sibling
/// `aether.window` actor (ADR-0160 §Decision 3). The hand-rolled per-arm
/// `record_finished` + `discharge()` pairs that #1325 / #1704 added retired
/// with the framework drain: dropping `mail` on either arm settles.
///
/// On the per-frame path the reply rides a bare, lineage-less `Settled`
/// notice, so its `root` is `MailId::NONE` and the drop's `record_finished`
/// is a counter no-op; the live obligation it discharges is the debug
/// guard the real `route_mail` Inbox arm armed.
//
// `mail` is taken by value so its guard's `Drop` (the settlement) binds
// to this scope; the body only calls `&self` accessors, which clippy
// reads as a needless by-value.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn consume_lifecycle_reply(mail: InboundMail) -> LifecycleReplyOutcome {
    if mail.kind() == <LifecycleAdvanceComplete as Kind>::ID {
        LifecycleReplyOutcome::Complete(
            LifecycleAdvanceComplete::decode_from_bytes(mail.payload()).map(|complete| complete.next),
        )
    } else {
        LifecycleReplyOutcome::Unexpected
    }
    // `mail` drops here — both arms settle (ADR-0106).
}

#[cfg(test)]
mod tests {
    // Tests derive chassis mailbox ids by name to address lifecycle mail
    // in fixtures — reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use aether_actor::Addressable;
    use aether_data::mailbox_id_from_name;
    use aether_substrate::Mailer;
    use aether_substrate::SettlingInbox;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::mail::{Source, SourceAddr};
    use std::sync::Arc;

    /// iamacoffeepot/aether#1704: the lifecycle reply inbox is a
    /// hand-rolled `claim_mailbox` consumer, so it must run the ADR-0094
    /// obligation + ADR-0080 §2 settlement bracket itself — the sibling
    /// window arm's #1325 fix, applied at the reply consume site. Route a
    /// real, obligation-**armed** `LifecycleAdvanceComplete` reply through
    /// the registered inbox (the production `route_mail` Inbox arm arms the
    /// guard), then drive `consume_lifecycle_reply` over it and assert the
    /// guard is disarmed (no abort on drop) AND the settlement counter
    /// balances — on both a non-`NONE`-root reply (the discharge is the
    /// sole `Finished`, so the root settles) and the `NONE`-root per-frame
    /// reply (a counter no-op). Pre-#1704 the armed guard aborted the
    /// process when the consume site dropped the envelope without
    /// `discharge`.
    #[test]
    fn consume_lifecycle_reply_discharges_armed_reply_and_balances_settlement() {
        use std::sync::mpsc;

        use aether_data::MailId;

        use aether_substrate::chassis::settlement::SettlementRegistry;
        use aether_substrate::mail::registry::{InboxHandler, Registry};

        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));

        // Wire one settlement registry into both seams (the chassis builder
        // does both installs at boot) so the counter's zero-transition can
        // `fire_settled`.
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));

        // Register the reply inbox exactly as `claim_mailbox` does: forward
        // the obligation-armed envelope onto the `SettlingInbox`'s channel,
        // carrying its guard with it so the framework drain owns the
        // discharge.
        let (tx, rx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: Envelope| {
            let _ = tx.send(dispatch);
        });
        let reply_mailbox =
            registry.try_register_inbox("aether.lifecycle.advance_reply", handler).expect("register the reply inbox");
        let inbox = SettlingInbox::new(reply_mailbox, rx, Arc::clone(&mailer));

        let cap_mailbox = mailbox_id_from_name(<aether_lifecycle::LifecycleCapability as Addressable>::NAMESPACE);

        // (1) Non-`NONE`-root reply (the degraded `on_advance` inline-reply
        // shape): the producer hook records the reply's `Sent` against
        // `root`, and the `InboundMail` guard's `Drop` inside
        // `consume_lifecycle_reply` is the sole `Finished`, so the root
        // settles.
        let root = MailId::new(cap_mailbox, 1);
        // 1<<63 is the disjoint reply-lineage base (#1701) — a non-`NONE`
        // mail_id, so the real Inbox arm arms the obligation guard.
        let armed_reply_id = MailId::new(cap_mailbox, 1 << 63);
        let settle_rx = settlement.subscribe_settlement(root);
        let sender = Source::with_correlation(SourceAddr::Component(reply_mailbox), 7);
        mailer.send_reply(sender, &LifecycleAdvanceComplete { completed: 1, next: 42 }, armed_reply_id, root, None);
        let mail = inbox.try_next().expect("armed reply routed to the inbox");
        match consume_lifecycle_reply(mail) {
            LifecycleReplyOutcome::Complete(next) => {
                assert_eq!(next, Some(42), "decodes the advance-complete `next`");
            }
            LifecycleReplyOutcome::Unexpected => panic!("expected the advance-complete arm"),
        }
        settle_rx.recv().expect("the guard's Finished balances the reply's Sent and settles the root");

        // (2) `NONE`-root reply (the real per-frame deferred path, replying
        // to a bare lineage-less `Settled` notice): the producer's
        // `record_sent_inflight` no-ops, the drop's `record_finished` is a
        // counter no-op, and the armed guard is still disarmed so the
        // envelope drops without the ADR-0094 abort. Reaching the assert at
        // all proves the guard was disarmed.
        let armed_reply_id_2 = MailId::new(cap_mailbox, (1 << 63) + 1);
        let sender_2 = Source::with_correlation(SourceAddr::Component(reply_mailbox), 8);
        mailer.send_reply(
            sender_2,
            &LifecycleAdvanceComplete { completed: 2, next: 0 },
            armed_reply_id_2,
            MailId::NONE,
            None,
        );
        let mail_2 = inbox.try_next().expect("second armed reply routed to the inbox");
        match consume_lifecycle_reply(mail_2) {
            LifecycleReplyOutcome::Complete(next) => {
                assert_eq!(next, Some(0), "the terminal reply decodes `next == 0`");
            }
            LifecycleReplyOutcome::Unexpected => panic!("expected the advance-complete arm"),
        }
    }
}
