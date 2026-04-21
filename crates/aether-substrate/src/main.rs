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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub};
use aether_kinds::{
    CaptureFrameResult, EngineInfo, FrameStats, GpuBackend, GpuDeviceType, GpuInfo, InputStream,
    Key, MonitorInfo, MouseButton, MouseMove, OsInfo, PlatformInfoResult, SetWindowModeResult,
    Tick, VideoMode, WindowInfo, WindowMode,
};
use aether_mail::Kind;
use aether_mail::{encode, encode_empty};
use aether_substrate::{
    CaptureQueue, Chassis, ChassisCapabilities, HUB_CLAUDE_BROADCAST, HubClient, HubOutbound,
    InputSubscribers, MailQueue, Registry, Scheduler, SubstrateCtx, UserEvent,
    chassis_control_handler, host_fns,
    mail::{Mail, MailboxId},
    subscribers_for,
};
use render::Gpu;
use wasmtime::{Engine, Linker};
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
    queue: Arc<MailQueue>,
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
            UserEvent::PlatformInfo { sender } => {
                let result = self.snapshot_platform_info(event_loop);
                self.outbound.send_reply(sender, &result);
            }
            UserEvent::SetWindowMode {
                sender,
                mode,
                width,
                height,
            } => {
                let result = self.apply_window_mode(mode, width, height);
                self.outbound.send_reply(sender, &result);
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
        let mut attrs = Window::default_attributes().with_title("aether hello-triangle");
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
        self.window = Some(window);
        self.started = Some(Instant::now());
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
                self.queue.wait_idle();
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
                            self.outbound.send_reply(req.sender, &result);
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
                        req.sender,
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
    // Reserved well-known sink: mail sent here is forwarded to every
    // attached Claude session via the hub. The handle is created up
    // front (before the hub is dialed) so the log capture layer can
    // share it; the hub client populates it later if `AETHER_HUB_URL`
    // is set, otherwise sends are silently dropped.
    let outbound = HubOutbound::disconnected();

    // ADR-0023: install the tracing subscriber + log capture early so
    // bring-up errors (renderer init, hub handshake) are captured.
    aether_substrate::log_capture::init(Arc::clone(&outbound));

    let engine = Arc::new(Engine::default());

    let registry = Arc::new(Registry::new());

    // Pre-register every substrate-owned kind with its descriptor so
    // the Registry agrees with what the hub receives at `Hello` and
    // ADR-0010's load-time conflict check has the right reference.
    // Ids are dense and assigned in the order below; not otherwise
    // meaningful — consumers always resolve by name.
    let boot_descriptors = aether_kinds::descriptors::all();
    for d in &boot_descriptors {
        registry
            .register_kind_with_descriptor(d.clone())
            .expect("duplicate kind in substrate init");
    }
    let kind_tick = registry.kind_id(Tick::NAME).expect("Tick registered");
    let kind_key = registry.kind_id(Key::NAME).expect("Key registered");
    let kind_mouse_button = registry
        .kind_id(MouseButton::NAME)
        .expect("MouseButton registered");
    let kind_mouse_move = registry
        .kind_id(MouseMove::NAME)
        .expect("MouseMove registered");
    let kind_frame_stats = registry
        .kind_id(FrameStats::NAME)
        .expect("FrameStats registered");

    let frame_vertices = Arc::new(Mutex::new(Vec::<u8>::with_capacity(4096)));
    let triangles_rendered = Arc::new(AtomicU64::new(0));

    let verts_for_sink = Arc::clone(&frame_vertices);
    let tris_for_sink = Arc::clone(&triangles_rendered);
    registry.register_sink(
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

    // Reserved well-known sink: mail sent here is forwarded to every
    // attached Claude session via the hub. If no hub is connected, the
    // outbound handle is disconnected and the sink silently drops —
    // the component doesn't have to care either way. (Handle created
    // earlier so the log capture layer could share it.)
    let broadcast_mbox = {
        let outbound = Arc::clone(&outbound);
        registry.register_sink(
            HUB_CLAUDE_BROADCAST,
            Arc::new(
                move |_kind_id: u64,
                      kind_name: &str,
                      origin: Option<&str>,
                      _sender,
                      bytes: &[u8],
                      _count: u32| {
                    if kind_name.is_empty() {
                        tracing::warn!(
                            target: "aether_substrate::broadcast",
                            "{HUB_CLAUDE_BROADCAST} received mail with unregistered kind — dropping",
                        );
                        return;
                    }
                    outbound.send(EngineToHub::Mail(EngineMailFrame {
                        address: ClaudeAddress::Broadcast,
                        kind_name: kind_name.to_owned(),
                        payload: bytes.to_vec(),
                        origin: origin.map(str::to_owned),
                    }));
                },
            ),
        )
    };

    let queue = Arc::new(MailQueue::new());

    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker)?;
    let linker = Arc::new(linker);

    // Substrate boots componentless (ADR-0010). Runtime load_component
    // is how components arrive; the scheduler's runtime-mutable table
    // accepts them as they're instantiated by the control plane.
    let scheduler = Scheduler::new(
        Arc::clone(&registry),
        Arc::clone(&queue),
        HashMap::new(),
        WORKERS,
    );

    // ADR-0021 subscriber table, shared with the control plane so
    // subscribe / unsubscribe / drop write through the same `Arc`
    // the platform thread reads when publishing events.
    let input_subscribers = aether_substrate::new_subscribers();

    // Build the event loop up front so we can hand its proxy to
    // `CaptureQueue` as a waker — the capture handler pokes the
    // proxy, which wakes the loop even when the window is occluded.
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Poll);

    // The capture queue handoff slot is shared between the chassis-
    // side capture handler (pushes requests) and the render thread
    // (drains on each redraw). Only desktop handles captures — core
    // doesn't know about it.
    let capture_queue = CaptureQueue::new();

    // Wire the ADR-0010 control plane. Registered after the scheduler
    // exists so the handler can capture the runtime component table
    // directly rather than an `Arc<Scheduler>` (which would cycle back
    // through the registry via this sink). The chassis_handler hook
    // registers desktop-specific kinds (capture_frame, set_window_mode,
    // platform_info); core dispatches only load/drop/replace/subscribe/
    // unsubscribe itself.
    let chassis_handler = chassis_control_handler(
        event_loop.create_proxy(),
        capture_queue.clone(),
        Arc::clone(&registry),
        Arc::clone(&queue),
        Arc::clone(&outbound),
    );
    {
        let control_plane = aether_substrate::ControlPlane {
            engine: Arc::clone(&engine),
            linker: Arc::clone(&linker),
            registry: Arc::clone(&registry),
            queue: Arc::clone(&queue),
            outbound: Arc::clone(&outbound),
            components: scheduler.components().clone(),
            input_subscribers: Arc::clone(&input_subscribers),
            default_name_counter: Arc::new(AtomicU64::new(0)),
            chassis_handler: Some(chassis_handler),
        };
        registry.register_sink(
            aether_substrate::AETHER_CONTROL,
            control_plane.into_sink_handler(),
        );
    }

    // Optional hub connection. If `AETHER_HUB_URL` is set, dial it and
    // keep the `HubClient` alive for the lifetime of the process so the
    // reader/heartbeat threads stay spawned. Failure to connect logs and
    // continues — the substrate still renders locally.
    let _hub = match std::env::var("AETHER_HUB_URL") {
        Ok(url) => match HubClient::connect(
            url.as_str(),
            "hello-triangle",
            env!("CARGO_PKG_VERSION"),
            boot_descriptors.clone(),
            Arc::clone(&registry),
            Arc::clone(&queue),
            Arc::clone(&outbound),
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!(
                    target: "aether_substrate::boot",
                    url = %url,
                    error = %e,
                    "hub connect failed",
                );
                None
            }
        },
        Err(_) => None,
    };

    tracing::info!(
        target: "aether_substrate::boot",
        workers = WORKERS,
        "componentless boot — close window to exit; load a component via aether.control.load_component",
    );

    let boot_kinds_count = boot_descriptors.len() as u32;
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
    let app = App {
        queue,
        input_subscribers,
        broadcast_mbox,
        kind_tick,
        kind_key,
        kind_mouse_button,
        kind_mouse_move,
        kind_frame_stats,
        frame_vertices,
        triangles_rendered: Arc::clone(&triangles_rendered),
        capture_queue,
        outbound: Arc::clone(&outbound),
        boot_kinds_count,
        window: None,
        gpu: None,
        started: None,
        frame: 0,
        occluded: false,
        boot_mode: boot_mode.clone(),
        boot_size,
        current_mode: boot_mode,
        _scheduler: scheduler,
    };

    let chassis = DesktopChassis {
        event_loop,
        app,
        triangles_rendered,
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
