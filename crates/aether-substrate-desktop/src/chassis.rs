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
    CaptureFrame, CaptureFrameResult, PlatformInfo, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, WindowMode,
};
use aether_mail::Kind;
use aether_substrate_core::{
    ChassisControlHandler, HubOutbound, Mailer, Registry, ReplyTo,
    control::{decode_payload, resolve_bundle},
};
use winit::event_loop::EventLoopProxy;

use crate::capture::{CaptureQueue, PendingCapture};

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
        move |kind_id: u64, kind_name: &str, sender: ReplyTo, bytes: &[u8]| {
            if kind_id == CaptureFrame::ID {
                handle_capture_frame(
                    &proxy,
                    &capture_queue,
                    &registry,
                    &queue,
                    &outbound,
                    sender,
                    bytes,
                );
            } else if kind_id == PlatformInfo::ID {
                // Empty payload; forward the sender straight to the
                // event loop and let it snapshot + reply on its own
                // thread (winit monitor / scale-factor APIs require it).
                let _ = proxy.send_event(UserEvent::PlatformInfo { reply_to: sender });
            } else if kind_id == SetWindowMode::ID {
                handle_set_window_mode(&proxy, &outbound, sender, bytes);
            } else if kind_id == SetWindowTitle::ID {
                handle_set_window_title(&proxy, &outbound, sender, bytes);
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

/// Two-phase capture request (pre-capture mail push + render handoff).
/// Ports the old `ControlPlane::handle_capture_frame` verbatim: resolve
/// both mail bundles atomically against the registry before touching
/// the queue, push pre-mails, enqueue the capture, poke the event loop.
/// Decode / resolve / queue-full errors reply inline; the render thread
/// fulfils the happy path on its next redraw.
fn handle_capture_frame(
    proxy: &EventLoopProxy<UserEvent>,
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
    // `queue.drain_all()` on the render thread is what enforces
    // "capture after all mail processed" (per-mailbox drain under
    // ADR-0038 Phase 3).
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
    // `send_event` only fails if the event loop has shut down; in
    // that case nothing listens for captures anyway.
    let _ = proxy.send_event(UserEvent::Capture);
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
