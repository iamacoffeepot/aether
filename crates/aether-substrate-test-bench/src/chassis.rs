//! Test-bench chassis-registered control-plane handler (ADR-0067).
//!
//! Mirrors desktop's `chassis_control_handler` for `capture_frame` —
//! resolves the pre/post mail bundles atomically, pushes the
//! pre-bundle, hands off a `PendingCapture` to the render thread
//! via `CaptureQueue`. Test-bench has no winit event loop, so there
//! is no `EventLoopProxy` hop — the std-timer tick loop polls the
//! capture queue every iteration.
//!
//! `set_window_mode`, `set_window_title`, and `platform_info` reply
//! `Err { error: "unsupported on test-bench chassis" }` — the
//! chassis is GPU-capable but has no window peripherals to address.
//! Same shape as headless's chassis handler for these kinds.

use std::sync::Arc;

use aether_kinds::{
    CaptureFrame, CaptureFrameResult, PlatformInfo, PlatformInfoResult, SetWindowMode,
    SetWindowModeResult, SetWindowTitle, SetWindowTitleResult,
};
use aether_mail::Kind;
use aether_substrate_core::{
    ChassisControlHandler, HubOutbound, Mailer, Registry, ReplyTo,
    control::{decode_payload, resolve_bundle},
};

use crate::capture::{CaptureQueue, PendingCapture};

const UNSUPPORTED_WINDOW: &str = "unsupported on test-bench chassis — no window peripherals (set_window_mode, set_window_title, \
     platform_info are desktop-only)";

pub fn chassis_control_handler(
    capture_queue: CaptureQueue,
    registry: Arc<Registry>,
    queue: Arc<Mailer>,
    outbound: Arc<HubOutbound>,
) -> ChassisControlHandler {
    Arc::new(
        move |kind_id: u64, kind_name: &str, sender: ReplyTo, bytes: &[u8]| {
            if kind_id == CaptureFrame::ID {
                handle_capture_frame(&capture_queue, &registry, &queue, &outbound, sender, bytes);
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
/// enqueue the capture, return. The tick loop polls the capture
/// queue each iteration; if a request is pending it calls
/// `render_and_capture`, pushes after_mails, and replies.
fn handle_capture_frame(
    capture_queue: &CaptureQueue,
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
    }
}
