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
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_data::Kind;
use aether_data::{encode, encode_empty, mailbox_id_from_name};
use aether_input::InputCapability;
use aether_kinds::{
    CaptureFrameResult, FocusWindow, FocusWindowResult, ImePreedit, Key, KeyRelease, Modifiers, MouseButton,
    MouseButtonRelease, MouseMove, MouseWheel, Quit, SetWindowMode, SetWindowModeResult, SetWindowTitle,
    SetWindowTitleResult, TextInput, Tick, WindowMode, WindowSize,
};
use aether_render::{CaptureBackend, RenderHandles};
use aether_substrate::actor::native::local;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::settlement::{TerminalDisposition, WaitOutcome, await_internal_signal};
use aether_substrate::runtime::lifecycle as runtime_lifecycle;
use aether_substrate::{
    ChassisCtx, HubOutbound, InboundMail, Mailer, SettlingInbox, SharedActorSlots, Source, SourceAddr, SubstrateBoot,
    chassis::frame_loop,
    mail::{Mail, MailId, MailboxId},
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use super::chassis::UserEvent;
use super::render::Gpu;
use aether_substrate::capture::CaptureQueue;
use capture::{OccludedCaptureDisposition, occluded_capture_disposition};
use config::resolve_fullscreen;
use input::{TextSource, map_mouse_button, map_winit_keycode, normalize_wheel, text_input_gate};
use lifecycle::{LifecycleReplyOutcome, consume_lifecycle_reply, try_framework_dispatch};
use shutdown::install_shutdown_handler;
use std::io;
use winit::dpi::PhysicalSize;

mod capture;
mod config;
mod input;
mod lifecycle;
mod shutdown;

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
    /// fires one `LifecycleAdvance` here; the cap broadcasts the `Tick`
    /// stage directly to its stage subscribers (issue 1490 retired the
    /// `Tick → aether.input` relay; components subscribe `Tick` on
    /// `aether.lifecycle`), then the driver waits for settlement before
    /// submitting the frame.
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
    /// guard (the same reply-gate the substrate-harness frame loop uses,
    /// iamacoffeepot/aether#999).
    lifecycle_reply_inbox: SettlingInbox,
    /// Mailbox id of [`Self::lifecycle_reply_inbox`], used as the
    /// `Component` reply target stamped onto each `LifecycleAdvance`.
    lifecycle_reply_mailbox: MailboxId,
    kind_key: aether_data::KindId,
    kind_key_release: aether_data::KindId,
    kind_mouse_button: aether_data::KindId,
    kind_mouse_button_release: aether_data::KindId,
    kind_mouse_wheel: aether_data::KindId,
    kind_mouse_move: aether_data::KindId,
    kind_window_size: aether_data::KindId,
    kind_text_input: aether_data::KindId,
    kind_ime_preedit: aether_data::KindId,
    kind_modifiers: aether_data::KindId,
    /// Last cursor position seen in a `CursorMoved` event, in window
    /// coordinates. Stamped onto each button / wheel event so a click or
    /// scroll carries its location without the consumer correlating a
    /// separate `MouseMove`.
    last_cursor: (f32, f32),
    /// Composition gate for the text-input stream. `true` while an IME
    /// composition is active (a non-empty `Ime::Preedit` opened it,
    /// `Ime::Commit` / `Ime::Disabled` / a synthetic empty `Preedit`
    /// closes it). While composing, raw `KeyEvent.text` is suppressed so
    /// a committed character is published once (via `Ime::Commit`), never
    /// doubled. Driven by [`text_input_gate`].
    composing: bool,
    /// Cloned out of `RenderCapability::handles()` before the cap
    /// moves into the chassis builder. The app holds a clone so
    /// `Gpu::new` can install wgpu state and the per-frame loop can
    /// call `record_frame` / `record_capture_copy` / `finish_capture`.
    render_handles: RenderHandles,
    /// Shared single-slot queue with the control plane. On each
    /// redraw we `take()` any pending capture and, if present, use
    /// `render_and_capture`, then reply to the sender through the
    /// request's retained inbound guard (`req.reply.reply`, ADR-0106 /
    /// #1758) — the reply joins the inbound's ADR-0080 causal chain and
    /// settles it when the guard drops post-reply.
    capture_queue: CaptureQueue,
    /// Hub outbound — held for log egress to the hub and
    /// `lifecycle::fatal_abort`. NOT used for chassis replies:
    /// `HubOutbound::send_reply` only routes `Session` / `EngineMailbox`
    /// targets and silently drops `SourceAddr::Component`, but mail
    /// dispatched by this engine's own `RpcServerCapability` (every
    /// hub/MCP call lands via the proxy → local RPC server) carries a
    /// `Component(rpc_server)` reply target. Replies go through the
    /// `Mailer` instead (`mail.reply` for the chain-joined window arms,
    /// `req.reply.reply` through the retained inbound guard for the
    /// deferred capture replies, #1758), which pushes the reply as local
    /// mail so the RPC server's `on_any` lifts it into a `ReplyEvent`
    /// (iamacoffeepot/aether#1316).
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
    /// Resolved `AETHER_WIREFRAME` config value, threaded to `Gpu::new`
    /// when `resumed` creates the window. `WireframeMode::from_config_value`
    /// owns the tri-state parse.
    boot_wireframe: Option<String>,
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
    window_inbox: SettlingInbox,
    /// Per-actor [`local::ActorSlots`] carried out of the
    /// [`aether_substrate::MailboxClaim`] this driver produced at boot.
    /// Stamped into TLS via [`local::with_stamped`] around
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
        self.queue.push_chassis_root_mail(correlation, recipient, kind, payload, count)
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
        self.push_chassis_root(self.lifecycle_mailbox, <Quit as Kind>::ID, encode_empty::<Quit>(), 1);
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
        let reply_to = Source::with_correlation(SourceAddr::Component(self.lifecycle_reply_mailbox), correlation);
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
            let mail = self.lifecycle_reply_inbox.recv_timeout(FRAME_SETTLEMENT_CAP)?;
            // ADR-0106: the consumed reply settles when its `InboundMail`
            // guard drops inside `consume_lifecycle_reply` — no hand-rolled
            // bracket. Pre-#1704 a dropped armed guard aborted the process
            // on every painted frame.
            match consume_lifecycle_reply(mail) {
                LifecycleReplyOutcome::Complete(next) => return next,
                // Unexpected kind on this dedicated reply inbox (nothing
                // else targets it); already settled — keep waiting for the
                // advance reply rather than mis-gating the cycle.
                LifecycleReplyOutcome::Unexpected => {}
            }
        }
    }

    fn apply_window_mode(&mut self, mode: WindowMode, width: Option<u32>, height: Option<u32>) -> SetWindowModeResult {
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
        SetWindowModeResult::Ok { mode, width: size.width, height: size.height }
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
            return FocusWindowResult::Err { error: "focus requested before window initialized".to_owned() };
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
    /// [`local::with_stamped`] against
    /// [`Self::actor_slots`] so the framework arms reach this driver's
    /// per-actor `ActorLogRing` / `ActorTraceRing` (the same property
    /// `DispatcherSlot::run_cycle` opens for every standard actor).
    fn drain_window_inbox(&mut self) {
        // Stamp once around the whole drain rather than per-envelope —
        // the stamp is cheap (single TLS write + RAII guard) but keeping
        // it open across the full burst means a handler that fires
        // `tracing::*` (e.g. apply_window_mode's failure log) also lands
        // in the driver's ring.
        let slots = self.actor_slots.clone();
        local::with_stamped(slots.slots(), || {
            // ADR-0106: `try_next` yields each mail as an owned
            // `InboundMail` guard, so dispatch can still take `&mut self`
            // (the guard borrows nothing from `self.window_inbox`). Each
            // guard settles its inbound when `dispatch_window_envelope`
            // returns — no hand-rolled per-arm bracket.
            while let Some(mail) = self.window_inbox.try_next() {
                self.dispatch_window_envelope(mail);
            }
        });
    }

    // ADR-0106: `mail` is an owned `InboundMail` guard. It settles its
    // ADR-0080 §2 bracket + ADR-0094 obligation when it falls out of
    // scope at the end of this function — on every arm, including the
    // decode-error early returns — so there is no hand-rolled
    // `record_finished` / `discharge` pair. Replies go through
    // `mail.reply`, which joins the inbound's causal chain (ADR-0080
    // §5/§6) instead of minting the bare lineage-less NONE triple.
    //
    // `mail` is taken by value so its guard's `Drop` (the settlement)
    // binds to this scope; the body only calls `&self` methods on it,
    // which clippy reads as a needless by-value.
    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_window_envelope(&mut self, mail: InboundMail) {
        // iamacoffeepot/aether#1272: framework-built-in dispatch arms
        // run BEFORE the driver-specific kinds, matching
        // `DispatcherSlot::run_cycle`'s ordering. Factored into a free
        // fn so the desktop-driver unit test exercises the routing shape
        // directly without standing up a winit `App`. On a match the
        // helper has already replied through `mail`'s drain guard (the
        // chain-joined path, #1710); the guard settles on return.
        if try_framework_dispatch(&self.queue, self.window_mailbox, &mail) {
            return;
        }
        if mail.kind() == self.kind_set_window_mode {
            let Some(payload) = SetWindowMode::decode_from_bytes(mail.payload()) else {
                mail.reply(&SetWindowModeResult::Err { error: "SetWindowMode decode failed".to_owned() });
                return;
            };
            let result = self.apply_window_mode(payload.mode, payload.width, payload.height);
            mail.reply(&result);
        } else if mail.kind() == self.kind_set_window_title {
            let Some(payload) = SetWindowTitle::decode_from_bytes(mail.payload()) else {
                mail.reply(&SetWindowTitleResult::Err { error: "SetWindowTitle decode failed".to_owned() });
                return;
            };
            let result = self.apply_window_title(payload.title);
            mail.reply(&result);
        } else if mail.kind() == self.kind_focus_window {
            // `FocusWindow` is a unit payload — nothing to decode.
            // Reply through `mail.reply` (the chain-joined `Mailer`
            // path), never `self.outbound` (`HubOutbound` drops
            // `SourceAddr::Component`, iamacoffeepot/aether#1316).
            let result = self.apply_window_focus();
            mail.reply(&result);
        } else {
            tracing::warn!(
                target: "aether_substrate::driver",
                kind = %mail.kind_name(),
                "desktop driver dropped unrecognised aether.window kind",
            );
        }
        // `mail` drops here — the success arms AND the unrecognised-kind
        // warn-drop arm settle (ADR-0106).
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
    /// the wire `Call` would hang on its open inbound chain until timeout.
    /// Here we take the parked entry, drain its `after_mails` (parity with
    /// the `RedrawRequested` service arm), reply `Err` through the parked
    /// request's retained inbound guard (`request.reply.reply`, ADR-0106 /
    /// #1758), then let the request drop *after* the reply so the inbound's
    /// `Finished` records after the reply's `Sent` (ADR-0080 §6 /
    /// iamacoffeepot/aether#1273). The reply joins the inbound's causal
    /// chain through the same guard primitive every claimed-inbox consumer
    /// uses, so it lifts into a `ReplyEvent` even for the `Component`
    /// reply target an engine-local RPC Call carries
    /// (iamacoffeepot/aether#1316).
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
                // Move the request out of its box onto the stack so the
                // partial-move drain + retained-guard reply read cleanly.
                let request = *request;
                for mail in request.after_mails {
                    self.queue.push(mail);
                }
                // Reply through the retained inbound guard, then let
                // `request` (which still owns `request.reply` after the
                // partial move above) drop at end of this scope — *after*
                // `reply` returns — so the inbound's `Finished` records
                // after the reply's `Sent` (ADR-0080 §6 / #1758). Don't
                // restructure to move the reply below other work
                // (iamacoffeepot/aether#1273 drop-order discipline).
                request.reply.reply(&result);
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
        // Opt this window into IME event delivery. Most platforms send no
        // `Ime` events (and therefore no composed/committed CJK text)
        // unless the window has explicitly allowed IME. Candidate-window
        // placement (`set_ime_cursor_area`) is deferred — its absence only
        // floats the IME popup at a default position.
        window.set_ime_allowed(true);
        self.gpu = Some(Gpu::new(Arc::clone(&window), self.render_handles.clone(), self.boot_wireframe.as_deref()));
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
                // `DrawTriangle` / `aether.view_projection` mail into render as
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
                            runtime_lifecycle::fatal_abort(&self.outbound, wedge.reason());
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
                        None => runtime_lifecycle::fatal_abort(
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
                            // rendering, mirroring the substrate-harness fix.
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
                                || {
                                    CaptureFrameResult::from(
                                        gpu.render_and_capture(&req.checks, req.reference.as_ref()),
                                    )
                                },
                                |error| CaptureFrameResult::Err { error },
                            );
                            for mail in req.after_mails {
                                //noinspection DuplicatedCode
                                self.queue.push(mail);
                            }
                            // Reply through the retained inbound guard
                            // (ADR-0106 / #1758): the reply joins the
                            // inbound's ADR-0080 causal chain, and `req`
                            // (which still owns `req.reply` after the partial
                            // moves above) drops at end of this scope —
                            // *after* `reply` returns — so the inbound's
                            // `Finished` records after the reply's `Sent`
                            // (§6). Don't restructure to move the reply below
                            // other work in this arm (iamacoffeepot/aether#1273).
                            req.reply.reply(&result);
                        }
                        None => {
                            gpu.render();
                        }
                    }
                } else if let Some(req) = pending_capture {
                    // No GPU yet: reply `Err` through the retained guard,
                    // which then drops and settles the inbound chain (#1758).
                    req.reply.reply(&CaptureFrameResult::Err {
                        error: "capture requested before GPU initialized".to_owned(),
                    });
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
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                // Text path: publish the layout-resolved characters from
                // `KeyEvent.text` when no IME composition is active. Repeats
                // are forwarded here (holding a key types a run of
                // characters), unlike the named-key edge path below.
                if key_event.state == ElementState::Pressed
                    && let Some(text) = &key_event.text
                    && let Some(committed) = text_input_gate(&mut self.composing, TextSource::KeyText(text.to_string()))
                {
                    let payload = TextInput { text: committed }.encode_into_bytes();
                    self.push_chassis_root(self.input_mailbox, self.kind_text_input, payload, 1);
                }
                // Named-key edge path: `Key` / `KeyRelease` keep their
                // no-repeat contract and their `#[repr(C)]` cast payload.
                if !key_event.repeat
                    && let Some(code) = (match key_event.physical_key {
                        PhysicalKey::Code(k) => map_winit_keycode(k),
                        PhysicalKey::Unidentified(_) => None,
                    })
                {
                    match key_event.state {
                        ElementState::Pressed => {
                            self.push_chassis_root(self.input_mailbox, self.kind_key, encode(&Key { code }), 1);
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
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, cursor) => {
                    text_input_gate(&mut self.composing, TextSource::Preedit { active: !text.is_empty() });
                    // winit reports the cursor span as byte offsets into
                    // the preedit string (usize); the wire kind carries
                    // u32. A preedit is a handful of characters, far inside
                    // u32.
                    #[allow(clippy::cast_possible_truncation)]
                    let (cursor_begin, cursor_end) = match cursor {
                        Some((begin, end)) => (Some(begin as u32), Some(end as u32)),
                        None => (None, None),
                    };
                    let payload = ImePreedit { text, cursor_begin, cursor_end }.encode_into_bytes();
                    self.push_chassis_root(self.input_mailbox, self.kind_ime_preedit, payload, 1);
                }
                Ime::Commit(text) => {
                    if let Some(committed) = text_input_gate(&mut self.composing, TextSource::Commit(text)) {
                        let payload = TextInput { text: committed }.encode_into_bytes();
                        self.push_chassis_root(self.input_mailbox, self.kind_text_input, payload, 1);
                    }
                }
                Ime::Disabled => {
                    text_input_gate(&mut self.composing, TextSource::Disabled);
                }
                Ime::Enabled => {}
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                let payload = Modifiers {
                    shift: state.shift_key(),
                    ctrl: state.control_key(),
                    alt: state.alt_key(),
                    meta: state.super_key(),
                }
                .encode_into_bytes();
                self.push_chassis_root(self.input_mailbox, self.kind_modifiers, payload, 1);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // winit's `Other(n)` buttons map to no engine constant and
                // produce no mail, mirroring the unmapped-key contract.
                if let Some(button) = map_mouse_button(button) {
                    let (x, y) = self.last_cursor;
                    match state {
                        ElementState::Pressed => {
                            self.push_chassis_root(
                                self.input_mailbox,
                                self.kind_mouse_button,
                                encode(&MouseButton { button, x, y }),
                                1,
                            );
                        }
                        ElementState::Released => {
                            self.push_chassis_root(
                                self.input_mailbox,
                                self.kind_mouse_button_release,
                                encode(&MouseButtonRelease { button, x, y }),
                                1,
                            );
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = normalize_wheel(delta);
                let (x, y) = self.last_cursor;
                let payload = encode(&MouseWheel { delta_x, delta_y, x, y });
                self.push_chassis_root(self.input_mailbox, self.kind_mouse_wheel, payload, 1);
            }
            WindowEvent::CursorMoved { position, .. } => {
                // winit reports cursor position as f64; the input wire
                // kind carries f32. Realistic window sizes (< 2^20 px)
                // stay well inside f32 mantissa.
                #[allow(clippy::cast_possible_truncation)]
                let (x, y) = (position.x as f32, position.y as f32);
                self.last_cursor = (x, y);
                let payload = encode(&MouseMove { x, y });
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
    pub boot_wireframe: Option<String>,
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

    /// ADR-0155 §4 / issue #3834: reserve the `aether.window` inbox at the
    /// Claim stage. The desktop driver is the cap for `aether.window` (issue
    /// 603 Phase 3), but its winit `EventLoop` does not exist at claim time —
    /// so this value-free hook only reserves the registry slot, stashing the
    /// live `MailboxClaim` for [`Self::boot`] to recover at Start (where the
    /// event loop exists) and install the `EventLoopProxy` wake. Splitting
    /// the reservation off Start is what lets `--describe` claim
    /// `aether.window` on a headless host without an event loop.
    fn claim(ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        ctx.claim_driver_mailbox("aether.window")
    }

    // One-shot boot wiring: kind-id lookups, mailbox claims, and the
    // `App` construction all thread through a single flat sequence;
    // splitting would just pass the same dozen fields through a helper.
    #[allow(clippy::too_many_lines)]
    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let Self { event_loop, boot, capture_queue, boot_mode, boot_size, boot_title, boot_wireframe } = self;

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

        // ADR-0155 §4: the capture backend is a Start-stage handoff, not a
        // `RenderConfig` field. Build it from the capture queue + an
        // `EventLoopProxy` wake + the reply egress and install it into the
        // published `RenderHandles`, so the render cap's `on_capture_frame`
        // parks its request here and pokes `UserEvent::Capture` for the next
        // `RedrawRequested` to service. The winit event loop that could not
        // exist at claim time is present here at Start — which is exactly why
        // this wiring is Start-stage and not config.
        let capture_proxy = event_loop.create_proxy();
        render_handles.install_capture_backend(CaptureBackend {
            queue: capture_queue.clone(),
            wake: Arc::new(move || {
                let _ = capture_proxy.send_event(UserEvent::Capture);
                Ok(())
            }),
            outbound: Arc::clone(&boot.outbound),
        });

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let kind_key = boot.registry.kind_id(Key::NAME).expect("Key registered");
        let kind_key_release = boot.registry.kind_id(KeyRelease::NAME).expect("KeyRelease registered");
        let kind_mouse_button = boot.registry.kind_id(MouseButton::NAME).expect("MouseButton registered");
        let kind_mouse_button_release =
            boot.registry.kind_id(MouseButtonRelease::NAME).expect("MouseButtonRelease registered");
        let kind_mouse_wheel = boot.registry.kind_id(MouseWheel::NAME).expect("MouseWheel registered");
        let kind_mouse_move = boot.registry.kind_id(MouseMove::NAME).expect("MouseMove registered");
        let kind_window_size = boot.registry.kind_id(WindowSize::NAME).expect("WindowSize registered");
        let kind_text_input = boot.registry.kind_id(TextInput::NAME).expect("TextInput registered");
        let kind_ime_preedit = boot.registry.kind_id(ImePreedit::NAME).expect("ImePreedit registered");
        let kind_modifiers = boot.registry.kind_id(Modifiers::NAME).expect("Modifiers registered");
        let kind_set_window_mode = boot.registry.kind_id(SetWindowMode::NAME).expect("SetWindowMode registered");
        let kind_set_window_title = boot.registry.kind_id(SetWindowTitle::NAME).expect("SetWindowTitle registered");
        let kind_focus_window = boot.registry.kind_id(FocusWindow::NAME).expect("FocusWindow registered");

        // Issue 603 Phase 3: the desktop driver is the cap for
        // `aether.window`. ADR-0155 §4: the inbox was reserved at the Claim
        // stage by `DesktopDriverCapability::claim`; recover the live
        // `MailboxClaim` here at Start rather than re-claiming (a second
        // claim would collide). The receiver lives on `App` and
        // `about_to_wait` drains it inline between frames.
        //
        // iamacoffeepot/aether#1318: install an `EventLoopProxy` wake on
        // the recovered claim so window-control mail (`focus` / `set_mode` /
        // `set_title`) arriving at an occluded window pokes
        // `UserEvent::WindowMail`, letting winit run `about_to_wait` and
        // drain even under `ControlFlow::Wait`. The proxy is minted here
        // while `event_loop` is still owned by the capability (it moves
        // into `DesktopDriverRunning` after `boot`) — the winit event loop
        // that could not exist at claim time is present at Start, which is
        // exactly why the wake install is Start-stage.
        let window_claim = ctx.take_claimed_mailbox("aether.window").ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "DesktopDriverCapability::boot: the aether.window claim is missing — \
                 DriverCapability::claim must reserve it at the Claim stage before boot (ADR-0155 §4)",
            )))
        })?;
        let window_mail_proxy = event_loop.create_proxy();
        window_claim.wake_slot.set(Arc::new(move || {
            let _ = window_mail_proxy.send_event(UserEvent::WindowMail);
        }));

        // Chassis route-freezing: the desktop driver wires its event loop to
        // the lifecycle cap's own id (its NAMESPACE) at construction time —
        // ctx-less, no sibling resolver in scope.
        #[allow(clippy::disallowed_methods)]
        let lifecycle_mailbox = mailbox_id_from_name(<aether_lifecycle::LifecycleCapability as Addressable>::NAMESPACE);
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
            // Chassis route-freezing: the input cap's own id (its NAMESPACE),
            // ctx-less, no sibling resolver in scope.
            #[allow(clippy::disallowed_methods)]
            input_mailbox: mailbox_id_from_name(InputCapability::NAMESPACE),
            lifecycle_mailbox,
            kind_lifecycle_advance,
            lifecycle_reply_inbox: lifecycle_reply_claim.inbox,
            lifecycle_reply_mailbox: lifecycle_reply_claim.id,
            kind_key,
            kind_key_release,
            kind_mouse_button,
            kind_mouse_button_release,
            kind_mouse_wheel,
            kind_mouse_move,
            kind_window_size,
            kind_text_input,
            kind_ime_preedit,
            kind_modifiers,
            last_cursor: (0.0, 0.0),
            composing: false,
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
            boot_wireframe,
            current_mode: boot_mode,
            window_inbox: window_claim.inbox,
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
            // Held to the end of `run()` so the scheduler joins workers on
            // drop; the `_` prefix keeps the binding alive without a use.
            _boot,
        } = *self;

        event_loop.run_app(&mut app).map_err(|e| RunError::Other(format!("event loop: {e}").into()))?;

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
