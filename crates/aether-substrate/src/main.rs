// Milestone 2 frame-loop driver: winit owns the event loop and ticks
// the component on each redraw request. Input events are encoded into
// mail (per-family MailKind, byte payload) and pushed to the component
// as they arrive. Window close triggers scheduler shutdown via Drop.
//
// No GPU surface yet — the window is blank. The point of milestone 2
// is to prove that winit's cadence and events fit the mail envelope
// without re-architecting the library landed in milestone 1.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_substrate::{
    Component, MailQueue, Registry, Scheduler, SubstrateCtx, host_fns,
    mail::{Mail, MailboxId},
};
use wasmtime::{Engine, Linker, Module};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

const HELLO_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello_component.wasm"));

const KIND_TICK: u32 = 1;
const KIND_KEY: u32 = 10;
const KIND_MOUSE_BUTTON: u32 = 11;
const KIND_MOUSE_MOVE: u32 = 12;

const WORKERS: usize = 2;
const LOG_EVERY_FRAMES: u64 = 120;

struct App {
    queue: Arc<MailQueue>,
    component_mbox: MailboxId,
    heartbeats: Arc<AtomicU64>,
    last_key: Arc<Mutex<Option<u32>>>,
    window: Option<Arc<Window>>,
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
        let window = event_loop
            .create_window(Window::default_attributes().with_title("aether hello-winit"))
            .expect("create_window");
        window.request_redraw();
        self.window = Some(Arc::new(window));
        self.started = Some(Instant::now());
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                self.queue
                    .push(Mail::new(self.component_mbox, KIND_TICK, vec![], 1));
                self.queue.wait_idle();
                self.frame += 1;
                if self.frame.is_multiple_of(LOG_EVERY_FRAMES) {
                    let last = *self.last_key.lock().unwrap();
                    eprintln!(
                        "  frame {:>5}  heartbeats={}  last_key={:?}",
                        self.frame,
                        self.heartbeats.load(Ordering::Relaxed),
                        last,
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
                        KIND_KEY,
                        code.to_le_bytes().to_vec(),
                        1,
                    ));
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            } => {
                self.queue
                    .push(Mail::new(self.component_mbox, KIND_MOUSE_BUTTON, vec![], 1));
            }
            WindowEvent::CursorMoved { position, .. } => {
                let mut payload = Vec::with_capacity(8);
                payload.extend_from_slice(&(position.x as f32).to_le_bytes());
                payload.extend_from_slice(&(position.y as f32).to_le_bytes());
                self.queue
                    .push(Mail::new(self.component_mbox, KIND_MOUSE_MOVE, payload, 1));
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

    let heartbeats = Arc::new(AtomicU64::new(0));
    let last_key = Arc::new(Mutex::new(None::<u32>));

    let hb_for_sink = Arc::clone(&heartbeats);
    let last_key_for_sink = Arc::clone(&last_key);
    let sink_mbox = registry.register_sink(
        "heartbeat",
        Arc::new(move |bytes: &[u8], count: u32| {
            hb_for_sink.fetch_add(u64::from(count), Ordering::Relaxed);
            if bytes.len() >= 4 {
                let code = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                *last_key_for_sink.lock().unwrap() = Some(code);
            }
        }),
    );

    // Same contract as milestone 1: component=0, heartbeat sink=1. The
    // component hardcodes 1 in send_mail; assert here so a mismatch is a
    // loud panic, not a silent dropped mail.
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

    eprintln!("aether-substrate: milestone 2 winit loop — {WORKERS} workers, close window to exit");

    let event_loop = EventLoop::new()?;
    // Poll so RedrawRequested fires continuously; milestone 2 has no pacing
    // story yet — winit's default Wait would idle between events.
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        queue,
        component_mbox,
        heartbeats: Arc::clone(&heartbeats),
        last_key,
        window: None,
        started: None,
        frame: 0,
        _scheduler: scheduler,
    };

    event_loop.run_app(&mut app)?;

    let total = heartbeats.load(Ordering::Relaxed);
    let elapsed = app.started.map(|s| s.elapsed()).unwrap_or_default();
    eprintln!(
        "\nran {} frames in {:.2}ms ({:.1} fps) — heartbeats received = {}",
        app.frame,
        elapsed.as_secs_f64() * 1000.0,
        app.frame as f64 / elapsed.as_secs_f64().max(0.001),
        total,
    );
    Ok(())
}
