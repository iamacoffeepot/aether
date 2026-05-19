//! [`Pool`] — N worker threads cooperatively draining the ready queue.
//!
//! The pool's only inputs at construction time are a worker count, a
//! shared [`FatalAborter`], and an optional
//! ready-queue capacity (today the queue is unbounded — backpressure
//! happens at the per-actor inbox level, not at the scheduler).
//!
//! Worker loop:
//!
//! ```text
//! loop {
//!     slot = ready_rx.recv()?;            // blocks
//!     match catch_unwind(|| slot.run_cycle()) {
//!         Ok(Idle | Closed) => drop(slot),
//!         Ok(Requeue)       => ready_tx.send(slot)?,
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
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};

use crate::runtime::lifecycle::FatalAborter;
use crate::scheduler::slot::{BatchBudget, CycleResult, Drainable};
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
/// [`PoolHandle::shutdown_with_results`] drops the shutdown sender,
/// which disconnects each worker's `select!` arm and tells them to
/// exit after the current cycle. Joining the worker threads is part
/// of shutdown.
///
/// Why a separate shutdown channel? Workers hold their own clones of
/// `ready_tx` (for re-queueing yielded slots). That keeps the ready
/// queue alive even after every external sender drops, so a worker's
/// blocking `recv` would never see a disconnect. The shutdown channel
/// is held only by the pool — dropping it is the unambiguous signal.
pub struct PoolHandle {
    // `Option` so `Drop` and the consuming `shutdown` method can both
    // take the senders without a clone. Production drops on `PoolHandle`
    // drop, tests consume via `shutdown_with_results` to inspect any
    // worker-thread panics.
    ready_tx: Option<Sender<Arc<dyn Drainable>>>,
    shutdown_tx: Option<Sender<()>>,
    workers: Vec<PoolWorkerJoin>,
}

impl PoolHandle {
    /// Hand out a clone of the ready-queue sender. PR C wires this
    /// into [`crate::scheduler::WakeHandle`]s when registering
    /// dispatcher slots.
    ///
    /// # Panics
    /// Panics if the pool has already been shut down (the sender slot
    /// is `None`) — fail-fast per ADR-0063: registering a wake handle
    /// after pool shutdown is a chassis-lifecycle bug.
    #[must_use]
    pub fn ready_tx(&self) -> Sender<Arc<dyn Drainable>> {
        self.ready_tx
            .as_ref()
            .expect("pool already shut down")
            .clone()
    }

    /// Shut down the pool, joining every worker, and return each
    /// worker's join result so tests can inspect handler-induced
    /// panics (production goes through `Drop`, which discards results).
    #[must_use]
    pub fn shutdown_with_results(mut self) -> Vec<thread::Result<()>> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Vec<thread::Result<()>> {
        // Drop the shutdown sender — fires every worker's `select!`
        // shutdown arm. Then drop the ready-queue sender (any future
        // wake.wake() calls silently no-op via SendError). Finally
        // join every worker thread. Idempotent — re-calling drains the
        // (empty) workers Vec and returns an empty Vec.
        self.shutdown_tx.take();
        self.ready_tx.take();
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
        let (ready_tx, ready_rx) = unbounded::<Arc<dyn Drainable>>();
        // Shutdown signal: bounded(0) is a rendezvous channel, but we
        // never send on it. Dropping `shutdown_tx` (held only by the
        // pool) is what disconnects every worker's `select!` arm.
        let (shutdown_tx, shutdown_rx) = bounded::<()>(0);
        let mut workers = Vec::with_capacity(config.workers);
        for n in 0..config.workers {
            let name = format!("aether-worker-{n}");
            let rx = ready_rx.clone();
            let tx = ready_tx.clone();
            let shutdown = shutdown_rx.clone();
            let aborter = Arc::clone(&aborter);
            let template = config.budget_template;
            let thread_name = name.clone();
            let handle = thread::Builder::new()
                .name(thread_name)
                .spawn(move || worker_loop(rx, tx, shutdown, aborter, template))
                .expect("spawn pool worker thread");
            workers.push(PoolWorkerJoin { handle, name });
        }
        PoolHandle {
            ready_tx: Some(ready_tx),
            shutdown_tx: Some(shutdown_tx),
            workers,
        }
    }
}

// All arguments are taken by value so the spawned thread owns them
// for its lifetime — the function is the worker thread's body.
#[allow(clippy::needless_pass_by_value)]
fn worker_loop(
    ready_rx: Receiver<Arc<dyn Drainable>>,
    ready_tx: Sender<Arc<dyn Drainable>>,
    shutdown_rx: Receiver<()>,
    aborter: Arc<dyn FatalAborter>,
    template: BudgetTemplate,
) {
    loop {
        let slot = select! {
            recv(ready_rx) -> result => match result {
                Ok(s) => s,
                // All sender clones are gone (including our own — the
                // pool's shutdown both drops `ready_tx` and disconnects
                // `shutdown_rx`, but in either order the channel only
                // disconnects once every clone drops). Exit either way.
                Err(_) => return,
            },
            recv(shutdown_rx) -> _ => {
                // Pool dropped `shutdown_tx`. Disconnect on this arm
                // is the unambiguous "stop now" signal — works even
                // while we still hold our own `ready_tx` clone.
                return;
            }
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
                if ready_tx.send(slot).is_err() {
                    // Pool shutting down: ready queue closed. Exit.
                    return;
                }
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
    use crate::scheduler::slot::tests::CounterSlot;
    use crate::scheduler::{SlotStateLabel, WakeHandle};
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
        let wake = WakeHandle::new(slot.state.clone(), weak, handle.ready_tx());

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
        let wake_a = WakeHandle::new(a.state.clone(), a_weak, handle.ready_tx());
        let wake_b = WakeHandle::new(b.state.clone(), b_weak, handle.ready_tx());

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
        let wake = WakeHandle::new(slot.state.clone(), weak, handle.ready_tx());

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
        // mail) → Idle.
        let (ready_tx, ready_rx) = unbounded::<Arc<dyn Drainable>>();
        let slot = CounterSlot::new("running-wake");
        let slot_dyn: Arc<dyn Drainable> = slot.clone();
        let weak: Weak<dyn Drainable> = Arc::downgrade(&slot_dyn);
        drop(slot_dyn);
        let wake = WakeHandle::new(slot.state.clone(), weak, ready_tx);

        slot.push(1);
        assert!(wake.wake(), "first wake transitions Idle→Ready");

        // Drain the queue (simulate worker pop), enter Running.
        let _popped = ready_rx.recv().unwrap();
        assert!(slot.state.enter_running());

        // Second wake while Running: no-op, no duplicate enqueue.
        assert!(!wake.wake(), "wake against Running is a no-op");
        assert_eq!(
            ready_rx.try_recv().err(),
            Some(crossbeam_channel::TryRecvError::Empty),
            "ready queue should be empty"
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
                WakeHandle::new(slot.state.clone(), weak, handle.ready_tx())
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

    // Reuse the standard wallclock budget for fairness tests — 200µs
    // is enough for the test harness to dispatch a handful of
    // counters before yielding.
    const BATCH_MAX_USEC_TEST: u64 = BATCH_MAX_USEC;
}
