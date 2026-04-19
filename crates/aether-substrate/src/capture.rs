// Cross-thread handoff for `aether.control.capture_frame` requests.
//
// The control-plane handler runs on a scheduler worker; the actual
// capture (swapchain → staging buffer → PNG) happens on the render
// thread where the wgpu `Device` lives. `CaptureQueue` is the
// single-slot mailbox between them.
//
// One in-flight capture at a time is plenty for v1. If a second
// request arrives while one is pending, the control handler rejects
// it immediately with `CaptureFrameResult::Err` rather than queuing —
// keeps the slot a scalar and avoids unbounded buildup if the render
// thread stalls.

use std::sync::{Arc, Mutex};

use aether_hub_protocol::SessionToken;

/// One pending capture request. Carries the originating session's
/// token so the render thread can reply-to-sender once it has bytes.
#[derive(Copy, Clone, Debug)]
pub struct PendingCapture {
    pub sender: SessionToken,
}

/// Single-slot queue. Cheaply cloneable (wraps an `Arc`), shared
/// between the control-plane handler and the render thread.
#[derive(Clone, Default)]
pub struct CaptureQueue {
    slot: Arc<Mutex<Option<PendingCapture>>>,
}

impl CaptureQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to install `sender` as the pending capture. Returns `true`
    /// if the slot was empty and the request is now pending; `false`
    /// if a capture is already in flight.
    pub fn request(&self, sender: SessionToken) -> bool {
        let mut slot = self.slot.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        *slot = Some(PendingCapture { sender });
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
    use aether_hub_protocol::Uuid;

    fn token(u: u128) -> SessionToken {
        SessionToken(Uuid::from_u128(u))
    }

    #[test]
    fn request_into_empty_slot_succeeds() {
        let q = CaptureQueue::new();
        assert!(q.request(token(1)));
    }

    #[test]
    fn second_request_rejected_while_pending() {
        let q = CaptureQueue::new();
        assert!(q.request(token(1)));
        assert!(!q.request(token(2)));
    }

    #[test]
    fn take_clears_slot_for_next_request() {
        let q = CaptureQueue::new();
        assert!(q.request(token(1)));
        let got = q.take().expect("pending");
        assert_eq!(got.sender, token(1));
        // Slot is empty again.
        assert!(q.take().is_none());
        // Next request lands.
        assert!(q.request(token(2)));
    }
}
