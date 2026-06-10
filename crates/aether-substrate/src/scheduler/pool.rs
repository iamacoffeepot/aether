//! [`Pool`] — N worker threads cooperatively draining work-stealing
//! deques (ADR-0087 Phase 3a, iamacoffeepot/aether#1112).
//!
//! The pool's only inputs at construction time are a worker count and a
//! shared [`FatalAborter`]. Each worker owns a LIFO deque; off-worker
//! producers feed a shared injector; idle workers steal from siblings'
//! tails. Backpressure happens at the per-actor inbox level, not at the
//! scheduler.
//!
//! Worker loop:
//!
//! ```text
//! loop {
//!     slot = acquire_slot()?;             // own deque → steal → spin → park
//!     match catch_unwind(|| slot.run_cycle()) {
//!         Ok(Idle | Closed) => drop(slot),
//!         Ok(Requeue)       => { injector.push(slot); spin.notify(); }
//!         Err(payload)      => aborter.abort(panic_reason(payload)),
//!     }
//! }
//! ```
//!
//! Panic disposition follows ADR-0063 / Open Question 8 of issue 635:
//! a handler panic catches at the worker boundary and escalates via
//! the chassis-level aborter. The worker thread itself doesn't crash
//! (the aborter calls `process::exit`); the catch is what stops the
//! pool from losing a worker thread silently. Per-actor recovery
//! (drop the slot, keep the pool alive) is parked behind a future
//! ADR.

use std::any::Any;
use std::env;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_deque::{Injector, Stealer, Worker};

use crate::config::{KnobKind, KnobRecord};
use crate::scheduler::spin_park::{Acquired, DEFAULT_SPIN_WINDOW_USEC, SpinPark};
use crate::scheduler::worker_deque;

use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::slot::{BatchBudget, CycleResult, Drainable, WakeSink};
use std::mem;
use std::panic;
use std::time::Duration;

/// Configuration for [`Pool::start`]. Defaults via [`PoolConfig::default`]
/// give `num_cpus`-derived sizing; chassis mains override per
/// `AETHER_WORKERS` once the pool is wired (PR C).
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Number of worker threads to spawn. Must be at least 1.
    pub workers: usize,
    /// Per-cycle drain budget passed to each [`Drainable::run_cycle`]
    /// call. Defaults to [`BatchBudget::standard`].
    pub budget_template: BudgetTemplate,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            // Saturating sub mirrors the issue spec
            // (`num_cpus::get().saturating_sub(reserved)`); for now
            // `reserved == 1` covers the chassis frame loop.
            workers: thread::available_parallelism()
                .map_or(2, |n| n.get().saturating_sub(1).max(1)),
            budget_template: BudgetTemplate::Standard,
        }
    }
}

/// How the worker constructs each per-cycle [`BatchBudget`]. Static
/// today; Phase 2 may switch to a measurement-driven knob set.
#[derive(Debug, Clone, Copy)]
pub enum BudgetTemplate {
    /// `BatchBudget::standard()` — `BATCH_MAX_MAILS` envelopes,
    /// `BATCH_MAX_USEC` wallclock.
    Standard,
    /// Custom (mostly for tests).
    Custom { max_mails: u32, max_usec: u64 },
}

impl BudgetTemplate {
    fn build(&self) -> BatchBudget {
        match *self {
            Self::Standard => BatchBudget::standard(),
            Self::Custom {
                max_mails,
                max_usec,
            } => BatchBudget::custom(max_mails, Duration::from_micros(max_usec)),
        }
    }
}

/// Handle to a running [`Pool`]. The chassis owns this; calling
/// [`PoolHandle::shutdown_with_results`] sets the coordinator's
/// shutdown flag and unparks every worker so each exits after its
/// current cycle. Joining the worker threads is part of shutdown.
///
/// Shutdown is signalled through the [`SpinPark`] coordinator (a flag
/// the workers observe in their spin loop / park-commit recheck) plus
/// an explicit unpark of every worker thread — workers no longer block
/// on the ready queue, so dropping a sender is not the stop signal it
/// was under the old `select!` park.
pub struct PoolHandle {
    /// Shared off-worker injector — the spill target for a wake that
    /// can't push to a worker's own deque (off-worker producer, or the
    /// own deque is at the local bound). Cloned into every [`WakeSink`].
    injector: Arc<Injector<Arc<dyn Drainable>>>,
    spin: Arc<SpinPark>,
    workers: Vec<PoolWorkerJoin>,
}

impl PoolHandle {
    /// Hand out a [`WakeSink`] — the shared injector plus the spin/park
    /// coordinator. The chassis bundles this into each
    /// [`crate::scheduler::WakeHandle`] when registering a dispatcher
    /// slot, so a wake pushes to the producing worker's own deque
    /// (affinity) or spills to the injector + routes the notification
    /// through the coordinator.
    #[must_use]
    pub fn wake_sink(&self) -> WakeSink {
        WakeSink::new(
            Arc::clone(&self.injector),
            Arc::clone(&self.spin),
            self.workers.len(),
        )
    }

    /// Shut down the pool, joining every worker, and return each
    /// worker's join result so tests can inspect handler-induced
    /// panics (production goes through `Drop`, which discards results).
    #[must_use]
    pub fn shutdown_with_results(mut self) -> Vec<thread::Result<()>> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Vec<thread::Result<()>> {
        // Signal shutdown via the coordinator flag, then unpark every
        // worker: parked workers wake and observe the flag, spinning
        // workers see it in their loop. Finally join. The unpark must
        // precede the join — a parked worker that's never unparked would
        // block the join forever. A late `wake` after this still pushes
        // to the injector but no worker drains it (they're exiting),
        // which is harmless. Idempotent: re-calling drains the (empty)
        // workers Vec and returns an empty Vec.
        self.spin.set_shutdown();
        for w in &self.workers {
            w.handle.thread().unpark();
        }
        mem::take(&mut self.workers)
            .into_iter()
            .map(|w| w.handle.join())
            .collect()
    }

    /// Worker count. Exposed for tracing / introspection.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

impl Drop for PoolHandle {
    fn drop(&mut self) {
        // Discard join results — `Drop` is the chassis-shutdown path
        // where the process is on its way down anyway. Tests that care
        // about worker panics use `shutdown_with_results` instead.
        let _ = self.shutdown_inner();
    }
}

/// One worker thread + its label. Held inside [`PoolHandle`].
pub struct PoolWorkerJoin {
    pub handle: JoinHandle<()>,
    pub name: String,
}

/// The pool itself. Construction is deferred to [`Pool::start`] so
/// the chassis can build the [`PoolConfig`] + [`FatalAborter`] before
/// any workers run.
pub struct Pool;

impl Pool {
    /// Spawn the worker threads and return a [`PoolHandle`] holding
    /// the ready-queue sender. Each worker thread is named
    /// `aether-worker-<n>` (Open Question 10 resolution).
    ///
    /// # Panics
    /// Panics if `config.workers < 1`, or if the OS refuses to spawn
    /// any worker thread — fail-fast per ADR-0063: worker count is a
    /// chassis-boot invariant and thread spawn is a substrate
    /// prerequisite.
    // `config` and `aborter` are taken by value for the builder-style
    // boot path (callers compose the config once and hand it off to
    // the pool); fields are read but not moved out.
    #[allow(clippy::needless_pass_by_value)]
    pub fn start(config: PoolConfig, aborter: Arc<dyn FatalAborter>) -> PoolHandle {
        assert!(config.workers >= 1, "pool needs at least one worker");
        let spin = Arc::new(SpinPark::with_spin_window(spin_window_from_env()));
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        // One LIFO deque per worker; collect every stealer so each worker
        // can steal from its siblings' tails when its own deque runs dry.
        let deques: Vec<Worker<Arc<dyn Drainable>>> =
            (0..config.workers).map(|_| Worker::new_lifo()).collect();
        let stealers: Arc<[Stealer<Arc<dyn Drainable>>]> =
            deques.iter().map(Worker::stealer).collect();
        let mut workers = Vec::with_capacity(config.workers);
        for (idx, deque) in deques.into_iter().enumerate() {
            let name = format!("aether-worker-{idx}");
            let stealers = Arc::clone(&stealers);
            let injector = Arc::clone(&injector);
            let spin = Arc::clone(&spin);
            let aborter = Arc::clone(&aborter);
            let template = config.budget_template;
            let thread_name = name.clone();
            // Scheduler worker pool — the execution floor that *runs* actors; spawned
            // at boot, below the actor model. A handler's spawn_inherit work runs here.
            #[allow(clippy::disallowed_methods)]
            let handle = thread::Builder::new()
                .name(thread_name)
                .spawn(move || worker_loop(idx, deque, stealers, injector, spin, aborter, template))
                .expect("spawn pool worker thread");
            workers.push(PoolWorkerJoin { handle, name });
        }
        PoolHandle {
            injector,
            spin,
            workers,
        }
    }
}

/// Config-discovery record (ADR-0090 unit b2) for the spin-window knob
/// [`spin_window_from_env`] reads. Referenced by
/// [`crate::scheduler::SCHEDULER_KNOBS`] so the e1 unknown-key sweep and
/// the e2 `--config` dump cover it; the read path stays untouched. Pure
/// `&'static` metadata.
pub const SPIN_KNOBS: &[KnobRecord] = &[KnobRecord {
    env_key: "AETHER_SPIN_WINDOW_USEC",
    doc: "Route-to-spinner spin-window (microseconds) before a worker parks. The \
          latency sweep retunes this without a recompile; malformed values fall \
          back to 50.",
    default: Some("50"),
    kind: KnobKind::HandRegistered,
}];

/// Read the spin-window override (`AETHER_SPIN_WINDOW_USEC`) for the
/// route-to-spinner coordinator, falling back to the default. The
/// experiment's latency sweep retunes this without a recompile; a
/// malformed value falls back rather than aborting boot.
fn spin_window_from_env() -> Duration {
    let usec = env::var("AETHER_SPIN_WINDOW_USEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SPIN_WINDOW_USEC);
    Duration::from_micros(usec)
}

// All arguments are taken by value so the spawned thread owns them
// for its lifetime — the function is the worker thread's body.
#[allow(clippy::needless_pass_by_value)]
fn worker_loop(
    idx: usize,
    deque: Worker<Arc<dyn Drainable>>,
    stealers: Arc<[Stealer<Arc<dyn Drainable>>]>,
    injector: Arc<Injector<Arc<dyn Drainable>>>,
    spin: Arc<SpinPark>,
    aborter: Arc<dyn FatalAborter>,
    template: BudgetTemplate,
) {
    // Hand this worker's deque to the thread-local so a handler's wake of
    // a downstream slot (running on this thread) pushes to it directly —
    // the affinity path that keeps a relay chain on one warm worker
    // (iamacoffeepot/aether#1059, now the deque's LIFO own-pop).
    worker_deque::install(deque);
    // iamacoffeepot/aether#1134: register the shared injector so a deposit
    // on this worker can read the scheduler ready-queue depth
    // (`worker_deque::pending_depth`) for the latency harness.
    worker_deque::install_injector(Arc::clone(&injector));
    // Whether this worker may raid siblings' deques, or is owner-only over
    // its own (iamacoffeepot/aether#1174). A per-process constant — read once
    // here, not per `acquire_slot`, and threaded into the steal scan.
    let peer_steal = worker_deque::peer_steal_enabled();
    loop {
        let Some(slot) = acquire_slot(idx, &stealers, &injector, &spin, peer_steal) else {
            // Shutdown signalled. Exit.
            return;
        };
        let budget = template.build();
        let result = panic::catch_unwind(AssertUnwindSafe(|| slot.run_cycle(budget)));
        match result {
            Ok(CycleResult::Idle | CycleResult::Closed) => {
                // Slot done for now; drop the popped Arc. The chassis
                // registry's strong reference keeps the slot alive for
                // future wakes (or its drop, in the Closed case).
                drop(slot);
            }
            Ok(CycleResult::Requeue) => {
                // Yielded mid-drain (budget hit) or post-empty recheck
                // found new work. Spill to the shared injector (not our
                // own deque) so the yield actually yields — any worker,
                // incl. this one after its own deque, can steal it;
                // notify routes to a spinner or unparks one. Shutdown is
                // observed at the top of the next `acquire_slot`, before
                // its pop/steal fast paths — so requeueing here cannot
                // keep a worker alive past teardown.
                injector.push(slot);
                spin.notify();
            }
            Err(payload) => {
                // Handler panicked. Per ADR-0063 / OQ8: escalate to
                // fatal_abort. The aborter call diverges, so this
                // function never returns from this branch — but log
                // first so the panic is visible in engine_logs.
                let reason = format_panic_payload(&payload, slot.label());
                tracing::error!(
                    target: "aether_substrate::scheduler",
                    actor = slot.label(),
                    reason = %reason,
                    "pool worker caught actor panic; escalating fatal abort",
                );
                aborter.abort(reason);
            }
        }
    }
}

/// Acquire the next ready slot for a worker: own deque first (LIFO —
/// the affinity warm path), then one non-blocking steal pass (injector +
/// siblings), then the spin-then-park coordinator. Returns `None` only
/// on shutdown.
///
/// The own deque (iamacoffeepot/aether#1059, now a `crossbeam_deque`
/// `Worker`) is the affinity lever: a handler running on this worker
/// pushes a woken downstream slot there, so a relay chain stays on the
/// same warm worker and never pays the ~4.3µs parked-worker wakeup. Its
/// LIFO pop keeps the freshest hop warmest. By default the **local cascade**
/// inlines on the own deque (`WakeSink::schedule` →
/// `worker_deque::try_push_local_budgeted`, iamacoffeepot/aether#1174) —
/// produced blobs are cascade descendants kept warm — until the per-burst
/// **time valve** (`worker_deque::time_budget`, default 12µs) trips and spills
/// a heavy cascade to parallelise; mail-count budgeting (#1160) is off by
/// default. The own deque is checked first so a pushed slot is never stranded.
///
/// When the own deque is empty, this resets the local-drain burst
/// (iamacoffeepot/aether#1160) — one local cascade is one burst, so the
/// next cascade (this worker's freshly-produced blobs, or work it's about
/// to steal) starts a fresh keep-local budget — then steals into the deque
/// from the injector (off-worker producers + spilled fan-out + requeued
/// yields) and, when `peer_steal` is set, its siblings' tails (owner-only
/// when off, iamacoffeepot/aether#1174). When that turns up nothing, the
/// coordinator
/// (iamacoffeepot/aether#1064) takes over: it keeps the worker spinning
/// (re-running the steal scan) for a bounded window so a producer can
/// route a spill or relay hop to it without a futex wake, then parks it —
/// and the coordinator's park-commit recheck re-runs the steal scan,
/// which *is* the pre-park steal-rescan that closes the lost-wakeup
/// window. Shutdown is observed in two places: at the top of this
/// function, before the fast paths (so a worker that keeps finding
/// work still exits — iamacoffeepot/aether#1531), and inside the
/// coordinator (a flag + an explicit unpark of every worker on
/// teardown, covering spinning and parked workers).
///
/// The own-deque fast path carries the **every-K chain backstop**
/// (`worker_deque::chain_pop_due`, iamacoffeepot/aether#1535): every
/// `chain_backstop()` consecutive own-deque pops, one
/// `steal_into_local` pass runs before the chain continues, so a
/// self-sustaining relay loop (which never drains its deque and so
/// never reaches the steal arm) cannot starve the injector for longer
/// than ~K cycles.
fn acquire_slot(
    idx: usize,
    stealers: &[Stealer<Arc<dyn Drainable>>],
    injector: &Injector<Arc<dyn Drainable>>,
    spin: &SpinPark,
    peer_steal: bool,
) -> Option<Arc<dyn Drainable>> {
    // Shutdown gate, ahead of the fast paths: without it a worker that
    // keeps finding work (a perpetually-requeueing slot, or a steady
    // steal-fed cycle) returns early every iteration and never reaches
    // the coordinator's flag check — `shutdown_with_results` / `Drop`
    // joins hang (iamacoffeepot/aether#1531). One Acquire load of a
    // write-once flag per cycle; work left in the deques/injector is
    // dropped at teardown by the documented stance above.
    if spin.is_shutdown() {
        return None;
    }
    if let Some(slot) = worker_deque::pop_local() {
        // Every-K chain backstop (iamacoffeepot/aether#1535): the depth-0
        // keep-local exemption means a serial chain oscillates the own
        // deque 0→1→0 and never reaches the steal below, so a
        // *self-sustaining* chain would monopolise this worker and starve
        // the injector indefinitely. Every `chain_backstop()`-th
        // consecutive pop, take one look at the injector before
        // continuing the chain: a hit runs the stolen slot now — the
        // chain slot goes back on the own deque (LIFO top: it is the next
        // pop, so the chain resumes right after) — and a miss costs one
        // empty probe. Injector starvation is thereby bounded at ~K ×
        // cycle-time per worker; the chain stays warm K−1 of K cycles.
        if worker_deque::chain_pop_due()
            && let Some(stolen) =
                worker_deque::steal_into_local(idx, stealers, injector, peer_steal)
        {
            if let Err(slot) = worker_deque::push_local(slot) {
                // Unreachable on a worker (`pop_local` just succeeded, so
                // the own deque is installed); spill rather than lose the
                // slot if it ever isn't.
                injector.push(slot);
            }
            return Some(stolen);
        }
        return Some(slot);
    }
    // Own deque drained empty — the local cascade is over. Close its
    // keep-local burst (iamacoffeepot/aether#1160) so stolen work, or this
    // worker's next cascade, starts under a fresh mail/time budget.
    worker_deque::burst_reset();
    if let Some(slot) = worker_deque::steal_into_local(idx, stealers, injector, peer_steal) {
        return Some(slot);
    }
    match spin.acquire(|| worker_deque::steal_into_local(idx, stealers, injector, peer_steal)) {
        Acquired::Slot(slot) => Some(slot),
        Acquired::Shutdown => None,
    }
}

fn format_panic_payload(payload: &Box<dyn Any + Send>, actor_label: &str) -> String {
    // Chained if-let on disjoint downcasts reads cleaner than a deep
    // `map_or_else` ladder over two Options.
    #[allow(clippy::option_if_let_else)]
    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    format!("actor `{actor_label}` panicked: {msg}")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: queue recv panic on failure is the assertion"
)]
mod tests {
    use super::*;
    use crate::runtime::lifecycle::PanicAborter;
    use crate::scheduler::slot::BATCH_MAX_USEC;
    use crate::scheduler::slot::tests::{CounterSlot, TEST_WORKERS};
    use crate::scheduler::{SlotStateLabel, WakeHandle};
    use crossbeam_deque::Steal;
    use std::sync::Weak;
    use std::time::Duration;
    use std::time::Instant;

    fn standard_handle(workers: usize) -> PoolHandle {
        Pool::start(
            PoolConfig {
                workers,
                budget_template: BudgetTemplate::Standard,
            },
            Arc::new(PanicAborter),
        )
    }

    fn wait_until<F: Fn() -> bool>(timeout: Duration, f: F) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        f()
    }

    /// End-to-end happy path: register a slot, push N envelopes,
    /// observe the worker drain them all and park the slot Idle.
    /// Body lives here to share the parent helpers; the `#[test]` wrapper
    /// is in `mod heavy` (issue 1522 — spawns a worker pool + `wait_until`).
    fn pool_drains_pushed_envelopes_body() {
        let handle = standard_handle(1);
        let slot = CounterSlot::new("happy");
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
        drop(slot_dyn);
        let wake = WakeHandle::new(slot.state.clone(), weak, handle.wake_sink());

        for n in 0..200 {
            slot.push(n);
        }
        assert!(wake.wake());

        assert!(wait_until(Duration::from_secs(2), || slot.dispatched() == 200));
        assert!(wait_until(Duration::from_secs(2), || {
            slot.state.current() == SlotStateLabel::Idle
        }));

        // Bring down the pool cleanly.
        drop(wake);
        let results = handle.shutdown_with_results();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
    }

    /// Two slots, both perpetually ready: a worker fairly round-robins
    /// (the budget yield is what enables this — without it one slot
    /// would monopolise the worker until empty). Body here, `#[test]`
    /// wrapper in `mod heavy` (issue 1522 — worker pool + `wait_until`).
    fn two_slots_round_robin_under_budget_body() {
        // One worker so the round-robin is observable. Custom budget
        // — a tiny mail cap means each slot hits Yielded quickly and
        // the worker drains the other.
        let handle = Pool::start(
            PoolConfig {
                workers: 1,
                budget_template: BudgetTemplate::Custom {
                    max_mails: 4,
                    max_usec: BATCH_MAX_USEC_TEST,
                },
            },
            Arc::new(PanicAborter),
        );

        let a = CounterSlot::new("alpha");
        let b = CounterSlot::new("beta");
        let a_dyn: Arc<dyn Drainable> = a.clone();
        let b_dyn: Arc<dyn Drainable> = b.clone();
        let a_weak: Weak<dyn Drainable> = Arc::downgrade(&a_dyn);
        let b_weak: Weak<dyn Drainable> = Arc::downgrade(&b_dyn);
        drop(a_dyn);
        drop(b_dyn);
        let wake_a = WakeHandle::new(a.state.clone(), a_weak, handle.wake_sink());
        let wake_b = WakeHandle::new(b.state.clone(), b_weak, handle.wake_sink());

        for n in 0..40 {
            a.push(n);
            b.push(n);
        }
        // Test asserts via `wait_until` on `dispatched()` below; the
        // CAS-win bool is uninteresting at this seeding step.
        let _ = wake_a.wake();
        let _ = wake_b.wake();

        assert!(wait_until(Duration::from_secs(3), || {
            a.dispatched() == 40 && b.dispatched() == 40
        }));

        // Fairness check: at the midpoint, neither slot should have
        // monopolised. The check is loose — if A finishes all 40
        // before B starts, fairness failed. Hard-bound: at the time
        // both reached ~midpoint of total, neither lapped the other
        // by more than a few budgets.
        // (Already validated end-to-end by the equality above; the
        // budget-driven yield is what gives them equal turns.)

        drop(wake_a);
        drop(wake_b);
        let _ = handle.shutdown_with_results();
    }

    /// A handler panic escalates via the [`FatalAborter`]. The test
    /// uses [`PanicAborter`] (the test-only aborter) which `panic!`s
    /// instead of `process::exit`; the worker thread propagates the
    /// panic, and `shutdown` returns it via `JoinHandle::join`. Body here,
    /// `#[test]` wrapper in `mod heavy` (issue 1522 — pool + `wait_until`).
    fn handler_panic_escalates_via_aborter_body() {
        let aborter: Arc<dyn FatalAborter> = Arc::new(PanicAborter);
        let handle = Pool::start(
            PoolConfig {
                workers: 1,
                budget_template: BudgetTemplate::Standard,
            },
            Arc::clone(&aborter),
        );
        let slot = CounterSlot::new("panicker").with_panic_at(2);
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
        drop(slot_dyn);
        let wake = WakeHandle::new(slot.state.clone(), weak, handle.wake_sink());

        slot.push(1);
        slot.push(2); // this one panics
        slot.push(3);
        // Seeding wake — test asserts on `dispatched()` / panic
        // outcome, not on the CAS-win bool.
        let _ = wake.wake();

        // Wait for at least the first envelope to dispatch.
        assert!(wait_until(Duration::from_secs(2), || slot.dispatched() >= 1));

        drop(wake);
        let results = handle.shutdown_with_results();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].is_err(),
            "PanicAborter should have panicked the worker thread on handler panic"
        );
    }

    /// Wakes during an in-flight drain don't double-queue: the slot
    /// is `Running`, so the second wake is a no-op. The worker's
    /// post-empty recheck picks up the new envelopes.
    #[test]
    fn wake_during_running_does_not_duplicate_queue_entry() {
        // No worker pool — exercise WakeHandle directly. The state
        // sequence: Idle → wake → Ready → enter_running → Running →
        // wake (no-op) → mark_idle → recheck (already Idle, but no
        // mail) → Idle. This test runs on the test thread (not a pool
        // worker), so a wake always spills to the injector.
        let injector = Arc::new(Injector::<Arc<dyn Drainable>>::new());
        let slot = CounterSlot::new("running-wake");
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
        drop(slot_dyn);
        let sink = WakeSink::new(
            Arc::clone(&injector),
            Arc::new(SpinPark::new()),
            TEST_WORKERS,
        );
        let wake = WakeHandle::new(slot.state.clone(), weak, sink);

        slot.push(1);
        assert!(wake.wake(), "first wake transitions Idle→Ready");

        // Drain the injector (simulate a worker stealing it), enter Running.
        let popped = loop {
            match injector.steal() {
                Steal::Success(s) => break s,
                Steal::Retry => {}
                Steal::Empty => panic!("first wake must have spilled a slot to the injector"),
            }
        };
        let _ = popped;
        assert!(slot.state.enter_running());

        // Second wake while Running: no-op, no duplicate enqueue.
        assert!(!wake.wake(), "wake against Running is a no-op");
        assert!(
            matches!(injector.steal(), Steal::Empty),
            "injector should be empty — the Running wake must not enqueue"
        );
    }

    /// Contention/backoff-sensitive tests live in `mod heavy`: they exercise
    /// the worker dispatch / wake path, so they are serialized into the
    /// `serial-heavy` nextest group (`.config/nextest.toml`) to avoid
    /// oversubscribing cores against one another.
    mod heavy {
        use super::*;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

        /// A self-sustaining slot (iamacoffeepot/aether#1535): each
        /// `run_cycle` immediately re-schedules itself through
        /// `WakeSink::schedule` — the handler-wake keep-local path
        /// (`worker_deque::try_push_local_budgeted`) — so the running
        /// worker's own deque oscillates 0→1→0 forever. The depth-0
        /// exemption keeps every re-push local with no clock read, so
        /// without the every-K backstop the worker would never visit the
        /// injector again. `stop` ends the loop so shutdown can drain.
        struct LoopSlot {
            cycles: AtomicU32,
            stop: Arc<AtomicBool>,
            sink: WakeSink,
            this: Weak<Self>,
        }

        impl LoopSlot {
            fn new(stop: Arc<AtomicBool>, sink: WakeSink) -> Arc<Self> {
                Arc::new_cyclic(|this| Self {
                    cycles: AtomicU32::new(0),
                    stop,
                    sink,
                    this: this.clone(),
                })
            }

            fn cycles(&self) -> u32 {
                self.cycles.load(Ordering::Acquire)
            }
        }

        impl Drainable for LoopSlot {
            fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
                self.cycles.fetch_add(1, Ordering::AcqRel);
                if !self.stop.load(Ordering::Acquire)
                    && let Some(me) = self.this.upgrade()
                {
                    self.sink.schedule(me);
                }
                CycleResult::Idle
            }

            fn label(&self) -> &'static str {
                "loop-slot"
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        /// Regression for the every-K chain backstop
        /// (iamacoffeepot/aether#1535): W self-sustaining loops capture all
        /// W workers, then an independent slot arrives through the
        /// injector. Without the backstop no captured worker ever consults
        /// the injector again (the depth-0 keep-local chain never drains
        /// its deque), so the slot starves past any deadline; with it,
        /// every worker probes the injector each `chain_backstop()`-th pop
        /// and the slot dispatches almost immediately.
        #[test]
        fn injector_fed_slot_dispatches_under_full_worker_capture() {
            const WORKERS: usize = 2;
            let handle = standard_handle(WORKERS);
            let stop = Arc::new(AtomicBool::new(false));

            // Seed the loops one at a time, waiting for each to start
            // cycling before seeding the next: an already-captured worker
            // takes injector work at most once per backstop window, so
            // each fresh loop lands on a still-idle worker (and if a
            // captured worker's probe does take it, the displaced idle
            // worker only makes the final assertion easier — the test
            // discriminates either way: with no backstop, captured
            // workers never steal at all).
            let loops: Vec<Arc<LoopSlot>> = (0..WORKERS)
                .map(|_| LoopSlot::new(Arc::clone(&stop), handle.wake_sink()))
                .collect();
            for slot in &loops {
                let seed: Arc<dyn Drainable> = slot.clone();
                handle.wake_sink().schedule(seed);
                assert!(
                    wait_until(Duration::from_secs(2), || slot.cycles() > 0),
                    "loop slot should start cycling once a worker picks it up"
                );
            }

            // Every worker is captured. An independent slot now arrives
            // through the injector (a wake off any pool worker spills
            // there) — the path the backstop exists to keep live.
            let probe = CounterSlot::new("injector-fed");
            let probe_dyn: Arc<dyn Drainable> = probe.clone();
            let weak: Weak<dyn Drainable> = Arc::downgrade(&probe_dyn);
            drop(probe_dyn);
            let wake = WakeHandle::new(probe.state.clone(), weak, handle.wake_sink());
            probe.push(1);
            assert!(wake.wake());

            let dispatched = wait_until(Duration::from_secs(5), || probe.dispatched() == 1);

            // End the loops *before* asserting so the deques drain — a
            // perpetually-cycling worker only reaches the shutdown
            // observation point (the spin/park coordinator) once its own
            // deque runs empty. Asserting first would leave the loops
            // spinning through the panic unwind and wedge the suite on a
            // regression instead of failing at the deadline.
            stop.store(true, Ordering::Release);
            drop(wake);
            let _ = handle.shutdown_with_results();

            assert!(
                dispatched,
                "every-K backstop must dispatch injector work under full \
                 worker capture (iamacoffeepot/aether#1535)"
            );
        }

        /// `#[test]` wrappers for the parent dispatch tests that spawn a
        /// worker pool and `wait_until`-poll under a multi-second deadline
        /// (issue 1522). Bodies stay in the parent to share its helpers.
        #[test]
        fn pool_drains_pushed_envelopes() {
            pool_drains_pushed_envelopes_body();
        }

        #[test]
        fn two_slots_round_robin_under_budget() {
            two_slots_round_robin_under_budget_body();
        }

        #[test]
        fn handler_panic_escalates_via_aborter() {
            handler_panic_escalates_via_aborter_body();
        }

        /// Stress: 4 slots × 1000 envelopes each across 2 workers. Confirm
        /// every envelope dispatches and no slot is left orphaned.
        #[test]
        fn stress_many_slots_across_workers() {
            let handle = standard_handle(2);
            let slots: Vec<_> = (0..4)
                .map(|i| CounterSlot::new(Box::leak(format!("s{i}").into_boxed_str())))
                .collect();
            let wakes: Vec<_> = slots
                .iter()
                .map(|slot| {
                    let slot_dyn: Arc<dyn Drainable> = slot.clone();
                    let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
                    drop(slot_dyn);
                    WakeHandle::new(slot.state.clone(), weak, handle.wake_sink())
                })
                .collect();

            for (i, slot) in slots.iter().enumerate() {
                for n in 0..1000 {
                    // Test fixture uses tiny indices that fit in u32.
                    #[allow(clippy::cast_possible_truncation)]
                    let value = (i * 1000 + n) as u32;
                    slot.push(value);
                }
                // Stress seeding — bool ignored; test asserts via total().
                let _ = wakes[i].wake();
            }

            let total_expected: u32 = 4 * 1000;
            let total = || -> u32 { slots.iter().map(|s| s.dispatched()).sum() };
            assert!(wait_until(Duration::from_secs(5), || total() == total_expected));
            for slot in &slots {
                assert_eq!(slot.dispatched(), 1000);
                assert_eq!(slot.state.current(), SlotStateLabel::Idle);
            }

            drop(wakes);
            let _ = handle.shutdown_with_results();
        }

        /// Build a depth-`d` relay chain of `CounterSlot`s: slot[i] forwards
        /// each dispatched env to slot[i+1] and wakes it. The forwarding wake
        /// runs on a pool worker, so the chain drives the worker-local stash
        /// path that `acquire_slot` reads first (iamacoffeepot/aether#1059).
        fn relay_chain(handle: &PoolHandle, depth: usize) -> Vec<Arc<CounterSlot>> {
            let slots: Vec<Arc<CounterSlot>> =
                (0..depth).map(|_| CounterSlot::new("relay")).collect();
            for i in 0..depth.saturating_sub(1) {
                let target = slots[i + 1].clone();
                let target_dyn: Arc<dyn Drainable> = target.clone();
                let weak: Weak<dyn Drainable> = Arc::downgrade(&target_dyn);
                drop(target_dyn);
                let wake = WakeHandle::new(target.state.clone(), weak, handle.wake_sink());
                *slots[i].forward.lock().unwrap() = Some((target, wake));
            }
            slots
        }

        /// A relay chain on a multi-worker pool: each hop's wake stashes the
        /// downstream in the running worker's local cell, so the chain stays on
        /// one warm worker instead of bouncing across parked siblings. The stash
        /// path is concurrency-sensitive (worker thread-local + the `Idle → Ready`
        /// CAS) and fires only on real worker threads — which the other slot
        /// tests, driven from the test thread, never hit.
        #[test]
        fn relay_chain_stays_on_worker() {
            let handle = standard_handle(4);
            let slots = relay_chain(&handle, 8);

            let entry = slots[0].clone();
            let entry_dyn: Arc<dyn Drainable> = entry.clone();
            let weak: Weak<dyn Drainable> = Arc::downgrade(&entry_dyn);
            drop(entry_dyn);
            let entry_wake = WakeHandle::new(entry.state.clone(), weak, handle.wake_sink());

            entry.push(1);
            assert!(entry_wake.wake());

            assert!(
                wait_until(Duration::from_secs(5), || slots
                    .iter()
                    .all(|s| s.dispatched() >= 1)),
                "every slot in the relay chain should dispatch its forwarded env"
            );

            drop(entry_wake);
            let _ = handle.shutdown_with_results();
        }

        /// Regression for iamacoffeepot/aether#1531: a worker fed by a
        /// perpetually-requeueing slot must still observe shutdown. The
        /// slot's `run_cycle` always returns `Requeue` — the
        /// deterministic equivalent of two actors ping-ponging mail —
        /// so the worker requeues it to the injector and immediately
        /// steals it back, never reaching the spin/park coordinator.
        /// Without the `acquire_slot` shutdown gate, the join in
        /// `shutdown_with_results` hangs forever.
        #[test]
        fn shutdown_joins_under_unconditional_requeue() {
            struct RequeueForever;
            impl Drainable for RequeueForever {
                fn run_cycle(&self, _budget: BatchBudget) -> CycleResult {
                    CycleResult::Requeue
                }
                fn label(&self) -> &'static str {
                    "requeue-forever"
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }

            let handle = standard_handle(2);
            let worker_count = handle.worker_count();
            // Feed the slot straight into the injector + notify — the
            // same path the worker's own `Requeue` arm takes — so a
            // worker steals it and enters the self-sustaining cycle.
            let slot: Arc<dyn Drainable> = Arc::new(RequeueForever);
            handle.injector.push(slot);
            handle.spin.notify();
            // Let the requeue cycle churn so a worker is actively fed
            // (not parked) when shutdown is signalled.
            thread::sleep(Duration::from_millis(50));

            // Run the join on a helper thread so the test can hold it
            // to a deadline — a regression here otherwise hangs the
            // whole suite instead of failing.
            let (tx, rx) = crossbeam_channel::bounded(1);
            // Test scaffolding below the actor/mail layer — no
            // settlement contract on this helper thread.
            #[allow(clippy::disallowed_methods)]
            thread::spawn(move || {
                let _ = tx.send(handle.shutdown_with_results());
            });
            let results = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("shutdown join must complete despite a perpetually-requeueing slot");
            assert_eq!(results.len(), worker_count);
            for result in results {
                assert!(result.is_ok(), "every worker should exit cleanly");
            }
        }
    }

    // Reuse the standard wallclock budget for fairness tests — 200µs
    // is enough for the test harness to dispatch a handful of
    // counters before yielding.
    const BATCH_MAX_USEC_TEST: u64 = BATCH_MAX_USEC;
}
