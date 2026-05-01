//! Desktop chassis driver capability — ADR-0071 phase 3.
//!
//! Holds the winit `App` struct, the `ApplicationHandler` impl that
//! drives per-frame work, the small bag of winit/wgpu mapping helpers
//! the chassis needs to read its own state, and the
//! `AETHER_WINDOW_MODE` parser. Wraps everything in a
//! [`DesktopDriverCapability`] so [`crate::chassis::DesktopChassis`]
//! composes one driver alongside its passive capabilities
//! (LogCapability, IoCapability, NetCapability, AudioCapability,
//! RenderCapability — composed via `chassis_builder::Builder::with`
//! per ADR-0071 phase B).
//!
//! `DesktopDriverRunning::run` blocks on `event_loop.run_app(&mut app)`
//! and emits the shutdown telemetry the previous `DesktopChassis::run`
//! body owned. Returning means the user closed the window or the
//! event loop exited cleanly; the chassis_builder then tears down
//! every passive in reverse boot order via `BootedPassives::Drop`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_data::Kind;
use aether_data::{encode, encode_empty};
use aether_hub::HubClient;
use aether_kinds::{
    CaptureFrameResult, EngineInfo, FrameStats, GpuBackend, GpuDeviceType, GpuInfo, Key,
    KeyRelease, MonitorInfo, MouseButton, MouseMove, OsInfo, PlatformInfoResult,
    SetWindowModeResult, SetWindowTitleResult, Tick, VideoMode, WindowInfo, WindowMode, WindowSize,
    keycode,
};
use aether_substrate_core::capability::BootError;
use aether_substrate_core::chassis_builder::{
    DriverCapability, DriverCtx, DriverRunning, RunError,
};
use aether_substrate_core::{
    HubOutbound, InputSubscribers, Mailer, SubstrateBoot,
    capabilities::{RenderHandles, RenderRunning},
    frame_loop,
    mail::{Mail, MailboxId},
    subscribers_for,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::monitor::{MonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window, WindowId};

use crate::chassis::UserEvent;
use crate::render::Gpu;

/// Wire-stable `EngineInfo.workers` value (ADR-0038: post actor-per-
/// component, the scheduler doesn't read this — it's retained on the
/// hub-protocol wire for compatibility). Stays chassis-side because
/// it's declarative for `aether.control.platform_info`, not loop
/// policy. The shared frame-loop policy (drain budget, frame-stats
/// cadence) lives in `aether_substrate_core::frame_loop`.
pub const WORKERS: usize = 2;

pub struct App {
    queue: Arc<Mailer>,
    /// ADR-0021 per-stream subscribers. Shared with the control plane
    /// so subscribe / unsubscribe / drop write through the same table
    /// the platform thread reads on each event. Empty sets — the
    /// boot state — mean the event is dropped at the source.
    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: aether_data::KindId,
    kind_key: aether_data::KindId,
    kind_key_release: aether_data::KindId,
    kind_mouse_button: aether_data::KindId,
    kind_mouse_move: aether_data::KindId,
    kind_window_size: aether_data::KindId,
    kind_frame_stats: aether_data::KindId,
    /// Cloned out of `RenderRunning` at driver boot. Source-of-truth
    /// lives in core's `RenderCapability`; the app holds a clone so
    /// `Gpu::new` can install wgpu state and the per-frame loop can
    /// call `record_frame` / `record_capture_copy` / `finish_capture`.
    render_running: Arc<RenderRunning>,
    triangles_rendered: Arc<AtomicU64>,
    /// Shared single-slot queue with the control plane. On each
    /// redraw we `take()` any pending capture and, if present, use
    /// `render_and_capture`, then reply-to-sender on `outbound`.
    capture_queue: crate::CaptureQueue,
    /// Hub outbound — also shared with the log-capture layer and the
    /// broadcast sink. The capture-reply path is the third consumer.
    outbound: Arc<HubOutbound>,
    /// How many kinds the substrate registered at boot. Captured once
    /// and cached so `platform_info` can report it without having to
    /// consult the live registry (which also contains runtime-loaded
    /// kinds — those aren't part of the build fingerprint).
    boot_kinds_count: u32,
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

/// Copy winit's `VideoModeHandle` fields into the wire-stable mirror
/// in `aether-kinds`. Separate type so the kind's schema doesn't ride
/// winit's layout.
fn mirror_video_mode(m: winit::monitor::VideoModeHandle) -> VideoMode {
    VideoMode {
        width: m.size().width,
        height: m.size().height,
        refresh_mhz: m.refresh_rate_millihertz(),
        bit_depth: m.bit_depth(),
    }
}

/// Convert wgpu's `DeviceType` into the wire-stable mirror enum in
/// `aether-kinds`. Separate enum so the schema doesn't drift with
/// wgpu versions.
fn map_device_type(t: wgpu::DeviceType) -> GpuDeviceType {
    match t {
        wgpu::DeviceType::Other => GpuDeviceType::Other,
        wgpu::DeviceType::IntegratedGpu => GpuDeviceType::IntegratedGpu,
        wgpu::DeviceType::DiscreteGpu => GpuDeviceType::DiscreteGpu,
        wgpu::DeviceType::VirtualGpu => GpuDeviceType::VirtualGpu,
        wgpu::DeviceType::Cpu => GpuDeviceType::Cpu,
    }
}

/// Convert wgpu's `Backend` into the wire-stable mirror. `Empty` is
/// coalesced into `Noop` — the substrate never uses the empty
/// backend, but the match needs to be exhaustive.
fn map_backend(b: wgpu::Backend) -> GpuBackend {
    match b {
        wgpu::Backend::Noop => GpuBackend::Noop,
        wgpu::Backend::Vulkan => GpuBackend::Vulkan,
        wgpu::Backend::Metal => GpuBackend::Metal,
        wgpu::Backend::Dx12 => GpuBackend::Dx12,
        wgpu::Backend::Gl => GpuBackend::Gl,
        wgpu::Backend::BrowserWebGpu => GpuBackend::BrowserWebGpu,
    }
}

/// Parse `AETHER_WINDOW_MODE`. Grammar:
///   `windowed`              — default size
///   `windowed:WxH`          — windowed, WxH physical pixels
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
    /// Build a `PlatformInfoResult::Ok` from whatever the event loop
    /// knows right now: OS via `std::env::consts` + `os_info`, engine
    /// via compile-time + boot-time facts, GPU via the cached
    /// `AdapterInfo` on `Gpu`, monitors via winit. `window` is `None`
    /// until `resumed` fires and `self.window` / `self.gpu` are set.
    fn snapshot_platform_info(&self, event_loop: &ActiveEventLoop) -> PlatformInfoResult {
        let os_info = os_info::get();
        let os = OsInfo {
            name: std::env::consts::OS.to_owned(),
            version: os_info.version().to_string(),
            arch: std::env::consts::ARCH.to_owned(),
        };
        let engine = EngineInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            workers: WORKERS as u32,
            kinds_count: self.boot_kinds_count,
        };

        let Some(gpu) = self.gpu.as_ref() else {
            return PlatformInfoResult::Err {
                error: "platform_info requested before GPU and window initialized".to_owned(),
            };
        };

        let gpu_info = GpuInfo {
            name: gpu.adapter_info.name.clone(),
            vendor_id: gpu.adapter_info.vendor,
            device_id: gpu.adapter_info.device,
            device_type: map_device_type(gpu.adapter_info.device_type),
            backend: map_backend(gpu.adapter_info.backend),
            driver: gpu.adapter_info.driver.clone(),
            driver_info: gpu.adapter_info.driver_info.clone(),
            max_texture_dim_2d: gpu.limits.max_texture_dimension_2d,
            max_buffer_size: gpu.limits.max_buffer_size,
            max_bind_groups: gpu.limits.max_bind_groups,
        };

        let primary = event_loop.primary_monitor();
        let monitors: Vec<MonitorInfo> = event_loop
            .available_monitors()
            .map(|m| {
                let pos = m.position();
                let size = m.size();
                let current_refresh = m.refresh_rate_millihertz();
                let modes: Vec<VideoMode> = m.video_modes().map(mirror_video_mode).collect();
                let current_mode = current_refresh.and_then(|mhz| {
                    modes.iter().copied().find(|v| {
                        v.width == size.width && v.height == size.height && v.refresh_mhz == mhz
                    })
                });
                MonitorInfo {
                    name: m.name(),
                    is_primary: primary.as_ref() == Some(&m),
                    position_x: pos.x,
                    position_y: pos.y,
                    width: size.width,
                    height: size.height,
                    scale_factor: m.scale_factor(),
                    current_mode,
                    modes,
                }
            })
            .collect();

        let window = self.window.as_ref().map(|w| {
            let size = w.inner_size();
            let monitor_index = w
                .current_monitor()
                .and_then(|m| event_loop.available_monitors().position(|other| other == m))
                .map(|idx| idx as u32);
            WindowInfo {
                mode: self.current_mode.clone(),
                width: size.width,
                height: size.height,
                scale_factor: w.scale_factor(),
                monitor_index,
            }
        });

        PlatformInfoResult::Ok {
            os,
            engine,
            gpu: gpu_info,
            monitors,
            window,
        }
    }

    fn apply_window_mode(
        &mut self,
        mode: WindowMode,
        width: Option<u32>,
        height: Option<u32>,
    ) -> SetWindowModeResult {
        let Some(window) = self.window.as_ref().cloned() else {
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

    fn publish_window_size(&self, width: u32, height: u32) {
        let subs = subscribers_for(&self.input_subscribers, WindowSize::ID);
        if subs.is_empty() {
            return;
        }
        let payload = encode(&WindowSize { width, height });
        for mbox in subs {
            self.queue
                .push(Mail::new(mbox, self.kind_window_size, payload.clone(), 1));
        }
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
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Capture => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::PlatformInfo { reply_to } => {
                let result = self.snapshot_platform_info(event_loop);
                self.outbound.send_reply(reply_to, &result);
            }
            UserEvent::SetWindowMode {
                reply_to,
                mode,
                width,
                height,
            } => {
                let result = self.apply_window_mode(mode, width, height);
                self.outbound.send_reply(reply_to, &result);
            }
            UserEvent::SetWindowTitle { reply_to, title } => {
                let result = self.apply_window_title(title);
                self.outbound.send_reply(reply_to, &result);
            }
        }
    }

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
        self.gpu = Some(Gpu::new(
            Arc::clone(&window),
            Arc::clone(&self.render_running),
        ));
        window.request_redraw();
        let initial_size = window.inner_size();
        self.window = Some(window);
        self.started = Some(Instant::now());
        self.publish_window_size(initial_size.width, initial_size.height);
    }

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
                let tick_subs = subscribers_for(&self.input_subscribers, Tick::ID);
                for mbox in tick_subs {
                    self.queue
                        .push(Mail::new(mbox, self.kind_tick, encode_empty::<Tick>(), 1));
                }
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    if size.width != 0 && size.height != 0 {
                        self.publish_window_size(size.width, size.height);
                    }
                }
                frame_loop::drain_or_abort(&self.queue, &self.outbound);
                if let Some(gpu) = self.gpu.as_mut() {
                    match pending_capture {
                        Some(req) => {
                            let result = match gpu.render_and_capture() {
                                Ok(png) => CaptureFrameResult::Ok { png },
                                Err(error) => CaptureFrameResult::Err { error },
                            };
                            for mail in req.after_mails {
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
                if self.frame.is_multiple_of(frame_loop::LOG_EVERY_FRAMES) {
                    let triangles = self.triangles_rendered.load(Ordering::Relaxed);
                    tracing::info!(
                        target: "aether_substrate::frame_loop",
                        frame = self.frame,
                        triangles,
                        "frame stats",
                    );
                    frame_loop::emit_frame_stats(
                        &self.queue,
                        self.broadcast_mbox,
                        self.broadcast_mbox,
                        self.kind_frame_stats,
                        self.frame,
                        triangles,
                    );
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
                        let subs = subscribers_for(&self.input_subscribers, Key::ID);
                        for mbox in subs {
                            self.queue.push(Mail::new(
                                mbox,
                                self.kind_key,
                                encode(&Key { code }),
                                1,
                            ));
                        }
                    }
                    ElementState::Released => {
                        let subs = subscribers_for(&self.input_subscribers, KeyRelease::ID);
                        for mbox in subs {
                            self.queue.push(Mail::new(
                                mbox,
                                self.kind_key_release,
                                encode(&KeyRelease { code }),
                                1,
                            ));
                        }
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            } => {
                let subs = subscribers_for(&self.input_subscribers, MouseButton::ID);
                for mbox in subs {
                    self.queue.push(Mail::new(
                        mbox,
                        self.kind_mouse_button,
                        encode_empty::<MouseButton>(),
                        1,
                    ));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let subs = subscribers_for(&self.input_subscribers, MouseMove::ID);
                if !subs.is_empty() {
                    let payload = encode(&MouseMove {
                        x: position.x as f32,
                        y: position.y as f32,
                    });
                    for mbox in subs {
                        self.queue
                            .push(Mail::new(mbox, self.kind_mouse_move, payload.clone(), 1));
                    }
                }
            }
            _ => {}
        }
    }
}

/// ADR-0071 driver capability for the desktop chassis. Owns the
/// pieces the winit event-loop body needs at construction time, then
/// `boot()`-builds the App + DriverRunning that drives the loop.
/// `boot()` looks up [`RenderRunning`] via [`DriverCtx::expect`]
/// (booted earlier in the `.with()` chain) and pulls the accumulator
/// handles out of it.
///
/// The substrate-core handle (`SubstrateBoot`) rides along on the
/// running so the scheduler stays alive for the chassis's lifetime.
pub struct DesktopDriverCapability {
    pub event_loop: EventLoop<UserEvent>,
    pub boot: SubstrateBoot,
    pub capture_queue: crate::CaptureQueue,
    pub boot_kinds_count: u32,
    pub boot_mode: WindowMode,
    pub boot_size: Option<(u32, u32)>,
    pub boot_title: String,
    /// Held for the chassis lifetime so the hub reader + heartbeat
    /// threads stay spawned. `None` when `AETHER_HUB_URL` was unset.
    pub hub: Option<HubClient>,
}

pub struct DesktopDriverRunning {
    app: App,
    event_loop: EventLoop<UserEvent>,
    triangles_rendered: Arc<AtomicU64>,
    /// `SubstrateBoot` drops at the end of `run()`. The chassis_builder
    /// `BootedPassives` (holding render/audio/io/net/log runnings)
    /// drops just after, tearing down each passive in reverse boot
    /// order via `RunningCapability::shutdown`.
    _boot: SubstrateBoot,
    _hub: Option<HubClient>,
}

impl DriverCapability for DesktopDriverCapability {
    type Running = DesktopDriverRunning;

    fn boot(self, ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        let DesktopDriverCapability {
            event_loop,
            boot,
            capture_queue,
            boot_kinds_count,
            boot_mode,
            boot_size,
            boot_title,
            hub,
        } = self;

        // Look up RenderCapability's running via the chassis_builder
        // typed-store (ADR-0071). The render passive booted before
        // this driver, so its `RenderRunning` is in the typed map;
        // pull the accumulator handles for the per-frame loop to
        // read.
        let render_running: Arc<RenderRunning> = ctx.expect();
        let RenderHandles {
            frame_vertices: _,
            camera_state: _,
            triangles_rendered,
        } = render_running.handles();

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
        let kind_frame_stats = boot
            .registry
            .kind_id(FrameStats::NAME)
            .expect("FrameStats registered");

        let app = App {
            queue: Arc::clone(&boot.queue),
            input_subscribers: Arc::clone(&boot.input_subscribers),
            broadcast_mbox: boot.broadcast_mbox,
            kind_tick,
            kind_key,
            kind_key_release,
            kind_mouse_button,
            kind_mouse_move,
            kind_window_size,
            kind_frame_stats,
            render_running: Arc::clone(&render_running),
            triangles_rendered: Arc::clone(&triangles_rendered),
            capture_queue,
            outbound: Arc::clone(&boot.outbound),
            boot_kinds_count,
            window: None,
            gpu: None,
            started: None,
            frame: 0,
            occluded: false,
            boot_mode: boot_mode.clone(),
            boot_size,
            boot_title,
            current_mode: boot_mode,
        };

        Ok(DesktopDriverRunning {
            app,
            event_loop,
            triangles_rendered,
            // `boot` stays alive on the running so its scheduler joins
            // workers on drop and its `BootedChassis` (legacy
            // capabilities added via `boot.add_capability`) shut down
            // in reverse boot order. Drop ordering on
            // `DesktopDriverRunning::run` exit: app → event_loop →
            // triangles_rendered → _boot → _hub, which means
            // capabilities tear down before the hub disconnects.
            _boot: boot,
            _hub: hub,
        })
    }
}

impl DriverRunning for DesktopDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        let DesktopDriverRunning {
            mut app,
            event_loop,
            triangles_rendered,
            _boot,
            _hub,
        } = *self;

        event_loop
            .run_app(&mut app)
            .map_err(|e| RunError::Other(format!("event loop: {e}").into()))?;

        let total = triangles_rendered.load(Ordering::Relaxed);
        let elapsed = app.started.map(|s| s.elapsed()).unwrap_or_default();
        tracing::info!(
            target: "aether_substrate::shutdown",
            frames = app.frame,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            fps = app.frame as f64 / elapsed.as_secs_f64().max(0.001),
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
        let (m, s) = parse_window_mode_env("windowed").unwrap();
        assert!(matches!(m, WindowMode::Windowed));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_windowed_with_size() {
        let (m, s) = parse_window_mode_env("windowed:1280x720").unwrap();
        assert!(matches!(m, WindowMode::Windowed));
        assert_eq!(s, Some((1280, 720)));
    }

    #[test]
    fn parse_fullscreen_borderless() {
        let (m, s) = parse_window_mode_env("fullscreen-borderless").unwrap();
        assert!(matches!(m, WindowMode::FullscreenBorderless));
        assert_eq!(s, None);
    }

    #[test]
    fn parse_exclusive_converts_hz_to_mhz() {
        let (m, s) = parse_window_mode_env("exclusive:1920x1080@60").unwrap();
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
        let (m, _) = parse_window_mode_env("  windowed  ").unwrap();
        assert!(matches!(m, WindowMode::Windowed));
    }
}
