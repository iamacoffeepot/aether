use aether_kinds::CaptureFrameResult;
use aether_substrate::capture::PendingCapture;

/// The disposition of a `capture_frame` wake against the current window
/// occlusion state — the pure branch-selection half of the occluded
/// fail-fast (iamacoffeepot/aether#1317), factored out of the winit
/// wiring so it is unit-testable without standing up a winit `App`
/// (mirroring `try_framework_dispatch`).
pub(super) enum OccludedCaptureDisposition {
    /// Window is visible — the caller falls through to `request_redraw`
    /// so `RedrawRequested` services the (still-parked) capture normally.
    Redraw,
    /// Window is occluded but no capture was parked (already serviced, or
    /// a stale wake) — nothing to do.
    Empty,
    /// Window is occluded and a capture is parked — fail it fast. Carries
    /// the taken [`PendingCapture`] (so the caller drains `after_mails`,
    /// replies through its retained inbound guard, then drops it to settle
    /// the inbound chain) and the `Err` reply to send.
    FailFast {
        // Boxed: `PendingCapture` carries a retained `InboundMail` guard, so
        // it dwarfs the zero-size `Redraw` / `Empty` variants
        // (clippy::large_enum_variant).
        request: Box<PendingCapture>,
        result: CaptureFrameResult,
    },
}

/// The capture-frame error message naming `aether.window.focus` as the
/// remedy. Shared by every occluded fail-fast site so the wording can't
/// drift.
const OCCLUDED_CAPTURE_ERROR: &str = "capture_frame: window is occluded (hidden/minimized); bring it to the \
     foreground via aether.window.focus and retry";

/// Select the disposition for a `capture_frame` wake (or an occlusion
/// onset) given the window's occlusion state and any parked capture.
///
/// The winit side only `take()`s the [`aether_substrate::capture::CaptureQueue`]
/// slot when occluded, so a visible-window wake never steals the entry
/// that `RedrawRequested` is about to service — the `Redraw` arm carries
/// no `PendingCapture`. The occluded arms move the taken request through
/// so the caller can drain `after_mails`, reply through the retained
/// inbound guard, and drop the request *after* the reply (settling the
/// inbound chain post-reply, ADR-0080 §6 / iamacoffeepot/aether#1273).
pub(super) fn occluded_capture_disposition(
    occluded: bool,
    pending: Option<PendingCapture>,
) -> OccludedCaptureDisposition {
    if !occluded {
        return OccludedCaptureDisposition::Redraw;
    }
    pending.map_or(OccludedCaptureDisposition::Empty, |request| OccludedCaptureDisposition::FailFast {
        request: Box::new(request),
        result: CaptureFrameResult::Err { error: OCCLUDED_CAPTURE_ERROR.to_owned() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use aether_substrate::actor::native::envelope::Envelope;
    use aether_substrate::mail::MailboxId;
    use aether_substrate::{Mailer, SettlingInbox};

    /// iamacoffeepot/aether#1317: the occluded-capture branch selection.
    /// This is the CI-runnable core of the fail-fast — the winit wiring
    /// (`fail_capture_if_occluded` → `take` → `reply.reply` → drop) stays
    /// MCP-manual because `ControlFlow::Wait` + `RedrawRequested`
    /// suppression has no CI display surface. Here we pin only the pure
    /// branch logic: occluded-with-parked-capture selects an `Err` reply,
    /// occluded-with-empty-slot and visible-window are no-ops/redraw.
    #[test]
    fn occluded_capture_disposition_selects_failfast_only_when_occluded_and_parked() {
        use std::sync::mpsc;

        use aether_data::{KindId, MailId};
        use aether_kinds::trace::Nanos;
        use aether_substrate::capture::PendingCapture;
        use aether_substrate::mail::MailRef;
        use aether_substrate::mail::Source;
        use aether_substrate::mail::registry::{OwnedDispatch, Registry};

        fn parked() -> PendingCapture {
            // A NONE-lineage disarmed inbound: its retained guard carries no
            // settlement obligation, so dropping it is a clean no-op and
            // this branch-selection test needs no settlement registry. Sent
            // straight onto the `SettlingInbox`'s channel, then drained to
            // the guard.
            let registry = Arc::new(Registry::new());
            let mailer = Arc::new(Mailer::new(registry));
            let mailbox = MailboxId(0x0CAB);
            let (tx, rx) = mpsc::channel::<Envelope>();
            tx.send(OwnedDispatch::disarmed(
                KindId(0),
                "test.capture.parked".to_owned(),
                None,
                Source::NONE,
                MailRef::from(Vec::new()),
                1,
                MailId::NONE,
                MailId::NONE,
                None,
                Nanos(0),
                0,
                mailbox,
            ))
            .expect("queue the parked inbound");
            let inbox = SettlingInbox::new(mailbox, rx, mailer);
            let reply = inbox.try_next().expect("one queued");
            PendingCapture {
                reply,
                after_mails: Vec::new(),
                pre_settlements: Vec::new(),
                checks: Vec::new(),
                reference: None,
            }
        }

        // Occluded + a parked capture → fail it fast with an `Err` reply
        // whose message names `aether.window.focus` as the remedy.
        match occluded_capture_disposition(true, Some(parked())) {
            OccludedCaptureDisposition::FailFast { result, .. } => match result {
                CaptureFrameResult::Err { error } => {
                    assert!(
                        error.contains("occluded") && error.contains("aether.window.focus"),
                        "Err reply must name occlusion + the focus remedy, got: {error}",
                    );
                }
                CaptureFrameResult::Ok { .. } => panic!("occluded capture must fail, not Ok"),
            },
            _ => panic!("occluded + parked capture must select FailFast"),
        }

        // Occluded but nothing parked → no-op (already serviced / stale wake).
        assert!(
            matches!(occluded_capture_disposition(true, None), OccludedCaptureDisposition::Empty),
            "occluded + empty slot must be a no-op",
        );

        // Visible window → fall through to redraw, regardless of the slot.
        assert!(
            matches!(occluded_capture_disposition(false, Some(parked())), OccludedCaptureDisposition::Redraw),
            "visible window must fall through to redraw",
        );
        assert!(
            matches!(occluded_capture_disposition(false, None), OccludedCaptureDisposition::Redraw),
            "visible window must fall through to redraw",
        );
    }

    /// iamacoffeepot/aether#1758: the deferred capture reply joins the
    /// inbound's ADR-0080 causal chain through the `InboundMail` guard the
    /// render cap parked on `PendingCapture` (via `take_inbound`). Replying
    /// through `req.reply.reply` records the reply's `Sent`; dropping `req`
    /// records the inbound's `Finished` *after* it (ADR-0080 §6), so the
    /// caller's root stays open until the reply itself finishes — settling
    /// exactly once. This pins the capture-migration's reply-before-drop
    /// order without standing up wgpu/winit, mirroring the claimed-inbox
    /// `reply_sent_recorded_before_inbound_finished` shape through the
    /// capture queue's parked request.
    #[test]
    fn capture_reply_joins_held_chain() {
        use std::sync::mpsc;

        use aether_data::{Kind, MailId};
        use aether_kinds::{CaptureFrame, CaptureFrameResult, descriptors};

        use aether_substrate::capture::PendingCapture;
        use aether_substrate::chassis::settlement::SettlementRegistry;
        use aether_substrate::mail::registry::{InboxHandler, Registry};
        use aether_substrate::mail::{Mail, Source, SourceAddr};

        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));

        // One settlement registry wired into both seams (the chassis builder
        // does both installs at boot) so the inbound + reply settle cleanly.
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer.trace_handle().install_settlement_registry(Arc::clone(&settlement));

        // A `Component` reply target captures the deferred reply so the test
        // reads its delivered `root` and finishes it — the desktop MCP
        // capture reply target is a `Component(rpc_server)`.
        let (reply_tx, reply_rx) = mpsc::channel::<Envelope>();
        let target = registry.register_inbox(
            "test.capture.reply_target",
            Arc::new(move |d: Envelope| {
                let _ = reply_tx.send(d);
            }) as Arc<dyn InboxHandler>,
        );

        // Push a real armed `CaptureFrame` Call whose reply target is the
        // captured inbox, then drain it to the `PendingCapture`'s retained
        // guard (the render cap's `take_inbound`, mirrored out-of-crate).
        let render_mailbox = MailboxId(0x0CA6);
        let (tx, rx) = mpsc::channel::<Envelope>();
        let handler: Arc<dyn InboxHandler> = Arc::new(move |d: Envelope| {
            let _ = tx.send(d);
        });
        registry
            .try_register_inbox_with_id(render_mailbox, "test.capture.render", handler)
            .expect("register the render inbox");
        let inbox = SettlingInbox::new(render_mailbox, rx, Arc::clone(&mailer));

        let root = MailId::new(render_mailbox, 1);
        let mail_id = MailId::new(render_mailbox, 2);
        mailer.record_sent_inflight(root);
        let settle = settlement.subscribe_settlement(root);
        let caller = Source::with_correlation(SourceAddr::Component(target), 0x42);
        mailer.push(
            Mail::new(render_mailbox, <CaptureFrame as Kind>::ID, Vec::new(), 1)
                .with_reply_to(caller)
                .with_lineage(mail_id, root, None),
        );
        let reply = inbox.try_next().expect("the armed CaptureFrame Call is queued");
        let req = PendingCapture {
            reply,
            after_mails: Vec::new(),
            pre_settlements: Vec::new(),
            checks: Vec::new(),
            reference: None,
        };

        // The driver's reply path: reply through the retained guard, then
        // let `req` (the parked request) drop.
        assert!(
            req.reply.reply(&CaptureFrameResult::Err { error: "deferred".to_owned() }),
            "the deferred capture reply routed to the Component target",
        );
        assert!(settle.try_recv().is_err(), "the reply's Sent holds the chain open");
        drop(req);
        assert!(
            settle.try_recv().is_err(),
            "inbound Finished alone does not settle — the deferred reply is still in flight",
        );

        // Finish the reply the way the RPC server's dispatcher would; only
        // now does the root settle — exactly once.
        let reply_env = reply_rx.recv().expect("deferred reply routed to the target");
        assert_eq!(
            reply_env.kind_name,
            <CaptureFrameResult as Kind>::NAME,
            "the deferred reply is a CaptureFrameResult",
        );
        assert_eq!(reply_env.root, root, "the deferred capture reply joins the inbound's causal chain (#1758)");
        let reply_id = reply_env.mail_id;
        reply_env.discharge();
        mailer.record_finished(reply_id, root);
        settle.recv().expect("root settles once the deferred capture reply finishes");
    }
}
