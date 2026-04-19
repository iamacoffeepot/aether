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
    CaptureFrameResult, FrameStats, InputStream, Key, MouseButton, MouseMove, Tick,
};
use aether_mail::Kind;
use aether_mail::{encode, encode_empty};
use aether_substrate::{
    CaptureQueue, HUB_CLAUDE_BROADCAST, HubClient, HubOutbound, InputSubscribers, MailQueue,
    Registry, Scheduler, SubstrateCtx,
    capture::CaptureWaker,
    host_fns,
    mail::{Mail, MailboxId},
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
/// The one variant exists so `CaptureRequestWaker` can nudge the loop
/// when a capture lands while the window is occluded — without it the
/// loop would sleep in `ControlFlow::Wait` and the capture would hang.
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    CaptureRequested,
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

impl App {
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
    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::CaptureRequested => {
                // When occluded, `ControlFlow::Wait` stops the normal
                // redraw cadence — request one explicitly so the
                // capture handler in `RedrawRequested` runs.
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
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
