//! Desktop chassis: `DesktopChassis` implementing the core `Chassis`
//! trait (ADR-0035), plus the chassis-registered control-plane
//! handler that owns the three desktop-only kinds (`capture_frame`,
//! `set_window_mode`, `platform_info`). Core's dispatch covers
//! load/drop/replace/subscribe/unsubscribe only; anything else falls
//! through to the `chassis_control_handler` closure this module
//! builds.
//!
//! The handler runs on a scheduler worker (same thread as every
//! other sink handler), so the two operations that need winit/wgpu
//! access forward to the event-loop thread via
//! `EventLoopProxy<UserEvent>`. `capture_frame` orchestrates its own
//! mail envelopes (pre-capture bundle push + after-capture bundle
//! resolution) and routes through `CaptureQueue` to hand off to the
//! render thread.

use std::sync::Arc;

use aether_kinds::{
    Advance, CaptureFrame, PlatformInfo, SetWindowMode, SetWindowModeResult, SetWindowTitle,
    SetWindowTitleResult, WindowMode,
};
use aether_mail::Kind;
use aether_substrate_core::{
    ChassisControlHandler, HubOutbound, Mailer, Registry, ReplyTo,
    capture::{CaptureQueue, begin_capture_request, reply_unsupported_advance},
    control::decode_payload,
};
use winit::event_loop::EventLoopProxy;

/// Event the event-loop thread consumes from the desktop chassis.
/// Either a chassis-originated request for work that needs winit/wgpu
/// context (platform info, window mode, capture) or a wake-up so the
/// loop picks up a queued capture on the next redraw.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// A capture was just enqueued on `CaptureQueue`; wake the loop
    /// so `RedrawRequested` pulls and fulfils it, even under
    /// `ControlFlow::Wait` when the window is occluded.
    Capture,
    /// An MCP session asked for a `platform_info` snapshot. The
    /// event-loop thread snapshots + replies via outbound.
    PlatformInfo { reply_to: ReplyTo },
    /// An MCP session asked to switch the window mode. The event
    /// loop resolves fullscreen modes against the current monitor,
    /// applies the change, and replies with the new state.
    SetWindowMode {
        reply_to: ReplyTo,
        mode: WindowMode,
        width: Option<u32>,
        height: Option<u32>,
    },
    /// An MCP session asked to update the window title. The event
    /// loop calls `Window::set_title` and echoes the applied title
    /// back on the reply. A missing window (before `resumed`) replies
    /// with an `Err`.
    SetWindowTitle { reply_to: ReplyTo, title: String },
}

/// Build the `ChassisControlHandler` closure desktop installs on
/// `ControlPlane::chassis_handler`. Captures the handles each
/// chassis-specific kind needs: the event-loop proxy for hand-off to
/// winit/wgpu context; the capture queue for render-thread handoff;
/// the registry + queue for capture_frame's mail-bundle orchestration;
/// the outbound handle for inline error replies.
pub fn chassis_control_handler(
    proxy: EventLoopProxy<UserEvent>,
    capture_queue: CaptureQueue,
    registry: Arc<Registry>,
    queue: Arc<Mailer>,
    outbound: Arc<HubOutbound>,
) -> ChassisControlHandler {
    Arc::new(
        move |kind: aether_mail::KindId, kind_name: &str, sender: ReplyTo, bytes: &[u8]| {
            if kind == aether_mail::KindId(CaptureFrame::ID) {
                let proxy = proxy.clone();
                begin_capture_request(
                    &queue,
                    &capture_queue,
                    &registry,
                    &outbound,
                    sender,
                    bytes,
                    move || {
                        // `send_event` only fails if the event loop
                        // has shut down; in that case nothing listens
                        // for captures anyway, so swallow the error
                        // and let the queued capture sit until exit.
                        let _ = proxy.send_event(UserEvent::Capture);
                        Ok(())
                    },
                );
            } else if kind == aether_mail::KindId(PlatformInfo::ID) {
                // Empty payload; forward the sender straight to the
                // event loop and let it snapshot + reply on its own
                // thread (winit monitor / scale-factor APIs require it).
                let _ = proxy.send_event(UserEvent::PlatformInfo { reply_to: sender });
            } else if kind == aether_mail::KindId(SetWindowMode::ID) {
                handle_set_window_mode(&proxy, &outbound, sender, bytes);
            } else if kind == aether_mail::KindId(SetWindowTitle::ID) {
                handle_set_window_title(&proxy, &outbound, sender, bytes);
            } else if kind == aether_mail::KindId(Advance::ID) {
                reply_unsupported_advance(
                    &outbound,
                    sender,
                    "unsupported on desktop chassis — aether.test_bench.advance is \
                     test-bench-only (ADR-0067)",
                );
            } else {
                tracing::warn!(
                    target: "aether_substrate::chassis",
                    kind = %kind_name,
                    "desktop chassis has no handler for control kind — dropping",
                );
            }
        },
    )
}

/// Decode + forward to the event loop. Applying the mode requires
/// winit APIs that only live on the main thread, so this handler
/// doesn't reply inline on the happy path — the event loop does.
fn handle_set_window_mode(
    proxy: &EventLoopProxy<UserEvent>,
    outbound: &HubOutbound,
    sender: ReplyTo,
    bytes: &[u8],
) {
    let payload: SetWindowMode = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &SetWindowModeResult::Err { error });
            return;
        }
    };
    let _ = proxy.send_event(UserEvent::SetWindowMode {
        reply_to: sender,
        mode: payload.mode,
        width: payload.width,
        height: payload.height,
    });
}

/// Decode + forward to the event loop. `Window::set_title` needs to
/// run on the main thread on every winit platform, so the same
/// event-loop proxy hand-off `set_window_mode` uses.
fn handle_set_window_title(
    proxy: &EventLoopProxy<UserEvent>,
    outbound: &HubOutbound,
    sender: ReplyTo,
    bytes: &[u8],
) {
    let payload: SetWindowTitle = match decode_payload(bytes) {
        Ok(p) => p,
        Err(error) => {
            outbound.send_reply(sender, &SetWindowTitleResult::Err { error });
            return;
        }
    };
    let _ = proxy.send_event(UserEvent::SetWindowTitle {
        reply_to: sender,
        title: payload.title,
    });
}
