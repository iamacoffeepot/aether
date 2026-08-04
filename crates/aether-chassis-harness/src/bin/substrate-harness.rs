// Test-harness chassis binary entry point.
//
// Reads chassis-relevant env vars into a `SubstrateHarnessEnv`, asks
// `SubstrateHarnessChassis::build_passive` to assemble the substrate plus
// every capability (Log, Io if roots pre-validate, etc.) through the
// chassis_builder `Builder`, boots the pumped `aether.render` actor
// offscreen, then drives the events_rx loop on the main thread. The chassis
// is embedder-driven (no `DriverCapability`) — `main()` IS the driver, the
// pump host for the render slot.
//
// In-process counterpart lives in `aether-harness-substrate::SubstrateHarness`
// (the `SubstrateHarness::start()` API ADR-0067 introduced); both paths
// share `SubstrateHarnessChassis::build_passive` and the pumped render path
// (ADR-0161).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_data::{Kind, KindId, encode_empty, mailbox_id_from_name};
use aether_fs::NamespaceRoots;
use aether_kinds::{AdvanceResult, LifecycleAdvance};
use aether_lifecycle::LifecycleCapability;
use aether_substrate::actor::native::PumpedSlot;
use aether_substrate::chassis::settlement::{
    PumpWake, SettlementRegistry, TerminalDisposition, WaitOutcome, await_settlement_pumped,
};
use aether_substrate::runtime::lifecycle;
use aether_substrate::{Chassis, HubOutbound, Mailer, chassis::frame_loop, mail::MailboxId};

use aether_chassis::next_chassis_correlation;
use aether_chassis::{RenderSizeConfig, resolve_teardown_budget};
use aether_harness_substrate::{
    SubstrateHarnessBuild, SubstrateHarnessChassis, SubstrateHarnessEnv, WORKERS,
    events::{self, ChassisEvent},
};
use aether_render::{Frame, RenderCapability, RenderCapabilityState, RenderParams, RenderTuningConfig};
use aether_substrate::render::VERTEX_BUFFER_BYTES;
use crossbeam_channel::{Receiver, Sender};

/// Cumulative patience cap for the per-frame advance settlement gate,
/// matching the desktop driver. The per-round budget is
/// `frame_loop::DRAIN_BUDGET`; a starved-but-healthy chain resolves before
/// this cap, a genuine wedge exhausts it (issue #1305).
const FRAME_SETTLEMENT_CAP: Duration = Duration::from_secs(30);

fn main() -> anyhow::Result<()> {
    let (events_tx, events_rx) = events::channel();

    // Per issue 464, this `main()` is the env-reading edge.
    let namespace_roots = NamespaceRoots::from_env();
    // Resolve the `assets` root for `capture_frame` similarity references
    // before `namespace_roots` moves into the env.
    let assets_dir = namespace_roots.assets.clone();
    let (width, height) = RenderSizeConfig::from_env().to_size();

    // The pumped render wake feeds two consumers: the advance-loop settlement
    // wait (`PumpWake::Mail`) and the parked event loop (`ChassisEvent::RenderMail`).
    let render_events_tx = events_tx.clone();

    let env = SubstrateHarnessEnv {
        workers: WORKERS,
        pool_workers: None,
        // Issue 1990: the standalone binary keeps the default ring capacities;
        // the in-process `SubstrateHarness` builder is the surface for tuning
        // them (per-harness, no process env).
        ring_capacities: aether_substrate::RingCapacities::default(),
        // Issue 2485: the standalone binary keeps the built-in scheduler
        // tuning (per-harness, no process env).
        scheduler_tuning: aether_substrate::SchedulerTuning::default(),
        observed_kinds: None,
        events_tx,
        namespace_roots: Some(namespace_roots),
        // Issue #3764: the standalone binary is the MCP-drivable chassis,
        // so it composes the full cap set — an in-process harness composes
        // per scenario instead.
        component_host: true,
        compose: vec![
            Box::new(|b| b.with_actor::<aether_tcp::TcpCapability>(())),
            Box::new(|b| b.with_actor::<aether_text::TextCapability>(())),
            Box::new(|b| {
                b.with_actor::<aether_clipboard::ClipboardCapability>(aether_clipboard::ClipboardParams::InMemory)
            }),
            Box::new(|b| {
                b.with_actor_configured::<aether_game::GameGatewayCapability>(
                    aether_game::GameGatewayParams::default(),
                    aether_game::GameGatewayConfig::default(),
                )
            }),
        ],
        // Issue #2509: the standalone binary is an env-reading edge, so
        // its teardown gate honors `AETHER_SETTLEMENT_CAP_SECS` (including
        // the `0 → wait forever` sentinel) — the same knob the settlement
        // gates read.
        teardown_budget: resolve_teardown_budget(),
    };

    let SubstrateHarnessBuild { passive, boot, kind_tick } = SubstrateHarnessChassis::build_passive(env)?;
    let _ = kind_tick; // PR 3c retired the direct Tick push; the bin drives
    // `LifecycleAdvance` and the lifecycle driver broadcasts Tick.

    // ADR-0161: boot the pumped `aether.render` actor offscreen. It claims the
    // `aether.render` slot post-`build_passive` (a no-driver chassis reserved
    // none at Claim), owning the surfaceless GPU, accumulators, and pending
    // capture as plain state; the GPU boots lazily on the first frame from
    // `offscreen_size`. `..Default::default()` fills `wireframe` and — under a
    // feature-unified build that enables aether-render/desktop — the
    // desktop-only `window: None`, so this literal is robust to unification.
    let (mut render_slot, render_wake_slot) = passive.boot_pumped_actor::<RenderCapability>(
        RenderTuningConfig {
            vertex_buffer_bytes: VERTEX_BUFFER_BYTES,
            clear_color: aether_render::DEFAULT_CLEAR_COLOR.to_owned(),
            // Operator-resolvable, unlike the two pinned knobs above:
            // read through the cap's own ADR-0090 layer so
            // `AETHER_RENDER_PASS_TIMINGS` reaches the instrument here
            // the same way it does on a chassis.
            pass_timings: RenderTuningConfig::from_env().pass_timings,
        },
        RenderParams {
            observed_kinds: None,
            assets_dir: Some(assets_dir),
            offscreen_size: Some((width, height)),
            ..Default::default()
        },
    )?;

    // ADR-0161 §Decision 2: the unified `PumpWake` channel. The render slot's
    // mailbox wake sends `PumpWake::Mail` so the advance-loop
    // `await_settlement_pumped` drains on mail arrival, and *also* pokes
    // `ChassisEvent::RenderMail` so a render mail landing while the loop is
    // parked (a settled capture pre-mail's `pre_settled` notice) turns the
    // loop and drains the slot. `subscribe_settlement_with` sends
    // `PumpWake::Settled` into the same channel per advance.
    let (pump_tx, pump_rx) = crossbeam_channel::unbounded::<PumpWake>();
    let wake_pump_tx = pump_tx.clone();
    render_wake_slot.set(Arc::new(move || {
        let _ = wake_pump_tx.send(PumpWake::Mail);
        let _ = render_events_tx.send(ChassisEvent::RenderMail);
    }));

    // ADR-0160 drain-at-pump-start: a pumped driver whose loop starts parked
    // drains once before its first real pump so any mail queued during
    // `init` / `wire` dispatches.
    render_slot.drain_available();

    // Chassis route-freezing: the pumped render actor's own id (its NAMESPACE),
    // the recipient for the per-frame `Frame` request. ctx-less driver setup,
    // no sibling resolver in scope.
    #[allow(clippy::disallowed_methods)]
    let render_mailbox = mailbox_id_from_name(<RenderCapability as Addressable>::NAMESPACE);

    // Chassis route-freezing: the bin wires its loop to the lifecycle cap's own
    // id (its NAMESPACE) — ctx-less driver setup, no sibling resolver in scope.
    #[allow(clippy::disallowed_methods)]
    let lifecycle_mailbox = mailbox_id_from_name(<LifecycleCapability as Addressable>::NAMESPACE);
    let kind_lifecycle_advance = <LifecycleAdvance as Kind>::ID;
    let settlement_registry = Arc::clone(passive.settlement_registry());

    tracing::info!(
        target: "aether_substrate::boot",
        width,
        height,
        workers = WORKERS,
        profile = SubstrateHarnessChassis::PROFILE,
        "substrate-harness componentless boot — drive ticks via aether.substrate_harness.advance; the render runtime boots lazily offscreen on the first frame",
    );

    let mut driver = HarnessDriver {
        queue: Arc::clone(&boot.queue),
        outbound: Arc::clone(&boot.outbound),
        lifecycle_mailbox,
        kind_lifecycle_advance,
        render_mailbox,
        settlement_registry,
        render_slot,
        pump_tx,
        pump_rx,
        chassis_correlation: AtomicU64::new(1),
    };
    driver.run(&events_rx);

    // Drop ordering: run the pumped render actor's Closed-path teardown
    // (`unwire` logs the triangle count) BEFORE dropping `passive` (Log shuts
    // down) → `boot` (legacy capabilities + scheduler join). Listed last-first
    // since locals drop in reverse declaration order.
    driver.render_slot.shutdown();
    drop(driver);
    drop(passive);
    drop(boot);
    Ok(())
}

/// Loopback-driven render pump host for the standalone harness chassis
/// (ADR-0161). Owns the pumped `aether.render` slot and drives the advance /
/// capture frame loop, mirroring the desktop driver's pump shape off winit.
struct HarnessDriver {
    queue: Arc<Mailer>,
    outbound: Arc<HubOutbound>,
    lifecycle_mailbox: MailboxId,
    kind_lifecycle_advance: KindId,
    render_mailbox: MailboxId,
    settlement_registry: Arc<SettlementRegistry>,
    render_slot: PumpedSlot<RenderCapability>,
    /// `PumpWake::Settled` sender cloned into each advance's settlement
    /// subscription; the slot's mailbox wake feeds the same channel with
    /// `PumpWake::Mail`.
    pump_tx: Sender<PumpWake>,
    pump_rx: Receiver<PumpWake>,
    /// ADR-0080 §6 chassis-root correlation counter (issue 723).
    chassis_correlation: AtomicU64,
}

impl HarnessDriver {
    /// Drive the chassis event loop on the main thread. `Advance` runs the
    /// requested frames; `RenderMail` drains the pumped slot so a settled
    /// capture pre-mail is serviced while the loop is otherwise idle. After
    /// each event the loop parks with `recv_timeout(capture_deadline)` when a
    /// capture is pending, so a wedged pre-chain still reaches the actor's
    /// deadline check. Runs until every `EventSender` clone drops (clean
    /// shutdown) or a fatal abort tears the process down.
    fn run(&mut self, events_rx: &events::EventReceiver) {
        loop {
            // One recv site: park on the capture deadline when one is pending,
            // otherwise block until the next event.
            let event = match self.render_slot.read_state(RenderCapabilityState::capture_deadline).flatten() {
                Some(deadline) => match events_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        // The deadline elapsed with no wake — record a frame so
                        // the actor's expiry branch replies `Err` to the wedged
                        // capture.
                        self.record_frame(false);
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match events_rx.recv() {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };
            match event {
                ChassisEvent::Advance { reply_to, ticks } => {
                    for _ in 0..ticks {
                        self.advance_frame();
                    }
                    self.outbound.send_reply(reply_to, &AdvanceResult::Ok { ticks_completed: ticks });
                    // A capture can become ready during an advance (its
                    // pre-mails settled while the slot drained mid-wait).
                    self.capture_if_ready();
                }
                ChassisEvent::RenderMail => {
                    self.render_slot.drain_available();
                    self.capture_if_ready();
                }
            }
        }
    }

    /// Run one advance frame: push a chassis-root `LifecycleAdvance`, wait for
    /// the frame chain to settle while pumping the render slot (the draw mail
    /// the chain is gated on lands on this slot, so a non-pumping wait would
    /// deadlock — the ADR-0161 §Decision 2 rule), then record the frame.
    fn advance_frame(&mut self) {
        let advance_root = self.queue.push_chassis_root_mail(
            next_chassis_correlation(&self.chassis_correlation),
            self.lifecycle_mailbox,
            self.kind_lifecycle_advance,
            encode_empty::<LifecycleAdvance>(),
            1,
        );
        let pump_tx = self.pump_tx.clone();
        self.settlement_registry.subscribe_settlement_with(advance_root, move || {
            let _ = pump_tx.send(PumpWake::Settled);
        });
        // A frame chain that never settles is a wedged dispatcher, not a
        // "submit anyway" — fail-fast (ADR-0063) with the escalating-patience
        // bookkeeping of issue #1305 carried through the pumped wait.
        if let WaitOutcome::Wedged(wedge) = await_settlement_pumped(
            &self.pump_rx,
            &mut self.render_slot,
            "substrate_harness_bin.frame_advance",
            frame_loop::DRAIN_BUDGET,
            FRAME_SETTLEMENT_CAP,
            TerminalDisposition::Abort,
        ) {
            lifecycle::fatal_abort(&self.outbound, wedge.reason());
        }
        // ADR-0161 §Decision 1: record by mailing one frame and draining. The
        // advance commits current producer state (`replay_cache_when_idle:
        // false`).
        self.record_frame(false);
    }

    /// Record a frame: mail one chassis-root `aether.render.frame` and drain
    /// the slot so its `on_frame` handler runs inline on this thread.
    fn record_frame(&mut self, replay_cache_when_idle: bool) {
        self.queue.push_chassis_root_mail(
            next_chassis_correlation(&self.chassis_correlation),
            self.render_mailbox,
            <Frame as Kind>::ID,
            Frame { replay_cache_when_idle, windows: Vec::new() }.encode_into_bytes(),
            1,
        );
        self.render_slot.drain_available();
    }

    /// The capture-ready ordering barrier (ADR-0161 R4): drive exactly one
    /// capture frame once the parked capture is *ready* — every pre-mail chain
    /// settled onto the accumulators — so the record never runs against a
    /// still-filling accumulator. `replay_cache_when_idle: true` replays the
    /// last committed frame (issue 847); the parked capture reads back on this
    /// frame and replies through its retained guard.
    fn capture_if_ready(&mut self) {
        self.render_slot.drain_available();
        if self.render_slot.read_state(RenderCapabilityState::capture_ready).unwrap_or(false) {
            self.record_frame(true);
        }
    }
}
