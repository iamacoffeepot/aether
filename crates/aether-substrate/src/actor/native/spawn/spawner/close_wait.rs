//! The test-support wait for one spawned actor's close: its close cycle has
//! run and the registry owner has applied its route drop.

use std::sync::Arc;

use crate::chassis::frame_loop;
use crate::chassis::settlement::{TerminalDisposition, await_internal_signal};
use crate::config::SettlementConfig;
use crate::mail::MailboxId;

use super::Spawner;

impl Spawner {
    /// Block until the pooled instanced actor at `id` has finished its
    /// close cycle *and* the registry owner has applied its route drop —
    /// the test-support wait behind `PassiveChassis::await_closed`.
    ///
    /// Two existing signals compose the proof. The slot's close-done
    /// sender fires after the close's registry tail has queued the
    /// `DropMailbox` on the owner (an already-closed slot fires it at
    /// once through `set_close_done_tx`'s fast path). [`Self::await_registry_applied`]
    /// submitted after that lands behind the drop in the owner's one
    /// FIFO queue, and the owner applies and publishes a whole drain
    /// before completing any batch in it, so the barrier's completion
    /// proves the drop is applied and published. When the owner has
    /// already closed there is nothing left to wait for.
    ///
    /// An actor whose close cycle already ended has released its slot
    /// (issue #7402), so there is no close-done signal left to wait on.
    /// The slot is released after the registry close, which leaves the
    /// actor registry's slot `Dead` and queues the route drop, so an
    /// absent entry for a closed actor means the drop is already queued
    /// and the barrier alone proves it applied.
    ///
    /// A slot holds one close-done sender, so a second concurrent waiter
    /// on the same actor replaces the first; the displaced wait fails at
    /// the gate as a disconnect. `gate` names both waits in the slow-log
    /// and in the panic at the settlement cap.
    ///
    /// # Panics
    /// Panics when `id` is neither a pooled instanced actor this spawner
    /// holds nor an actor that has closed (a singleton or a pumped slot
    /// that is still open has no close-done signal here), or when either
    /// wait passes the settlement cap.
    pub(crate) fn await_closed(&self, id: MailboxId, gate: &str) {
        let retained = self
            .instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned; fail-fast per ADR-0063")
            .get(&id)
            .map(|entry| Arc::clone(&entry.slot));

        match retained {
            Some(slot) => {
                let (tx, rx) = crossbeam_channel::bounded::<()>(1);
                slot.set_close_done_tx(tx);
                let cap = SettlementConfig::from_env().to_cap();
                let _ =
                    await_internal_signal(&rx, gate, frame_loop::DRAIN_BUDGET, cap, TerminalDisposition::Panic, None);
            }
            None => assert!(
                self.actor_registry.is_closed_at(id),
                "{gate}: {id} is not a pooled instanced actor; await_closed covers only those"
            ),
        }

        self.await_registry_applied(gate);
    }
}
