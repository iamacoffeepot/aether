//! Desktop chassis driver capability — ADR-0071 phase 3.
//!
//! Holds the winit `App` struct, the `ApplicationHandler` impl that
//! drives per-frame work, the small bag of winit/wgpu mapping helpers
//! the chassis needs to read its own state, and the
//! `AETHER_WINDOW_MODE` parser. Wraps everything in a
//! `DesktopDriverCapability` so `crate::chassis::DesktopChassis`
//! composes one driver alongside its passive capabilities
//! (`LogCapability`, `FsCapability`, `HttpCapability`, `AudioCapability`,
//! `RenderCapability` — composed via `chassis_builder::Builder::with_actor`
//! per ADR-0071 phase B).
//!
//! `DesktopDriverRunning::run` blocks on `event_loop.run_app(&mut app)`
//! and emits the shutdown telemetry the previous `DesktopChassis::run`
//! body owned. Returning means the user closed the window or the
//! event loop exited cleanly; the `chassis_builder` then tears down
//! every passive in reverse boot order via `BootedPassives::Drop`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use aether_actor::Actor;
use aether_actor::local;
use aether_capabilities::InputCapability;
use aether_capabilities::RenderHandles;
use aether_data::Kind;
use aether_data::{encode, encode_empty, mailbox_id_from_name};
use aether_kinds::{
    CaptureFrameResult, FocusWindow, FocusWindowResult, Key, KeyRelease, LifecycleAdvanceComplete,
    MouseButton, MouseMove, Quit, SetWindowMode, SetWindowModeResult, SetWindowTitle,
    SetWindowTitleResult, Tick, WindowMode, WindowSize, keycode,
};
use aether_substrate::actor::native::envelope::Envelope;
use aether_substrate::actor::native::{
    dispatch_cost_tail_if_matching_free, dispatch_log_tail_if_matching_free,
    dispatch_trace_tail_if_matching_free,
};
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::settlement::{
    TerminalDisposition, WaitOutcome, await_internal_signal,
};
use aether_substrate::runtime::lifecycle;
use aether_substrate::{
    HubOutbound, Mailer, ReplyTarget, ReplyTo, SharedActorSlots, SubstrateBoot,
    chassis::frame_loop,
    mail::{Mail, MailId, MailboxId},
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::monitor::{MonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window, WindowId};

use super::chassis::UserEvent;
use super::render::Gpu;
use aether_substrate::capture::{CaptureQueue, PendingCapture};
use std::io;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use winit::dpi::PhysicalSize;

/// Cumulative patience cap for the per-frame settlement gates (advance +
/// capture pre-mail). The per-round budget is `frame_loop::DRAIN_BUDGET`
/// (the log cadence); a starved-but-healthy chain resolves before this
/// cap, a genuine wedge exhausts it (issue #1305).
const FRAME_SETTLEMENT_CAP: Duration = Duration::from_secs(30);

pub struct App {
    queue: Arc<Mailer>,
    /// `aether.input` mailbox id, cached at driver boot. Each platform
    /// event fans through a single mail push to this mailbox; the
    /// `InputCapability` actor owns the subscriber table and fans
    /// out per-subscriber on its own dispatcher (issue 640).
    input_mailbox: MailboxId,
    /// `aether.lifecycle` mailbox id, cached at boot. Each redraw
    /// fires one `LifecycleAdvance` here; the driver broadcasts
    /// Tick to `aether.input` via the chassis's `initial_subscribers`
    /// relay, then waits for settlement before submitting the frame.
    lifecycle_mailbox: MailboxId,
    kind_lifecycle_advance: aether_data::KindId,
    /// `aether.lifecycle.advance_reply` inbox claimed at boot (issue
    /// 1378). The per-frame `Tick → Render` cycle pushes each
    /// `LifecycleAdvance` with this mailbox as its `Component` reply
    /// target, then synchronously drains the receiver for the cap's
    /// `LifecycleAdvanceComplete` reply. The reply is emitted only after
    /// the cap clears its pending-advance guard (ADR-0082 §6), so gating
    /// the next advance on it — rather than on the raw settlement channel
    /// — keeps the back-to-back advances from racing the cap's overlap
    /// guard (the same reply-gate the test-bench frame loop uses,
    /// iamacoffeepot/aether#999).
    lifecycle_reply_inbox: Receiver<Envelope>,
    /// Mailbox id of [`Self::lifecycle_reply_inbox`], used as the
    /// `Component` reply target stamped onto each `LifecycleAdvance`.
    lifecycle_reply_mailbox: MailboxId,
    kind_key: aether_data::KindId,
    kind_key_release: aether_data::KindId,
    kind_mouse_button: aether_data::KindId,
    kind_mouse_move: aether_data::KindId,
    kind_window_size: aether_data::KindId,
    /// Cloned out of `RenderCapability::handles()` before the cap
    /// moves into the chassis builder. The app holds a clone so
    /// `Gpu::new` can install wgpu state and the per-frame loop can
    /// call `record_frame` / `record_capture_copy` / `finish_capture`.
    render_handles: RenderHandles,
    /// Shared single-slot queue with the control plane. On each
    /// redraw we `take()` any pending capture and, if present, use
    /// `render_and_capture`, then reply to the sender via
    /// `queue.send_reply` (the `Mailer`, which routes every
    /// `ReplyTarget` — see `outbound`).
    capture_queue: CaptureQueue,
    /// Hub outbound — held for log egress to the hub and
    /// `lifecycle::fatal_abort`. NOT used for chassis replies:
    /// `HubOutbound::send_reply` only routes `Session` / `EngineMailbox`
    /// targets and silently drops `ReplyTarget::Component`, but mail
    /// dispatched by this engine's own `RpcServerCapability` (every
    /// hub/MCP call lands via the proxy → local RPC server) carries a
    /// `Component(rpc_server)` reply target. Replies go through
    /// `self.queue.send_reply` (the `Mailer`) instead, which pushes the
    /// reply as local mail so the RPC server's `on_any` lifts it into a
    /// `ReplyEvent` (iamacoffeepot/aether#1316).
    outbound: Arc<HubOutbound>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    pub(crate) started: Option<Instant>,
    pub(crate) frame: u64,
    occluded: bool,
    /// Initial window mode, parsed from `AETHER_WINDOW_MODE` at boot
    /// and applied when `resumed` creates the window. Kept so the
    /// window attributes can reference it even when `resumed` fires
    /// lazily (and for logging).
    boot_mode: WindowMode,
    /// Optional initial windowed size from `AETHER_WINDOW_MODE`.
    /// Only consulted when `boot_mode == Windowed`.
    boot_size: Option<(u32, u32)>,
    /// Initial window title, parsed from `AETHER_WINDOW_TITLE` at
    /// boot and applied when `resumed` creates the window. Runtime
    /// `set_window_title` mail overrides this but doesn't update the
    /// field — the current title lives on the `Window` itself.
    boot_title: String,
    /// Currently-applied window mode. Updated by `set_window_mode`
    /// and read by `platform_info`'s window-state field. Starts as
    /// `boot_mode`.
    current_mode: WindowMode,
    /// `aether.window` inbox claimed via `DriverCtx::claim_mailbox`
    /// at boot (issue 603 Phase 3). The driver is the cap — drained
    /// inside [`ApplicationHandler::about_to_wait`] between frames to
    /// apply `SetWindowMode` / `SetWindowTitle` / `FocusWindow` inline
    /// on the chassis main thread (winit / macOS require window
    /// mutations there). No dispatcher thread; the receiver is the
    /// drain source. Mail arrival pokes `UserEvent::WindowMail` via the
    /// claim's wake slot (iamacoffeepot/aether#1318), so `about_to_wait`
    /// runs and drains even under `ControlFlow::Wait` (set when the
    /// window occludes) — the case `aether.window.focus` most needs,
    /// since the loop is otherwise parked until a winit event arrives.
    window_inbox: Receiver<Envelope>,
    /// Per-actor [`aether_actor::local::ActorSlots`] carried out of the
    /// [`aether_substrate::MailboxClaim`] this driver produced at boot.
    /// Stamped into TLS via [`aether_actor::local::with_stamped`] around
    /// the bespoke `aether.window` inbox drain so framework-built-in
    /// dispatch arms (`aether.log.tail` / `aether.trace.tail` /
    /// `aether.cost.tail`) reach the driver's per-actor `Local<T>`
    /// rings — the same shape the standard
    /// `DispatcherSlot::run_cycle` path opens for every other actor
    /// (iamacoffeepot/aether#1272).
    actor_slots: SharedActorSlots,
    /// The driver's own mailbox id (`aether.window` claim). Threaded
    /// through to the cost-tail dispatch arm, which filters the global
    /// cost table by `self_mailbox` (the standard variant pulls this
    /// from `NativeBinding::self_mailbox`; driver-as-actor has no
    /// binding, so we cache the id directly).
    window_mailbox: MailboxId,
    kind_set_window_mode: aether_data::KindId,
    kind_set_window_title: aether_data::KindId,
    /// `aether.window.focus` kind id, resolved at boot. The dispatch
    /// arm calls [`App::apply_window_focus`] to raise the window
    /// (iamacoffeepot/aether#1318).
    kind_focus_window: aether_data::KindId,
    /// ADR-0080 §6 chassis-root correlation counter (issue
    /// iamacoffeepot/aether#723). Bumped per chassis-source push so
    /// every input/window/frame-stats emission carries a fresh
    /// `MailId` for the trace observer to root a tree on. Symmetric
    /// with the per-actor counter on `NativeBinding`.
    chassis_correlation: AtomicU64,
    /// True once a graceful-shutdown `Quit` has been pushed to
    /// `aether.lifecycle` (iamacoffeepot/aether#1489), via either
    /// `WindowEvent::CloseRequested` or an observed SIGINT/SIGTERM.
    /// Guards [`App::request_quit`] so the `Quit` mail is pushed exactly
    /// once, and bypasses the `RedrawRequested` occlusion early-return so
    /// the shutdown frame still drives the lifecycle to its `Shutdown`
    /// terminal even on a minimized/hidden window.
    quit_requested: bool,
    /// SIGINT/SIGTERM shutdown flag, flipped by the signal-watcher
    /// installed in [`DesktopDriverCapability::boot`]
    /// (iamacoffeepot/aether#1489). Polled at the top of
    /// [`ApplicationHandler::about_to_wait`]; on first observation the
    /// driver runs the same `Quit`-push path as `CloseRequested`. A
    /// struct field (mirroring headless's flag) so the watcher and the
    /// winit loop share one source of truth.
    shutdown: Arc<AtomicBool>,
}

/// Translate a winit `KeyCode` into the engine's stable named-key u32
/// space (`aether_kinds::keycode`). Returns `None` for any key the
/// engine doesn't name yet — the event then drops at the source rather
/// than leaking winit's unstable discriminants onto the wire. Adding
/// a new key is a paired change: a constant in `aether-kinds::keycode`
/// plus an arm here.
fn map_winit_keycode(k: KeyCode) -> Option<u32> {
    Some(match k {
        KeyCode::KeyA => keycode::KEY_A,
        KeyCode::KeyB => keycode::KEY_B,
        KeyCode::KeyC => keycode::KEY_C,
        KeyCode::KeyD => keycode::KEY_D,
        KeyCode::KeyE => keycode::KEY_E,
        KeyCode::KeyF => keycode::KEY_F,
        KeyCode::KeyG => keycode::KEY_G,
        KeyCode::KeyH => keycode::KEY_H,
        KeyCode::KeyI => keycode::KEY_I,
        KeyCode::KeyJ => keycode::KEY_J,
        KeyCode::KeyK => keycode::KEY_K,
        KeyCode::KeyL => keycode::KEY_L,
        KeyCode::KeyM => keycode::KEY_M,
        KeyCode::KeyN => keycode::KEY_N,
        KeyCode::KeyO => keycode::KEY_O,
        KeyCode::KeyP => keycode::KEY_P,
        KeyCode::KeyQ => keycode::KEY_Q,
        KeyCode::KeyR => keycode::KEY_R,
        KeyCode::KeyS => keycode::KEY_S,
        KeyCode::KeyT => keycode::KEY_T,
        KeyCode::KeyU => keycode::KEY_U,
        KeyCode::KeyV => keycode::KEY_V,
        KeyCode::KeyW => keycode::KEY_W,
        KeyCode::KeyX => keycode::KEY_X,
        KeyCode::KeyY => keycode::KEY_Y,
        KeyCode::KeyZ => keycode::KEY_Z,
        KeyCode::Digit0 => keycode::KEY_0,
        KeyCode::Digit1 => keycode::KEY_1,
        KeyCode::Digit2 => keycode::KEY_2,
        KeyCode::Digit3 => keycode::KEY_3,
        KeyCode::Digit4 => keycode::KEY_4,
        KeyCode::Digit5 => keycode::KEY_5,
        KeyCode::Digit6 => keycode::KEY_6,
        KeyCode::Digit7 => keycode::KEY_7,
        KeyCode::Digit8 => keycode::KEY_8,
        KeyCode::Digit9 => keycode::KEY_9,
        KeyCode::Space => keycode::KEY_SPACE,
        KeyCode::Escape => keycode::KEY_ESCAPE,
        KeyCode::Enter => keycode::KEY_ENTER,
        KeyCode::Tab => keycode::KEY_TAB,
        KeyCode::Backspace => keycode::KEY_BACKSPACE,
        KeyCode::ArrowLeft => keycode::KEY_LEFT,
        KeyCode::ArrowRight => keycode::KEY_RIGHT,
        KeyCode::ArrowUp => keycode::KEY_UP,
        KeyCode::ArrowDown => keycode::KEY_DOWN,
        KeyCode::ShiftLeft => keycode::KEY_SHIFT_LEFT,
        KeyCode::ShiftRight => keycode::KEY_SHIFT_RIGHT,
        KeyCode::ControlLeft => keycode::KEY_CTRL_LEFT,
        KeyCode::ControlRight => keycode::KEY_CTRL_RIGHT,
        KeyCode::AltLeft => keycode::KEY_ALT_LEFT,
        KeyCode::AltRight => keycode::KEY_ALT_RIGHT,
        _ => return None,
    })
}

/// Parse `AETHER_WINDOW_MODE`. Grammar:
///   `windowed`              — default size
///   `windowed:WxH`          — windowed, `WxH` physical pixels
///   `fullscreen-borderless` — borderless on current monitor
///   `exclusive:WxH@HZ`      — exclusive, matched against monitor modes
/// Refresh is integer Hz (converted to mhz by *1000); non-integer
/// refresh isn't expressible from the env var today — runtime
/// `set_window_mode` accepts full-precision mhz directly.
pub fn parse_window_mode_env(s: &str) -> Result<(WindowMode, Option<(u32, u32)>), String> {
    let s = s.trim();
    if s == "windowed" {
        return Ok((WindowMode::Windowed, None));
    }
    if let Some(rest) = s.strip_prefix("windowed:") {
        let (w, h) = parse_wxh(rest)?;
        return Ok((WindowMode::Windowed, Some((w, h))));
    }
    if s == "fullscreen-borderless" {
        return Ok((WindowMode::FullscreenBorderless, None));
    }
    if let Some(rest) = s.strip_prefix("exclusive:") {
        let (dim, hz) = rest
            .split_once('@')
            .ok_or_else(|| format!("exclusive mode missing @HZ in {s:?}"))?;
        let (width, height) = parse_wxh(dim)?;
        let hz: u32 = hz.parse().map_err(|e| format!("invalid Hz {hz:?}: {e}"))?;
        return Ok((
            WindowMode::FullscreenExclusive {
                width,
                height,
                refresh_mhz: hz.saturating_mul(1000),
            },
            None,
        ));
    }
    Err(format!("unrecognised AETHER_WINDOW_MODE value {s:?}"))
}

fn parse_wxh(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("expected WxH, got {s:?}"))?;
    let w: u32 = w.parse().map_err(|e| format!("invalid width {w:?}: {e}"))?;
    let h: u32 = h
        .parse()
        .map_err(|e| format!("invalid height {h:?}: {e}"))?;
    Ok((w, h))
}

/// Find a `VideoModeHandle` on `monitor` matching the given size +
/// refresh exactly. Returns `None` if no match — the caller surfaces
/// this as `SetWindowModeResult::Err` rather than falling back
/// silently to something close.
fn find_exclusive_mode(
    monitor: &MonitorHandle,
    width: u32,
    height: u32,
    refresh_mhz: u32,
) -> Option<VideoModeHandle> {
    monitor.video_modes().find(|m| {
        m.size().width == width
            && m.size().height == height
            && m.refresh_rate_millihertz() == refresh_mhz
    })
}

/// Build winit's `Option<Fullscreen>` for the requested mode.
/// `monitor_for_exclusive` is the monitor to match video modes
/// against — the window's current monitor at runtime, the primary at
/// boot.
fn resolve_fullscreen(
    mode: &WindowMode,
    monitor_for_exclusive: Option<&MonitorHandle>,
) -> Result<Option<Fullscreen>, String> {
    match mode {
        WindowMode::Windowed => Ok(None),
        WindowMode::FullscreenBorderless => Ok(Some(Fullscreen::Borderless(None))),
        WindowMode::FullscreenExclusive {
            width,
            height,
            refresh_mhz,
        } => {
            let monitor = monitor_for_exclusive.ok_or_else(|| {
                "fullscreen-exclusive requested but no monitor available".to_owned()
            })?;
            let handle =
                find_exclusive_mode(monitor, *width, *height, *refresh_mhz).ok_or_else(|| {
                    format!(
                        "no video mode matches {width}x{height}@{refresh_mhz}mhz on monitor {:?}",
                        monitor.name()
                    )
                })?;
            Ok(Some(Fullscreen::Exclusive(handle)))
        }
    }
}

/// iamacoffeepot/aether#1272: route an inbound `aether.window` envelope
/// through the framework-built-in dispatch arms (`aether.log.tail` /
/// `aether.trace.tail` / `aether.cost.tail`) before the driver-specific
/// `SetWindowMode` / `SetWindowTitle` arms get their turn. Returns
/// `true` when one of the framework arms matched (a reply has already
/// been routed); `false` otherwise. ADR-0081 §1 promises every mailbox
/// serves these kinds — see the issue body for the contract.
///
/// Caller invariant: must run inside a `local::with_stamped` block
/// against the driver's [`aether_actor::local::ActorSlots`] so the
/// log / trace arms reach the driver's per-actor ring. Factored out of
/// [`App::dispatch_window_envelope`] so the unit test directly drives
/// the routing shape without standing up a winit `App`.
fn try_framework_dispatch(mailer: &Arc<Mailer>, self_mailbox: MailboxId, env: &Envelope) -> bool {
    let m = mailer.as_ref();
    dispatch_log_tail_if_matching_free(m, env.sender, env)
        || dispatch_trace_tail_if_matching_free(m, env.sender, env)
        || dispatch_cost_tail_if_matching_free(m, env.sender, self_mailbox, env)
}

/// Discharge the ADR-0080 §2 settlement bracket for one inbound
/// `aether.window` envelope. `aether.window` is an `Inbox`
/// (actor-enqueue) mailbox: the mailer records *no* settlement bracket
/// on the producer side (`mail/mailer.rs` `Inbox` arm), so the
/// `InboxHandler` contract (`mail/registry.rs`, ADR-0080 §2) puts the
/// obligation on the downstream consumer — this hand-rolled window
/// drain. Without it the inbound Call's root `in_flight` never reaches
/// zero, no `Settled` fires, no wire `ReplyEnd` is emitted, and a
/// blocking `send_mail` to `set_mode` / `set_title` / `focus` hangs
/// (iamacoffeepot/aether#1325, a recurrence of the #846 dropped-bracket
/// class).
///
/// Mirrors the bracket template in
/// [`DispatcherSlot::dispatch_one`](aether_substrate::actor::native)
/// (`actor/native/dispatcher_slot.rs:289`): `record_finished` after the
/// reply, so the reply child's `Sent` is accounted before the inbound
/// parent's `Finished` (the #1150 flush-before-Finished ordering).
///
/// Deliberately **settlement-only**: it discharges `in_flight` via
/// `record_finished` but does not additionally push `Received` /
/// `Finished` `TraceEvent`s into the driver's per-actor ring the way
/// `dispatch_one` does. The bug is a settlement leak, not a
/// trace-visibility gap; full trace-event emission is a separable
/// trace-fidelity change the minimal fix does not need.
///
/// Early-returns on `mail_id == MailId::NONE` for legible intent —
/// `record_finished` also no-ops on `NONE`, so this is belt-and-braces
/// for the chassis-internal window-size / frame-stats pushes minted with
/// `MailId::NONE` roots via `push_chassis_root`.
fn discharge_settlement(mailer: &Mailer, mail_id: MailId, root: MailId) {
    if mail_id == MailId::NONE {
        return;
    }
    mailer.record_finished(mail_id, root);
}

/// The disposition of a `capture_frame` wake against the current window
/// occlusion state — the pure branch-selection half of the occluded
/// fail-fast (iamacoffeepot/aether#1317), factored out of the winit
/// wiring so it is unit-testable without standing up a winit `App`
/// (mirroring [`try_framework_dispatch`]).
enum OccludedCaptureDisposition {
    /// Window is visible — the caller falls through to `request_redraw`
    /// so `RedrawRequested` services the (still-parked) capture normally.
    Redraw,
    /// Window is occluded but no capture was parked (already serviced, or
    /// a stale wake) — nothing to do.
    Empty,
    /// Window is occluded and a capture is parked — fail it fast. Carries
    /// the taken [`PendingCapture`] (so the caller drains `after_mails`,
    /// replies, then drops it to release the settlement hold) and the
    /// `Err` reply to send.
    FailFast {
        request: PendingCapture,
        result: CaptureFrameResult,
    },
}

/// The capture-frame error message naming `aether.window.focus` as the
/// remedy. Shared by every occluded fail-fast site so the wording can't
/// drift.
const OCCLUDED_CAPTURE_ERROR: &str = "capture_frame: window is occluded (hidden/minimized); bring it to the \
     foreground via aether.window.focus and retry";

/// Select the disposition for a `capture_frame` wake (or an occlusion
/// onset) given the window's occlusion state and any parked capture.
///
/// The winit side only `take()`s the [`CaptureQueue`] slot when occluded,
/// so a visible-window wake never steals the entry that `RedrawRequested`
/// is about to service — the `Redraw` arm carries no `PendingCapture`. The
/// occluded arms move the taken request through so the caller can drain
/// `after_mails`, reply via the `Mailer`, and drop the request *after* the
/// reply (releasing the ADR-0086 §12 settlement hold, iamacoffeepot/aether#1273).
fn occluded_capture_disposition(
    occluded: bool,
    pending: Option<PendingCapture>,
) -> OccludedCaptureDisposition {
    if !occluded {
        return OccludedCaptureDisposition::Redraw;
    }
    pending.map_or(OccludedCaptureDisposition::Empty, |request| {
        OccludedCaptureDisposition::FailFast {
            request,
            result: CaptureFrameResult::Err {
                error: OCCLUDED_CAPTURE_ERROR.to_owned(),
            },
        }
    })
}

/// Install a SIGINT/SIGTERM → graceful-shutdown bridge for the desktop
/// chassis (iamacoffeepot/aether#1489). On the first delivered signal it
/// flips `shutdown` (the flag [`App::about_to_wait`] polls) and sends
/// [`UserEvent::Quit`] through `proxy` to wake a parked
/// (`ControlFlow::Wait`, occluded) loop — the loop then runs
/// `about_to_wait`, observes the flag, and drives the lifecycle to
/// `Shutdown` (the desktop analogue of headless's tick-loop flag poll).
///
/// Unlike headless's `signal_hook::flag::register` — which is
/// async-signal-safe but can only flip a bool — the desktop loop must be
/// *woken*, and `EventLoopProxy::send_event` is not async-signal-safe.
/// So a dedicated watcher thread blocks on the signal stream and does
/// both the flag flip and the proxy wake; it doesn't freeze the winit
/// loop (a separate thread), so the constraint that ruled this out for
/// the single-threaded headless tick loop doesn't apply here. SIGTERM
/// joins SIGINT so supervisors / `kill` (no `-9`) / CI cancellation also
/// run teardown. Best-effort: a failed install warn-logs and leaves
/// shutdown to `WindowEvent::CloseRequested` only.
#[cfg(unix)]
fn install_shutdown_handler(shutdown: &Arc<AtomicBool>, proxy: EventLoopProxy<UserEvent>) {
    use std::thread;

    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let shutdown = Arc::clone(shutdown);
    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::boot",
                error = %e,
                "desktop: shutdown signal handler install failed; \
                 only window-close will trigger graceful shutdown",
            );
            return;
        }
    };
    // Infra thread: it blocks on the OS signal stream and holds no
    // settlement/trace contract — the work it triggers (the `Quit` push)
    // happens later, on the winit main thread, through the normal mail
    // path. A separate thread (not the single-threaded winit loop), so
    // it never freezes the loop.
    #[allow(clippy::disallowed_methods)]
    let spawned = thread::Builder::new()
        .name("aether-desktop-signal".into())
        .spawn(move || {
            // The first signal begins graceful shutdown; the iterator
            // only ends if the underlying fd closes (it doesn't for the
            // thread's lifetime), so a single `next()` is the whole job.
            if signals.forever().next().is_some() {
                shutdown.store(true, Ordering::SeqCst);
                let _ = proxy.send_event(UserEvent::Quit);
            }
        });
    if let Err(e) = spawned {
        tracing::error!(
            target: "aether_substrate::boot",
            error = %e,
            "desktop: shutdown signal-watcher thread failed to spawn; \
             only window-close will trigger graceful shutdown",
        );
    }
}

#[cfg(not(unix))]
fn install_shutdown_handler(shutdown: &Arc<AtomicBool>, proxy: EventLoopProxy<UserEvent>) {
    let shutdown = Arc::clone(shutdown);
    if let Err(e) = ctrlc::set_handler(move || {
        shutdown.store(true, Ordering::SeqCst);
        let _ = proxy.send_event(UserEvent::Quit);
    }) {
        tracing::error!(
            target: "aether_substrate::boot",
            error = %e,
            "desktop: ctrl-c handler install failed; \
             only window-close will trigger graceful shutdown",
        );
    }
}

impl App {
    /// ADR-0080 §6 chassis-source push helper (issue
    /// iamacoffeepot/aether#723). Mints a fresh correlation, calls
    /// `push_chassis_root_mail` so the trace observer sees a `Sent`
    /// event for every input/window/frame-stats emission. Returns the
    /// minted chain-root [`MailId`] so frame-gating callers can
    /// subscribe its settlement (ADR-0082 §6).
    fn push_chassis_root(
        &self,
        recipient: MailboxId,
        kind: aether_data::KindId,
        payload: Vec<u8>,
        count: u32,
    ) -> MailId {
        let mut correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        if correlation == 0 {
            correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        }
        self.queue
            .push_chassis_root_mail(correlation, recipient, kind, payload, count)
    }

    /// Begin graceful shutdown (iamacoffeepot/aether#1489). Pushes a
    /// chassis-root [`Quit`] mail to `aether.lifecycle` (which sets the
    /// cap's `quit_pending`), marks `quit_requested`, and pokes a redraw
    /// so the `RedrawRequested` advance loop runs. The cap consumes the
    /// quit at its `Present` stage (ADR-0082 §3) — so the in-flight
    /// `Tick → Render → Present` frame finishes composing — then advances
    /// to the `Shutdown` terminal; the advance loop's terminal break
    /// drives `event_loop.exit()` (settle-then-exit, ADR-0082 §11).
    ///
    /// Idempotent on `quit_requested`: the bridges (`CloseRequested`, the
    /// signal flag, `UserEvent::Quit`) can all fire, but `Quit` is pushed
    /// once. The set flag also bypasses the `RedrawRequested` occlusion
    /// early-return so a shutdown requested on a hidden/minimized window
    /// still drives the lifecycle to `Shutdown`.
    fn request_quit(&mut self) {
        if self.quit_requested {
            return;
        }
        self.quit_requested = true;
        self.push_chassis_root(
            self.lifecycle_mailbox,
            <Quit as Kind>::ID,
            encode_empty::<Quit>(),
            1,
        );
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Mint a chassis-root `LifecycleAdvance` and push it to the
    /// `aether.lifecycle` cap with [`Self::lifecycle_reply_mailbox`] as
    /// its `Component` reply target (issue 1378). Open-codes the
    /// chassis-root push (`push_chassis_root_mail` doesn't carry a reply
    /// target): mint id → record `Sent` for the trace subtree → push with
    /// both the chassis-root lineage and the reply-to. Returns the minted
    /// chain root so the caller can subscribe its settlement.
    fn push_lifecycle_advance(&self) -> MailId {
        let mut correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        if correlation == 0 {
            correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        }
        let advance_root = MailId::new(MailboxId::CHASSIS_MAILBOX_ID, correlation);
        self.queue.record_sent(
            advance_root,
            advance_root,
            None,
            MailboxId::CHASSIS_MAILBOX_ID,
            self.lifecycle_mailbox,
            self.kind_lifecycle_advance,
        );
        let reply_to = ReplyTo::with_correlation(
            ReplyTarget::Component(self.lifecycle_reply_mailbox),
            correlation,
        );
        self.queue.push(
            Mail::new(
                self.lifecycle_mailbox,
                self.kind_lifecycle_advance,
                encode_empty::<aether_kinds::LifecycleAdvance>(),
                1,
            )
            .with_lineage(advance_root, advance_root, None)
            .with_reply_to(reply_to),
        );
        advance_root
    }

    /// Block (bounded) for the `LifecycleAdvanceComplete` reply to the
    /// just-issued `LifecycleAdvance`, returning its `next` stage kind id
    /// (issue 1378). The reply lands on [`Self::lifecycle_reply_inbox`]
    /// and is emitted by the cap *after* it clears its pending-advance
    /// guard, so the caller can safely issue the next advance once this
    /// returns. The settlement wait the caller runs first guarantees the
    /// reply is imminent; the generous timeout is a wedge backstop, not
    /// the normal path. `None` on timeout (no reply after settlement) so
    /// the caller can fail-fast.
    fn recv_lifecycle_advance_next(&self) -> Option<u64> {
        loop {
            let env = self
                .lifecycle_reply_inbox
                .recv_timeout(FRAME_SETTLEMENT_CAP)
                .ok()?;
            if env.kind == <LifecycleAdvanceComplete as Kind>::ID {
                return LifecycleAdvanceComplete::decode_from_bytes(env.payload.bytes())
                    .map(|complete| complete.next);
            }
            // Any other kind on this dedicated reply inbox is unexpected
            // (nothing else targets it); drop and keep waiting for the
            // advance reply rather than mis-gating the cycle.
        }
    }

    fn apply_window_mode(
        &mut self,
        mode: WindowMode,
        width: Option<u32>,
        height: Option<u32>,
    ) -> SetWindowModeResult {
        let Some(window) = self.window.clone() else {
            return SetWindowModeResult::Err {
                error: "set_window_mode requested before window initialized".to_owned(),
            };
        };
        let monitor = window.current_monitor();
        let fullscreen = match resolve_fullscreen(&mode, monitor.as_ref()) {
            Ok(fs) => fs,
            Err(e) => return SetWindowModeResult::Err { error: e },
        };
        window.set_fullscreen(fullscreen);
        if matches!(mode, WindowMode::Windowed)
            && let (Some(w), Some(h)) = (width, height)
        {
            let _ = window.request_inner_size(PhysicalSize::new(w, h));
        }

        self.current_mode = mode.clone();
        let size = window.inner_size();
        SetWindowModeResult::Ok {
            mode,
            width: size.width,
            height: size.height,
        }
    }

    fn apply_window_title(&self, title: String) -> SetWindowTitleResult {
        let Some(window) = self.window.as_ref() else {
            return SetWindowTitleResult::Err {
                error: "set_window_title requested before window initialized".to_owned(),
            };
        };
        window.set_title(&title);
        SetWindowTitleResult::Ok { title }
    }

    /// Bring the window to the foreground (iamacoffeepot/aether#1318):
    /// un-minimize, show if hidden, then raise + focus. winit's
    /// `focus_window` is best-effort per platform, but the three calls
    /// are the full lever the substrate has. `Err` if the window isn't
    /// created yet (mail arrived before `resumed`).
    fn apply_window_focus(&self) -> FocusWindowResult {
        let Some(window) = self.window.as_ref() else {
            return FocusWindowResult::Err {
                error: "focus requested before window initialized".to_owned(),
            };
        };
        window.set_minimized(false);
        window.set_visible(true);
        window.focus_window();
        FocusWindowResult::Ok
    }

    /// Drain the `aether.window` inbox without blocking. Called from
    /// `about_to_wait` (per-frame cadence). Each envelope dispatches
    /// inline against the framework-built-in arms first
    /// (`aether.log.tail` / `aether.trace.tail` / `aether.cost.tail`,
    /// iamacoffeepot/aether#1272) and only then the driver-specific
    /// `kind_set_window_mode` / `kind_set_window_title` arms; anything
    /// else warns and drops.
    ///
    /// The whole drain is wrapped in
    /// [`aether_actor::local::with_stamped`] against
    /// [`Self::actor_slots`] so the framework arms reach this driver's
    /// per-actor `ActorLogRing` / `ActorTraceRing` (the same property
    /// `DispatcherSlot::run_cycle` opens for every standard actor).
    fn drain_window_inbox(&mut self) {
        use std::sync::mpsc::TryRecvError;
        // Stamp once around the whole drain rather than per-envelope —
        // the stamp is cheap (single TLS write + RAII guard) but keeping
        // it open across the full burst means a handler that fires
        // `tracing::*` (e.g. apply_window_mode's failure log) also lands
        // in the driver's ring.
        let slots = self.actor_slots.clone();
        local::with_stamped(slots.slots(), || {
            loop {
                match self.window_inbox.try_recv() {
                    Ok(env) => self.dispatch_window_envelope(env),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
                }
            }
        });
    }

    // `env` is owned because the dispatch borrows `env.sender`,
    // `env.payload`, and `env.kind` separately as it walks the
    // window-control kind set; taking `&Envelope` works but loses the
    // owning-handoff symmetry with the rest of the dispatch surface.
    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_window_envelope(&mut self, env: Envelope) {
        // iamacoffeepot/aether#1325: capture the inbound settlement
        // identity before any arm moves fields out of the owned `env`,
        // so the ADR-0080 §2 bracket is discharged for every
        // driver-specific arm below (see `discharge_settlement`).
        let mail_id = env.mail_id;
        let root = env.root;
        // iamacoffeepot/aether#1272: framework-built-in dispatch arms
        // run BEFORE the driver-specific kinds, matching
        // `DispatcherSlot::run_cycle`'s ordering. Factored into a free
        // fn so the desktop-driver unit test exercises the routing
        // shape directly without standing up a winit `App`. The
        // matching `DispatcherSlot::run_cycle`'s ordering. The framework
        // arms reply but (unlike `dispatch_one`, which records `Finished`
        // unconditionally at its tail for every arm) do NOT record their
        // own settlement bracket — so we discharge here on the
        // early-return path too, mirroring `dispatch_one`'s unconditional
        // tail. ADR-0094: `env.discharge()` disarms the obligation guard
        // beside that settlement discharge.
        if try_framework_dispatch(&self.queue, self.window_mailbox, &env) {
            discharge_settlement(&self.queue, mail_id, root);
            env.discharge();
            return;
        }
        if env.kind == self.kind_set_window_mode {
            let payload: SetWindowMode = match postcard::from_bytes(env.payload.bytes()) {
                Ok(p) => p,
                Err(e) => {
                    self.queue.send_reply(
                        env.sender,
                        &SetWindowModeResult::Err {
                            error: format!("postcard decode failed: {e}"),
                        },
                    );
                    discharge_settlement(&self.queue, mail_id, root);
                    env.discharge();
                    return;
                }
            };
            let result = self.apply_window_mode(payload.mode, payload.width, payload.height);
            self.queue.send_reply(env.sender, &result);
        } else if env.kind == self.kind_set_window_title {
            let payload: SetWindowTitle = match postcard::from_bytes(env.payload.bytes()) {
                Ok(p) => p,
                Err(e) => {
                    self.queue.send_reply(
                        env.sender,
                        &SetWindowTitleResult::Err {
                            error: format!("postcard decode failed: {e}"),
                        },
                    );
                    discharge_settlement(&self.queue, mail_id, root);
                    env.discharge();
                    return;
                }
            };
            let result = self.apply_window_title(payload.title);
            self.queue.send_reply(env.sender, &result);
        } else if env.kind == self.kind_focus_window {
            // `FocusWindow` is a unit payload — nothing to decode.
            // Reply through `self.queue.send_reply` (the `Mailer`),
            // never `self.outbound` (`HubOutbound` drops
            // `ReplyTarget::Component`, iamacoffeepot/aether#1316).
            let result = self.apply_window_focus();
            self.queue.send_reply(env.sender, &result);
        } else {
            tracing::warn!(
                target: "aether_substrate::driver",
                kind = %env.kind_name,
                "desktop driver dropped unrecognised aether.window kind",
            );
        }
        // iamacoffeepot/aether#1325 / §Side finding #2: discharge the
        // ADR-0080 §2 settlement bracket once per envelope at the
        // drain-loop level (after `send_reply`), covering the two
        // success arms AND the unrecognised-kind warn-drop arm — a
        // blocking send of an unhandled window kind carrying a non-NONE
        // root would otherwise leak settlement the same way. Mirrors
        // `dispatch_one` (`dispatcher_slot.rs:289`).
        discharge_settlement(&self.queue, mail_id, root);
        // ADR-0094: disarm the obligation guard beside the settlement
        // discharge for the success + unrecognised-kind arms.
        env.discharge();
    }

    fn publish_window_size(&self, width: u32, height: u32) {
        let payload = encode(&WindowSize { width, height });
        self.push_chassis_root(self.input_mailbox, self.kind_window_size, payload, 1);
    }

    /// Fail-fast any parked `capture_frame` while the window is occluded
    /// (iamacoffeepot/aether#1317). Returns `true` when the wake was
    /// consumed (the window is occluded — whether or not a capture was
    /// parked); `false` when the window is visible, signalling the caller
    /// to fall through to its normal `request_redraw`.
    ///
    /// macOS does not deliver `RedrawRequested` to a hidden window, so a
    /// capture parked while occluded would otherwise never be serviced and
    /// the wire `Call` would hang on its settlement hold until timeout.
    /// Here we take the parked entry, drain its `after_mails` (parity with
    /// the `RedrawRequested` service arm), reply `Err` via the `Mailer`
    /// (`self.queue.send_reply`, never `self.outbound` — `HubOutbound`
    /// drops `ReplyTarget::Component`, iamacoffeepot/aether#1316), then let
    /// the request drop *after* the reply so the ADR-0086 §12 settlement
    /// hold's `Release` fires post-reply (iamacoffeepot/aether#1273).
    ///
    /// The slot is taken only when occluded, so a visible-window wake never
    /// steals the entry `RedrawRequested` is about to service.
    fn fail_capture_if_occluded(&mut self) -> bool {
        let pending = if self.occluded {
            self.capture_queue.take()
        } else {
            None
        };
        match occluded_capture_disposition(self.occluded, pending) {
            OccludedCaptureDisposition::Redraw => false,
            OccludedCaptureDisposition::Empty => true,
            OccludedCaptureDisposition::FailFast { request, result } => {
                for mail in request.after_mails {
                    self.queue.push(mail);
                }
                // `reply_to` is `Copy`, so this read leaves `request` whole;
                // it (and its `PendingCapture._hold`) drops at end of this
                // scope — *after* `send_reply` returns — firing `Release` on
                // the trace root so `Settled{root}` is exact post-reply.
                // Don't restructure to move the reply below other work
                // (iamacoffeepot/aether#1273 drop-order discipline).
                self.queue.send_reply(request.reply_to, &result);
                true
            }
        }
    }

    fn set_occluded(&mut self, occluded: bool, event_loop: &ActiveEventLoop) {
        if self.occluded == occluded {
            return;
        }
        self.occluded = occluded;
        if occluded {
            event_loop.set_control_flow(ControlFlow::Wait);
            // iamacoffeepot/aether#1317 (race fold-in): a capture poked
            // while the window was visible can land here before its
            // `RedrawRequested` is delivered — and macOS suppresses that
            // redraw once hidden. Fail any such parked capture fast on the
            // occlusion transition, with the same disposition the
            // wake-time path uses, so it never hangs on its settlement hold.
            self.fail_capture_if_occluded();
        } else {
            event_loop.set_control_flow(ControlFlow::Poll);
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = Window::default_attributes().with_title(&self.boot_title);
        if let Some((w, h)) = self.boot_size {
            attrs = attrs.with_inner_size(PhysicalSize::new(w, h));
        }
        match resolve_fullscreen(&self.boot_mode, event_loop.primary_monitor().as_ref()) {
            Ok(fs) => attrs = attrs.with_fullscreen(fs),
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::boot",
                    error = %e,
                    "AETHER_WINDOW_MODE boot request rejected — falling back to Windowed",
                );
                self.boot_mode = WindowMode::Windowed;
                self.current_mode = WindowMode::Windowed;
            }
        }
        let window = Arc::new(event_loop.create_window(attrs).expect("create_window"));
        self.gpu = Some(Gpu::new(Arc::clone(&window), self.render_handles.clone()));
        window.request_redraw();
        let initial_size = window.inner_size();
        self.window = Some(window);
        self.started = Some(Instant::now());
        self.publish_window_size(initial_size.width, initial_size.height);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        // Both proxy events nudge a redraw so the loop turns — but
        // `Capture` first checks occlusion. A capture needs a rendered
        // frame, and macOS does not deliver `RedrawRequested` to a hidden
        // window under `ControlFlow::Wait`; so when occluded we fail the
        // parked capture fast (`fail_capture_if_occluded`) rather than
        // poking a redraw that never lands and leaves the call hung on its
        // settlement hold (iamacoffeepot/aether#1317). When visible,
        // `Capture` falls through to `request_redraw` so `RedrawRequested`
        // pulls the queued capture. `WindowMail`
        // (iamacoffeepot/aether#1318) always pokes a redraw so winit runs
        // `about_to_wait` (which drains the `aether.window` inbox) even
        // under `ControlFlow::Wait`. Neither arm does the work itself —
        // the redraw / drain handlers do.
        match event {
            UserEvent::Capture => {
                if self.fail_capture_if_occluded() {
                    return;
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::WindowMail => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Quit => {
                // iamacoffeepot/aether#1489: the signal-watcher thread
                // flips the shutdown flag and sends this to wake a parked
                // (`ControlFlow::Wait`, occluded) loop. The flag-poll in
                // `about_to_wait` does the actual `Quit`-push; this arm is
                // the wake only, mirroring `WindowMail`.
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
    }

    // winit's `window_event` dispatches one arm per `WindowEvent`
    // variant; we route every variant through this single fn so the
    // event-to-engine bridging lives in one place.
    #[allow(clippy::too_many_lines)]
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            // iamacoffeepot/aether#1489: route OS-close through `Quit`
            // mail rather than tearing winit down directly, so the
            // lifecycle drains the in-flight frame and broadcasts
            // `Shutdown` before the loop exits. `request_quit` pushes the
            // `Quit` and pokes the redraw; the advance loop below drives
            // to the terminal and calls `event_loop.exit()` there.
            WindowEvent::CloseRequested => self.request_quit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size);
                }
                self.set_occluded(size.width == 0 || size.height == 0, event_loop);
                if size.width != 0 && size.height != 0 {
                    self.publish_window_size(size.width, size.height);
                }
            }
            WindowEvent::Occluded(occluded) => {
                self.set_occluded(occluded, event_loop);
            }
            WindowEvent::RedrawRequested => {
                let pending_capture = self.capture_queue.take();
                // iamacoffeepot/aether#1489: a quit-driven frame must run
                // even when occluded so the lifecycle reaches `Shutdown`;
                // the `!self.quit_requested` clause bypasses the
                // power-save early-return for the shutdown frame.
                if self.occluded && pending_capture.is_none() && !self.quit_requested {
                    return;
                }
                // Publish the live window size once per frame so
                // `WindowSize` subscribers (the camera's aspect tracking)
                // read it during the Tick stage.
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    if size.width != 0 && size.height != 0 {
                        self.publish_window_size(size.width, size.height);
                    }
                }
                // ADR-0082 §11 / issues 1378 + 1489: drive one full
                // `Tick → Render → Present` cycle. Each `LifecycleAdvance`
                // broadcasts the cap's current stage; components emit their
                // `DrawTriangle` / `aether.camera` mail into render as
                // descendants of that advance's chain root. We wait for the
                // broadcast root to settle (ADR-0080 §6 — the
                // causal-completion replacement for the retired
                // `drain_frame_bound_or_abort` poll), then read
                // `LifecycleAdvanceComplete.next` to learn the cap's
                // resolved next stage and loop until it returns to `Tick`
                // (one full cycle) or reaches the `Shutdown` terminal
                // (`next == 0`, set after a `Quit` was consumed at
                // `Present`). Reading the reply — not the raw settlement
                // channel — gates the next advance on the cap having
                // cleared its pending-advance guard, so the back-to-back
                // advances never race it (iamacoffeepot/aether#999). GPU
                // submit + present below runs after the `Render` chain
                // settles, so every actor's per-frame Tick compute and
                // Render submission is integrated before readback.
                let mut reached_terminal = false;
                loop {
                    let advance_root = self.push_lifecycle_advance();
                    if let Some(registry) = self.queue.settlement_registry() {
                        let rx = registry.subscribe_settlement(advance_root);
                        // A frame chain that doesn't settle is a wedged
                        // dispatcher — same fail-fast disposition the old
                        // drain barrier had (ADR-0063). Escalating-patience
                        // wait (issue #1305) replaces the bare wall-clock:
                        // a starved-but-healthy chain resolves before the
                        // cumulative cap, a genuine wedge exhausts it.
                        if let WaitOutcome::Wedged(wedge) = await_internal_signal(
                            &rx,
                            "desktop.frame_advance",
                            frame_loop::DRAIN_BUDGET,
                            FRAME_SETTLEMENT_CAP,
                            TerminalDisposition::Abort,
                        ) {
                            lifecycle::fatal_abort(&self.outbound, wedge.reason());
                        }
                    }
                    match self.recv_lifecycle_advance_next() {
                        // Terminal reached (`next == 0`): the `Shutdown`
                        // broadcast has fired and settled. Present this
                        // last frame, then `event_loop.exit()` below
                        // (settle-then-exit, ADR-0082 §11).
                        Some(0) => {
                            reached_terminal = true;
                            break;
                        }
                        // Back at Tick (cycle complete) — stop and present.
                        Some(next) if next == <Tick as Kind>::ID.0 => break,
                        // Mid-cycle (Tick → Render → Present) — keep advancing.
                        Some(_) => {}
                        // Settlement fired but the reply never arrived —
                        // a wedge in the reply path; fail-fast like the
                        // settlement wait above.
                        None => lifecycle::fatal_abort(
                            &self.outbound,
                            "desktop.frame_advance: LifecycleAdvanceComplete reply did not \
                             arrive after settlement"
                                .to_owned(),
                        ),
                    }
                }
                if let Some(gpu) = self.gpu.as_mut() {
                    match pending_capture {
                        Some(req) => {
                            // iamacoffeepot/aether#860: wait for each
                            // pre-mail's causal chain to settle before
                            // rendering, mirroring the test-bench fix.
                            // The desktop driver doesn't have a
                            // `SettlementTimeout` error to surface, so
                            // a stuck chain replies the capture with
                            // an `Err` and continues the frame loop
                            // (the user can retry without crashing
                            // the chassis).
                            let mut pre_failed: Option<String> = None;
                            for rx in req.pre_settlements {
                                if let WaitOutcome::Wedged(wedge) = await_internal_signal(
                                    &rx,
                                    "desktop.capture_pre_mail",
                                    frame_loop::DRAIN_BUDGET,
                                    FRAME_SETTLEMENT_CAP,
                                    TerminalDisposition::ReplyErr,
                                ) {
                                    pre_failed = Some(wedge.reason());
                                    break;
                                }
                            }
                            let result = pre_failed.map_or_else(
                                || CaptureFrameResult::from(gpu.render_and_capture()),
                                |error| CaptureFrameResult::Err { error },
                            );
                            for mail in req.after_mails {
                                //noinspection DuplicatedCode
                                self.queue.push(mail);
                            }
                            self.queue.send_reply(req.reply_to, &result);
                            // iamacoffeepot/aether#1273: `req` still owns
                            // `PendingCapture._hold` after the partial moves
                            // above; the field drops at end of this scope —
                            // *after* `send_reply` returns — firing
                            // `Release` on the trace root so `Settled{root}`
                            // is exact at post-reply. Don't restructure to
                            // move the reply below other work in this arm.
                        }
                        None => {
                            gpu.render();
                        }
                    }
                } else if let Some(req) = pending_capture {
                    self.queue.send_reply(
                        req.reply_to,
                        &CaptureFrameResult::Err {
                            error: "capture requested before GPU initialized".to_owned(),
                        },
                    );
                }
                self.frame += 1;
                // iamacoffeepot/aether#1489: the lifecycle reached its
                // `Shutdown` terminal and broadcast it (the advance loop
                // gates on settlement, so every `Shutdown` subscriber's
                // graceful-cleanup chain has drained). The final frame is
                // now presented — exit winit. `run_app` returns and the
                // chassis runs each passive's teardown + per-actor
                // `unwire` in reverse boot order. Don't request another
                // redraw on this path.
                if reached_terminal {
                    event_loop.exit();
                    return;
                }
                if !self.occluded
                    && let Some(w) = &self.window
                {
                    w.request_redraw();
                }
            }
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } if !key_event.repeat => {
                let Some(code) = (match key_event.physical_key {
                    PhysicalKey::Code(k) => map_winit_keycode(k),
                    PhysicalKey::Unidentified(_) => None,
                }) else {
                    return;
                };
                match key_event.state {
                    ElementState::Pressed => {
                        self.push_chassis_root(
                            self.input_mailbox,
                            self.kind_key,
                            encode(&Key { code }),
                            1,
                        );
                    }
                    ElementState::Released => {
                        self.push_chassis_root(
                            self.input_mailbox,
                            self.kind_key_release,
                            encode(&KeyRelease { code }),
                            1,
                        );
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            } => {
                self.push_chassis_root(
                    self.input_mailbox,
                    self.kind_mouse_button,
                    encode_empty::<MouseButton>(),
                    1,
                );
            }
            WindowEvent::CursorMoved { position, .. } => {
                // winit reports cursor position as f64; the input wire
                // kind carries f32. Realistic window sizes (< 2^20 px)
                // stay well inside f32 mantissa.
                #[allow(clippy::cast_possible_truncation)]
                let payload = encode(&MouseMove {
                    x: position.x as f32,
                    y: position.y as f32,
                });
                self.push_chassis_root(self.input_mailbox, self.kind_mouse_move, payload, 1);
            }
            _ => {}
        }
    }

    /// winit fires this between events. Issue 603 Phase 3 makes the
    /// driver itself the cap for `aether.window`, so the per-frame
    /// drain happens here instead of riding through `EventLoopProxy`
    /// from a separate dispatcher thread.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_window_inbox();
        // iamacoffeepot/aether#1489: poll the SIGINT/SIGTERM flag the
        // signal-watcher flips. On first observation, run the same
        // graceful-shutdown path `CloseRequested` uses. Force
        // `ControlFlow::Poll` so the loop keeps turning until the
        // shutdown frame drives the lifecycle to its `Shutdown` terminal,
        // even if the window was occluded (which had set `Wait`).
        if self.shutdown.load(Ordering::Relaxed) && !self.quit_requested {
            event_loop.set_control_flow(ControlFlow::Poll);
            self.request_quit();
        }
    }
}

/// ADR-0071 driver capability for the desktop chassis. Owns the
/// pieces the winit event-loop body needs at construction time, then
/// `boot()`-builds the App + `DriverRunning` that drives the loop.
/// `boot()` looks up `RenderCapability` via `DriverCtx::expect`
/// (booted earlier in the `.with()` chain) and pulls the accumulator
/// handles out of it.
///
/// The substrate-core handle (`SubstrateBoot`) rides along on the
/// running so the scheduler stays alive for the chassis's lifetime.
pub struct DesktopDriverCapability {
    pub event_loop: EventLoop<UserEvent>,
    pub boot: SubstrateBoot,
    pub capture_queue: CaptureQueue,
    pub boot_mode: WindowMode,
    pub boot_size: Option<(u32, u32)>,
    pub boot_title: String,
}

pub struct DesktopDriverRunning {
    app: App,
    event_loop: EventLoop<UserEvent>,
    triangles_rendered: Arc<AtomicU64>,
    /// `SubstrateBoot` drops at the end of `run()`. The `chassis_builder`
    /// `BootedPassives` (holding render/audio/io/http/log runnings)
    /// drops just after, tearing down each passive in reverse boot
    /// order via `RunningCapability::shutdown`.
    _boot: SubstrateBoot,
}

impl DriverCapability for DesktopDriverCapability {
    type Running = DesktopDriverRunning;

    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self {
            event_loop,
            boot,
            capture_queue,
            boot_mode,
            boot_size,
            boot_title,
        } = self;

        // Issue 629 / Phase A: render publishes its `RenderHandles`
        // bundle on the chassis's `ExportedHandles` map during `init`.
        // The driver retrieves the bundle via `DriverCtx::handle::<H>()`
        // — no `Arc<RenderCapability>` ever escapes the dispatcher
        // thread. The frame-bound pending counter is registered through
        // the FRAME_BARRIER claim machinery and surfaces via
        // `ctx.frame_bound_pending()`.
        let render_handles: RenderHandles = ctx.handle::<RenderHandles>().ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "DesktopDriverCapability::boot: RenderHandles must be published before the driver \
                 (verify the chassis builder calls `with_actor::<RenderCapability>(config)` before `driver(...)`)",
            )))
        })?;
        let triangles_rendered = Arc::clone(&render_handles.triangles_rendered);

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let kind_key = boot.registry.kind_id(Key::NAME).expect("Key registered");
        let kind_key_release = boot
            .registry
            .kind_id(KeyRelease::NAME)
            .expect("KeyRelease registered");
        let kind_mouse_button = boot
            .registry
            .kind_id(MouseButton::NAME)
            .expect("MouseButton registered");
        let kind_mouse_move = boot
            .registry
            .kind_id(MouseMove::NAME)
            .expect("MouseMove registered");
        let kind_window_size = boot
            .registry
            .kind_id(WindowSize::NAME)
            .expect("WindowSize registered");
        let kind_set_window_mode = boot
            .registry
            .kind_id(SetWindowMode::NAME)
            .expect("SetWindowMode registered");
        let kind_set_window_title = boot
            .registry
            .kind_id(SetWindowTitle::NAME)
            .expect("SetWindowTitle registered");
        let kind_focus_window = boot
            .registry
            .kind_id(FocusWindow::NAME)
            .expect("FocusWindow registered");

        // Issue 603 Phase 3: the desktop driver is the cap for
        // `aether.window`. Claim the inbox here; the receiver lives on
        // `App` and `about_to_wait` drains it inline between frames.
        //
        // iamacoffeepot/aether#1318: install an `EventLoopProxy` wake on
        // the claim so window-control mail (`focus` / `set_mode` /
        // `set_title`) arriving at an occluded window pokes
        // `UserEvent::WindowMail`, letting winit run `about_to_wait` and
        // drain even under `ControlFlow::Wait`. The proxy is minted here
        // while `event_loop` is still owned by the capability (it moves
        // into `DesktopDriverRunning` after `boot`).
        let window_claim = ctx.claim_mailbox("aether.window")?;
        let window_mail_proxy = event_loop.create_proxy();
        window_claim.wake_slot.set(Arc::new(move || {
            let _ = window_mail_proxy.send_event(UserEvent::WindowMail);
        }));

        let lifecycle_mailbox =
            mailbox_id_from_name(<aether_capabilities::LifecycleCapability as Actor>::NAMESPACE);
        let kind_lifecycle_advance = <aether_kinds::LifecycleAdvance as Kind>::ID;

        // iamacoffeepot/aether#1489: install the SIGINT/SIGTERM →
        // graceful-shutdown bridge. The flag is shared with `App`
        // (`about_to_wait` polls it); the watcher sends `UserEvent::Quit`
        // via this proxy to wake a parked loop. Minted here while
        // `event_loop` is still owned by the capability.
        let shutdown = Arc::new(AtomicBool::new(false));
        install_shutdown_handler(&shutdown, event_loop.create_proxy());

        // Issue 1378: claim a dedicated inbox for the cap's
        // `LifecycleAdvanceComplete` replies. The per-frame `Tick →
        // Render` cycle stamps this as the `Component` reply target on
        // each `LifecycleAdvance` and drains the receiver synchronously
        // to gate the next advance (see `recv_lifecycle_advance_next`).
        let lifecycle_reply_claim = ctx.claim_mailbox("aether.lifecycle.advance_reply")?;
        let _ = kind_tick; // PR 3b retired direct Tick push; the
        // chassis still resolves the kind id via `boot.registry` for
        // compatibility but the redraw handler no longer reads it.

        let app = App {
            queue: Arc::clone(&boot.queue),
            input_mailbox: mailbox_id_from_name(InputCapability::NAMESPACE),
            lifecycle_mailbox,
            kind_lifecycle_advance,
            lifecycle_reply_inbox: lifecycle_reply_claim.receiver,
            lifecycle_reply_mailbox: lifecycle_reply_claim.id,
            kind_key,
            kind_key_release,
            kind_mouse_button,
            kind_mouse_move,
            kind_window_size,
            render_handles,
            capture_queue,
            outbound: Arc::clone(&boot.outbound),
            window: None,
            gpu: None,
            started: None,
            frame: 0,
            occluded: false,
            boot_mode: boot_mode.clone(),
            boot_size,
            boot_title,
            current_mode: boot_mode,
            window_inbox: window_claim.receiver,
            actor_slots: window_claim.actor_slots,
            window_mailbox: window_claim.id,
            kind_set_window_mode,
            kind_set_window_title,
            kind_focus_window,
            // 0 is the "no correlation" sentinel; mirror NativeBinding's
            // start-at-1 convention.
            chassis_correlation: AtomicU64::new(1),
            quit_requested: false,
            shutdown,
        };

        Ok(DesktopDriverRunning {
            app,
            event_loop,
            triangles_rendered,
            // `boot` stays alive on the running so its scheduler joins
            // workers on drop. Drop ordering on
            // `DesktopDriverRunning::run` exit: app → event_loop →
            // triangles_rendered → _boot, which means capabilities
            // (held by `app`) tear down before the scheduler joins.
            _boot: boot,
        })
    }
}

impl DriverRunning for DesktopDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let Self {
            mut app,
            event_loop,
            triangles_rendered,
            // Bound (not `_boot`) so the teardown snapshot below can reach
            // the handle store; still held to the end of `run()` so the
            // scheduler joins workers on drop.
            _boot: boot,
        } = *self;

        event_loop
            .run_app(&mut app)
            .map_err(|e| RunError::Other(format!("event loop: {e}").into()))?;

        // ADR-0049 §3 boot fast-path (issue #1446): the event loop has
        // exited cleanly (window closed), so write the `index.bin`
        // snapshot of the live disk index. The next boot loads it in one
        // read + decode instead of one `open()` per `.meta` sidecar; a
        // crash that skips this teardown leaves the directory scan as the
        // fallback. Best-effort + a no-op when persistence is disabled.
        boot.handle_store.snapshot_index();

        let total = triangles_rendered.load(Ordering::Relaxed);
        let elapsed = app.started.map(|s| s.elapsed()).unwrap_or_default();
        // Frame count cast to f64 for FPS report — runs at shutdown,
        // bounded well below 2^53.
        #[allow(clippy::cast_precision_loss)]
        let fps = app.frame as f64 / elapsed.as_secs_f64().max(0.001);
        tracing::info!(
            target: "aether_substrate::shutdown",
            frames = app.frame,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            fps = fps,
            triangles = total,
            "frame loop exited",
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_windowed_defaults() {
        let (m, s) =
            parse_window_mode_env("windowed").expect("test setup: \"windowed\" is a valid spec");
        assert!(matches!(m, WindowMode::Windowed));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_windowed_with_size() {
        let (m, s) = parse_window_mode_env("windowed:1280x720")
            .expect("test setup: \"windowed:WxH\" is a valid spec");
        assert!(matches!(m, WindowMode::Windowed));
        assert_eq!(s, Some((1280, 720)));
    }

    #[test]
    fn parse_fullscreen_borderless() {
        let (m, s) = parse_window_mode_env("fullscreen-borderless")
            .expect("test setup: \"fullscreen-borderless\" is a valid spec");
        assert!(matches!(m, WindowMode::FullscreenBorderless));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_exclusive_converts_hz_to_mhz() {
        let (m, s) = parse_window_mode_env("exclusive:1920x1080@60")
            .expect("test setup: \"exclusive:WxH@HZ\" is a valid spec");
        let WindowMode::FullscreenExclusive {
            width,
            height,
            refresh_mhz,
        } = m
        else {
            panic!("expected exclusive");
        };
        assert_eq!((width, height, refresh_mhz), (1920, 1080, 60_000));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_rejects_unknown_variant() {
        assert!(parse_window_mode_env("garbage").is_err());
        assert!(parse_window_mode_env("exclusive:1920x1080").is_err());
        assert!(parse_window_mode_env("windowed:notxwide").is_err());
    }

    #[test]
    fn parse_ignores_whitespace() {
        let (m, _) = parse_window_mode_env("  windowed  ")
            .expect("test setup: surrounding whitespace is trimmed");
        assert!(matches!(m, WindowMode::Windowed));
    }

    /// iamacoffeepot/aether#1272: a `LogTail` envelope routed at the
    /// driver's `aether.window` mailbox produces a `LogTailResult`
    /// reply via the framework-built-in dispatch arm. Pre-fix the
    /// driver's bespoke "unrecognised kind → warn-drop" tail ate the
    /// envelope and `actor_logs aether.window` hung waiting for a
    /// reply that never came; this test pins the fix at the
    /// driver-dispatch layer without standing up wgpu/winit.
    #[test]
    fn try_framework_dispatch_replies_to_log_tail() {
        use aether_actor::local::{ActorSlots, with_stamped};
        use aether_data::KindId;
        use aether_data::{MailId, SessionToken};
        use aether_kinds::descriptors;
        use aether_kinds::trace::Nanos;
        use aether_kinds::{LogTail, LogTailResult};
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::mail::registry::Registry;
        use aether_substrate::mail::{MailRef, ReplyTarget, ReplyTo};

        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = HubOutbound::attached_loopback();
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(registry, store).with_outbound(outbound));

        let request = LogTail {
            max: 8,
            min_level: None,
            since: None,
        };
        let bytes = postcard::to_allocvec(&request).expect("encode LogTail");
        let session = SessionToken::NIL;
        let reply_to = ReplyTo::with_correlation(ReplyTarget::Session(session), 0x99);
        let env = Envelope::disarmed(
            KindId(<LogTail as Kind>::ID.0),
            <LogTail as Kind>::NAME.to_owned(),
            None,
            reply_to,
            MailRef::from(bytes),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        );

        let window_mailbox = mailbox_id_from_name("aether.window");
        let slots = ActorSlots::new();
        let matched = with_stamped(&slots, || {
            try_framework_dispatch(&mailer, window_mailbox, &env)
        });
        assert!(
            matched,
            "framework dispatch arm must match a LogTail envelope at aether.window",
        );

        let event = rx.try_recv().expect("framework arm routed a reply");
        match event {
            EgressEvent::ToSession {
                kind_name,
                correlation_id,
                ..
            } => {
                assert_eq!(kind_name, <LogTailResult as Kind>::NAME);
                assert_eq!(correlation_id, 0x99);
            }
            other => panic!("expected ToSession reply, got {other:?}"),
        }
    }

    /// A non-framework kind (here `SetWindowTitle`) does NOT trip the
    /// framework arms — the driver-specific path keeps its turn so
    /// `actor_logs`-style queries don't shadow the existing window
    /// controls.
    #[test]
    fn try_framework_dispatch_skips_window_kinds() {
        use aether_actor::local::{ActorSlots, with_stamped};
        use aether_data::KindId;
        use aether_data::MailId;
        use aether_kinds::descriptors;
        use aether_kinds::trace::Nanos;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::outbound::HubOutbound;
        use aether_substrate::mail::registry::Registry;
        use aether_substrate::mail::{MailRef, ReplyTo};

        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, rx) = HubOutbound::attached_loopback();
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(registry, store).with_outbound(outbound));

        let payload = postcard::to_allocvec(&SetWindowTitle {
            title: "ignored".to_owned(),
        })
        .expect("encode SetWindowTitle");
        let env = Envelope::disarmed(
            KindId(<SetWindowTitle as Kind>::ID.0),
            <SetWindowTitle as Kind>::NAME.to_owned(),
            None,
            ReplyTo::NONE,
            MailRef::from(payload),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            MailboxId(0),
        );

        let window_mailbox = mailbox_id_from_name("aether.window");
        let slots = ActorSlots::new();
        let matched = with_stamped(&slots, || {
            try_framework_dispatch(&mailer, window_mailbox, &env)
        });
        assert!(!matched, "SetWindowTitle is a driver-specific kind");
        assert!(rx.try_recv().is_err(), "no reply emitted on skip path");
    }

    /// iamacoffeepot/aether#1325: the window inbox drain owns the
    /// ADR-0080 §2 settlement bracket for every inbound envelope (the
    /// `Inbox` mailbox records none on the producer side). Drive the
    /// extracted `discharge_settlement` free fn — the same call the
    /// driver makes per envelope — against a seeded in-flight root and
    /// assert it settles. This is the CI-runnable regression guard for
    /// every window-kind arm without standing up winit/wgpu; the
    /// windowed end-to-end blocking-send path stays MCP-manual.
    #[test]
    fn discharge_settlement_settles_window_root() {
        use aether_data::MailId;
        use aether_substrate::chassis::settlement::SettlementRegistry;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::registry::Registry;

        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Mailer::new(registry, store);

        // Wire a settlement registry into the trace handle (the chassis
        // builder does both installs at boot, builder.rs:1119-1122) so
        // the emit-time counter's zero-transition can `fire_settled`.
        let settlement = Arc::new(SettlementRegistry::new());
        mailer.install_settlement_registry(Arc::clone(&settlement));
        mailer
            .trace_handle()
            .install_settlement_registry(Arc::clone(&settlement));

        // Mint a root, seed its emit-time `in_flight`, and subscribe its
        // settlement the way the driver does at driver.rs:606. A second
        // root stays seeded but is only ever poked by the NONE discharge
        // below — its receiver proves that arm is a no-op.
        let window_mailbox = mailbox_id_from_name("aether.window");
        let root = MailId::new(window_mailbox, 1);
        let mail_id = MailId::new(window_mailbox, 2);
        let guard_root = MailId::new(window_mailbox, 3);
        mailer.record_sent_inflight(root);
        mailer.record_sent_inflight(guard_root);
        let rx = settlement.subscribe_settlement(root);
        let guard_rx = settlement.subscribe_settlement(guard_root);

        // The per-envelope discharge the drain loop performs after
        // `send_reply`. With it, the inbound root's `in_flight` reaches
        // zero and `Settled` fires.
        discharge_settlement(&mailer, mail_id, root);
        rx.recv().expect("window root settles after discharge");

        // The chassis-internal-push guard: a `MailId::NONE` envelope
        // (window-size / frame-stats pushes) is a no-op — `guard_root`
        // stays in-flight and its receiver never wakes.
        discharge_settlement(&mailer, MailId::NONE, guard_root);
        assert!(
            guard_rx.try_recv().is_err(),
            "NONE discharge must not settle any root",
        );
    }

    /// iamacoffeepot/aether#1317: the occluded-capture branch selection.
    /// This is the CI-runnable core of the fail-fast — the winit wiring
    /// (`fail_capture_if_occluded` → `take` → `send_reply` → drop) stays
    /// MCP-manual because `ControlFlow::Wait` + `RedrawRequested`
    /// suppression has no CI display surface. Here we pin only the pure
    /// branch logic: occluded-with-parked-capture selects an `Err` reply,
    /// occluded-with-empty-slot and visible-window are no-ops/redraw.
    #[test]
    fn occluded_capture_disposition_selects_failfast_only_when_occluded_and_parked() {
        use aether_data::MailId;
        use aether_substrate::ReplyTarget;
        use aether_substrate::capture::PendingCapture;
        use aether_substrate::mail::ReplyTo;
        use aether_substrate::runtime::trace::TraceHandle;

        fn parked() -> PendingCapture {
            // `MailId::NONE` keeps the hold's acquire/release a counter
            // no-op; the test exercises branch selection, not settlement.
            let hold = TraceHandle::new().acquire_settlement_hold(MailId::NONE);
            PendingCapture {
                reply_to: ReplyTo::to(ReplyTarget::Session(aether_data::SessionToken::NIL)),
                after_mails: Vec::new(),
                pre_settlements: Vec::new(),
                hold,
            }
        }

        // Occluded + a parked capture → fail it fast with an `Err` reply
        // whose message names `aether.window.focus` as the remedy.
        match occluded_capture_disposition(true, Some(parked())) {
            OccludedCaptureDisposition::FailFast { result, .. } => match result {
                CaptureFrameResult::Err { error } => {
                    assert!(
                        error.contains("occluded") && error.contains("aether.window.focus"),
                        "Err reply must name occlusion + the focus remedy, got: {error}",
                    );
                }
                CaptureFrameResult::Ok { .. } => panic!("occluded capture must fail, not Ok"),
            },
            _ => panic!("occluded + parked capture must select FailFast"),
        }

        // Occluded but nothing parked → no-op (already serviced / stale wake).
        assert!(
            matches!(
                occluded_capture_disposition(true, None),
                OccludedCaptureDisposition::Empty
            ),
            "occluded + empty slot must be a no-op",
        );

        // Visible window → fall through to redraw, regardless of the slot.
        assert!(
            matches!(
                occluded_capture_disposition(false, Some(parked())),
                OccludedCaptureDisposition::Redraw
            ),
            "visible window must fall through to redraw",
        );
        assert!(
            matches!(
                occluded_capture_disposition(false, None),
                OccludedCaptureDisposition::Redraw
            ),
            "visible window must fall through to redraw",
        );
    }
}
