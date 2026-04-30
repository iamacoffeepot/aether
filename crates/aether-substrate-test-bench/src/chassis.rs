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
    Advance, AdvanceResult, CaptureFrame, PlatformInfo, SetWindowMode, SetWindowTitle,
};
use aether_mail::{Kind, KindId};
use aether_substrate_core::{
    ChassisControlHandler, HubOutbound, Mailer, Registry, ReplyTo,
    capture::{
        CaptureQueue, begin_capture_request, reply_unsupported_platform_info,
        reply_unsupported_window_mode, reply_unsupported_window_title,
    },
    control::decode_payload,
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
        move |kind: KindId, kind_name: &str, sender: ReplyTo, bytes: &[u8]| {
            if kind == KindId(CaptureFrame::ID) {
                let events = events.clone();
                begin_capture_request(
                    &queue,
                    &capture_queue,
                    &registry,
                    &outbound,
                    sender,
                    bytes,
                    move || {
                        events
                            .send(ChassisEvent::CaptureRequested)
                            .map_err(|_| "test-bench chassis shutting down — capture aborted")
                    },
                );
            } else if kind == KindId(Advance::ID) {
                handle_advance(&events, &outbound, sender, bytes);
            } else if kind == KindId(SetWindowMode::ID) {
                reply_unsupported_window_mode(&outbound, sender, UNSUPPORTED_WINDOW);
            } else if kind == KindId(SetWindowTitle::ID) {
                reply_unsupported_window_title(&outbound, sender, UNSUPPORTED_WINDOW);
            } else if kind == KindId(PlatformInfo::ID) {
                reply_unsupported_platform_info(&outbound, sender, UNSUPPORTED_WINDOW);
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
