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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_actor::Actor;
use aether_capabilities::InputCapability;
use aether_capabilities::RenderHandles;
use aether_data::Kind;
use aether_data::{encode, encode_empty, mailbox_id_from_name};
use aether_kinds::{
    CaptureFrameResult, Key, KeyRelease, MouseButton, MouseMove, SetWindowMode,
    SetWindowModeResult, SetWindowTitle, SetWindowTitleResult, Tick, WindowMode, WindowSize,
    keycode,
};
use aether_substrate::actor::native::envelope::Envelope;
use aether_substrate::chassis::builder::{DriverCapability, DriverCtx, DriverRunning, RunError};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{
    HubOutbound, Mailer, SubstrateBoot, chassis::frame_loop, mail::MailboxId,
    runtime::trace::push_chassis_root_mail,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::monitor::{MonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window, WindowId};

use super::chassis::UserEvent;
use super::render::Gpu;

pub struct App {
    queue: Arc<Mailer>,
    /// `aether.input` mailbox id, cached at driver boot. Each platform
    /// event fans through a single mail push to this mailbox; the
    /// `InputCapability` actor owns the subscriber table and fans
    /// out per-subscriber on its own dispatcher (issue 640).
    input_mailbox: MailboxId,
    kind_tick: aether_data::KindId,
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
    /// `render_and_capture`, then reply-to-sender on `outbound`.
    capture_queue: aether_substrate::capture::CaptureQueue,
    /// Hub outbound — shared with the log-capture layer and the
    /// capture-reply path.
    outbound: Arc<HubOutbound>,
    /// Snapshot of every frame-bound capability's pending counter
    /// (ADR-0074 §Decision 5). Today: render. Cloned out of
    /// `DriverCtx::frame_bound_pending` at driver boot; `RedrawRequested`
    /// hands it to `frame_loop::drain_frame_bound_or_abort` after the
    /// component drain so render's inbox quiesces before submit.
    frame_bound_pending: Vec<(MailboxId, Arc<AtomicU64>)>,
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
    /// apply `SetWindowMode` / `SetWindowTitle` inline on the chassis
    /// main thread (winit / macOS require window mutations there). No
    /// dispatcher thread, no `EventLoopProxy` bounce; the receiver
    /// is the wake. Under `ControlFlow::Wait` (set when the window
    /// occludes) `about_to_wait` only fires after a winit event, so
    /// window-kind mail can stall briefly until the user nudges the
    /// window — accepted limitation for v1.
    window_inbox: std::sync::mpsc::Receiver<Envelope>,
    kind_set_window_mode: aether_data::KindId,
    kind_set_window_title: aether_data::KindId,
    /// ADR-0080 §6 chassis-root correlation counter (issue
    /// iamacoffeepot/aether#723). Bumped per chassis-source push so
    /// every input/window/frame-stats emission carries a fresh
    /// `MailId` for the trace observer to root a tree on. Symmetric
    /// with the per-actor counter on `NativeBinding`.
    chassis_correlation: AtomicU64,
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

impl App {
    /// ADR-0080 §6 chassis-source push helper (issue
    /// iamacoffeepot/aether#723). Mints a fresh correlation, calls
    /// `push_chassis_root_mail` so the trace observer sees a `Sent`
    /// event for every input/window/frame-stats emission.
    fn push_chassis_root(
        &self,
        recipient: MailboxId,
        kind: aether_data::KindId,
        payload: Vec<u8>,
        count: u32,
    ) {
        let mut correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        if correlation == 0 {
            correlation = self.chassis_correlation.fetch_add(1, Ordering::Relaxed);
        }
        push_chassis_root_mail(&self.queue, correlation, recipient, kind, payload, count);
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
            let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
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

    /// Drain the `aether.window` inbox without blocking. Called from
    /// `about_to_wait` (per-frame cadence). Each envelope dispatches
    /// inline against `kind_set_window_mode` / `kind_set_window_title`;
    /// any other kind warns and drops.
    fn drain_window_inbox(&mut self) {
        use std::sync::mpsc::TryRecvError;
        loop {
            match self.window_inbox.try_recv() {
                Ok(env) => self.dispatch_window_envelope(env),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    // `env` is owned because the dispatch borrows `env.sender`,
    // `env.payload`, and `env.kind` separately as it walks the
    // window-control kind set; taking `&Envelope` works but loses the
    // owning-handoff symmetry with the rest of the dispatch surface.
    #[allow(clippy::needless_pass_by_value)]
    fn dispatch_window_envelope(&mut self, env: Envelope) {
        if env.kind == self.kind_set_window_mode {
            let payload: SetWindowMode = match postcard::from_bytes(&env.payload) {
                Ok(p) => p,
                Err(e) => {
                    self.outbound.send_reply(
                        env.sender,
                        &SetWindowModeResult::Err {
                            error: format!("postcard decode failed: {e}"),
                        },
                    );
                    return;
                }
            };
            let result = self.apply_window_mode(payload.mode, payload.width, payload.height);
            self.outbound.send_reply(env.sender, &result);
        } else if env.kind == self.kind_set_window_title {
            let payload: SetWindowTitle = match postcard::from_bytes(&env.payload) {
                Ok(p) => p,
                Err(e) => {
                    self.outbound.send_reply(
                        env.sender,
                        &SetWindowTitleResult::Err {
                            error: format!("postcard decode failed: {e}"),
                        },
                    );
                    return;
                }
            };
            let result = self.apply_window_title(payload.title);
            self.outbound.send_reply(env.sender, &result);
        } else {
            tracing::warn!(
                target: "aether_substrate::driver",
                kind = %env.kind_name,
                "desktop driver dropped unrecognised aether.window kind",
            );
        }
    }

    fn publish_window_size(&self, width: u32, height: u32) {
        let payload = encode(&WindowSize { width, height });
        self.push_chassis_root(self.input_mailbox, self.kind_window_size, payload, 1);
    }

    fn set_occluded(&mut self, occluded: bool, event_loop: &ActiveEventLoop) {
        if self.occluded == occluded {
            return;
        }
        self.occluded = occluded;
        if occluded {
            event_loop.set_control_flow(ControlFlow::Wait);
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
            attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h));
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
        match event {
            UserEvent::Capture => {
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
            WindowEvent::CloseRequested => event_loop.exit(),
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
                if self.occluded && pending_capture.is_none() {
                    return;
                }
                self.push_chassis_root(
                    self.input_mailbox,
                    self.kind_tick,
                    encode_empty::<Tick>(),
                    1,
                );
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    if size.width != 0 && size.height != 0 {
                        self.publish_window_size(size.width, size.height);
                    }
                }
                // ADR-0074 §Decision 5: render's inbox must quiesce
                // before submit so any DrawTriangle / aether.camera
                // mail this frame is integrated into the recorded
                // pass. (The pre-Phase-4 component drain barrier is
                // retired; trampoline traps fail-fast directly via
                // `NativeBinding::fatal_abort`.)
                frame_loop::drain_frame_bound_or_abort(&self.frame_bound_pending, &self.outbound);
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
                                if rx.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
                                    pre_failed = Some(
                                        "capture pre-mail chain failed to settle within 5s — \
                                         a downstream cap is wedged"
                                            .to_owned(),
                                    );
                                    break;
                                }
                            }
                            let result = pre_failed.map_or_else(
                                || match gpu.render_and_capture() {
                                    Ok(png) => CaptureFrameResult::Ok { png },
                                    Err(error) => CaptureFrameResult::Err { error },
                                },
                                |error| CaptureFrameResult::Err { error },
                            );
                            for mail in req.after_mails {
                                //noinspection DuplicatedCode
                                self.queue.push(mail);
                            }
                            self.outbound.send_reply(req.reply_to, &result);
                        }
                        None => {
                            gpu.render();
                        }
                    }
                } else if let Some(req) = pending_capture {
                    self.outbound.send_reply(
                        req.reply_to,
                        &CaptureFrameResult::Err {
                            error: "capture requested before GPU initialized".to_owned(),
                        },
                    );
                }
                self.frame += 1;
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
    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.drain_window_inbox();
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
    pub capture_queue: aether_substrate::capture::CaptureQueue,
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
            BootError::Other(Box::new(std::io::Error::other(
                "DesktopDriverCapability::boot: RenderHandles must be published before the driver \
                 (verify the chassis builder calls `with_actor::<RenderCapability>(config)` before `driver(...)`)",
            )))
        })?;
        let triangles_rendered = Arc::clone(&render_handles.triangles_rendered);
        let frame_bound_pending = ctx.frame_bound_pending();

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

        // Issue 603 Phase 3: the desktop driver is the cap for
        // `aether.window`. Claim the inbox here; the receiver lives on
        // `App` and `about_to_wait` drains it inline between frames.
        let window_claim = ctx.claim_mailbox("aether.window")?;

        let app = App {
            queue: Arc::clone(&boot.queue),
            input_mailbox: mailbox_id_from_name(InputCapability::NAMESPACE),
            kind_tick,
            kind_key,
            kind_key_release,
            kind_mouse_button,
            kind_mouse_move,
            kind_window_size,
            render_handles,
            capture_queue,
            outbound: Arc::clone(&boot.outbound),
            frame_bound_pending,
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
            kind_set_window_mode,
            kind_set_window_title,
            // 0 is the "no correlation" sentinel; mirror NativeBinding's
            // start-at-1 convention.
            chassis_correlation: AtomicU64::new(1),
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
            _boot,
        } = *self;

        event_loop
            .run_app(&mut app)
            .map_err(|e| RunError::Other(format!("event loop: {e}").into()))?;

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
}
