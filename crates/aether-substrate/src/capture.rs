// Cross-thread handoff for `aether.render.capture_frame` requests.
//
// The cap dispatcher thread runs `RenderCapability::on_capture_frame`;
// the actual capture (offscreen texture → staging buffer → PNG)
// happens on the render thread where the wgpu `Device` lives.
// `CaptureQueue` is the single-slot mailbox between them.
//
// One in-flight capture at a time is plenty for v1. If a second
// request arrives while one is pending, the cap handler rejects it
// immediately with `CaptureFrameResult::Err` rather than queuing —
// keeps the slot a scalar and avoids unbounded buildup if the render
// thread stalls.
//
// Waking the event loop on enqueue is the caller's job — after a
// successful `request()`, `RenderCapability` pokes the
// `CaptureBackend.wake` closure (desktop sends `UserEvent::Capture`
// on the `EventLoopProxy`; test-bench sends `ChassisEvent::CaptureRequested`
// on the embedder channel). Keeping the wake out of `CaptureQueue`
// means this type has zero chassis-awareness and lives anywhere a
// chassis cares about captures.
//
// Pre-issue-603 a sibling `reply_unsupported_*` family lived next to
// `CaptureQueue` for chassis-handler closures that replied `Err` to
// peripheral kinds (capture/window/platform_info/advance). Phases 2-4
// retired those closures by giving each kind its own cap; the helpers
// retired with them. See issue 429 for the original consolidation.

use std::sync::{Arc, Mutex};

use crossbeam_channel::Receiver;

use crate::chassis::inbox::InboundMail;
use crate::mail::Mail;

/// One pending capture request. Carries the retained inbound guard so
/// the render thread can reply once it has bytes, plus a resolved list
/// of `after_mails` the control plane already validated; the
/// render thread pushes them onto the queue after readback, before
/// replying.
///
/// `pre_settlements` is one settlement receiver per chassis-rooted
/// pre-mail the render cap pushed before parking this request
/// (iamacoffeepot/aether#860). The driver waits on each receiver
/// before rendering so the cross-thread causal chain triggered by
/// the pre-mails (component handlers → emitted `DrawTriangle` →
/// render cap accumulator) has fully landed before readback. Empty
/// when there were no pre-mails or when the chassis didn't install
/// a settlement registry (in which case the driver renders
/// immediately, preserving pre-fix behaviour on test fixtures).
pub struct PendingCapture {
    /// The retained inbound guard (ADR-0106 / iamacoffeepot/aether#1758).
    /// `RenderCapability::on_capture_frame` takes the dispatched
    /// `CaptureFrame` envelope out of its ctx via
    /// [`NativeCtx::take_inbound`](crate::actor::native::ctx::NativeCtx::take_inbound)
    /// and parks it here, so the render thread can reply a frame later
    /// through `reply.reply(&result)`. The guard's un-fired
    /// `record_finished` *is* the settlement hold: it keeps the inbound's
    /// chain open until the render thread replies and drops it, recording
    /// the reply's `Sent` before the inbound's `Finished` (ADR-0080 §6).
    /// This retires the hand-rolled `SettlementHold` + reply-id mint the
    /// deferred capture reply used to carry (iamacoffeepot/aether#1273 /
    /// #1719).
    pub reply: InboundMail,
    pub after_mails: Vec<Mail>,
    pub pre_settlements: Vec<Receiver<()>>,
    /// The `CaptureFrame.checks` request copied through from the cap
    /// handler. The render thread scores these reductions on the raw
    /// RGBA after readback and lands the verdict on the reply
    /// (iamacoffeepot/aether#1777). Empty when no verdict was requested.
    pub checks: Vec<aether_kinds::FrameCheck>,
}

/// Single-slot queue. Cheaply cloneable (wraps an `Arc`), shared
/// between the chassis-side control handler and the render thread.
#[derive(Clone, Default)]
pub struct CaptureQueue {
    slot: Arc<Mutex<Option<PendingCapture>>>,
}

impl CaptureQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to install `pending` as the pending capture. Returns `Ok(())`
    /// if the slot was empty and the request is now pending; `Err(pending)`
    /// hands the request back (boxed — it owns a large retained guard) when
    /// a capture is already in flight, so the caller can reply `Err`
    /// through the rejected request's retained `reply` guard before it
    /// drops (keeping the reply-before-`Finished` order, ADR-0080 §6). The
    /// caller wakes the event loop on success — `CaptureQueue` itself stays
    /// chassis-agnostic.
    ///
    /// # Panics
    /// Panics if the slot `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    #[must_use = "a rejected request still owns its inbound guard; reply through it before it drops"]
    pub fn request(&self, pending: PendingCapture) -> Result<(), Box<PendingCapture>> {
        let mut slot = self
            .slot
            .lock()
            .expect("capture slot mutex poisoned; fail-fast per ADR-0063");
        if slot.is_some() {
            return Err(Box::new(pending));
        }
        *slot = Some(pending);
        // Release the slot lock before returning — the retained guard in
        // `pending` has a significant `Drop`, so tighten the critical
        // section to just the check-and-set (clippy::significant_drop_tightening).
        drop(slot);
        Ok(())
    }

    /// Take the pending capture if one is set. Called by the render
    /// thread at the start of a frame; leaves the slot empty so the
    /// next capture request can land before this one completes.
    ///
    /// # Panics
    /// Panics if the slot `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    #[must_use]
    pub fn take(&self) -> Option<PendingCapture> {
        self.slot
            .lock()
            .expect("capture slot mutex poisoned; fail-fast per ADR-0063")
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::native::envelope::Envelope;
    use crate::chassis::inbox::SettlingInbox;
    use crate::handle_store::HandleStore;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::{OwnedDispatch, Registry};
    use crate::mail::{MailRef, Source};
    use aether_data::{KindId, MailId, MailboxId};
    use aether_kinds::trace::Nanos;
    use std::sync::mpsc;

    /// A `PendingCapture` whose retained `reply` guard wraps a NONE-lineage
    /// disarmed inbound — dropping it records nothing and never panics, so
    /// these slot-mechanics tests need no settlement registry. The inbound
    /// is sent straight onto the `SettlingInbox`'s channel (no `Mailer`
    /// route), then drained to the guard.
    fn pending() -> PendingCapture {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(registry, store));
        let id = MailboxId(0x0CA8);
        let (tx, rx) = mpsc::channel::<Envelope>();
        tx.send(OwnedDispatch::disarmed(
            KindId(0),
            "test.capture.pending".to_owned(),
            None,
            Source::NONE,
            MailRef::from(Vec::new()),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            id,
        ))
        .expect("queue the inbound");
        let inbox = SettlingInbox::new(id, rx, mailer);
        let reply = inbox.try_next().expect("one queued");
        PendingCapture {
            reply,
            after_mails: Vec::new(),
            pre_settlements: Vec::new(),
            checks: Vec::new(),
        }
    }

    #[test]
    fn second_request_rejected_while_pending() {
        let q = CaptureQueue::new();
        assert!(q.request(pending()).is_ok());
        assert!(
            q.request(pending()).is_err(),
            "a second request is rejected (and handed back) while one is pending",
        );
    }

    #[test]
    fn take_clears_slot_for_next_request() {
        let q = CaptureQueue::new();
        assert!(q.request(pending()).is_ok());
        assert!(q.take().is_some(), "the pending capture is taken");
        // Slot is empty again.
        assert!(q.take().is_none());
        // Next request lands.
        assert!(q.request(pending()).is_ok());
    }
}
