//! Per-worker local run-queue for chain + fan-out affinity (iamacoffeepot/aether#1059).
//!
//! When a handler running on a pool worker wakes a downstream actor's
//! slot, the wake stashes it here instead of the shared ready queue, and
//! the worker's `acquire_slot` checks here first. A relay chain therefore
//! stays on one warm worker — no shared-queue round-trip and, crucially,
//! no parked-sibling futex wake (~4.3µs).
//!
//! The queue holds up to a **stickiness cap** slots
//! (`AETHER_LOCAL_STICKY_MAX`, default `1`). At cap `1` this reproduces the
//! original single-cell behaviour: the chain head stays local and every
//! fan-out extra spills to the shared queue, so independent work
//! parallelises across workers. A higher cap keeps fan-out *extras* on the
//! same warm worker too — the producing worker drains them itself in
//! sequence rather than scattering them to sibling workers that pick each
//! child up cold. The latency harness showed warm fan-out getting *worse*
//! with more workers (cross-worker handoff + cache-cold pickup outweighs the
//! parallelism at trivial leaf widths); a cap > 1 trades that scatter for
//! locality. A load-aware / trace-driven choice of *when* to spill rides on
//! top of this mechanism later.
//!
//! Only pool-worker threads call [`mark_pool_worker`]; on any other thread
//! (chassis main, the hub, the trace drainer) [`try_stash_next`] is a
//! no-op and the wake falls through to the shared queue as before.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::env;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::scheduler::slot::Drainable;

thread_local! {
    static IS_POOL_WORKER: Cell<bool> = const { Cell::new(false) };
    static LOCAL_QUEUE: RefCell<VecDeque<Arc<dyn Drainable>>> =
        const { RefCell::new(VecDeque::new()) };
}

/// Mark the current thread as a pool worker. Called once at the top of
/// each worker's loop; enables local stashing on this thread.
pub fn mark_pool_worker() {
    IS_POOL_WORKER.with(|w| w.set(true));
}

/// Stickiness cap — the max number of slots a worker keeps in its local
/// run-queue before spilling to the shared queue. Read once from
/// `AETHER_LOCAL_STICKY_MAX`; values `< 1` and unparseable input fall back
/// to `1` (the original single-cell behaviour, a no-op default).
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

/// Try to stash a just-woken slot on this worker's local run-queue.
/// Returns `Ok(())` when stashed (the caller skips the shared queue), or
/// `Err(slot)` to hand the slot back for the shared queue — because this
/// isn't a pool-worker thread, or the local queue already holds `cap`
/// slots (the spill path that keeps independent work parallel). At
/// `cap == 1` the chain head stays local and every fan-out extra spills.
pub fn try_stash_next(slot: Arc<dyn Drainable>, cap: usize) -> Result<(), Arc<dyn Drainable>> {
    if !IS_POOL_WORKER.with(Cell::get) {
        return Err(slot);
    }
    LOCAL_QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if q.len() < cap {
            q.push_back(slot);
            Ok(())
        } else {
            Err(slot)
        }
    })
}

/// Take this worker's next stashed slot (FIFO), if any. Checked before the
/// shared queue and before the shutdown park, so a stashed slot is never
/// stranded across a worker's loop iteration. The queue is only ever
/// populated by *this* worker during its own `run_cycle`, so one pop per
/// acquire suffices.
pub fn take_next() -> Option<Arc<dyn Drainable>> {
    LOCAL_QUEUE.with(|q| q.borrow_mut().pop_front())
}

#[cfg(test)]
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

    fn noop() -> Arc<dyn Drainable> {
        Arc::new(Noop)
    }

    /// Drain any residue so the per-thread queue starts empty regardless
    /// of test scheduling order on a shared thread.
    fn drain_local() {
        while take_next().is_some() {}
    }

    #[test]
    fn non_pool_thread_never_stashes() {
        drain_local();
        // This test never marks itself a pool worker (and runs on its own
        // process under nextest), so the stash must spill regardless of cap.
        assert!(try_stash_next(noop(), 4).is_err());
        assert!(take_next().is_none());
    }

    #[test]
    fn stash_respects_cap_and_drains_fifo() {
        mark_pool_worker();
        drain_local();

        // cap 2: first two stash, the third spills.
        assert!(try_stash_next(noop(), 2).is_ok());
        assert!(try_stash_next(noop(), 2).is_ok());
        assert!(
            try_stash_next(noop(), 2).is_err(),
            "third stash past cap 2 must spill"
        );

        // FIFO drain of the two stashed, then empty.
        assert!(take_next().is_some());
        assert!(take_next().is_some());
        assert!(take_next().is_none());
    }

    #[test]
    fn cap_one_reproduces_single_cell() {
        mark_pool_worker();
        drain_local();

        assert!(try_stash_next(noop(), 1).is_ok());
        assert!(
            try_stash_next(noop(), 1).is_err(),
            "cap 1 keeps only the chain head; the extra spills"
        );
        assert!(take_next().is_some());
        assert!(take_next().is_none());
    }
}
