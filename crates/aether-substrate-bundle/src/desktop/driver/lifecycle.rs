use std::sync::Arc;

use aether_data::Kind;
use aether_kinds::LifecycleAdvanceComplete;
use aether_substrate::actor::native::{
    dispatch_cost_tail_if_matching_free, dispatch_log_tail_if_matching_free, dispatch_trace_tail_if_matching_free,
};
use aether_substrate::mail::MailboxId;
use aether_substrate::{InboundMail, Mailer};

/// iamacoffeepot/aether#1272 / #1710: route an inbound `aether.window`
/// envelope through the framework-built-in dispatch arms
/// (`aether.log.tail` / `aether.trace.tail` / `aether.cost.tail`) before
/// the driver-specific `SetWindowMode` / `SetWindowTitle` arms get their
/// turn. Each helper computes-and-returns its `*TailResult`; on a match
/// the reply rides the inbound's drain guard — [`InboundMail::reply`]
/// mints the reply id on the drain-owned counter and stamps the
/// inbound's `root` / parent, so the framework-arm reply joins the
/// caller's ADR-0080 chain exactly like the window-mode / title arms.
/// Returns `true` when one of the framework arms matched (the reply has
/// been routed); `false` otherwise. ADR-0081 §1 promises every mailbox
/// serves these kinds — see the issue body for the contract.
///
/// Caller invariant: must run inside a `local::with_stamped` block
/// against the driver's [`aether_substrate::actor::native::local::ActorSlots`]
/// so the log / trace arms reach the driver's per-actor ring. Factored
/// out of `App::dispatch_window_envelope` so the unit test directly
/// drives the routing shape without standing up a winit `App`.
pub(super) fn try_framework_dispatch(mailer: &Arc<Mailer>, self_mailbox: MailboxId, mail: &InboundMail) -> bool {
    let env = mail.envelope();
    if let Some(result) = dispatch_log_tail_if_matching_free(env) {
        mail.reply(&result);
        return true;
    }
    if let Some(result) = dispatch_trace_tail_if_matching_free(env) {
        mail.reply(&result);
        return true;
    }
    if let Some(result) = dispatch_cost_tail_if_matching_free(mailer.as_ref(), self_mailbox, env) {
        mail.reply(&result);
        return true;
    }
    false
}

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
/// settle `App::dispatch_window_envelope` relies on for the sibling
/// `aether.window` inbox. The hand-rolled per-arm `record_finished` +
/// `discharge()` pairs that #1325 / #1704 added retired with the
/// framework drain: dropping `mail` on either arm settles.
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
    // Tests derive chassis mailbox ids by name to address window/lifecycle mail
    // in fixtures — reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use aether_actor::Addressable;
    use aether_data::mailbox_id_from_name;
    use aether_kinds::SetWindowTitle;
    use aether_substrate::SettlingInbox;
    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::mail::{Source, SourceAddr};

    /// iamacoffeepot/aether#1272 / #1710: a `LogTail` Call drained at the
    /// driver's `aether.window` mailbox produces a `LogTailResult` reply
    /// through the inbound's drain guard — and that reply joins the
    /// inbound's ADR-0080 causal chain (it carries the inbound's `root`)
    /// rather than minting the lineage-less `MailId::NONE` triple the
    /// pre-#1710 bare `Mailer::send_reply` form did. Drives a real armed
    /// Call through a `SettlingInbox` (mirroring
    /// `window_inbox_drain_settles_root_on_guard_drop`), routes the reply
    /// at a captured `Component` inbox, and reads the delivered reply's
    /// `root` — without standing up wgpu/winit.
    #[test]
    fn try_framework_dispatch_replies_to_log_tail() {
        use std::sync::mpsc;
        use std::time::Duration;

        use aether_actor::local::ActorSlots;
        use aether_data::MailId;
        use aether_kinds::descriptors;
        use aether_kinds::{LogTail, LogTailResult};
        use aether_substrate::actor::native::local::with_stamped;

        use aether_substrate::chassis::settlement::SettlementRegistry;
        use aether_substrate::mail::Mail;
        use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch, Registry};
        use aether_substrate::mail::{Source, SourceAddr};

        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));

        // Wire one settlement registry into both seams (the chassis builder
        // does both installs at boot) so an armed Call drains cleanly.
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));

        // A `Component` caller inbox captures the reply so the test reads
        // its delivered `root`.
        let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
        let caller_mailbox = registry.register_inbox(
            "test.window.log_tail.caller",
            Arc::new(move |dispatch: OwnedDispatch| {
                dispatch.discharge();
                let _ = reply_tx.send(dispatch);
            }) as Arc<dyn InboxHandler>,
        );

        // The window inbox forwards armed envelopes onto the
        // `SettlingInbox`'s channel, exactly as `claim_mailbox` does.
        let window_mailbox = mailbox_id_from_name(<aether_window::HeadlessWindowCapability as Addressable>::NAMESPACE);
        let (tx, rx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
            let _ = tx.send(d);
        });
        registry
            .try_register_inbox_with_id(window_mailbox, "aether.window", handler)
            .expect("register the window inbox");
        let inbox = SettlingInbox::new(window_mailbox, rx, Arc::clone(&mailer));

        // Push a real armed `LogTail` Call whose reply target is the
        // caller inbox, then drain it to an `InboundMail` guard.
        let root = MailId::new(window_mailbox, 1);
        let mail_id = MailId::new(window_mailbox, 2);
        mailer.record_sent_inflight(root);
        let caller_source = Source::with_correlation(SourceAddr::Component(caller_mailbox), 0x99);
        let bytes = LogTail { max: 8, min_level: None, since: None, contains: None }.encode_into_bytes();
        mailer.push(
            Mail::new(window_mailbox, <LogTail as Kind>::ID, bytes, 1)
                .with_reply_to(caller_source)
                .with_lineage(mail_id, root, None),
        );
        let mail = inbox.try_next().expect("the armed LogTail Call is queued");

        let slots = ActorSlots::new();
        let matched = with_stamped(&slots, || try_framework_dispatch(&mailer, window_mailbox, &mail));
        assert!(matched, "framework dispatch arm must match a LogTail Call at aether.window");

        let dispatch =
            reply_rx.recv_timeout(Duration::from_secs(2)).expect("framework arm routed a reply to the caller inbox");
        assert_eq!(dispatch.kind_name, <LogTailResult as Kind>::NAME, "the reply is a LogTailResult");
        assert_eq!(dispatch.root, root, "the framework-arm reply joins the inbound's causal chain (#1710)");

        // Drop the guard last so its `Finished` records after the reply's
        // `Sent` (ADR-0080 §6) — settlement bookkeeping stays balanced.
        drop(mail);
    }

    /// A non-framework kind (here `SetWindowTitle`) does NOT trip the
    /// framework arms — the driver-specific path keeps its turn so
    /// `actor_logs`-style queries don't shadow the existing window
    /// controls. The helpers return `None` and the driver dispatch
    /// continues; nothing is replied here.
    #[test]
    fn try_framework_dispatch_skips_window_kinds() {
        use std::sync::mpsc;

        use aether_actor::local::ActorSlots;
        use aether_data::MailId;
        use aether_kinds::descriptors;
        use aether_substrate::actor::native::local::with_stamped;

        use aether_substrate::mail::Mail;
        use aether_substrate::mail::registry::{InboxHandler, Registry};

        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));

        let window_mailbox = mailbox_id_from_name(<aether_window::HeadlessWindowCapability as Addressable>::NAMESPACE);
        let (tx, rx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
            let _ = tx.send(d);
        });
        registry
            .try_register_inbox_with_id(window_mailbox, "aether.window", handler)
            .expect("register the window inbox");
        let inbox = SettlingInbox::new(window_mailbox, rx, Arc::clone(&mailer));

        // A `MailId::NONE` push keeps the drained guard disarmed (no armed
        // Call to settle) — the test pins only the skip verdict.
        let payload = SetWindowTitle { title: "ignored".to_owned() }.encode_into_bytes();
        mailer.push(Mail::new(window_mailbox, <SetWindowTitle as Kind>::ID, payload, 1).with_lineage(
            MailId::NONE,
            MailId::NONE,
            None,
        ));
        let mail = inbox.try_next().expect("the SetWindowTitle push is queued");

        let slots = ActorSlots::new();
        let matched = with_stamped(&slots, || try_framework_dispatch(&mailer, window_mailbox, &mail));
        assert!(!matched, "SetWindowTitle is a driver-specific kind");
    }

    /// iamacoffeepot/aether#1325, re-homed on ADR-0106: the window inbox
    /// drain owns the ADR-0080 §2 settlement bracket for every inbound
    /// envelope (the `Inbox` mailbox records none on the producer side),
    /// now by construction — draining a real armed Call through a
    /// `SettlingInbox` and letting the `InboundMail` guard fall out of
    /// scope settles the root and disarms the ADR-0094 guard (no #1704
    /// abort). The CI-runnable regression guard for the window drain
    /// without standing up winit/wgpu; the windowed end-to-end
    /// blocking-send path stays MCP-manual.
    #[test]
    fn window_inbox_drain_settles_root_on_guard_drop() {
        use std::sync::mpsc;

        use aether_data::{Kind, MailId};
        use aether_kinds::descriptors;

        use aether_substrate::chassis::settlement::SettlementRegistry;
        use aether_substrate::mail::Mail;
        use aether_substrate::mail::registry::{InboxHandler, Registry};

        fn title_payload() -> Vec<u8> {
            SetWindowTitle { title: "ignored".to_owned() }.encode_into_bytes()
        }

        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));

        // Wire a settlement registry into both seams (the chassis builder
        // does both installs at boot, builder.rs:1119-1122) so the
        // emit-time counter's zero-transition can `fire_settled`.
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));

        // Register the window mailbox forwarding armed envelopes onto the
        // `SettlingInbox`'s channel, exactly as `claim_mailbox` does.
        let window_mailbox = mailbox_id_from_name(<aether_window::HeadlessWindowCapability as Addressable>::NAMESPACE);
        let (tx, rx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
            let _ = tx.send(d);
        });
        registry
            .try_register_inbox_with_id(window_mailbox, "aether.window", handler)
            .expect("register the window inbox");
        let inbox = SettlingInbox::new(window_mailbox, rx, Arc::clone(&mailer));

        // A real armed Call: seed the root, push through the mailer so the
        // `route_mail` Inbox arm arms the obligation guard with `mail_id`,
        // drain via `try_next`, then drop — the guard's `Drop` records
        // `Finished` and disarms, settling the root.
        let root = MailId::new(window_mailbox, 1);
        let mail_id = MailId::new(window_mailbox, 2);
        mailer.record_sent_inflight(root);
        let settle = settlement.subscribe_settlement(root);
        mailer.push(
            Mail::new(window_mailbox, <SetWindowTitle as Kind>::ID, title_payload(), 1)
                .with_lineage(mail_id, root, None),
        );
        drop(inbox.try_next().expect("the armed Call is queued"));
        settle.recv().expect("window root settles when the drained mail's guard drops");

        // A `MailId::NONE` push (window-size / frame-stats) settles
        // nothing — the guard mints disarmed and `record_finished` no-ops.
        let guard_root = MailId::new(window_mailbox, 3);
        mailer.record_sent_inflight(guard_root);
        let guard_rx = settlement.subscribe_settlement(guard_root);
        mailer.push(Mail::new(window_mailbox, <SetWindowTitle as Kind>::ID, title_payload(), 1).with_lineage(
            MailId::NONE,
            guard_root,
            None,
        ));
        drop(inbox.try_next().expect("the NONE push is queued"));
        assert!(guard_rx.try_recv().is_err(), "a NONE inbound discharges no root");
    }

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

        let cap_mailbox = mailbox_id_from_name(<aether_capabilities::LifecycleCapability as Addressable>::NAMESPACE);

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
