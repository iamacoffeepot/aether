// Frame-loop driver. Substrate boots componentless (ADR-0010): no
// component is compiled in, no default mailbox is registered for
// input routing. The render sink is still wired so any runtime-loaded
// component can emit `aether.draw_triangle` mail and get pixels on
// screen; until a component is loaded and explicitly mailed, the
// window clears to its default and no triangles are drawn.
//
// Keyboard/mouse/tick events from winit are published per-stream
// (ADR-0021): the substrate consults an `InputSubscribers` table —
// shared with the control-plane handler — and enqueues one copy of
// the event per currently-subscribed mailbox. Empty subscriber sets
// drop the event at the source. Subscriptions are managed via
// `aether.control.subscribe_input` / `aether.control.unsubscribe_input`.

mod render;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_kinds::{
    CaptureFrameResult, EngineInfo, FrameStats, GpuBackend, GpuDeviceType, GpuInfo, InputStream,
    Key, MonitorInfo, MouseButton, MouseMove, OsInfo, PlatformInfoResult, SetWindowModeResult,
    SetWindowTitleResult, Tick, VideoMode, WindowInfo, WindowMode, WindowSize,
};
use aether_mail::Kind;
use aether_mail::{encode, encode_empty};
use aether_substrate_desktop::{
    CaptureQueue, Chassis, ChassisCapabilities, HubClient, HubOutbound, InputSubscribers, Mailer,
    Scheduler, SubstrateBoot, UserEvent, chassis_control_handler,
    mail::{Mail, MailboxId},
    subscribers_for,
};
use render::Gpu;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::monitor::{MonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window, WindowId};

const WORKERS: usize = 2;
const LOG_EVERY_FRAMES: u64 = 120;

/// ADR-0035 desktop chassis. Owns the winit event loop and the
/// `App` that drives it. The `Chassis` trait's `run(self) -> Result`
/// takes ownership and blocks until the event loop exits (normally
/// on window close); shutdown telemetry rides inside `run` so every
/// chassis type is responsible for its own exit log, matching each
/// chassis's own loop-termination shape.
struct DesktopChassis {
    event_loop: EventLoop<UserEvent>,
    app: App,
    triangles_rendered: Arc<AtomicU64>,
    // Retained so the hub's reader + heartbeat threads stay spawned
    // for the life of the chassis. `None` when `AETHER_HUB_URL` was
    // unset — the substrate still renders locally.
    _hub: Option<HubClient>,
}

impl Chassis for DesktopChassis {
    const KIND: &'static str = "desktop";
    const CAPABILITIES: ChassisCapabilities = ChassisCapabilities {
        has_gpu: true,
        has_window: true,
        has_tcp_listener: false,
    };

    fn run(self) -> wasmtime::Result<()> {
        let DesktopChassis {
            event_loop,
            mut app,
            triangles_rendered,
            _hub,
        } = self;
        event_loop
            .run_app(&mut app)
            .map_err(|e| wasmtime::Error::msg(format!("event loop: {e}")))?;

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

struct App {
    queue: Arc<Mailer>,
    /// ADR-0021 per-stream subscribers. Shared with the control plane
    /// so subscribe / unsubscribe / drop write through the same table
    /// the platform thread reads on each event. Empty sets — the
    /// boot state — mean the event is dropped at the source.
    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: u64,
    kind_key: u64,
    kind_mouse_button: u64,
    kind_mouse_move: u64,
    kind_window_size: u64,
    kind_frame_stats: u64,
    frame_vertices: Arc<Mutex<Vec<u8>>>,
    triangles_rendered: Arc<AtomicU64>,
    /// Shared single-slot queue with the control plane. On each
    /// redraw we `take()` any pending capture and, if present, use
    /// `render_and_capture`, then reply-to-sender on `outbound`.
    capture_queue: CaptureQueue,
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
    started: Option<Instant>,
    frame: u64,
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
    // Scheduler is owned so its workers are joined on Drop when the event
    // loop exits — we never reference it otherwise.
    _scheduler: Scheduler,
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
fn parse_window_mode_env(s: &str) -> Result<(WindowMode, Option<(u32, u32)>), String> {
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

        // `Gpu` is absent until `resumed`; without an adapter we
        // can't describe the GPU or the window. Surface that
        // cleanly as `Err` so the caller sees why, rather than
        // returning a half-populated snapshot.
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

        // Monitor list + primary comparison. winit's `MonitorHandle`
        // doesn't expose `is_primary` directly — compare against
        // `primary_monitor()` by value (the handle is `PartialEq`).
        let primary = event_loop.primary_monitor();
        let monitors: Vec<MonitorInfo> = event_loop
            .available_monitors()
            .map(|m| {
                let pos = m.position();
                let size = m.size();
                let current_refresh = m.refresh_rate_millihertz();
                let modes: Vec<VideoMode> = m.video_modes().map(mirror_video_mode).collect();
                // winit 0.30 exposes the monitor's current size +
                // refresh but not a `current_video_mode` handle — we
                // synthesize it by matching the listed modes against
                // the live size/refresh, and settle for `None` if
                // no entry matches (unusual but possible on virtual
                // displays).
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

    /// Apply a `SetWindowMode` request against the current window.
    /// Resolves fullscreen modes against the current monitor (so
    /// exclusive modes match the display the window is actually on),
    /// sets fullscreen + optional windowed size, and reads the new
    /// `inner_size()` back for the reply. A missing window (before
    /// `resumed`) replies `Err` rather than hanging.
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
        // `set_inner_size` returns `Option<PhysicalSize>` — the
        // platform may honour the request asynchronously or not at
        // all. We keep the request as the caller's intent; the reply
        // size is whatever winit reports *after* applying.
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

    /// Apply a `SetWindowTitle` request. `Window::set_title` is
    /// infallible on every winit platform, so the only failure mode
    /// is the pre-resume case where no window exists yet.
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
        let subs = subscribers_for(&self.input_subscribers, InputStream::WindowSize);
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
                // When occluded, `ControlFlow::Wait` stops the normal
                // redraw cadence — request one explicitly so the
                // capture handler in `RedrawRequested` runs.
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
        // Apply `AETHER_WINDOW_MODE` at window creation. Resolving
        // exclusive at boot uses the primary monitor since there's
        // no window yet to ask "which monitor am I on?".
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
        self.gpu = Some(Gpu::new(Arc::clone(&window)));
        window.request_redraw();
        let initial_size = window.inner_size();
        self.window = Some(window);
        self.started = Some(Instant::now());
        // Publish the first WindowSize so subscribers that auto-wired
        // at init time get a value before their first `MouseMove` or
        // tick — without this they'd only learn the size on the first
        // resize, which never happens for a user who just opens the
        // window and clicks.
        self.publish_window_size(initial_size.width, initial_size.height);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size);
                }
                // Windows reports minimize as a zero-dimension resize;
                // macOS uses Occluded. Treat both as "pause the loop".
                self.set_occluded(size.width == 0 || size.height == 0, event_loop);
                // Skip the zero-dim publish — a minimized window's
                // size isn't useful to components and would break
                // divide-by-width math downstream.
                if size.width != 0 && size.height != 0 {
                    self.publish_window_size(size.width, size.height);
                }
            }
            WindowEvent::Occluded(occluded) => {
                self.set_occluded(occluded, event_loop);
            }
            WindowEvent::RedrawRequested => {
                let pending_capture = self.capture_queue.take();
                // Occluded + nothing to capture: skip the frame
                // entirely. Captures still land via `user_event`
                // (which calls `request_redraw`), so even a hidden
                // window can produce frames for the agent.
                if self.occluded && pending_capture.is_none() {
                    return;
                }
                let tick_subs = subscribers_for(&self.input_subscribers, InputStream::Tick);
                for mbox in tick_subs {
                    self.queue
                        .push(Mail::new(mbox, self.kind_tick, encode_empty::<Tick>(), 1));
                }
                // Re-pulse WindowSize every tick so components that
                // subscribed *after* `resumed` fired (the common case
                // — they load via MCP long after boot) pick up the
                // current size within one frame. Steady-state cost is
                // one tiny 8-byte payload per subscriber per tick;
                // the subscriber-empty check keeps it to a hashmap
                // read when nobody cares.
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    if size.width != 0 && size.height != 0 {
                        self.publish_window_size(size.width, size.height);
                    }
                }
                self.queue.drain_all();
                let verts = std::mem::take(&mut *self.frame_vertices.lock().unwrap());
                if let Some(gpu) = self.gpu.as_mut() {
                    match pending_capture {
                        Some(req) => {
                            let result = match gpu.render_and_capture(&verts) {
                                Ok(png) => CaptureFrameResult::Ok { png },
                                Err(error) => CaptureFrameResult::Err { error },
                            };
                            // Post-capture cleanup: push every
                            // `after_mails` entry the control plane
                            // pre-resolved. Done before the reply so
                            // the cleanup mail is at least queued
                            // when the caller sees the PNG.
                            for mail in req.after_mails {
                                self.queue.push(mail);
                            }
                            self.outbound.send_reply(req.reply_to, &result);
                        }
                        None => {
                            gpu.render(&verts);
                        }
                    }
                } else if let Some(req) = pending_capture {
                    // No GPU yet — capture was requested before `resumed`.
                    // Reply with a diagnosable error rather than leaving the
                    // caller hanging on an await-reply slot. `after_mails`
                    // is dropped — the pre-capture bundle wasn't processed
                    // either, so there's nothing to clean up.
                    self.outbound.send_reply(
                        req.reply_to,
                        &CaptureFrameResult::Err {
                            error: "capture requested before GPU initialized".to_owned(),
                        },
                    );
                }
                self.frame += 1;
                if self.frame.is_multiple_of(LOG_EVERY_FRAMES) {
                    let triangles = self.triangles_rendered.load(Ordering::Relaxed);
                    tracing::info!(
                        target: "aether_substrate::frame_loop",
                        frame = self.frame,
                        triangles,
                        "frame stats",
                    );
                    // Emit an observation to every attached Claude
                    // session. No-op when no hub is connected.
                    self.queue.push(Mail::new(
                        self.broadcast_mbox,
                        self.kind_frame_stats,
                        encode(&FrameStats {
                            frame: self.frame,
                            triangles,
                        }),
                        1,
                    ));
                }
                // Only self-schedule the next redraw when the window
                // is visible — otherwise we'd spin under `Poll`. When
                // occluded, the next wake comes from `user_event`
                // (capture requested) or a window event.
                if !self.occluded
                    && let Some(w) = &self.window
                {
                    w.request_redraw();
                }
            }
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } if key_event.state == ElementState::Pressed && !key_event.repeat => {
                let subs = subscribers_for(&self.input_subscribers, InputStream::Key);
                if !subs.is_empty() {
                    let code: u32 = match key_event.physical_key {
                        PhysicalKey::Code(k) => k as u32,
                        PhysicalKey::Unidentified(_) => 0,
                    };
                    for mbox in subs {
                        self.queue
                            .push(Mail::new(mbox, self.kind_key, encode(&Key { code }), 1));
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            } => {
                let subs = subscribers_for(&self.input_subscribers, InputStream::MouseButton);
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
                let subs = subscribers_for(&self.input_subscribers, InputStream::MouseMove);
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

fn main() -> wasmtime::Result<()> {
    // Build the event loop + capture queue up front so the chassis
    // handler closure can capture them during `SubstrateBoot::build`.
    // The proxy wakes the loop on queued captures (important when the
    // window is occluded — capture still lands via `user_event` ->
    // `request_redraw`); the capture queue is the single-slot handoff
    // the control-plane handler writes and the render thread drains.
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let capture_queue = CaptureQueue::new();

    // Shared runtime bring-up: log capture, registry + kind descriptors,
    // broadcast sink, scheduler, control plane, optional hub connect.
    // The chassis handler closure is invoked during build() once
    // `registry` / `queue` / `outbound` exist but before the control
    // plane is wired, so it can `Arc::clone` what it needs to own.
    let boot = SubstrateBoot::builder("hello-triangle", env!("CARGO_PKG_VERSION"))
        .workers(WORKERS)
        .chassis_handler({
            let proxy = event_loop.create_proxy();
            let capture_queue = capture_queue.clone();
            move |ctx| {
                Some(chassis_control_handler(
                    proxy,
                    capture_queue,
                    Arc::clone(ctx.registry),
                    Arc::clone(ctx.queue),
                    Arc::clone(ctx.outbound),
                ))
            }
        })
        .build()?;

    let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
    let kind_key = boot.registry.kind_id(Key::NAME).expect("Key registered");
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

    // Desktop-only render sink: the winit render thread drains
    // `frame_vertices` each redraw, so every `DrawTriangle` emitted
    // before the next frame is consolidated into one vertex buffer.
    let frame_vertices = Arc::new(Mutex::new(Vec::<u8>::with_capacity(4096)));
    let triangles_rendered = Arc::new(AtomicU64::new(0));
    {
        let verts_for_sink = Arc::clone(&frame_vertices);
        let tris_for_sink = Arc::clone(&triangles_rendered);
        boot.registry.register_sink(
            "render",
            Arc::new(
                move |_kind_id: u64,
                      _kind_name: &str,
                      _origin: Option<&str>,
                      _sender,
                      bytes: &[u8],
                      count: u32| {
                    verts_for_sink.lock().unwrap().extend_from_slice(bytes);
                    tris_for_sink.fetch_add(u64::from(count), Ordering::Relaxed);
                },
            ),
        );
    }

    tracing::info!(
        target: "aether_substrate::boot",
        workers = WORKERS,
        "componentless boot — close window to exit; load a component via aether.control.load_component",
    );

    let boot_kinds_count = boot.boot_descriptors.len() as u32;
    // Parse `AETHER_WINDOW_MODE` at boot. Unset → Windowed (default
    // size); bad value → log + fall back to Windowed rather than
    // refusing to boot.
    let (boot_mode, boot_size) = match std::env::var("AETHER_WINDOW_MODE") {
        Ok(s) => match parse_window_mode_env(&s) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::boot",
                    value = %s,
                    error = %e,
                    "AETHER_WINDOW_MODE unparseable — falling back to Windowed",
                );
                (WindowMode::Windowed, None)
            }
        },
        Err(_) => (WindowMode::Windowed, None),
    };
    // `AETHER_WINDOW_TITLE` overrides the default title. Empty string
    // is accepted — winit treats it as "no title" on most platforms —
    // but unset gives the generic substrate name rather than leaking
    // whatever demo-ish string last shipped in source.
    let boot_title = std::env::var("AETHER_WINDOW_TITLE").unwrap_or_else(|_| "aether".to_owned());
    let app = App {
        queue: boot.queue,
        input_subscribers: boot.input_subscribers,
        broadcast_mbox: boot.broadcast_mbox,
        kind_tick,
        kind_key,
        kind_mouse_button,
        kind_mouse_move,
        kind_window_size,
        kind_frame_stats,
        frame_vertices,
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
        _scheduler: boot.scheduler,
    };

    let chassis = DesktopChassis {
        event_loop,
        app,
        triangles_rendered,
        _hub: boot.hub,
    };
    tracing::info!(
        target: "aether_substrate::boot",
        kind = DesktopChassis::KIND,
        has_gpu = DesktopChassis::CAPABILITIES.has_gpu,
        has_window = DesktopChassis::CAPABILITIES.has_window,
        has_tcp_listener = DesktopChassis::CAPABILITIES.has_tcp_listener,
        "chassis initialised",
    );
    chassis.run()
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
        assert!(parse_window_mode_env("exclusive:1920x1080").is_err()); // missing @hz
        assert!(parse_window_mode_env("windowed:notxwide").is_err());
    }

    #[test]
    fn parse_ignores_whitespace() {
        let (m, _) = parse_window_mode_env("  windowed  ").unwrap();
        assert!(matches!(m, WindowMode::Windowed));
    }
}
