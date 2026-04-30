//! Test-bench chassis-registered control-plane handler (ADR-0067).
//!
//! Owns three custom kinds:
//!
//! - `aether.control.capture_frame` — same two-phase resolve / push
//!   / handoff desktop uses, but the handoff target is the chassis
//!   event channel (not a winit `EventLoopProxy`). The `PendingCapture`
//!   itself rides in `CaptureQueue`; the event channel just signals
//!   the loop to wake up.
//! - `aether.test_bench.advance` — pushes an `Advance` event onto the
//!   chassis event channel. The loop runs N ticks (Tick fanout →
//!   drain → render or render-with-capture) and replies once they
//!   complete.
//! - `set_window_mode` / `set_window_title` / `platform_info` —
//!   reply `Err` with an "unsupported on test-bench chassis" message.
//!   Same fail-fast shape headless uses on these.

use std::sync::Arc;

use aether_kinds::{
    Advance, AdvanceResult, CaptureFrame, CaptureFrameResult, PlatformInfo, PlatformInfoResult,
    SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult,
};
use aether_mail::Kind;
use aether_substrate_core::{
    ChassisControlHandler, HubOutbound, Mailer, Registry, ReplyTo,
    capture::{CaptureQueue, PendingCapture},
    control::{decode_payload, resolve_bundle},
};

use crate::events::{ChassisEvent, EventSender};

const UNSUPPORTED_WINDOW: &str = "unsupported on test-bench chassis — no window peripherals (set_window_mode, set_window_title, \
     platform_info are desktop-only)";

pub fn chassis_control_handler(
    capture_queue: CaptureQueue,
    events: EventSender,
    registry: Arc<Registry>,
    queue: Arc<Mailer>,
    outbound: Arc<HubOutbound>,
) -> ChassisControlHandler {
    Arc::new(
        move |kind_id: u64, kind_name: &str, sender: ReplyTo, bytes: &[u8]| {
            if kind_id == CaptureFrame::ID {
                handle_capture_frame(
                    &capture_queue,
                    &events,
                    &registry,
                    &queue,
                    &outbound,
                    sender,
                    bytes,
                );
            } else if kind_id == Advance::ID {
                handle_advance(&events, &outbound, sender, bytes);
            } else if kind_id == SetWindowMode::ID {
                outbound.send_reply(
                    sender,
                    &SetWindowModeResult::Err {
                        error: UNSUPPORTED_WINDOW.to_owned(),
                    },
                );
            } else if kind_id == SetWindowTitle::ID {
                outbound.send_reply(
                    sender,
                    &SetWindowTitleResult::Err {
                        error: UNSUPPORTED_WINDOW.to_owned(),
                    },
                );
            } else if kind_id == PlatformInfo::ID {
                outbound.send_reply(
                    sender,
                    &PlatformInfoResult::Err {
                        error: UNSUPPORTED_WINDOW.to_owned(),
                    },
                );
            } else {
                tracing::warn!(
                    target: "aether_substrate::chassis",
                    kind = %kind_name,
                    "test-bench chassis has no handler for control kind — dropping",
                );
            }
        },
    )
}

/// Two-phase capture request. Resolve both mail bundles atomically
/// against the registry before touching the queue, push pre-mails,
/// enqueue the capture, signal the loop. The loop drains and renders-
/// with-capture (no Tick fanout — capture observes; advance ticks).
fn handle_capture_frame(
    capture_queue: &CaptureQueue,
    events: &EventSender,
    registry: &Registry,
    queue: &Mailer,
    outbound: &HubOutbound,
    sender: ReplyTo,
    bytes: &[u8],
) {
    let payload: CaptureFrame = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &CaptureFrameResult::Err { error });
            return;
        }
    };

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
                error: "capture already pending; try again once the in-flight request completes"
                    .to_owned(),
            },
        );
        return;
    }

    if events.send(ChassisEvent::CaptureRequested).is_err() {
        // The tick loop has dropped its receiver — chassis is
        // shutting down. The capture is queued but won't be drained.
        // Reply Err so the caller doesn't hang.
        let _ = capture_queue.take();
        outbound.send_reply(
            sender,
            &CaptureFrameResult::Err {
                error: "test-bench chassis shutting down — capture aborted".to_owned(),
            },
        );
    }
}

/// Decode `Advance { ticks }`, push the request onto the event
/// channel. The tick loop runs `ticks` cycles and replies. A
/// shut-down loop replies `Err` inline.
fn handle_advance(events: &EventSender, outbound: &HubOutbound, sender: ReplyTo, bytes: &[u8]) {
    let payload: Advance = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &AdvanceResult::Err { error });
            return;
        }
    };

    if events
        .send(ChassisEvent::Advance {
            reply_to: sender,
            ticks: payload.ticks,
        })
        .is_err()
    {
        outbound.send_reply(
            sender,
            &AdvanceResult::Err {
                error: "test-bench chassis shutting down — advance aborted".to_owned(),
            },
        );
    }
}
