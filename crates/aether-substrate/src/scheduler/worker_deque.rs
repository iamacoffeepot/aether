//! Per-worker work-stealing deque (ADR-0087 Phase 3a, iamacoffeepot/aether#1112).
//!
//! Each pool worker owns a `crossbeam_deque::Worker` deque, held in a
//! thread-local so both the worker's own loop (pop / steal-into) and the
//! inbox-sender wake path (push) — which run on the same thread when a
//! handler wakes a downstream slot — reach it without threading a
//! reference through every call site. Sibling workers hold `Stealer`s and
//! an off-worker [`Injector`] feeds producers with no worker thread.
//!
//! This supersedes the issue-1059 single-cell affinity stash: the deque's
//! **LIFO own-pop is the same warm-chain locality** the cell provided, so
//! a relay chain stays on one warm worker with no shared-queue round-trip
//! and no parked-sibling wake (~4.3µs). The stickiness cap is preserved
//! (the knob is repurposed as the **own-deque local bound** per
//! iamacoffeepot/aether#1106): a wake pushes to the own deque while it
//! holds fewer than `cap` slots, else spills to the injector + notifies —
//! exactly the issue-1074 policy, now backed by a deque whose tail an idle
//! worker can `steal_batch_and_pop` (the new pull path).
//!
//! Only pool-worker threads call [`install`]; on any other thread
//! (chassis main, the hub, the trace drainer) [`try_push_local`] is a
//! no-op spill and [`pop_local`] / [`steal_into_local`] yield nothing.

use std::cell::RefCell;
use std::env;
use std::sync::Arc;
use std::sync::OnceLock;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::scheduler::slot::Drainable;

/// The unit on the deques: a chassis-registered dispatcher slot. (Phase
/// 3b makes the blob the unit; 3a keeps the slot.)
type Slot = Arc<dyn Drainable>;

thread_local! {
    /// This worker's own deque. `Some` only on a pool-worker thread
    /// (set by [`install`] at the top of the worker loop). `RefCell`
    /// because both the worker loop and a nested handler wake touch it
    /// on the same thread — never across a `run_cycle`, so the borrows
    /// don't overlap.
    static LOCAL: RefCell<Option<Worker<Slot>>> = const { RefCell::new(None) };
}

/// Move this worker's deque into its thread-local. Called once at the top
/// of the worker loop; enables local push/pop on this thread.
pub fn install(worker: Worker<Slot>) {
    LOCAL.with(|w| *w.borrow_mut() = Some(worker));
}

/// Own-deque local bound — the max slots a worker keeps on its own deque
/// before a wake spills to the injector. Read once from
/// `AETHER_LOCAL_STICKY_MAX`; values `< 1` and unparseable input fall back
/// to `1` (the chain head stays local, fan-out extras spill — the
/// historical single-cell default).
#[must_use]
pub fn sticky_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        env::var("AETHER_LOCAL_STICKY_MAX")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(1)
    })
}

/// Try to push a just-woken slot onto this worker's own deque. Returns
/// `Ok(())` when pushed (the caller skips the injector + notify), or
/// `Err(slot)` to hand the slot back for the injector spill — because
/// this isn't a pool-worker thread, or the own deque already holds `cap`
/// slots (the spill keeps independent fan-out work stealable by idle
/// workers). At `cap == 1` the chain head stays local and every fan-out
/// extra spills.
pub fn try_push_local(slot: Slot, cap: usize) -> Result<(), Slot> {
    LOCAL.with(|w| {
        let w = w.borrow();
        match w.as_ref() {
            Some(worker) if worker.len() < cap => {
                worker.push(slot);
                Ok(())
            }
            _ => Err(slot),
        }
    })
}

/// Pop this worker's next own-deque slot (LIFO — most-recently-pushed,
/// i.e. the freshest relay hop, stays warmest). Checked before stealing
/// and before the park, so an own slot is never stranded.
pub fn pop_local() -> Option<Slot> {
    LOCAL.with(|w| w.borrow().as_ref().and_then(Worker::pop))
}

/// Steal work into this worker's own deque and return one slot to run.
/// Prefers the [`Injector`] (off-worker producers + spilled fan-out +
/// requeued yields, so external work isn't starved by sibling stealing),
/// then each sibling's [`Stealer`] (skipping our own `my_idx`). Returns
/// `None` when every source is empty. Non-blocking — safe as the
/// `SpinPark::acquire` scan closure (its spin loop + park-commit recheck
/// call it repeatedly).
pub fn steal_into_local(
    my_idx: usize,
    stealers: &[Stealer<Slot>],
    injector: &Injector<Slot>,
) -> Option<Slot> {
    LOCAL.with(|w| {
        let w = w.borrow();
        let worker = w.as_ref()?;
        // Retry the whole pass while any source reports transient
        // contention; return on the first success; `None` once all empty.
        loop {
            let mut retry = false;
            match injector.steal_batch_and_pop(worker) {
                Steal::Success(slot) => return Some(slot),
                Steal::Retry => retry = true,
                Steal::Empty => {}
            }
            for (i, stealer) in stealers.iter().enumerate() {
                if i != my_idx {
                    match stealer.steal_batch_and_pop(worker) {
                        Steal::Success(slot) => return Some(slot),
                        Steal::Retry => retry = true,
                        Steal::Empty => {}
                    }
                }
            }
            if !retry {
                return None;
            }
        }
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: a failed steal/pop assertion is the test signal"
)]
mod tests {
    use super::*;
    use crate::scheduler::slot::{BatchBudget, CycleResult, Drainable};
    use std::any::Any;

    struct Noop;
    impl Drainable for Noop {
        fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
            CycleResult::Idle
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn noop() -> Slot {
        Arc::new(Noop)
    }

    /// Drain any residue so the per-thread deque starts empty regardless
    /// of test scheduling order on a shared thread.
    fn drain_local() {
        while pop_local().is_some() {}
    }

    #[test]
    fn non_pool_thread_never_pushes_local() {
        // This test never calls `install`, so it isn't a pool worker:
        // every push must spill regardless of cap.
        assert!(try_push_local(noop(), 4).is_err());
        assert!(pop_local().is_none());
    }

    #[test]
    fn push_respects_cap_and_pops_lifo() {
        install(Worker::new_lifo());
        drain_local();

        // cap 2: first two push, the third spills.
        assert!(try_push_local(noop(), 2).is_ok());
        assert!(try_push_local(noop(), 2).is_ok());
        assert!(
            try_push_local(noop(), 2).is_err(),
            "third push past cap 2 must spill"
        );

        assert!(pop_local().is_some());
        assert!(pop_local().is_some());
        assert!(pop_local().is_none());
    }

    #[test]
    fn cap_one_keeps_only_the_chain_head() {
        install(Worker::new_lifo());
        drain_local();

        assert!(try_push_local(noop(), 1).is_ok());
        assert!(
            try_push_local(noop(), 1).is_err(),
            "cap 1 keeps only the chain head; the extra spills"
        );
        assert!(pop_local().is_some());
        assert!(pop_local().is_none());
    }

    #[test]
    fn steal_pulls_from_injector_and_siblings() {
        install(Worker::new_lifo());
        drain_local();

        // Injector work is pulled.
        let injector: Injector<Slot> = Injector::new();
        injector.push(noop());
        assert!(steal_into_local(0, &[], &injector).is_some());

        // A sibling's deque is stolen from (own index 0 is skipped).
        let sibling: Worker<Slot> = Worker::new_lifo();
        sibling.push(noop());
        sibling.push(noop());
        let stealers = [Worker::<Slot>::new_lifo().stealer(), sibling.stealer()];
        assert!(
            steal_into_local(0, &stealers, &Injector::new()).is_some(),
            "should steal from sibling index 1"
        );

        // Nothing anywhere → None.
        drain_local();
        assert!(steal_into_local(0, &[], &Injector::new()).is_none());
    }
}
