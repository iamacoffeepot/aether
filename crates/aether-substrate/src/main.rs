// Milestone 3b frame-loop driver: the component emits a triangle as
// KIND_DRAW_TRIANGLE mail on each tick; a substrate-owned render sink
// appends the vertex bytes to a per-frame buffer. After wait_idle the
// main thread drains that buffer and hands it to the GPU, which
// uploads it to the fixed vertex buffer and issues one draw call.
//
// The heartbeat sink from milestones 1/2 is gone — the visible
// triangle is the proof-of-life, and a `triangles_rendered` counter
// doubles as a headless sanity signal.

mod render;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_mail::Kind;
use aether_mail::{encode, encode_empty};
use aether_substrate::{
    Component, MailQueue, Registry, Scheduler, SubstrateCtx, host_fns,
    mail::{Mail, MailboxId},
};
use aether_substrate_mail::{DrawTriangle, Key, MouseButton, MouseMove, Tick};
use render::Gpu;
use wasmtime::{Engine, Linker, Module};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

const HELLO_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello_component.wasm"));

const WORKERS: usize = 2;
const LOG_EVERY_FRAMES: u64 = 120;

struct App {
    queue: Arc<MailQueue>,
    component_mbox: MailboxId,
    kind_tick: u32,
    kind_key: u32,
    kind_mouse_button: u32,
    kind_mouse_move: u32,
    frame_vertices: Arc<Mutex<Vec<u8>>>,
    triangles_rendered: Arc<AtomicU64>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    started: Option<Instant>,
    frame: u64,
    // Scheduler is owned so its workers are joined on Drop when the event
    // loop exits — we never reference it otherwise.
    _scheduler: Scheduler,
}

impl ApplicationHandler for App {
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
            }
            WindowEvent::RedrawRequested => {
                self.queue.push(Mail::new(
                    self.component_mbox,
                    self.kind_tick,
                    encode_empty::<Tick>(),
                    1,
                ));
                self.queue.wait_idle();
                let verts = std::mem::take(&mut *self.frame_vertices.lock().unwrap());
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.render(&verts);
                }
                self.frame += 1;
                if self.frame.is_multiple_of(LOG_EVERY_FRAMES) {
                    eprintln!(
                        "  frame {:>5}  triangles_rendered={}",
                        self.frame,
                        self.triangles_rendered.load(Ordering::Relaxed),
                    );
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::KeyboardInput {
                event: key_event, ..
            } => {
                if key_event.state == ElementState::Pressed && !key_event.repeat {
                    let code: u32 = match key_event.physical_key {
                        PhysicalKey::Code(k) => k as u32,
                        PhysicalKey::Unidentified(_) => 0,
                    };
                    self.queue.push(Mail::new(
                        self.component_mbox,
                        self.kind_key,
                        encode(&Key { code }),
                        1,
                    ));
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            } => {
                self.queue.push(Mail::new(
                    self.component_mbox,
                    self.kind_mouse_button,
                    encode_empty::<MouseButton>(),
                    1,
                ));
            }
            WindowEvent::CursorMoved { position, .. } => {
                let payload = encode(&MouseMove {
                    x: position.x as f32,
                    y: position.y as f32,
                });
                self.queue.push(Mail::new(
                    self.component_mbox,
                    self.kind_mouse_move,
                    payload,
                    1,
                ));
            }
            _ => {}
        }
    }
}

fn main() -> wasmtime::Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, HELLO_WASM)?;

    let mut registry = Registry::new();
    let component_mbox = registry.register_component("hello");

    // Pre-register every substrate-owned kind by name so the component
    // can resolve them during `init` via the `resolve_kind` host fn.
    // Ids are dense and assigned in the order below; not otherwise
    // meaningful — consumers always resolve by name.
    let kind_tick = registry.register_kind(Tick::NAME);
    let kind_key = registry.register_kind(Key::NAME);
    let kind_mouse_button = registry.register_kind(MouseButton::NAME);
    let kind_mouse_move = registry.register_kind(MouseMove::NAME);
    registry.register_kind(DrawTriangle::NAME);

    let frame_vertices = Arc::new(Mutex::new(Vec::<u8>::with_capacity(4096)));
    let triangles_rendered = Arc::new(AtomicU64::new(0));

    let verts_for_sink = Arc::clone(&frame_vertices);
    let tris_for_sink = Arc::clone(&triangles_rendered);
    let sink_mbox = registry.register_sink(
        "render",
        Arc::new(move |bytes: &[u8], count: u32| {
            verts_for_sink.lock().unwrap().extend_from_slice(bytes);
            tris_for_sink.fetch_add(u64::from(count), Ordering::Relaxed);
        }),
    );

    // Mailbox contract: component=0, render sink=1. The component
    // hardcodes 1 as its send_mail recipient; assert here so a mismatch
    // is a loud panic, not a silent dropped mail.
    assert_eq!(component_mbox, MailboxId(0));
    assert_eq!(sink_mbox, MailboxId(1));

    let registry = Arc::new(registry);
    let queue = Arc::new(MailQueue::new());

    let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
    host_fns::register(&mut linker)?;

    let ctx = SubstrateCtx {
        sender: component_mbox,
        registry: Arc::clone(&registry),
        queue: Arc::clone(&queue),
    };
    let component = Component::instantiate(&engine, &linker, &module, ctx)?;

    let mut components = HashMap::new();
    components.insert(component_mbox, component);
    let scheduler = Scheduler::new(registry, Arc::clone(&queue), components, WORKERS);

    eprintln!(
        "aether-substrate: milestone 3b hello-triangle — {WORKERS} workers, close window to exit"
    );

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        queue,
        component_mbox,
        kind_tick,
        kind_key,
        kind_mouse_button,
        kind_mouse_move,
        frame_vertices,
        triangles_rendered: Arc::clone(&triangles_rendered),
        window: None,
        gpu: None,
        started: None,
        frame: 0,
        _scheduler: scheduler,
    };

    event_loop.run_app(&mut app)?;

    let total = triangles_rendered.load(Ordering::Relaxed);
    let elapsed = app.started.map(|s| s.elapsed()).unwrap_or_default();
    eprintln!(
        "\nran {} frames in {:.2}ms ({:.1} fps) — triangles rendered = {}",
        app.frame,
        elapsed.as_secs_f64() * 1000.0,
        app.frame as f64 / elapsed.as_secs_f64().max(0.001),
        total,
    );
    Ok(())
}
