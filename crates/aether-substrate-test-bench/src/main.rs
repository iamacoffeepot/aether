// Test-bench chassis binary (ADR-0067). GPU-capable, no window, no
// winit. wgpu initialises without a presentation surface; every
// frame renders into an offscreen color target paired with a depth
// target; capture_frame reads back from that same offscreen.
//
// Tick driver is control-mail (ADR-0067): the chassis loop blocks
// waiting for `aether.test_bench.advance { ticks }` events from the
// chassis-control handler. Each Advance runs `ticks` complete
// frames (Tick fanout → drain → render-or-capture), then replies
// with `AdvanceResult::Ok`. Capture-frame requests wake the loop
// for one drain → render-with-capture cycle without dispatching
// Tick (capture observes; advance ticks). With no Advance, the
// world doesn't tick — the chassis is fully deterministic.

mod chassis;
mod events;
mod render;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aether_kinds::{AdvanceResult, CaptureFrameResult, FrameStats, InputStream, Tick};
use aether_mail::{Kind, encode, encode_empty};
use aether_substrate_core::{
    Chassis, ChassisCapabilities, HubOutbound, InputSubscribers, Mailer, ReplyTo, Scheduler,
    SubstrateBoot,
    capture::CaptureQueue,
    mail::{Mail, MailboxId},
    subscribers_for,
};

use crate::events::{ChassisEvent, EventReceiver};
use crate::render::{Gpu, IDENTITY_VIEW_PROJ, VERTEX_BUFFER_BYTES};

const WORKERS: usize = 2;
const LOG_EVERY_FRAMES: u64 = 120;

/// Wire size of one `aether.draw_triangle` mail item: three
/// vertices, each `f32 x,y,z + f32 r,g,b` (24 bytes). The render
/// sink clamps at whole-triangle multiples so we never write a
/// half-triangle into the GPU vertex buffer when the cap forces a
/// truncate.
const DRAW_TRIANGLE_BYTES: usize = 72;

/// Default offscreen target size when `AETHER_TEST_BENCH_SIZE` is
/// unset. 800x600 matches the scenario harness convention — large
/// enough that `min_non_bg_pixels` thresholds discriminate, small
/// enough that capture readback is cheap.
const DEFAULT_WIDTH: u32 = 800;
const DEFAULT_HEIGHT: u32 = 600;

/// ADR-0063 fail-fast budget for the per-tick drain barrier. Same
/// 5-second value desktop and headless use — same dispatcher kernel.
const DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Test-bench chassis. Owns the event loop, the GPU, the shared
/// frame state (vertex buffer, camera matrix), and the capture
/// queue. `run(self)` blocks on the event receiver — the loop
/// returns only when every sender has been dropped (chassis
/// shutdown). Process exit on SIGTERM (hub-spawned) is caught by
/// the chassis-control handler dropping its sender; SIGINT (manual
/// run) terminates the process directly.
struct TestBenchChassis {
    queue: Arc<Mailer>,
    input_subscribers: InputSubscribers,
    broadcast_mbox: MailboxId,
    kind_tick: u64,
    kind_frame_stats: u64,
    gpu: Gpu,
    frame_vertices: Arc<Mutex<Vec<u8>>>,
    camera_state: Arc<Mutex<[f32; 16]>>,
    triangles_rendered: Arc<AtomicU64>,
    capture_queue: CaptureQueue,
    outbound: Arc<HubOutbound>,
    events_rx: EventReceiver,
    _scheduler: Scheduler,
    _hub: Option<aether_substrate_core::HubClient>,
}

impl Chassis for TestBenchChassis {
    const KIND: &'static str = "test-bench";
    const CAPABILITIES: ChassisCapabilities = ChassisCapabilities {
        has_gpu: true,
        has_window: false,
        has_tcp_listener: false,
    };

    fn run(mut self) -> wasmtime::Result<()> {
        let started = Instant::now();
        let mut frame: u64 = 0;
        // `recv()` returns Err only when every sender has been
        // dropped — that's chassis shutdown, exit the loop cleanly.
        while let Ok(event) = self.events_rx.recv() {
            match event {
                ChassisEvent::Advance { reply_to, ticks } => {
                    for _ in 0..ticks {
                        frame += 1;
                        self.run_frame(frame, started, /* dispatch_tick */ true);
                    }
                    self.outbound.send_reply(
                        reply_to,
                        &AdvanceResult::Ok {
                            ticks_completed: ticks,
                        },
                    );
                }
                ChassisEvent::CaptureRequested => {
                    frame += 1;
                    self.run_frame(frame, started, /* dispatch_tick */ false);
                }
            }
        }
        Ok(())
    }
}

impl TestBenchChassis {
    /// Run one frame: optionally dispatch `Tick` to subscribers,
    /// drain the queue with the ADR-0063 budget, take any pending
    /// capture and render-with-capture (otherwise plain render),
    /// emit periodic frame_stats. Any death or wedge mid-drain
    /// aborts the chassis cleanly via `lifecycle::fatal_abort`.
    fn run_frame(&mut self, frame: u64, started: Instant, dispatch_tick: bool) {
        if dispatch_tick {
            let subs = subscribers_for(&self.input_subscribers, InputStream::Tick);
            for mbox in subs {
                self.queue
                    .push(Mail::new(mbox, self.kind_tick, encode_empty::<Tick>(), 1));
            }
        }
        let summary = self.queue.drain_all_with_budget(DRAIN_BUDGET);
        if let Some((mailbox, waited)) = summary.wedged {
            aether_substrate_core::lifecycle::fatal_abort(
                &self.outbound,
                format!("dispatcher wedged: mailbox={mailbox} waited={waited:?}"),
            );
        }
        if let Some(first) = summary.deaths.first() {
            for d in &summary.deaths {
                tracing::error!(
                    target: "aether_substrate::lifecycle",
                    mailbox = %d.mailbox,
                    mailbox_name = %d.mailbox_name,
                    last_kind = %d.last_kind,
                    reason = %d.reason,
                    "component died; substrate aborting (ADR-0063)",
                );
            }
            aether_substrate_core::lifecycle::fatal_abort(
                &self.outbound,
                format!(
                    "component died: {} (kind {}) — {}",
                    first.mailbox_name, first.last_kind, first.reason,
                ),
            );
        }

        // Drain accumulated vertices and the latest camera. Replace
        // the vertex buffer with an empty same-capacity Vec so the
        // 4 MiB allocation isn't rebuilt every frame.
        let verts = std::mem::replace(
            &mut *self.frame_vertices.lock().unwrap(),
            Vec::with_capacity(VERTEX_BUFFER_BYTES),
        );
        let view_proj = *self.camera_state.lock().unwrap();

        match self.capture_queue.take() {
            Some(req) => {
                let result = match self.gpu.render_and_capture(&verts, &view_proj) {
                    Ok(png) => CaptureFrameResult::Ok { png },
                    Err(error) => CaptureFrameResult::Err { error },
                };
                for mail in req.after_mails {
                    self.queue.push(mail);
                }
                self.outbound.send_reply(req.reply_to, &result);
            }
            None => {
                self.gpu.render(&verts, &view_proj);
            }
        }

        if frame.is_multiple_of(LOG_EVERY_FRAMES) {
            let triangles = self.triangles_rendered.load(Ordering::Relaxed);
            let stats = FrameStats { frame, triangles };
            self.queue.push(Mail::new(
                self.broadcast_mbox,
                self.kind_frame_stats,
                encode(&stats),
                1,
            ));
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
}

/// Parse `AETHER_TEST_BENCH_SIZE=WxH`. Falls back to defaults on
/// missing/unparseable input with a warn log so scenario scripts can
/// see what dimensions they actually got.
fn parse_size_env() -> (u32, u32) {
    let raw = match std::env::var("AETHER_TEST_BENCH_SIZE") {
        Ok(s) => s,
        Err(_) => return (DEFAULT_WIDTH, DEFAULT_HEIGHT),
    };
    let trimmed = raw.trim();
    if let Some((w, h)) = trimmed.split_once('x')
        && let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>())
        && w > 0
        && h > 0
    {
        return (w, h);
    }
    tracing::warn!(
        target: "aether_substrate::boot",
        value = %raw,
        "AETHER_TEST_BENCH_SIZE unparseable — falling back to default 800x600",
    );
    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
}

fn main() -> wasmtime::Result<()> {
    let capture_queue = CaptureQueue::new();
    let (events_tx, events_rx) = events::channel();

    let boot = SubstrateBoot::builder("test-bench", env!("CARGO_PKG_VERSION"))
        .workers(WORKERS)
        .chassis_handler({
            let cq = capture_queue.clone();
            let tx = events_tx.clone();
            move |ctx| {
                Some(chassis::chassis_control_handler(
                    cq,
                    tx,
                    Arc::clone(ctx.registry),
                    Arc::clone(ctx.queue),
                    Arc::clone(ctx.outbound),
                ))
            }
        })
        .build()?;

    let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
    let kind_frame_stats = boot
        .registry
        .kind_id(FrameStats::NAME)
        .expect("FrameStats registered");

    // `aether.sink.render`: real renderer. The frame loop drains
    // `frame_vertices` each frame, so every `DrawTriangle` emitted
    // before the next frame is consolidated into one vertex buffer.
    // Truncate at the sink boundary so a single oversized mesh
    // degrades gracefully.
    let frame_vertices = Arc::new(Mutex::new(Vec::<u8>::with_capacity(VERTEX_BUFFER_BYTES)));
    let triangles_rendered = Arc::new(AtomicU64::new(0));
    {
        let verts_for_sink = Arc::clone(&frame_vertices);
        let tris_for_sink = Arc::clone(&triangles_rendered);
        boot.registry.register_sink(
            "aether.sink.render",
            Arc::new(
                move |_kind_id: u64,
                      _kind_name: &str,
                      _origin: Option<&str>,
                      _sender: ReplyTo,
                      bytes: &[u8],
                      _count: u32| {
                    let mut verts = verts_for_sink.lock().unwrap();
                    let available = VERTEX_BUFFER_BYTES.saturating_sub(verts.len());
                    let write_len = bytes.len().min(available);
                    let write_len = write_len - (write_len % DRAW_TRIANGLE_BYTES);
                    if write_len > 0 {
                        verts.extend_from_slice(&bytes[..write_len]);
                        tris_for_sink
                            .fetch_add((write_len / DRAW_TRIANGLE_BYTES) as u64, Ordering::Relaxed);
                    }
                    if write_len < bytes.len() {
                        tracing::warn!(
                            target: "aether_substrate::render",
                            accepted_bytes = write_len,
                            dropped_bytes = bytes.len() - write_len,
                            cap = VERTEX_BUFFER_BYTES,
                            "render sink dropped triangles beyond fixed vertex buffer",
                        );
                    }
                },
            ),
        );
    }

    // `aether.sink.camera`: latest-value-wins. Decode the 64-byte
    // column-major view_proj and store; the frame loop reads it
    // each frame and uploads to the GPU uniform.
    let camera_state = Arc::new(Mutex::new(IDENTITY_VIEW_PROJ));
    {
        let cam_for_sink = Arc::clone(&camera_state);
        boot.registry.register_sink(
            "aether.sink.camera",
            Arc::new(
                move |_kind_id: u64,
                      _kind_name: &str,
                      _origin: Option<&str>,
                      _sender: ReplyTo,
                      bytes: &[u8],
                      _count: u32| {
                    if bytes.len() != 64 {
                        tracing::warn!(
                            target: "aether_substrate::camera",
                            got = bytes.len(),
                            expected = 64,
                            "camera sink: payload length mismatch, dropping",
                        );
                        return;
                    }
                    match bytemuck::try_pod_read_unaligned::<[f32; 16]>(bytes) {
                        Ok(mat) => *cam_for_sink.lock().unwrap() = mat,
                        Err(e) => tracing::warn!(
                            target: "aether_substrate::camera",
                            error = %e,
                            "camera sink: cast failed, dropping",
                        ),
                    }
                },
            ),
        );
    }

    // `aether.sink.io` per ADR-0041. Test-bench gets the same sink
    // as desktop / headless — the io path is purely I/O. Boot-time
    // filesystem failure logs loud and skips the sink (same policy
    // as the other chassis) rather than failing the whole chassis.
    match aether_substrate_core::io::build_default_registry() {
        Ok((registry, roots)) => {
            tracing::info!(
                target: "aether_substrate::io",
                save = %roots.save.display(),
                assets = %roots.assets.display(),
                config = %roots.config.display(),
                "io adapters registered",
            );
            boot.registry.register_sink(
                "aether.sink.io",
                aether_substrate_core::io::io_sink_handler(registry, Arc::clone(&boot.queue)),
            );
        }
        Err(e) => {
            tracing::error!(
                target: "aether_substrate::io",
                error = %e,
                "io adapter init failed — `io` sink not registered",
            );
        }
    }

    // `aether.sink.log` per ADR-0060. Same handler as desktop and
    // headless — guest log mail is independent of GPU / windowing.
    aether_substrate_core::log_sink::register_log_sink(&boot.registry);

    // ADR-0067 sink set is render + camera + io + log. Audio is
    // skipped (no cpal — scenarios don't need audio output and
    // skipping it removes a flaky-driver surface on CI runners).
    // Net is also out for v1 — scenarios don't need network
    // egress, and including it would add a real I/O side channel.

    let (width, height) = parse_size_env();
    let gpu = Gpu::new(width, height);
    tracing::info!(
        target: "aether_substrate::boot",
        adapter = %gpu.adapter_info.name,
        backend = ?gpu.adapter_info.backend,
        device_type = ?gpu.adapter_info.device_type,
        width,
        height,
        workers = WORKERS,
        "test-bench componentless boot — drive ticks via aether.test_bench.advance",
    );

    let hub = boot.connect_hub_from_env()?;

    // The chassis owns its receiver; the chassis-control handler
    // already holds a clone of the sender (captured into the boot
    // closure above). Drop the local `events_tx` so the receiver
    // hangs up cleanly when every chassis_control_handler clone is
    // released — matches the ADR-0063 lifecycle.
    drop(events_tx);

    let chassis = TestBenchChassis {
        queue: boot.queue,
        input_subscribers: boot.input_subscribers,
        broadcast_mbox: boot.broadcast_mbox,
        kind_tick,
        kind_frame_stats,
        gpu,
        frame_vertices,
        camera_state,
        triangles_rendered,
        capture_queue,
        outbound: boot.outbound,
        events_rx,
        _scheduler: boot.scheduler,
        _hub: hub,
    };
    tracing::info!(
        target: "aether_substrate::boot",
        kind = TestBenchChassis::KIND,
        has_gpu = TestBenchChassis::CAPABILITIES.has_gpu,
        has_window = TestBenchChassis::CAPABILITIES.has_window,
        has_tcp_listener = TestBenchChassis::CAPABILITIES.has_tcp_listener,
        "chassis initialised",
    );
    chassis.run()
}
