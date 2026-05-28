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

use crate::mail::{Mail, ReplyTo};
use crate::runtime::trace::SettlementHold;

/// One pending capture request. Carries the reply handle so the
/// render thread can reply once it has bytes, plus a resolved list
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
    pub reply_to: ReplyTo,
    pub after_mails: Vec<Mail>,
    pub pre_settlements: Vec<Receiver<()>>,
    /// ADR-0086 §12 settlement hold: held from `on_capture_frame` accept
    /// until the render thread's `send_reply` returns; drops with
    /// `PendingCapture` at end of the reply scope, firing `Release` so
    /// `Settled{root}` becomes exact post-reply. Without this guard the
    /// cap handler's `Finished` lands before the readback reply, the
    /// trace observer settles the chain, and the wire `Call` ends with
    /// zero reply events (iamacoffeepot/aether#1273). The pattern
    /// mirrors `contentgen::dispatch` (acquired before queue handoff so
    /// `HoldOpen` is visible to the settlement counter ahead of
    /// `Finished`).
    ///
    /// The field is never read — its sole job is to keep the
    /// [`SettlementHold`]'s `Drop` impl tied to the lifetime of this
    /// `PendingCapture`. `SettlementHold` carries `#[must_use]`, so
    /// stashing it on a named field (rather than `_`) is the only way
    /// to keep it alive across the queue handoff.
    pub hold: SettlementHold,
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

    /// Try to install `pending` as the pending capture. Returns `true`
    /// if the slot was empty and the request is now pending; `false`
    /// if a capture is already in flight. The caller wakes the event
    /// loop on success — `CaptureQueue` itself stays chassis-agnostic.
    ///
    /// # Panics
    /// Panics if the slot `Mutex` is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked under the guard.
    #[must_use]
    pub fn request(&self, pending: PendingCapture) -> bool {
        let mut slot = self
            .slot
            .lock()
            .expect("capture slot mutex poisoned; fail-fast per ADR-0063");
        if slot.is_some() {
            return false;
        }
        *slot = Some(pending);
        true
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
    use crate::ReplyTarget;
    use crate::runtime::trace::TraceHandle;
    use aether_data::{MailId, SessionToken, Uuid};

    fn reply_to(u: u128) -> ReplyTo {
        ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(u))))
    }

    fn pending(u: u128) -> PendingCapture {
        // Sentinel `MailId::NONE` keeps the acquire/release pair a no-op
        // in the settlement counter — the test exercises queue mechanics,
        // not the trace pipeline. `TraceHandle::new()` builds a stand-
        // alone handle with no installed registry.
        let hold = TraceHandle::new().acquire_settlement_hold(MailId::NONE);
        PendingCapture {
            reply_to: reply_to(u),
            after_mails: Vec::new(),
            pre_settlements: Vec::new(),
            hold,
        }
    }

    #[test]
    fn second_request_rejected_while_pending() {
        let q = CaptureQueue::new();
        assert!(q.request(pending(1)));
        assert!(!q.request(pending(2)));
    }

    #[test]
    fn take_clears_slot_for_next_request() {
        let q = CaptureQueue::new();
        assert!(q.request(pending(1)));
        let got = q.take().expect("pending");
        assert_eq!(got.reply_to, reply_to(1));
        // Slot is empty again.
        assert!(q.take().is_none());
        // Next request lands.
        assert!(q.request(pending(2)));
    }
}
