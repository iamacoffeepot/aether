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

use crate::actor::native::blob_work::RECRUIT_KNOBS;
use crate::config::{KnobKind, KnobRecord};

/// The lifecycle advance-timeout knob (ADR-0090 unit b2,
/// iamacoffeepot/aether#1255). The `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS`
/// override is read directly in `LifecycleCapability::init`
/// (`aether-capabilities`); the record lives here, substrate-side,
/// because the config-discovery aggregation below
/// ([`SCHEDULER_KNOBS`]) is substrate-side and `aether-capabilities`
/// depends on `aether-substrate`, not the reverse.
pub const LIFECYCLE_KNOBS: &[KnobRecord] = &[KnobRecord {
    env_key: "AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS",
    doc: "Deadline (ms) for a pending lifecycle advance's Settled before the next \
          inbound advance force-completes it (degrades a wedged settlement pipeline \
          into a visible stutter).",
    default: Some("1000"),
    kind: KnobKind::HandRegistered,
}];

/// The scheduler + lifecycle hot-path tuning knobs registered for
/// config discovery (ADR-0090 unit b2, iamacoffeepot/aether#1255).
/// Concatenates the five deque / keep-local-valve knobs
/// (`worker_deque::DEQUE_KNOBS`), the handoff-cost calibration knob
/// (`calibrate::CALIBRATE_KNOBS`), the lifecycle advance-timeout knob
/// ([`LIFECYCLE_KNOBS`]), the three blob-recruiter knobs
/// (`blob_work::RECRUIT_KNOBS`), and the spin-window knob
/// (`pool::SPIN_KNOBS`) into the single slice e1's `chassis_known_keys()`
/// folds into the known-key set and e2's `--config` dump renders. Pure
/// `&'static` metadata — there is no change to any hot-path `OnceLock`
/// read.
///
/// The element-by-element array (rather than a runtime concat) keeps
/// `SCHEDULER_KNOBS` a `const`: each record is still *defined* once in
/// its owning module; this only *references* it.
pub const SCHEDULER_KNOBS: &[KnobRecord] = &[
    worker_deque::DEQUE_KNOBS[0],
    worker_deque::DEQUE_KNOBS[1],
    worker_deque::DEQUE_KNOBS[2],
    worker_deque::DEQUE_KNOBS[3],
    worker_deque::DEQUE_KNOBS[4],
    calibrate::CALIBRATE_KNOBS[0],
    LIFECYCLE_KNOBS[0],
    RECRUIT_KNOBS[0],
    RECRUIT_KNOBS[1],
    RECRUIT_KNOBS[2],
    pool::SPIN_KNOBS[0],
];

#[cfg(test)]
mod knob_tests {
    use super::SCHEDULER_KNOBS;
    use crate::config::KnobKind;

    #[test]
    fn scheduler_knobs_cover_all_hot_path_env_keys() {
        let keys: Vec<&str> = SCHEDULER_KNOBS.iter().map(|r| r.env_key).collect();
        for expected in [
            "AETHER_LOCAL_STICKY_MAX",
            "AETHER_LOCAL_MAIL_BUDGET",
            "AETHER_LOCAL_TIME_BUDGET_US",
            "AETHER_PEER_STEAL",
            "AETHER_LOCAL_CHAIN_BACKSTOP",
            "AETHER_HANDOFF_COST_NS",
            "AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS",
            "AETHER_BLOB_RECRUIT_MIN",
            "AETHER_BLOB_RECRUIT_MAX",
            "AETHER_WAKE_COST_NANOS",
            "AETHER_SPIN_WINDOW_USEC",
        ] {
            assert!(
                keys.contains(&expected),
                "SCHEDULER_KNOBS missing {expected}; has {keys:?}",
            );
        }
        assert_eq!(SCHEDULER_KNOBS.len(), 11);
    }

    #[test]
    fn scheduler_knobs_are_all_hand_registered() {
        // None are confique fields — they ride OnceLock getters, so
        // every record is HandRegistered (the discriminator e2's dump
        // uses to know there's no Meta to walk).
        assert!(
            SCHEDULER_KNOBS
                .iter()
                .all(|r| matches!(r.kind, KnobKind::HandRegistered))
        );
    }

    #[test]
    fn adaptive_knobs_have_no_literal_default() {
        // time_budget / mail_budget are adaptive / off-by-default with
        // no single literal default (ADR-0090 unit b2): their record
        // default is None ("derived/unset"), satisfied by the doc text.
        for key in ["AETHER_LOCAL_TIME_BUDGET_US", "AETHER_LOCAL_MAIL_BUDGET"] {
            let rec = SCHEDULER_KNOBS
                .iter()
                .find(|r| r.env_key == key)
                .expect("knob present");
            assert!(
                rec.default.is_none(),
                "{key} should have no literal default"
            );
        }
    }
}
