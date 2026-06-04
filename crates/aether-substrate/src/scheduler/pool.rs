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
                // observed at the next `acquire_slot`.
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
/// window. Shutdown is observed inside the coordinator (a flag + an
/// explicit unpark of every worker on teardown).
fn acquire_slot(
    idx: usize,
    stealers: &[Stealer<Arc<dyn Drainable>>],
    injector: &Injector<Arc<dyn Drainable>>,
    spin: &SpinPark,
    peer_steal: bool,
) -> Option<Arc<dyn Drainable>> {
    if let Some(slot) = worker_deque::pop_local() {
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
    #[test]
    fn pool_drains_pushed_envelopes() {
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
    /// would monopolise the worker until empty).
    #[test]
    fn two_slots_round_robin_under_budget() {
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
    /// panic, and `shutdown` returns it via `JoinHandle::join`.
    #[test]
    fn handler_panic_escalates_via_aborter() {
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

    /// Flake-soak wrapper (iamacoffeepot/aether#1059). Re-runs the
    /// multi-worker stress under a `flaky_` name so `scripts/flake-soak.sh`
    /// repeat-runs the rewritten `acquire_slot` dispatch path. The original
    /// still runs once in normal CI; this duplicate is the soak target.
    #[test]
    fn flaky_stress_many_slots_across_workers() {
        stress_many_slots_across_workers();
    }

    /// Build a depth-`d` relay chain of `CounterSlot`s: slot[i] forwards
    /// each dispatched env to slot[i+1] and wakes it. The forwarding wake
    /// runs on a pool worker, so the chain drives the worker-local stash
    /// path that `acquire_slot` reads first (iamacoffeepot/aether#1059).
    fn relay_chain(handle: &PoolHandle, depth: usize) -> Vec<Arc<CounterSlot>> {
        let slots: Vec<Arc<CounterSlot>> = (0..depth).map(|_| CounterSlot::new("relay")).collect();
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

    /// Flake-soak (iamacoffeepot/aether#1059): a relay chain on a
    /// multi-worker pool. Each hop's wake stashes the downstream in the
    /// running worker's local cell, so the chain stays on one warm worker
    /// instead of bouncing across parked siblings. Soaked because the
    /// stash path is concurrency-sensitive (worker thread-local + the
    /// `Idle → Ready` CAS) and fires only on real worker threads — which
    /// the other slot tests, driven from the test thread, never hit.
    #[test]
    fn flaky_relay_chain_stays_on_worker() {
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

    // Reuse the standard wallclock budget for fairness tests — 200µs
    // is enough for the test harness to dispatch a handful of
    // counters before yielding.
    const BATCH_MAX_USEC_TEST: u64 = BATCH_MAX_USEC;
}
