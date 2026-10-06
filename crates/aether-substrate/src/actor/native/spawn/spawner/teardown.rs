//! Chassis-teardown walk over every instanced slot the spawner still holds:
//! the actors that are open at teardown. An actor that closed earlier
//! released its own entry and is not walked.
//!
//! Spawned actors close *first* (before the composed roots) so their
//! `MonitorNotice` mail, and anything their `unwire` sends, reaches a root
//! that is still open. Each slot is signalled, woken once so a pool worker
//! runs its close, and then waited on through the chassis's
//! [`TeardownGate`], the same gate the walk over the roots uses.

use crate::chassis::builder::{ClosingSlot, TeardownGate};
use crate::mail::MailboxId;

use super::{InstancedSlotEntry, Spawner};

impl Spawner {
    /// Issue 685: close every spawned instanced slot that is still open
    /// (an actor that already closed released its entry, so there is
    /// nothing of it to signal or wait for) through `gate`.
    ///
    /// Called from the chassis builder's `BootedPassives::shutdown_in_place`
    /// before the walk over the composed roots. The pool stays alive through
    /// this method (it drops via the `_pool: PoolHandle` field on
    /// `BootedPassives`, after the explicit `shutdown_in_place` call), so
    /// workers run the close cycles the gate wakes.
    ///
    /// Each wait is named `shutdown_instanced.close_done[<id>]` (issue
    /// #2509), so a wedge points at the actor whose close never ran rather
    /// than at a bare gate name.
    pub(crate) fn shutdown_instanced(&self, gate: &TeardownGate<'_>) {
        // A slot drained here while its own close cycle is running stays
        // alive through this entry, and its release then finds nothing to
        // remove.
        let entries: Vec<(MailboxId, InstancedSlotEntry)> = self
            .instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned; fail-fast per ADR-0063")
            .drain()
            .collect();
        let closing: Vec<ClosingSlot<'_>> = entries
            .iter()
            .map(|(id, entry)| ClosingSlot {
                gate: format!("shutdown_instanced.close_done[{id}]"),
                slot: &*entry.slot,
                wake: &entry.wake,
            })
            .collect();

        gate.close(&closing);
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::actor::registry::ActorRegistry;
    use crate::config::RingCapacities;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::Registry;
    use crate::runtime::lifecycle::{FatalAbortRecord, FatalAborter, PanicAborter};
    use crate::scheduler::{BatchBudget, CycleResult, Drainable, Pool, PoolConfig, SlotState, WakeHandle};

    use super::*;

    /// A `Drainable` that never runs its close cycle: it stashes the
    /// close-done sender the teardown gate installs and holds it alive
    /// without ever firing it. The gate's per-slot waiter therefore stays
    /// connected but silent, so the wait exhausts its cumulative cap — the
    /// starvation-shaped wedge (a healthy-but-slow / stuck close cycle)
    /// #2509 guards, not an immediate channel disconnect.
    struct NeverClosingSlot {
        close_done: Mutex<Option<crossbeam_channel::Sender<()>>>,
    }

    impl Drainable for NeverClosingSlot {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }
        fn set_close_done_tx(&self, tx: crossbeam_channel::Sender<()>) {
            *self.close_done.lock().expect("close_done mutex never poisoned in this single-threaded test") = Some(tx);
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// Tripwire (issue #2509): a wedged instanced-actor teardown gate
    /// names the slot that failed to close. The `expected` substring pins
    /// the gate label embedding the slot's `MailboxId` Display
    /// (`shutdown_instanced.close_done[<id>]`) — a real tagged mailbox id
    /// renders as `mbx-…`; this test's raw id falls back to the `{:#018x}`
    /// hex form. If the label ever drops the id and reverts to the bare
    /// `shutdown_instanced.close_done`, the substring stops matching and
    /// this test fails.
    ///
    /// Fast by construction: the cumulative cap is injected directly on
    /// the gate (20 ms), so the wedge fires in milliseconds rather than
    /// blocking on the 300 s default.
    #[test]
    #[should_panic(expected = "shutdown_instanced.close_done[0x000000000000abcd]")]
    fn shutdown_instanced_wedge_names_the_slot() {
        let registry = Arc::new(Registry::new());
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let actor_registry = Arc::new(ActorRegistry::new());
        // One worker is enough — the wedge comes from the close-done
        // signal never firing, not from anything the pool drains.
        let pool = Pool::start(PoolConfig { workers: 1, ..PoolConfig::default() }, Arc::clone(&aborter));
        let spawner = Spawner::new(
            Arc::clone(&registry),
            actor_registry,
            Arc::clone(&mailer),
            Arc::clone(&aborter),
            pool.wake_sink(),
            RingCapacities::default(),
        );

        let slot: Arc<dyn Drainable> = Arc::new(NeverClosingSlot { close_done: Mutex::new(None) });
        let wake = WakeHandle::new(Arc::new(SlotState::new()), Arc::downgrade(&slot), pool.wake_sink());
        spawner.retain_activated_slot(MailboxId(0xABCD), slot, wake, None);

        spawner.shutdown_instanced(&TeardownGate {
            round_budget: Duration::from_millis(1),
            cumulative_cap: Duration::from_millis(20),
            abort_record: &FatalAbortRecord::new(),
            aborter: &*aborter,
        });
    }
}
