//! Chassis-teardown walk over every instanced slot the spawner retained.
//!
//! Spawned actors close *first* (before the singleton shutdowns) so their
//! `MonitorNotice` mail reaches singleton watchers while those are still
//! alive. Each slot is signalled, woken once so a pool worker runs its close
//! path, and then waited on through the escalating-patience gate — a close
//! cycle that never runs left `unwire` unrun, so the wedge is unrecoverable
//! rather than something to warn past.

use std::time::Duration;

use crate::chassis::settlement::{TerminalDisposition, WaitOutcome, await_internal_signal};
use crate::mail::MailboxId;
use crate::runtime::lifecycle::FatalAbortRecord;

use super::{InstancedSlotEntry, Spawner};

impl Spawner {
    /// Issue 685: walk every spawned instanced slot, signal shutdown
    /// on its binding, fire one wake so a pool worker picks it up and
    /// runs the close path (drain residual → `unwire` → registry
    /// close + monitor fan-out), then wait per-slot on a one-shot
    /// completion channel until every slot has finished or `timeout`
    /// elapses.
    ///
    /// Called from the chassis builder's `BootedPassives::shutdown_in_place`
    /// before the singleton shutdowns walk. The ordering matters:
    /// spawned actors close *first* so their `MonitorNotice` mail
    /// reaches singleton watchers while they're still alive. The
    /// pool stays alive through this method (it drops via the
    /// `_pool: PoolHandle` field on `BootedPassives` which has a later
    /// drop order than the explicit `shutdown_in_place` call), so
    /// workers can drain the close cycles we just queued.
    ///
    /// Issue 714: the original implementation polled
    /// [`Drainable::is_closed`](crate::scheduler::Drainable::is_closed) every 2 ms with a
    /// `timeout`-bounded loop. Under nextest contention the worker that
    /// observed the wake could be scheduled out long enough that the
    /// 2 s deadline elapsed before the close cycle ran, surfacing as
    /// the `chassis_teardown_runs_unwire` flake. The waker now installs a
    /// one-shot `crossbeam_channel::bounded(1)` per entry; the slot's
    /// close cycle fires it after `unwire` + registry close land, so
    /// teardown wakes the instant the cycle settles instead of polling.
    ///
    /// Issue #1305: each close-done receiver is waited on via
    /// [`await_internal_signal`] with escalating patience rather than a
    /// bare wall-clock `recv_timeout`. A genuinely wedged close cycle is
    /// unrecoverable — `unwire` never ran, so teardown invariants are
    /// already corrupt — so the disposition is `Abort` in release
    /// (route the wedge through the Spawner's
    /// [`FatalAborter`](crate::runtime::lifecycle::FatalAborter)) and `Panic` in test/debug (so #1295's
    /// assertion fails attributably at the gate site instead of as a
    /// downstream `0 != 1`). The old silent `warn!`-and-return-anyway
    /// path that left an un-closed actor is gone.
    ///
    /// `round_budget` is the per-round patience interval (the log
    /// cadence); `cumulative_cap` is the total patience per slot before
    /// declaring a wedge.
    ///
    /// Issue #4193: `abort_record` is the chassis's [`FatalAbortRecord`],
    /// and it is what stops this gate laundering a handler panic into a
    /// bare timeout. A panicking handler escalates through the pool
    /// worker's [`FatalAborter`](crate::runtime::lifecycle::FatalAborter); under [`crate::runtime::lifecycle::PanicAborter`]
    /// that unwinds the worker, so the slot it was mid-turn on never
    /// fires close-done and every remaining slot waits out `cumulative_cap`
    /// — five minutes by default, longer than any test-runner ceiling, so
    /// the run reports a truncated hang and the panic that caused it never
    /// reaches the failure. Watching the record makes the gate report the
    /// abort reason instead, at the moment it looks.
    pub(crate) fn shutdown_instanced(
        &self,
        round_budget: Duration,
        cumulative_cap: Duration,
        abort_record: &FatalAbortRecord,
    ) {
        // Issue #2509: retain the slot's `MailboxId` alongside its entry
        // (previously dropped as `_id`) so a genuine teardown wedge names
        // the actor whose close cycle failed rather than a bare
        // gate-name panic.
        let entries: Vec<(MailboxId, InstancedSlotEntry)> = {
            let mut guard =
                self.instanced_slots.lock().expect("instanced_slots mutex poisoned; fail-fast per ADR-0063");
            guard.drain().collect()
        };
        if entries.is_empty() {
            return;
        }
        // Wire one (tx, rx) per entry up-front. Installing the tx on
        // the slot before signalling shutdown ensures the close cycle
        // sees the sender to fire — even if the worker enters the
        // close path before `signal_shutdown` returns control. The
        // slot's `set_close_done_tx` fast-paths an already-closed slot
        // by firing immediately, so there's no race window where the
        // close cycle ran without seeing the tx.
        let mut waiters: Vec<crossbeam_channel::Receiver<()>> = Vec::with_capacity(entries.len());
        for (_id, entry) in &entries {
            let (tx, rx) = crossbeam_channel::bounded::<()>(1);
            entry.slot.set_close_done_tx(tx);
            waiters.push(rx);
            entry.slot.signal_shutdown();
            // Shutdown wake: schedule the slot so the worker observes
            // the shutdown signal. The CAS-win bool is meaningful only
            // for callers wiring up first-time scheduling races; here
            // we just need *some* worker to pick the slot up.
            let _ = entry.wake.wake();
        }
        // `Panic` in test/debug (attributable failure at the gate),
        // `Abort` in release (the wedge is unrecoverable — route it
        // through the Spawner's aborter). The helper diverges itself on
        // `Panic`; on `Abort` it hands back the wedge for us to abort.
        let disposition = if cfg!(debug_assertions) {
            TerminalDisposition::Panic
        } else {
            TerminalDisposition::Abort
        };
        for ((id, _entry), rx) in entries.iter().zip(&waiters) {
            // Issue #2509: name the wedged slot in the gate label so a
            // teardown wedge panic/abort points at the actor whose close
            // cycle failed (e.g. `shutdown_instanced.close_done[mbx-…]`)
            // rather than the bare `shutdown_instanced.close_done`.
            let gate = format!("shutdown_instanced.close_done[{id}]");
            match await_internal_signal(rx, &gate, round_budget, cumulative_cap, disposition, Some(abort_record)) {
                WaitOutcome::Settled => {}
                WaitOutcome::Wedged(wedge) => {
                    // `Abort` disposition (release): the close cycle
                    // never ran `unwire`; teardown invariants are
                    // corrupt and unrecoverable. Route through the
                    // Spawner's aborter — diverges.
                    self.aborter.abort(wedge.reason());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::sync::{Arc, Mutex};

    use crate::actor::registry::ActorRegistry;
    use crate::config::RingCapacities;
    use crate::mail::mailer::Mailer;
    use crate::mail::registry::Registry;
    use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
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
    /// Fast by construction: the cumulative cap is injected directly as
    /// `shutdown_instanced`'s parameter (20 ms), so the wedge fires in
    /// milliseconds rather than blocking on the 300 s default.
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
        spawner
            .instanced_slots
            .lock()
            .expect("instanced_slots mutex poisoned")
            .insert(MailboxId(0xABCD), InstancedSlotEntry { slot, wake });

        spawner.shutdown_instanced(Duration::from_millis(1), Duration::from_millis(20), &FatalAbortRecord::new());
    }
}
