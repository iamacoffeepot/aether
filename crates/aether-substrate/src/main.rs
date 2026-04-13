// Milestone 1 frame-loop driver. Compiles the `aether-hello-component`
// WASM guest (baked in by build.rs), sets up the registry with the
// component and a heartbeat sink, builds a scheduler with a small
// worker pool, and runs N frames at unthrottled rate. Each frame
// pushes one tick mail to the component; the component responds with
// a heartbeat mail to the sink. The sink increments a shared counter
// which we read at shutdown.
//
// Throttling is not this milestone's concern — winit owns timing in
// milestone 2.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_substrate::{
    Component, MailQueue, Registry, Scheduler, SubstrateCtx, host_fns,
    mail::{Mail, MailboxId},
};
use wasmtime::{Engine, Linker, Module};

const HELLO_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello_component.wasm"));

const TOTAL_FRAMES: u64 = 600;
const LOG_EVERY: u64 = 100;
const KIND_TICK: u32 = 1;
const WORKERS: usize = 2;

fn main() -> wasmtime::Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, HELLO_WASM)?;

    let mut registry = Registry::new();
    let component_mbox = registry.register_component("hello");

    let heartbeats = Arc::new(AtomicU64::new(0));
    let hb_for_sink = Arc::clone(&heartbeats);
    let sink_mbox = registry.register_sink(
        "heartbeat",
        Arc::new(move |_bytes, count| {
            hb_for_sink.fetch_add(u64::from(count), Ordering::Relaxed);
        }),
    );

    // Fixed mailbox contract for milestone 1: component=0, heartbeat sink=1.
    // The component's send_mail calls hardcode id 1; assert here so the
    // contract breaking is a loud panic, not a silent dropped mail.
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
    let _scheduler = Scheduler::new(registry, Arc::clone(&queue), components, WORKERS);

    eprintln!(
        "aether-substrate: milestone 1 frame loop — {TOTAL_FRAMES} frames, {WORKERS} worker threads"
    );

    let started = Instant::now();
    for frame in 1..=TOTAL_FRAMES {
        queue.push(Mail::new(component_mbox, KIND_TICK, vec![], 1));
        queue.wait_idle();
        if frame % LOG_EVERY == 0 {
            eprintln!(
                "  frame {frame:>4} / {TOTAL_FRAMES}  heartbeats={}",
                heartbeats.load(Ordering::Relaxed)
            );
        }
    }
    let elapsed = started.elapsed();

    let total = heartbeats.load(Ordering::Relaxed);
    eprintln!(
        "\nran {TOTAL_FRAMES} frames in {:.2}ms ({:.1} fps) — heartbeats received = {total}",
        elapsed.as_secs_f64() * 1000.0,
        TOTAL_FRAMES as f64 / elapsed.as_secs_f64(),
    );
    assert_eq!(total, TOTAL_FRAMES, "expected one heartbeat per frame");
    Ok(())
}
