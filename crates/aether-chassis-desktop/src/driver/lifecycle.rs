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
/// notice, so it carries no `root` and the drop's `record_finished`
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
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use aether_kinds::{LifecycleAdvance, Shutdown, Tick};
    use aether_lifecycle::{LifecycleCapability, LifecycleConfig, LifecycleGraphData, LifecycleParams};
    use aether_substrate::SettlingInbox;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::mail::registry::InboxHandler;
    use aether_substrate::testing::{boot_test_chassis_with, fresh_substrate, registered_ref};

    use super::*;

    /// iamacoffeepot/aether#1704: the lifecycle reply inbox is a
    /// hand-rolled `claim_mailbox` consumer, so it must run the ADR-0094
    /// obligation + ADR-0080 §2 settlement bracket itself — the sibling
    /// window arm's #1325 fix, applied at the reply consume site. Drive a
    /// booted lifecycle cap through two advances the way the desktop frame
    /// loop does — a chassis-root `LifecycleAdvance` whose reply targets the
    /// registered inbox (the production `route_mail` Inbox arm arms the
    /// reply's guard) — and consume each reply with
    /// `consume_lifecycle_reply`. The first decodes the next stage and the
    /// terminal one `next == 0`; a misclassified reply would mis-gate the
    /// frame. Each consume drops an armed guard, so reaching the end at all
    /// proves it was discharged (pre-#1704 it aborted the process), and no
    /// advance chain is left in flight once both are consumed.
    #[test]
    fn consume_lifecycle_reply_discharges_armed_reply_and_balances_settlement() {
        let (registry, mailer) = fresh_substrate();
        let graph = LifecycleGraphData::builder()
            .state::<Tick>()
            .next::<Shutdown>()
            .terminal::<Shutdown>()
            .start::<Tick>()
            .build()
            .expect("test setup: graph builds");
        let chassis = boot_test_chassis_with::<LifecycleCapability>(
            &registry,
            &mailer,
            LifecycleConfig::default(),
            LifecycleParams { graph },
        );

        // Register the reply inbox exactly as `claim_mailbox` does: forward
        // the obligation-armed envelope onto the `SettlingInbox`'s channel,
        // carrying its guard with it so the framework drain owns the
        // discharge.
        let (tx, rx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |dispatch: Envelope| {
            let _ = tx.send(dispatch);
        });
        let reply_ref = registered_ref(&registry, "aether.lifecycle.advance_reply", handler);
        let inbox = SettlingInbox::new(reply_ref, rx, Arc::clone(&mailer));
        let lifecycle = chassis.root_pusher::<LifecycleCapability>();

        for (stage, expected) in
            [("the Tick advance", Some(<Shutdown as Kind>::ID.0)), ("the terminal advance", Some(0))]
        {
            lifecycle.push_root(&LifecycleAdvance { delta_micros: 0 }, Some(&inbox));
            let mail = inbox.recv_timeout(Duration::from_secs(5)).expect("the advance's reply reaches the inbox");
            let LifecycleReplyOutcome::Complete(next) = consume_lifecycle_reply(mail) else {
                panic!("{stage}'s reply is the advance-complete arm");
            };
            assert_eq!(next, expected, "{stage}'s reply decodes its `next`");
        }

        assert!(chassis.pending_settlement_roots().is_empty(), "no advance chain is left in flight");
    }
}
