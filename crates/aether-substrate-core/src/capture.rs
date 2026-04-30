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
//
// `begin_capture_request` and the `reply_unsupported_*` helpers shave
// ~50 lines off each chassis's control handler by collapsing the
// resolve-bundle / push / enqueue / wake workflow (and the repeated
// `XxxResult::Err { error: reason.to_owned() }` branches) into single
// calls. See issue 429.

use std::sync::{Arc, Mutex};

use aether_kinds::{
    AdvanceResult, CaptureFrame, CaptureFrameResult, PlatformInfoResult, SetWindowModeResult,
    SetWindowTitleResult,
};

use crate::control::{decode_payload, resolve_bundle};
use crate::hub_client::HubOutbound;
use crate::mail::{Mail, ReplyTo};
use crate::mailer::Mailer;
use crate::registry::Registry;

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

/// Run the full chassis-side capture-request workflow: decode the
/// `CaptureFrame` payload, resolve both mail bundles atomically against
/// the registry, push the pre-capture mails, install a `PendingCapture`
/// on `capture_queue`, and invoke `wake` to nudge the chassis loop.
///
/// On any failure (decode, resolve, queue full, wake-channel down) the
/// helper replies inline via `outbound` with `CaptureFrameResult::Err`
/// and rolls back the queue install if the wake step is what failed.
/// The caller doesn't see a `Result` — every error path is reported
/// through the same reply channel the happy path uses, matching the
/// rest of the control-plane shape.
///
/// The `wake` closure carries the chassis-specific bit. Desktop pokes
/// its `EventLoopProxy<UserEvent>` (which only fails if the loop has
/// shut down, and that case is a no-op anyway, so desktop returns
/// `Ok`). Test-bench sends on its `EventSender`; if the receiver has
/// been dropped (chassis shutting down), it returns the static reason
/// string this helper attaches to the rollback reply.
pub fn begin_capture_request(
    queue: &Mailer,
    capture_queue: &CaptureQueue,
    registry: &Registry,
    outbound: &HubOutbound,
    sender: ReplyTo,
    bytes: &[u8],
    wake: impl FnOnce() -> Result<(), &'static str>,
) {
    let payload: CaptureFrame = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &CaptureFrameResult::Err { error });
            return;
        }
    };

    // Phase 1: resolve every envelope in both bundles before pushing
    // anything or requesting a capture. Any failure aborts the whole
    // request so a partial dispatch can't leak into the next frame.
    let pre = match resolve_bundle(registry, &payload.mails, "capture bundle") {
        Ok(v) => v,
        Err(e) => {
            outbound.send_reply(sender, &CaptureFrameResult::Err { error: e });
            return;
        }
    };
    let after = match resolve_bundle(registry, &payload.after_mails, "capture after bundle") {
        Ok(v) => v,
        Err(e) => {
            outbound.send_reply(sender, &CaptureFrameResult::Err { error: e });
            return;
        }
    };

    // Phase 2: push resolved pre-mails, enqueue capture, wake loop.
    // The chassis loop's per-mailbox drain (ADR-0038 Phase 3) is what
    // enforces "capture after all mail processed".
    for mail in pre {
        queue.push(mail);
    }

    let pending = PendingCapture {
        reply_to: sender,
        after_mails: after,
    };
    if !capture_queue.request(pending) {
        outbound.send_reply(
            sender,
            &CaptureFrameResult::Err {
                error: "capture already pending; try again once the in-flight \
                    request completes"
                    .to_owned(),
            },
        );
        return;
    }

    if let Err(reason) = wake() {
        // Wake target is gone (chassis shutting down). Roll back the
        // queue install so a stray capture doesn't leak into the next
        // boot, and reply Err inline so the caller doesn't hang.
        let _ = capture_queue.take();
        outbound.send_reply(
            sender,
            &CaptureFrameResult::Err {
                error: reason.to_owned(),
            },
        );
    }
}

/// Reply `SetWindowModeResult::Err` with the given reason. Used by
/// chassis variants that don't own a window (headless, test-bench).
pub fn reply_unsupported_window_mode(outbound: &HubOutbound, sender: ReplyTo, reason: &str) {
    outbound.send_reply(
        sender,
        &SetWindowModeResult::Err {
            error: reason.to_owned(),
        },
    );
}

/// Reply `SetWindowTitleResult::Err` with the given reason. Used by
/// chassis variants that don't own a window (headless, test-bench).
pub fn reply_unsupported_window_title(outbound: &HubOutbound, sender: ReplyTo, reason: &str) {
    outbound.send_reply(
        sender,
        &SetWindowTitleResult::Err {
            error: reason.to_owned(),
        },
    );
}

/// Reply `PlatformInfoResult::Err` with the given reason. Used by
/// chassis variants that don't expose platform peripherals (headless,
/// test-bench).
pub fn reply_unsupported_platform_info(outbound: &HubOutbound, sender: ReplyTo, reason: &str) {
    outbound.send_reply(
        sender,
        &PlatformInfoResult::Err {
            error: reason.to_owned(),
        },
    );
}

/// Reply `AdvanceResult::Err` with the given reason. Used by chassis
/// variants that don't drive ticks via `aether.test_bench.advance`
/// (desktop, headless — only test-bench supports advance).
pub fn reply_unsupported_advance(outbound: &HubOutbound, sender: ReplyTo, reason: &str) {
    outbound.send_reply(
        sender,
        &AdvanceResult::Err {
            error: reason.to_owned(),
        },
    );
}

/// Reply `CaptureFrameResult::Err` with the given reason. Used by
/// chassis variants that have no GPU (headless).
pub fn reply_unsupported_capture_frame(outbound: &HubOutbound, sender: ReplyTo, reason: &str) {
    outbound.send_reply(
        sender,
        &CaptureFrameResult::Err {
            error: reason.to_owned(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReplyTarget;
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
