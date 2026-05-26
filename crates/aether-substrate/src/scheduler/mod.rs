//! Worker pool scheduler for actor dispatch (issue 635 PR B).
//!
//! The actor model post-ADR-0038 spawned one OS thread per actor. With
//! ~10 chassis caps that was fine; with N instanced actors (per
//! ADR-0079, the forcing function for issue 635) it isn't. The
//! scheduler replaces 1:1 with M:N — a small set of pool workers
//! cooperatively drains every actor. Blocking work (TCP `accept`,
//! file-watch reads, a parking request/reply) never
//! blocks a handler: it offloads to a `ctx.spawn`'d thread that feeds
//! results back as mail (issue 635 Phase 3 made `Pooled` the default;
//! issue 1187 removed the per-thread `Dedicated` opt-out entirely).
//!
//! ## Components
//!
//! - [`Pool`] owns the pool worker threads + the ready-queue sender.
//!   Constructed once at chassis boot.
//! - [`Drainable`] is the trait [`crate::actor`]-side dispatcher slots
//!   implement; the concrete `DispatcherSlot<A>` runs its `run_cycle`
//!   over the shared per-envelope helpers in
//!   `crate::actor::native::dispatch`.
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
//! PR B landed the pool primitive standalone (tested via a mock
//! [`Drainable`] fixture). PR C wired real `DispatcherSlot<A>` instances
//! at boot time; Phase 3 flipped the default so every cap drains on the
//! pool, and issue 1187 removed the per-thread opt-out — the pool is now
//! the sole dispatch path.

mod calibrate;
mod pool;
mod slot;
mod spin_park;
mod worker_deque;

pub use calibrate::{handoff_cost, handoff_cost_nanos, log_handoff_calibration};
pub use pool::{Pool, PoolConfig, PoolHandle, PoolWorkerJoin};
pub use slot::{
    BATCH_MAX_MAILS, BATCH_MAX_USEC, BatchBudget, CLOCK_CHECK_STRIDE, CycleResult, DrainOutcome,
    Drainable, SeizeHandle, SeizeSeed, SlotState, SlotStateLabel, WakeHandle, WakeSink,
};
pub use spin_park::{Acquired, SpinPark};
pub use worker_deque::{burst_note_mail, pending_depth, time_budget};
