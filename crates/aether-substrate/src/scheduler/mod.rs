//! Worker pool scheduler for `Pooled` actor dispatch (issue 635 PR B).
//!
//! The actor model post-ADR-0038 spawns one OS thread per actor. With
//! ~10 chassis caps the model is fine; with N instanced actors (per
//! ADR-0079, the forcing function for issue 635) it isn't. The
//! scheduler replaces 1:1 with M:N — a small set of pool workers
//! cooperatively drains many `Pooled` actors. Actors that own a long-
//! running blocking primitive (TCP `accept`, file-watch reads, parking
//! `wait_reply`) opt out via `Actor::SCHEDULING = Dedicated` and keep
//! their own thread.
//!
//! ## Components
//!
//! - [`Pool`] owns the pool worker threads + the ready-queue sender.
//!   Constructed once at chassis boot.
//! - [`Drainable`] is the trait [`crate::actor`]-side dispatcher slots
//!   implement (PR C wires the concrete `DispatcherSlot<A>` over the
//!   crate-internal `dispatch_loop_run` body in
//!   `crate::actor::native::dispatch`).
//! - [`SlotState`] is the per-slot atomic that orchestrates Idle ↔
//!   Ready ↔ Running transitions between sender-side wakeups and
//!   worker-side drain claims.
//! - [`WakeHandle`] is what the chassis hands to the inbox sender path
//!   (PR C). Calling [`WakeHandle::wake`] runs the
//!   `Idle → Ready` CAS and, on the winning transition, pushes the
//!   slot to the ready queue.
//!
//! ## Phasing
//!
//! PR B (this) lands the pool primitive standalone and tested via a
//! mock [`Drainable`] fixture. PR C wires real
//! `DispatcherSlot<A>` instances at boot time so `Pooled` actors
//! actually run on the pool. Phase 2 flips one cap to `Pooled`; Phase
//! 3 sweeps the rest. Until PR C the pool is unused infrastructure.

mod pool;
mod slot;
mod spin_park;
mod worker_deque;

pub use pool::{Pool, PoolConfig, PoolHandle, PoolWorkerJoin};
pub use slot::{
    BATCH_MAX_MAILS, BATCH_MAX_USEC, BatchBudget, CLOCK_CHECK_STRIDE, CycleResult, DrainOutcome,
    Drainable, SlotState, SlotStateLabel, WakeHandle, WakeSink,
};
pub use spin_park::{Acquired, SpinPark};
