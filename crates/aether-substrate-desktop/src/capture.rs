// Cross-thread handoff for `aether.control.capture_frame` requests.
//
// The chassis-side capture_frame handler runs on a scheduler worker;
// the actual capture (offscreen texture → staging buffer → PNG)
// happens on the render thread where the wgpu `Device` lives.
// `CaptureQueue` is the single-slot mailbox between them.
//
// One in-flight capture at a time is plenty for v1. If a second
// request arrives while one is pending, the chassis handler rejects
// it immediately with `CaptureFrameResult::Err` rather than queuing —
// keeps the slot a scalar and avoids unbounded buildup if the render
// thread stalls.
//
// Waking the event loop on enqueue is the caller's job now — after a
// successful `request()`, the desktop chassis handler pokes its
// `EventLoopProxy<UserEvent>` directly. Keeping the wake out of
// `CaptureQueue` means this type has zero chassis-awareness and could
// live in any chassis crate that ever supports captures.

use std::sync::{Arc, Mutex};

use aether_substrate_core::{Mail, ReplyTo};
#[cfg(test)]
use aether_substrate_core::ReplyTarget;

/// One pending capture request. Carries the reply handle so the
/// render thread can reply once it has bytes, plus a resolved list
/// of `after_mails` the control plane already validated; the
/// render thread pushes them onto the queue after readback, before
/// replying.
pub struct PendingCapture {
    pub reply_to: ReplyTo,
    pub after_mails: Vec<Mail>,
}

/// Single-slot queue. Cheaply cloneable (wraps an `Arc`), shared
/// between the chassis-side control handler and the render thread.
#[derive(Clone, Default)]
pub struct CaptureQueue {
    slot: Arc<Mutex<Option<PendingCapture>>>,
}

impl CaptureQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to install `pending` as the pending capture. Returns `true`
    /// if the slot was empty and the request is now pending; `false`
    /// if a capture is already in flight. The caller wakes the event
    /// loop on success — `CaptureQueue` itself stays chassis-agnostic.
    pub fn request(&self, pending: PendingCapture) -> bool {
        let mut slot = self.slot.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        *slot = Some(pending);
        true
    }

    /// Take the pending capture if one is set. Called by the render
    /// thread at the start of a frame; leaves the slot empty so the
    /// next capture request can land before this one completes.
    pub fn take(&self) -> Option<PendingCapture> {
        self.slot.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::{SessionToken, Uuid};

    fn reply_to(u: u128) -> ReplyTo {
        ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::from_u128(u))))
    }

    fn pending(u: u128) -> PendingCapture {
        PendingCapture {
            reply_to: reply_to(u),
            after_mails: Vec::new(),
        }
    }

    #[test]
    fn request_into_empty_slot_succeeds() {
        let q = CaptureQueue::new();
        assert!(q.request(pending(1)));
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
