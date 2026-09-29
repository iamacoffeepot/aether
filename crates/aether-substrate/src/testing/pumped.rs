//! Waiting on an actor the way the chassis drivers do (ADR-0161 §Decision 2).
//!
//! A test that owns a pumped actor is that actor's driver. [`PumpedDriver`]
//! boots it and waits exactly as the desktop and harness drivers wait: the
//! slot's mailbox wake and each awaited root's settlement feed one
//! [`PumpWake`] channel, and [`await_settlement_pumped`] drains the slot on
//! every `Mail` wake until the `Settled` wakes arrive. No wait here polls the
//! wall clock; a wedged wait fails at the chassis settlement cap
//! ([`SettlementConfig`]), not after a guessed deadline.
//!
//! A test that waits on a pooled actor's chain has no slot to pump, so it
//! waits on the tracked root's settlement receiver through [`await_settled`],
//! the gate a chassis waits on for the same signal.

use std::time::Instant;

use aether_actor::{Root, Single};
use aether_data::{Kind, MailId};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use super::TestChassis;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::{NativeActor, PumpedSlot};
use crate::chassis::builder::{ChassisTarget, PassiveChassis, ReplyTarget};
use crate::chassis::frame_loop;
use crate::chassis::settlement::{
    PumpWake, TerminalDisposition, await_internal_signal, await_settlement_pumped, install_pump_wake,
};
use crate::config::SettlementConfig;

/// A booted pumped actor `A` and the chassis it lives on, driven the way a
/// pumped chassis driver drives its slot.
///
/// The slot is not exposed: every drain happens inside [`Self::settle`] or
/// [`Self::pump_until`], on a mail wake, so a test cannot fall back to
/// draining in a timed loop. Dropping the driver shuts the slot down before
/// the chassis drops.
pub struct PumpedDriver<A: Root + NativeActor> {
    slot: PumpedSlot<A>,
    /// Cloned into each awaited root's settlement callback; the slot's
    /// mailbox wake holds another clone.
    wake_tx: Sender<PumpWake>,
    wake_rx: Receiver<PumpWake>,
    chassis: PassiveChassis<TestChassis>,
}

impl<A: Root + NativeActor> PumpedDriver<A> {
    /// Boot `A` pumped on `chassis`, install the wake the waits below read,
    /// and drain once: mail queued before the wake was installed fired no
    /// wake, so the pump-start drain is what dispatches it (ADR-0160).
    ///
    /// # Panics
    /// Panics if `A` fails to boot.
    pub fn boot(chassis: PassiveChassis<TestChassis>, config: A::Config, params: A::Params) -> Self {
        let (mut slot, wake_slot) = chassis.boot_pumped_actor::<A>(config, params).expect("the pumped actor boots");
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<PumpWake>();
        install_pump_wake(&wake_slot, wake_tx.clone());
        slot.drain_available();

        Self { slot, wake_tx, wake_rx, chassis }
    }

    /// The chassis the actor lives on, for its references and for mail to
    /// its pooled peers.
    pub fn chassis(&self) -> &PassiveChassis<TestChassis> {
        &self.chassis
    }

    /// Push `mail` to `to` as a tracked chassis root without draining, so
    /// several sends stage on the pumped inbox before any of their turns
    /// runs. Wait for the returned root with [`Self::settle`].
    pub fn send_tracked<K: Kind, I>(
        &self,
        to: impl ChassisTarget<K, I>,
        mail: &K,
        reply: Option<ReplyTarget>,
    ) -> MailId {
        self.chassis.send_tracked(to, mail, reply).0
    }

    /// Block until every root in `roots` has settled, draining the slot on
    /// each mail wake: the desktop driver's `pump_while_settling`.
    ///
    /// Each root's settlement callback fires exactly once (at once when the
    /// root has already settled), so the wait returns after one `Settled`
    /// wake per root. A reply precedes its root's settlement, so after this
    /// returns every reply the chains sent has been sent.
    ///
    /// # Panics
    /// Panics when a root stays unsettled past the settlement cap.
    pub fn settle(&mut self, roots: &[MailId]) {
        for &root in roots {
            let tx = self.wake_tx.clone();
            self.chassis.settlement_registry().subscribe_settlement_with(root, move || {
                let _ = tx.send(PumpWake::Settled);
            });
        }
        let cap = SettlementConfig::from_env().to_cap();
        for _ in roots {
            let _ = await_settlement_pumped(
                &self.wake_rx,
                &mut self.slot,
                "testing.pumped.settle",
                frame_loop::DRAIN_BUDGET,
                cap,
                TerminalDisposition::Panic,
            );
        }
    }

    /// [`Self::send_tracked`] then [`Self::settle`] on the one root.
    pub fn send_and_settle<K: Kind, I>(
        &mut self,
        to: impl ChassisTarget<K, I>,
        mail: &K,
        reply: Option<ReplyTarget>,
    ) -> MailId {
        let root = self.send_tracked(to, mail, reply);
        self.settle(&[root]);
        root
    }

    /// Wait for an effect that reaches this actor off any chain the test
    /// holds — a departure's `MonitorNotice`, a worker's self-wake — by
    /// draining on each mail wake until `done` holds over the actor's state.
    ///
    /// Only this thread's drains change the state `done` reads, so a mail
    /// wake is the only event that can make it true. Like
    /// [`await_settlement_pumped`], the timeout arm only logs and never
    /// drains; the wait fails at the settlement cap.
    ///
    /// # Panics
    /// Panics when `done` still does not hold at the settlement cap, or when
    /// the slot has shut down.
    pub fn pump_until(&mut self, what: &str, mut done: impl FnMut(&A::State) -> bool) {
        let cap = SettlementConfig::from_env().to_cap();
        let start = Instant::now();
        while !self.slot.read_state(&mut done).expect("the pumped slot is live") {
            match self.wake_rx.recv_timeout(frame_loop::DRAIN_BUDGET) {
                Ok(PumpWake::Mail | PumpWake::Settled) => self.slot.drain_available(),
                Err(RecvTimeoutError::Timeout) => {
                    let waited = start.elapsed();
                    assert!(waited < cap, "{what} did not happen within the settlement cap, waited {waited:?}");
                    tracing::warn!(
                        target: "aether_substrate::testing",
                        waited_millis = waited.as_millis(),
                        "{what} slow: waited {waited:?}, extending",
                    );
                }
                Err(RecvTimeoutError::Disconnected) => unreachable!("the driver holds a wake sender"),
            }
        }
    }

    /// Read the actor's state without draining.
    pub fn read_state<T>(&self, read: impl FnOnce(&A::State) -> T) -> Option<T> {
        self.slot.read_state(read)
    }

    /// Run one host turn against the actor's state without draining.
    pub fn host_turn<T>(&mut self, turn: impl FnOnce(&mut A::State, &mut NativeCtx<'_, A, Single>) -> T) -> Option<T> {
        self.slot.host_turn(turn)
    }
}

impl<A: Root + NativeActor> Drop for PumpedDriver<A> {
    fn drop(&mut self) {
        self.slot.shutdown();
    }
}

/// Wait on a tracked root's settlement receiver — the one `send_tracked`
/// returns — under the chassis gate's escalating patience: the wait for a
/// chain whose actors dispatch on the pool, with no slot for the test to
/// pump. `gate` names the wait in the slow-log and in the panic at the
/// settlement cap.
pub fn await_settled(settled: &Receiver<()>, gate: &str) {
    let _ = await_internal_signal(
        settled,
        gate,
        frame_loop::DRAIN_BUDGET,
        SettlementConfig::from_env().to_cap(),
        TerminalDisposition::Panic,
        None,
    );
}
