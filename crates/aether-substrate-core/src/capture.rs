// Cross-thread handoff for `aether.control.capture_frame` requests.
//
// The control-plane handler runs on a scheduler worker; the actual
// capture (offscreen texture → staging buffer → PNG) happens on the
// render thread where the wgpu `Device` lives. `CaptureQueue` is the
// single-slot mailbox between them.
//
// One in-flight capture at a time is plenty for v1. If a second
// request arrives while one is pending, the control handler rejects
// it immediately with `CaptureFrameResult::Err` rather than queuing —
// keeps the slot a scalar and avoids unbounded buildup if the render
// thread stalls.
//
// The attached `Chassis` pokes its event loop (or no-ops) whenever a
// capture lands so even an occluded window (on macOS) still processes
// the request rather than sleeping until the next window event.

use std::sync::{Arc, Mutex};

use aether_hub_protocol::SessionToken;

use crate::Mail;
use crate::chassis::Chassis;

/// One pending capture request. Carries the originating session's
/// token so the render thread can reply-to-sender once it has bytes,
/// plus a resolved list of `after_mails` the control plane already
/// validated; the render thread pushes them onto the queue after
/// readback, before replying.
pub struct PendingCapture {
    pub sender: SessionToken,
    pub after_mails: Vec<Mail>,
}

/// Single-slot queue. Cheaply cloneable (wraps an `Arc`), shared
/// between the control-plane handler and the render thread.
#[derive(Clone, Default)]
pub struct CaptureQueue {
    slot: Arc<Mutex<Option<PendingCapture>>>,
    chassis: Option<Arc<dyn Chassis>>,
}

impl CaptureQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_chassis(chassis: Arc<dyn Chassis>) -> Self {
        Self {
            slot: Arc::default(),
            chassis: Some(chassis),
        }
    }

    /// Try to install `pending` as the pending capture. Returns `true`
    /// if the slot was empty and the request is now pending; `false`
    /// if a capture is already in flight. On success, pokes the
    /// chassis (if any) so a sleeping event loop can pick up the
    /// request.
    pub fn request(&self, pending: PendingCapture) -> bool {
        let mut slot = self.slot.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        *slot = Some(pending);
        drop(slot);
        if let Some(chassis) = &self.chassis {
            chassis.wake_for_capture();
        }
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
    use std::sync::atomic::{AtomicU32, Ordering};

    fn token(u: u128) -> SessionToken {
        SessionToken(Uuid::from_u128(u))
    }

    fn pending(u: u128) -> PendingCapture {
        PendingCapture {
            sender: token(u),
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
        assert_eq!(got.sender, token(1));
        // Slot is empty again.
        assert!(q.take().is_none());
        // Next request lands.
        assert!(q.request(pending(2)));
    }

    #[test]
    fn chassis_wakes_on_successful_request_only() {
        use aether_kinds::WindowMode;
        struct Counter(AtomicU32);
        impl Chassis for Counter {
            fn wake_for_capture(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn request_platform_info(&self, _sender: SessionToken) {}
            fn request_set_window_mode(
                &self,
                _sender: SessionToken,
                _mode: WindowMode,
                _width: Option<u32>,
                _height: Option<u32>,
            ) {
            }
        }
        let chassis = Arc::new(Counter(AtomicU32::new(0)));
        let q = CaptureQueue::with_chassis(chassis.clone());
        assert!(q.request(pending(1)));
        assert_eq!(chassis.0.load(Ordering::SeqCst), 1);
        // Second request fails (slot full) — chassis must not wake again.
        assert!(!q.request(pending(2)));
        assert_eq!(chassis.0.load(Ordering::SeqCst), 1);
        // Drain + re-request: chassis wakes again.
        q.take();
        assert!(q.request(pending(3)));
        assert_eq!(chassis.0.load(Ordering::SeqCst), 2);
    }
}
