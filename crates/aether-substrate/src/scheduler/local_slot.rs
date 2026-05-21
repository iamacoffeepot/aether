//! Per-worker "next slot" cell for chain affinity (iamacoffeepot/aether#1059).
//!
//! When a handler running on a pool worker wakes a downstream actor's
//! slot, the wake stashes it here instead of the shared ready queue, and
//! the worker's `acquire_slot` checks here first. A relay chain therefore
//! stays on one warm worker — no shared-queue round-trip and, crucially,
//! no parked-sibling futex wake (~4.3µs). The cell holds at most one slot,
//! so a fan-out spills its extras to the shared queue and stays parallel
//! across workers.
//!
//! Only pool-worker threads call [`mark_pool_worker`]; on any other thread
//! (chassis main, the hub, the trace drainer) [`try_stash_next`] is a
//! no-op and the wake falls through to the shared queue as before.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use crate::scheduler::slot::Drainable;

thread_local! {
    static IS_POOL_WORKER: Cell<bool> = const { Cell::new(false) };
    static NEXT_SLOT: RefCell<Option<Arc<dyn Drainable>>> = const { RefCell::new(None) };
}

/// Mark the current thread as a pool worker. Called once at the top of
/// each worker's loop; enables local stashing on this thread.
pub fn mark_pool_worker() {
    IS_POOL_WORKER.with(|w| w.set(true));
}

/// Try to stash a just-woken slot for this worker to drain next. Returns
/// `Ok(())` when stashed (the caller skips the shared queue), or
/// `Err(slot)` to hand the slot back for the shared queue — either because
/// this isn't a pool-worker thread, or the cell is already occupied (the
/// fan-out spill path that keeps independent work parallel).
pub fn try_stash_next(slot: Arc<dyn Drainable>) -> Result<(), Arc<dyn Drainable>> {
    if !IS_POOL_WORKER.with(Cell::get) {
        return Err(slot);
    }
    NEXT_SLOT.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            *cell = Some(slot);
            Ok(())
        } else {
            Err(slot)
        }
    })
}

/// Take this worker's stashed next slot, if any. Checked before the shared
/// queue and before the shutdown park, so a stashed slot is never
/// stranded across a worker's loop iteration.
pub fn take_next() -> Option<Arc<dyn Drainable>> {
    NEXT_SLOT.with(|cell| cell.borrow_mut().take())
}
