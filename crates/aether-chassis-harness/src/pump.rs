//! The harness chassis's driver: a loopback-driven render pump host
//! (ADR-0161). The harness is a passive chassis, so this is not a
//! `DriverCapability` — the binary's `main` owns the loop on the thread that
//! owns the offscreen GPU, exactly as the desktop driver owns its pump off
//! winit.
//!
//! [`HarnessDriver`] owns the pumped `aether.render` slot and drives the
//! advance / capture frame loop: `Advance` runs the requested frames,
//! `RenderMail` drains the slot so a settled capture pre-mail is serviced while
//! the loop is otherwise idle.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_chassis::next_chassis_correlation;
use aether_data::{Kind, KindId, mailbox_id_from_name};
use aether_kinds::{AdvanceResult, LifecycleAdvance};
use aether_lifecycle::LifecycleCapability;
use aether_render::{Frame, RenderCapability, RenderCapabilityState};
use aether_substrate::actor::native::PumpedSlot;
use aether_substrate::chassis::settlement::{
    PumpWake, SettlementRegistry, TerminalDisposition, WaitOutcome, await_settlement_pumped,
};
use aether_substrate::runtime::lifecycle;
use aether_substrate::{HubOutbound, Mailer, SubstrateBoot, chassis::frame_loop, mail::MailboxId};
use aether_substrate_harness_cap::events::{ChassisEvent, EventReceiver};
use crossbeam_channel::{Receiver, Sender};

/// Cumulative patience cap for the per-frame advance settlement gate,
/// matching the desktop driver. The per-round budget is
/// `frame_loop::DRAIN_BUDGET`; a starved-but-healthy chain resolves before
/// this cap, a genuine wedge exhausts it (issue #1305).
const FRAME_SETTLEMENT_CAP: Duration = Duration::from_secs(30);

/// Loopback-driven render pump host for the harness chassis (ADR-0161). Owns
/// the pumped `aether.render` slot and drives the advance / capture frame loop,
/// mirroring the desktop driver's pump shape off winit.
pub struct HarnessDriver {
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
    /// Wire the loop to the booted substrate and the pumped render slot the
    /// binary claimed post-build.
    #[must_use]
    pub fn new(
        boot: &SubstrateBoot,
        settlement_registry: Arc<SettlementRegistry>,
        render_slot: PumpedSlot<RenderCapability>,
    ) -> Self {
        // Chassis route-freezing: the loop wires itself to the pumped render
        // actor's and the lifecycle cap's own ids (their NAMESPACEs) — ctx-less
        // driver setup, no sibling resolver in scope. Both allows relocate
        // verbatim from the binary this module was split out of.
        #[allow(clippy::disallowed_methods)] // aether-suppression-request: route-freeze to the actor's own NAMESPACE
        let render_mailbox = mailbox_id_from_name(<RenderCapability as Addressable>::NAMESPACE);
        #[allow(clippy::disallowed_methods)] // aether-suppression-request: route-freeze to the cap's own NAMESPACE
        let lifecycle_mailbox = mailbox_id_from_name(<LifecycleCapability as Addressable>::NAMESPACE);

        let (pump_tx, pump_rx) = crossbeam_channel::unbounded::<PumpWake>();
        Self {
            queue: Arc::clone(&boot.queue),
            outbound: Arc::clone(&boot.outbound),
            lifecycle_mailbox,
            kind_lifecycle_advance: <LifecycleAdvance as Kind>::ID,
            render_mailbox,
            settlement_registry,
            render_slot,
            pump_tx,
            pump_rx,
            chassis_correlation: AtomicU64::new(1),
        }
    }

    /// A `PumpWake::Mail` sender for the pumped slot's mailbox wake (ADR-0161
    /// §Decision 2: the unified wake channel). The binary installs a wake that
    /// pokes both this and its own event loop, so a render mail landing while
    /// the loop is parked turns the loop *and* drains the slot.
    #[must_use]
    pub fn wake_sender(&self) -> Sender<PumpWake> {
        self.pump_tx.clone()
    }

    /// ADR-0160 drain-at-pump-start: a pumped driver whose loop starts parked
    /// drains once before its first real pump so any mail queued during
    /// `init` / `wire` dispatches.
    pub fn prime(&mut self) {
        self.render_slot.drain_available();
    }

    /// Run the pumped slot's Closed-path teardown (`unwire` logs the triangle
    /// count). Called before the `PassiveChassis` and the boot drop.
    pub fn shutdown(&mut self) {
        self.render_slot.shutdown();
    }

    /// Drive the chassis event loop on the main thread. `Advance` runs the
    /// requested frames; `RenderMail` drains the pumped slot so a settled
    /// capture pre-mail is serviced while the loop is otherwise idle. After
    /// each event the loop parks with `recv_timeout(capture_deadline)` when a
    /// capture is pending, so a wedged pre-chain still reaches the actor's
    /// deadline check. Runs until every `EventSender` clone drops (clean
    /// shutdown) or a fatal abort tears the process down.
    pub fn run(&mut self, events_rx: &EventReceiver) {
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
                ChassisEvent::Advance { reply_to, ticks, delta_micros } => {
                    for _ in 0..ticks {
                        self.advance_frame(delta_micros);
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
    fn advance_frame(&mut self, delta_micros: u32) {
        let advance_root = self.queue.push_chassis_root_mail(
            next_chassis_correlation(&self.chassis_correlation),
            self.lifecycle_mailbox,
            self.kind_lifecycle_advance,
            LifecycleAdvance { delta_micros }.encode_into_bytes(),
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
