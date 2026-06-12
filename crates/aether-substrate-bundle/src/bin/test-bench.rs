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

use std::env;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use aether_actor::Actor;
use aether_capabilities::LifecycleCapability;
use aether_capabilities::fs::NamespaceRoots;
use aether_data::{Kind, encode_empty, mailbox_id_from_name};
use aether_kinds::{AdvanceResult, CaptureFrameResult, LifecycleAdvance};
use aether_substrate::chassis::settlement::{
    SettlementRegistry, TerminalDisposition, WaitOutcome, await_internal_signal,
};
use aether_substrate::runtime::lifecycle;
use aether_substrate::{Chassis, capture::CaptureQueue, chassis::frame_loop, mail::MailboxId};

/// Cumulative patience cap for the per-frame settlement gates (advance +
/// capture pre-mail), matching the desktop driver. The per-round budget
/// is `frame_loop::DRAIN_BUDGET`; a starved-but-healthy chain resolves
/// before this cap, a genuine wedge exhausts it (issue #1305).
const FRAME_SETTLEMENT_CAP: Duration = Duration::from_secs(30);
use aether_substrate_bundle::chassis_root::next_chassis_correlation;
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
    let Ok(raw) = env::var("AETHER_TEST_BENCH_SIZE") else {
        return (DEFAULT_WIDTH, DEFAULT_HEIGHT);
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
    let namespace_roots = NamespaceRoots::from_env();

    let env = TestBenchEnv {
        name: "test-bench".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        workers: WORKERS,
        pool_workers: None,
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
// All arguments take ownership for the lifetime of the main-thread
// event loop; the borrow form would just be `&` versions threaded
// through the same closure.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
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
    let _ = kind_tick; // PR 3c retired the direct Tick push; the bin now
    // drives `LifecycleAdvance` and the lifecycle driver broadcasts Tick.
    // Chassis route-freezing: the bin wires its loop to the lifecycle cap's own
    // id (its NAMESPACE) — ctx-less driver setup, no sibling resolver in scope.
    #[allow(clippy::disallowed_methods)]
    let lifecycle_mailbox = mailbox_id_from_name(<LifecycleCapability as Actor>::NAMESPACE);
    let kind_lifecycle_advance = <LifecycleAdvance as Kind>::ID;
    let settlement_registry = Arc::clone(passive.settlement_registry());
    // ADR-0080 §6 chassis-root correlation counter (issue
    // iamacoffeepot/aether#723). Threaded through `run_frame` so each
    // advance push carries observable lineage like the desktop and
    // headless drivers do.
    let chassis_correlation = AtomicU64::new(1);

    while let Ok(event) = events_rx.recv() {
        match event {
            ChassisEvent::Advance { reply_to, ticks } => {
                for _ in 0..ticks {
                    run_frame(
                        true,
                        &queue,
                        &outbound,
                        lifecycle_mailbox,
                        kind_lifecycle_advance,
                        &settlement_registry,
                        &capture_queue,
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
                    lifecycle_mailbox,
                    kind_lifecycle_advance,
                    &settlement_registry,
                    &capture_queue,
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
    lifecycle_mailbox: MailboxId,
    kind_lifecycle_advance: aether_data::KindId,
    settlement_registry: &Arc<SettlementRegistry>,
    capture_queue: &CaptureQueue,
    gpu: &mut Gpu,
    chassis_correlation: &AtomicU64,
) {
    if dispatch_tick {
        // ADR-0082 §6 / PR 3c: push LifecycleAdvance and wait for the
        // frame chain to settle before submit. Settlement replaces the
        // retired `drain_frame_bound_or_abort` pending-counter poll —
        // it waits for the whole Tick → component → DrawTriangle →
        // render chain rather than just render's inbox counter.
        let advance_root = queue.push_chassis_root_mail(
            next_chassis_correlation(chassis_correlation),
            lifecycle_mailbox,
            kind_lifecycle_advance,
            encode_empty::<LifecycleAdvance>(),
            1,
        );
        let rx = settlement_registry.subscribe_settlement(advance_root);
        // Reconciled to `Abort` to match the desktop advance gate
        // (issue #1305): a frame chain that never settles is a wedged
        // dispatcher, not a "submit anyway" — escalating patience under
        // the wait, fail-fast on a genuine wedge (ADR-0063).
        if let WaitOutcome::Wedged(wedge) = await_internal_signal(
            &rx,
            "test_bench_bin.frame_advance",
            frame_loop::DRAIN_BUDGET,
            FRAME_SETTLEMENT_CAP,
            TerminalDisposition::Abort,
        ) {
            lifecycle::fatal_abort(outbound, wedge.reason());
        }
    }

    match capture_queue.take() {
        Some(req) => {
            // iamacoffeepot/aether#860: wait for pre-mail settlement
            // before rendering (mirrors the test-bench lib fix). The
            // standalone bin replies `Err` on stuck-chain rather than
            // bailing out of the frame loop.
            let mut pre_failed: Option<String> = None;
            for rx in req.pre_settlements {
                if let WaitOutcome::Wedged(wedge) = await_internal_signal(
                    &rx,
                    "test_bench_bin.capture_pre_mail",
                    frame_loop::DRAIN_BUDGET,
                    FRAME_SETTLEMENT_CAP,
                    TerminalDisposition::ReplyErr,
                ) {
                    pre_failed = Some(wedge.reason());
                    break;
                }
            }
            let result = pre_failed.map_or_else(
                || CaptureFrameResult::from(gpu.render_and_capture()),
                |error| CaptureFrameResult::Err { error },
            );
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
