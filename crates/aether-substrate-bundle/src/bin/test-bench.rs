// Test-bench chassis binary entry point.
//
// Reads chassis-relevant env vars into a `TestBenchEnv`, asks
// `TestBenchChassis::build_passive` to assemble the substrate +
// passive capabilities (Log + Render) via the chassis_builder
// `Builder`, adds the io capability on the legacy
// `boot.add_capability` path, creates the offscreen `Gpu`, then
// drives the events_rx loop on the main thread. The chassis is
// embedder-driven (no `DriverCapability`) — `main()` IS the driver.
//
// In-process counterpart lives in `aether-substrate-test-bench::TestBench`
// (the `TestBench::start()` API ADR-0067 introduced); both paths
// share `TestBenchChassis::build_passive`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use aether_data::{Kind, encode_empty};
use aether_kinds::{AdvanceResult, CaptureFrameResult, IoCapability, Tick};
use aether_substrate::{
    Chassis, capabilities::IoAdapterBackend, capture::CaptureQueue, frame_loop, mail::Mail,
    subscribers_for,
};
use aether_substrate_bundle::test_bench::{
    TestBenchBuild, TestBenchChassis, TestBenchEnv, WORKERS,
    events::{self, ChassisEvent},
    render::Gpu,
};

/// Parse `AETHER_TEST_BENCH_SIZE=WxH`. Falls back to defaults on
/// missing/unparseable input with a warn log so scenario scripts can
/// see what dimensions they actually got.
fn parse_size_env() -> (u32, u32) {
    use aether_substrate_bundle::test_bench::{DEFAULT_HEIGHT, DEFAULT_WIDTH};
    let raw = match std::env::var("AETHER_TEST_BENCH_SIZE") {
        Ok(s) => s,
        Err(_) => return (DEFAULT_WIDTH, DEFAULT_HEIGHT),
    };
    match raw.split_once('x') {
        Some((w, h)) => match (w.parse::<u32>(), h.parse::<u32>()) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
            _ => {
                tracing::warn!(
                    target: "aether_substrate::boot",
                    value = %raw,
                    "AETHER_TEST_BENCH_SIZE unparseable — falling back to default",
                );
                (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            }
        },
        None => {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                "AETHER_TEST_BENCH_SIZE missing 'x' separator — falling back to default",
            );
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        }
    }
}

fn main() -> wasmtime::Result<()> {
    let capture_queue = CaptureQueue::new();
    let (events_tx, events_rx) = events::channel();

    // Per issue 464, this `main()` is the env-reading edge.
    let hub_url = std::env::var("AETHER_HUB_URL").ok();
    let namespace_roots = aether_substrate::capabilities::io::NamespaceRoots::from_env();

    let env = TestBenchEnv {
        name: "test-bench".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        workers: WORKERS,
        namespace_roots: Some(namespace_roots),
        hub_url,
        observed_kinds: None,
        events_tx,
        capture_queue: capture_queue.clone(),
    };

    let TestBenchBuild {
        passive,
        mut boot,
        render_handles,
        render_running,
        kind_tick,
        kind_frame_stats,
        hub,
    } = TestBenchChassis::build_passive(env)?;

    // Io facade on the `boot.add_facade` path (ADR-0075) —
    // the binary fails fast on adapter init failure (the in-process
    // API silent-skips for systems without writable default roots).
    let io_backend = IoAdapterBackend::new(boot.namespace_roots.clone(), Arc::clone(&boot.queue))?;
    boot.add_facade(IoCapability::new(io_backend))?;

    let (width, height) = parse_size_env();
    let gpu = Gpu::new(width, height, render_running);
    tracing::info!(
        target: "aether_substrate::boot",
        adapter = %gpu.adapter_info.name,
        backend = ?gpu.adapter_info.backend,
        device_type = ?gpu.adapter_info.device_type,
        width,
        height,
        workers = WORKERS,
        profile = TestBenchChassis::PROFILE,
        "test-bench componentless boot — drive ticks via aether.test_bench.advance",
    );

    drive_events_loop(
        events_rx,
        capture_queue,
        boot,
        passive,
        render_handles,
        gpu,
        kind_tick,
        kind_frame_stats,
        hub,
    )
}

/// Drive the chassis event loop on the main thread. Embedder is the
/// driver — runs until every `EventSender` clone drops (clean
/// shutdown via the chassis_control handler clones being released)
/// or a fatal abort tears the process down.
#[allow(clippy::too_many_arguments)]
fn drive_events_loop(
    events_rx: events::EventReceiver,
    capture_queue: CaptureQueue,
    boot: aether_substrate::SubstrateBoot,
    passive: aether_substrate::PassiveChassis<TestBenchChassis>,
    render_handles: aether_substrate::capabilities::RenderHandles,
    mut gpu: Gpu,
    kind_tick: aether_data::KindId,
    kind_frame_stats: aether_data::KindId,
    hub: Option<aether_substrate_bundle::hub::HubClient>,
) -> wasmtime::Result<()> {
    let queue = Arc::clone(&boot.queue);
    let outbound = Arc::clone(&boot.outbound);
    let input_subscribers = Arc::clone(&boot.input_subscribers);
    let broadcast_mbox = boot.broadcast_mbox;
    let frame_bound_pending = passive.frame_bound_pending();
    let started = Instant::now();
    let mut frame: u64 = 0;

    while let Ok(event) = events_rx.recv() {
        match event {
            ChassisEvent::Advance { reply_to, ticks } => {
                for _ in 0..ticks {
                    frame += 1;
                    run_frame(
                        frame,
                        started,
                        true,
                        &queue,
                        &outbound,
                        &input_subscribers,
                        broadcast_mbox,
                        kind_tick,
                        kind_frame_stats,
                        &capture_queue,
                        &render_handles,
                        &frame_bound_pending,
                        &mut gpu,
                    );
                }
                outbound.send_reply(
                    reply_to,
                    &AdvanceResult::Ok {
                        ticks_completed: ticks,
                    },
                );
            }
            ChassisEvent::CaptureRequested => {
                frame += 1;
                run_frame(
                    frame,
                    started,
                    false,
                    &queue,
                    &outbound,
                    &input_subscribers,
                    broadcast_mbox,
                    kind_tick,
                    kind_frame_stats,
                    &capture_queue,
                    &render_handles,
                    &frame_bound_pending,
                    &mut gpu,
                );
            }
        }
    }

    // Drop ordering: passive (chassis_builder Log + Render shut
    // down) → boot (legacy capabilities + scheduler join) → hub
    // (reader + heartbeat threads). Listed last-first since locals
    // drop in reverse declaration order.
    drop(passive);
    drop(boot);
    drop(hub);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_frame(
    frame: u64,
    started: Instant,
    dispatch_tick: bool,
    queue: &Arc<aether_substrate::Mailer>,
    outbound: &Arc<aether_substrate::HubOutbound>,
    input_subscribers: &aether_substrate::InputSubscribers,
    broadcast_mbox: aether_substrate::MailboxId,
    kind_tick: aether_data::KindId,
    kind_frame_stats: aether_data::KindId,
    capture_queue: &CaptureQueue,
    render_handles: &aether_substrate::capabilities::RenderHandles,
    frame_bound_pending: &[(
        aether_substrate::MailboxId,
        Arc<std::sync::atomic::AtomicU64>,
    )],
    gpu: &mut Gpu,
) {
    if dispatch_tick {
        let subs = subscribers_for(input_subscribers, Tick::ID);
        for mbox in subs {
            queue.push(Mail::new(mbox, kind_tick, encode_empty::<Tick>(), 1));
        }
    }
    frame_loop::drain_or_abort(queue, outbound);
    // ADR-0074 §Decision 5: render's inbox must quiesce before
    // submit so any DrawTriangle / aether.camera mail this frame is
    // integrated into the recorded pass.
    frame_loop::drain_frame_bound_or_abort(frame_bound_pending, outbound);

    match capture_queue.take() {
        Some(req) => {
            let result = match gpu.render_and_capture() {
                Ok(png) => CaptureFrameResult::Ok { png },
                Err(error) => CaptureFrameResult::Err { error },
            };
            for mail in req.after_mails {
                queue.push(mail);
            }
            outbound.send_reply(req.reply_to, &result);
        }
        None => {
            gpu.render();
        }
    }

    if frame.is_multiple_of(frame_loop::LOG_EVERY_FRAMES) {
        let triangles = render_handles.triangles_rendered.load(Ordering::Relaxed);
        frame_loop::emit_frame_stats(
            queue,
            broadcast_mbox,
            broadcast_mbox,
            kind_frame_stats,
            frame,
            triangles,
        );
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        tracing::info!(
            target: "aether_substrate::frame_loop",
            frame = frame,
            fps = frame as f64 / elapsed,
            triangles,
            "test-bench frame",
        );
    }
}
