// Test-bench chassis binary entry point.
//
// Reads chassis-relevant env vars into a `TestBenchEnv`, asks
// `TestBenchChassis::build_passive` to assemble the substrate plus
// every capability (Log, Render, Io if roots pre-validate, etc.)
// through the chassis_builder `Builder`, creates the offscreen
// `Gpu`, then drives the events_rx loop on the main thread. The
// chassis is embedder-driven (no `DriverCapability`) — `main()` IS
// the driver.
//
// In-process counterpart lives in `aether-substrate-test-bench::TestBench`
// (the `TestBench::start()` API ADR-0067 introduced); both paths
// share `TestBenchChassis::build_passive`.

use std::sync::Arc;

use aether_actor::Actor;
use aether_capabilities::InputCapability;
use aether_data::{encode_empty, mailbox_id_from_name};
use aether_kinds::{AdvanceResult, CaptureFrameResult, Tick};
use aether_substrate::{Chassis, capture::CaptureQueue, chassis::frame_loop, mail::MailboxId};
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
    if let Some((w, h)) = raw.split_once('x') {
        match (w.parse::<u32>(), h.parse::<u32>()) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h),
            _ => {
                tracing::warn!(
                    target: "aether_substrate::boot",
                    value = %raw,
                    "AETHER_TEST_BENCH_SIZE unparseable — falling back to default",
                );
                (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            }
        }
    } else {
        tracing::warn!(
            target: "aether_substrate::boot",
            value = %raw,
            "AETHER_TEST_BENCH_SIZE missing 'x' separator — falling back to default",
        );
        (DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }
}

fn main() -> anyhow::Result<()> {
    let capture_queue = CaptureQueue::new();
    let (events_tx, events_rx) = events::channel();

    // Per issue 464, this `main()` is the env-reading edge.
    let namespace_roots = aether_capabilities::fs::NamespaceRoots::from_env();

    let env = TestBenchEnv {
        name: "test-bench".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        workers: WORKERS,
        observed_kinds: None,
        events_tx,
        capture_queue: capture_queue.clone(),
        namespace_roots: Some(namespace_roots),
    };

    let TestBenchBuild {
        passive,
        boot,
        render_handles,
        kind_tick,
    } = TestBenchChassis::build_passive(env)?;

    let (width, height) = parse_size_env();
    let gpu = Gpu::new(width, height, render_handles);
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

    drive_events_loop(events_rx, capture_queue, boot, passive, gpu, kind_tick);
    Ok(())
}

/// Drive the chassis event loop on the main thread. Embedder is the
/// driver — runs until every `EventSender` clone drops (clean
/// shutdown via the `chassis_control` handler clones being released)
/// or a fatal abort tears the process down.
#[allow(clippy::too_many_arguments)]
fn drive_events_loop(
    events_rx: events::EventReceiver,
    capture_queue: CaptureQueue,
    boot: aether_substrate::SubstrateBoot,
    passive: aether_substrate::PassiveChassis<TestBenchChassis>,
    mut gpu: Gpu,
    kind_tick: aether_data::KindId,
) {
    let queue = Arc::clone(&boot.queue);
    let outbound = Arc::clone(&boot.outbound);
    let input_mailbox = mailbox_id_from_name(InputCapability::NAMESPACE);
    let frame_bound_pending = passive.frame_bound_pending();
    // ADR-0080 §6 chassis-root correlation counter (issue
    // iamacoffeepot/aether#723). Threaded through `run_frame` so each
    // tick push carries observable lineage like the desktop and
    // headless drivers do.
    let chassis_correlation = std::sync::atomic::AtomicU64::new(1);

    while let Ok(event) = events_rx.recv() {
        match event {
            ChassisEvent::Advance { reply_to, ticks } => {
                for _ in 0..ticks {
                    run_frame(
                        true,
                        &queue,
                        &outbound,
                        input_mailbox,
                        kind_tick,
                        &capture_queue,
                        &frame_bound_pending,
                        &mut gpu,
                        &chassis_correlation,
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
                run_frame(
                    false,
                    &queue,
                    &outbound,
                    input_mailbox,
                    kind_tick,
                    &capture_queue,
                    &frame_bound_pending,
                    &mut gpu,
                    &chassis_correlation,
                );
            }
        }
    }

    // Drop ordering: passive (chassis_builder Log + Render shut
    // down) → boot (legacy capabilities + scheduler join). Listed
    // last-first since locals drop in reverse declaration order.
    drop(passive);
    drop(boot);
}

#[allow(clippy::too_many_arguments)]
fn run_frame(
    dispatch_tick: bool,
    queue: &Arc<aether_substrate::Mailer>,
    outbound: &Arc<aether_substrate::HubOutbound>,
    input_mailbox: MailboxId,
    kind_tick: aether_data::KindId,
    capture_queue: &CaptureQueue,
    frame_bound_pending: &[(
        aether_substrate::MailboxId,
        Arc<std::sync::atomic::AtomicU64>,
    )],
    gpu: &mut Gpu,
    chassis_correlation: &std::sync::atomic::AtomicU64,
) {
    let next_correlation = || -> u64 {
        let id = chassis_correlation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if id == 0 {
            chassis_correlation.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        } else {
            id
        }
    };

    if dispatch_tick {
        aether_substrate::runtime::trace::push_chassis_root_mail(
            queue,
            next_correlation(),
            input_mailbox,
            kind_tick,
            encode_empty::<Tick>(),
            1,
        );
    }
    // ADR-0074 §Decision 5: render's inbox must quiesce before
    // submit so any DrawTriangle / aether.camera mail this frame is
    // integrated into the recorded pass. (The pre-Phase-4 component
    // drain barrier is retired; trampoline traps fail-fast directly
    // via `NativeBinding::fatal_abort`.)
    frame_loop::drain_frame_bound_or_abort(frame_bound_pending, outbound);

    match capture_queue.take() {
        Some(req) => {
            // iamacoffeepot/aether#860: wait for pre-mail settlement
            // before rendering (mirrors the test-bench lib fix). The
            // standalone bin replies `Err` on stuck-chain rather than
            // bailing out of the frame loop.
            let mut pre_failed: Option<String> = None;
            for rx in req.pre_settlements {
                if rx.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
                    pre_failed = Some(
                        "capture pre-mail chain failed to settle within 5s — \
                         a downstream cap is wedged"
                            .to_owned(),
                    );
                    break;
                }
            }
            let result = match pre_failed {
                Some(error) => CaptureFrameResult::Err { error },
                None => match gpu.render_and_capture() {
                    Ok(png) => CaptureFrameResult::Ok { png },
                    Err(error) => CaptureFrameResult::Err { error },
                },
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
}
