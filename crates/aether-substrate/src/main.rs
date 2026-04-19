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

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub, SessionToken};
use aether_kinds::{
    CaptureFrameResult, EngineInfo, FrameStats, GpuBackend, GpuDeviceType, GpuInfo, InputStream,
    Key, MonitorInfo, MouseButton, MouseMove, OsInfo, PlatformInfoResult, Tick, VideoMode,
    WindowInfo, WindowMode,
};
use aether_mail::Kind;
use aether_mail::{encode, encode_empty};
use aether_substrate::{
    CaptureQueue, HUB_CLAUDE_BROADCAST, HubClient, HubOutbound, InputSubscribers, MailQueue,
    Registry, Scheduler, SubstrateCtx,
    capture::CaptureWaker,
    host_fns,
    mail::{Mail, MailboxId},
    platform_info::PlatformInfoNotifier,
    subscribers_for,
};
use render::Gpu;
use wasmtime::{Engine, Linker};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// Events the event loop can receive from outside the winit thread.
/// Each variant corresponds to a control-plane request that needs to
/// run on the event-loop thread — either because winit APIs require
/// it (monitor enumeration, window state) or because the work has to
/// be ordered with frame rendering (captures).
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    /// A capture is pending on `CaptureQueue`; wake the loop so its
    /// `RedrawRequested` handler pulls and fulfils it, even if the
    /// window is occluded (and `ControlFlow::Wait` would otherwise
    /// keep the loop asleep).
    CaptureRequested,
    /// An MCP session asked for a `platform_info` snapshot. The
    /// handler is fire-and-forget — the sender rides inline, the
    /// event loop snapshots on receipt and replies via `outbound`.
    PlatformInfoRequested { sender: SessionToken },
}

/// Adapter that bridges `CaptureQueue::request` to the winit event
/// loop. `request()` pokes `wake()`, which sends a `CaptureRequested`
/// user event; `App::user_event` then runs a render pass even if the
/// window is occluded so the capture resolves.
struct CaptureRequestWaker {
    proxy: EventLoopProxy<UserEvent>,
}

impl CaptureWaker for CaptureRequestWaker {
    fn wake(&self) {
        // `send_event` only fails if the event loop has shut down; in
        // that case nothing listens for captures anyway.
        let _ = self.proxy.send_event(UserEvent::CaptureRequested);
    }
}

/// Adapter that bridges `ControlPlane`'s `platform_info_notifier` to
/// the winit event loop — same idea as `CaptureRequestWaker` but the
/// per-request payload (the originating session token) rides inline
/// on the event itself, so no shared queue is needed.
struct PlatformInfoProxy {
    proxy: EventLoopProxy<UserEvent>,
}

impl PlatformInfoNotifier for PlatformInfoProxy {
    fn notify(&self, sender: SessionToken) {
        let _ = self
            .proxy
            .send_event(UserEvent::PlatformInfoRequested { sender });
    }
}

const WORKERS: usize = 2;
const LOG_EVERY_FRAMES: u64 = 120;

struct App {
    queue: Arc<MailQueue>,
    /// ADR-0021 per-stream subscribers. Shared with the control plane
    /// so subscribe / unsubscribe / drop write through the same table
    /// the platform thread reads on each event. Empty sets — the
    /// boot state — mean the event is dropped at the source.
    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: u32,
    kind_key: u32,
    kind_mouse_button: u32,
    kind_mouse_move: u32,
    kind_frame_stats: u32,
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
    // Scheduler is owned so its workers are joined on Drop when the event
    // loop exits — we never reference it otherwise.
    _scheduler: Scheduler,
}

/// Encode + send a `CaptureFrameResult` reply addressed to the
/// originating session. Silent if the outbound is disconnected (no
/// hub attached) — the result goes nowhere, which matches the
/// broadcast sink's behavior for observations.
fn send_capture_reply(
    outbound: &HubOutbound,
    sender: aether_hub_protocol::SessionToken,
    result: CaptureFrameResult,
) {
    let payload = match postcard::to_allocvec(&result) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::capture",
                error = %e,
                "capture result encode failed",
            );
            return;
        }
    };
    outbound.send(EngineToHub::Mail(EngineMailFrame {
        address: ClaudeAddress::Session(sender),
        kind_name: CaptureFrameResult::NAME.to_owned(),
        payload,
        origin: None,
    }));
}

/// Encode + send a `PlatformInfoResult` reply addressed at the
/// originating session. Mirrors `send_capture_reply` in shape — silent
/// on disconnected outbound since `PlatformInfoNotifier` fired without
/// awaiting an ack anyway.
fn send_platform_info_reply(
    outbound: &HubOutbound,
    sender: aether_hub_protocol::SessionToken,
    result: PlatformInfoResult,
) {
    let payload = match postcard::to_allocvec(&result) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::platform_info",
                error = %e,
                "platform_info result encode failed",
            );
            return;
        }
    };
    outbound.send(EngineToHub::Mail(EngineMailFrame {
        address: ClaudeAddress::Session(sender),
        kind_name: PlatformInfoResult::NAME.to_owned(),
        payload,
        origin: None,
    }));
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
                error: "platform info requested before GPU / window came up".to_owned(),
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
                // PR B will track the active mode as actual state;
                // today the substrate only boots windowed.
                mode: WindowMode::Windowed,
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
            UserEvent::CaptureRequested => {
                // When occluded, `ControlFlow::Wait` stops the normal
                // redraw cadence — request one explicitly so the
                // capture handler in `RedrawRequested` runs.
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::PlatformInfoRequested { sender } => {
                let result = self.snapshot_platform_info(event_loop);
                send_platform_info_reply(&self.outbound, sender, result);
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_title("aether hello-triangle"))
                .expect("create_window"),
        );
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
                            send_capture_reply(&self.outbound, req.sender, result);
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
                    send_capture_reply(
                        &self.outbound,
                        req.sender,
                        CaptureFrameResult::Err {
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
            move |_kind: &str, _origin: Option<&str>, _sender, bytes: &[u8], count: u32| {
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
                move |kind_name: &str,
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

    // Shared capture-request slot between the control plane (where
    // the request arrives) and the render thread (which fulfils it).
    let capture_queue = CaptureQueue::with_waker(Arc::new(CaptureRequestWaker {
        proxy: event_loop.create_proxy(),
    }));

    // Fire-and-forget notifier for `platform_info`. Carries the
    // sender inline on each user event — no shared queue required.
    let platform_info_notifier = Arc::new(PlatformInfoProxy {
        proxy: event_loop.create_proxy(),
    });

    // Wire the ADR-0010 control plane. Registered after the scheduler
    // exists so the handler can capture the runtime component table
    // directly rather than an `Arc<Scheduler>` (which would cycle back
    // through the registry via this sink).
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
            capture_queue: capture_queue.clone(),
            platform_info_notifier: platform_info_notifier.clone(),
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
    let mut app = App {
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
        _scheduler: scheduler,
    };

    event_loop.run_app(&mut app)?;

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
