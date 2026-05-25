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
//! and no parked-sibling wake (~4.3µs). Whether a just-produced blob stays
//! on the own deque or spills to the injector + notify is the **keep-local
//! budget** (iamacoffeepot/aether#1160, [`try_push_local_budgeted`]): a
//! worker keeps draining its own cascade while under a per-burst mail +
//! sampled-time budget, then spills the backlog once it has done enough
//! cheap local work to justify waking a sibling. The default-preserving
//! config (`AETHER_LOCAL_MAIL_BUDGET=0`) reproduces the historical
//! `cap == 1` "spill any fan-out extra" behaviour; `AETHER_LOCAL_STICKY_MAX`
//! is repurposed as the deque-length safety backstop ([`hard_cap`]). The
//! tail an idle worker can `steal_batch_and_pop` is the pull path.
//!
//! Only pool-worker threads call [`install`]; on any other thread
//! (chassis main, the hub, the trace drainer) [`try_push_local_budgeted`]
//! is a no-op spill and [`pop_local`] / [`steal_into_local`] yield nothing.

use std::cell::{Cell, RefCell};
use std::env;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

    /// The shared off-worker [`Injector`], registered per worker by
    /// [`install_injector`] at the top of the worker loop
    /// (iamacoffeepot/aether#1134). Held only so [`pending_depth`] can read
    /// the injector backlog without threading a reference through the
    /// deposit path; `None` on non-worker threads (chassis main, hub,
    /// off-worker injects), where `pending_depth` reports `0`.
    static INJECTOR: RefCell<Option<Arc<Injector<Slot>>>> = const { RefCell::new(None) };

    /// Per-burst mail counter for the keep-local budget
    /// (iamacoffeepot/aether#1160). A *burst* is the run of local-deque work
    /// a worker drains between two "own deque drained empty" transitions —
    /// one local cascade. [`burst_note_mail`] increments it per dispatched
    /// envelope; [`burst_over_budget`] consults it per keep-vs-spill
    /// decision; [`burst_reset`] zeroes it when `acquire_slot` finds the
    /// deque empty. Single-writer (only the running worker touches it, never
    /// across a `run_cycle`), so a plain `Cell` — no atomics.
    static BURST_MAIL: Cell<u32> = const { Cell::new(0) };

    /// Start instant of the current burst, anchored on its first mail by
    /// [`burst_note_mail`] when time budgeting is on (iamacoffeepot/aether#1160).
    /// `None` when time budgeting is off (the default — no clock ever read)
    /// or before the burst's first mail. [`burst_over_budget`] reads
    /// `elapsed()` against it at decision time.
    static BURST_START: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Move this worker's deque into its thread-local. Called once at the top
/// of the worker loop; enables local push/pop on this thread.
pub fn install(worker: Worker<Slot>) {
    LOCAL.with(|w| *w.borrow_mut() = Some(worker));
}

/// Register the shared injector for this worker thread so
/// [`pending_depth`] can read its backlog (iamacoffeepot/aether#1134).
/// Called once alongside [`install`] at the top of the worker loop;
/// no-op effect on dispatch (depth is measurement-only).
pub fn install_injector(injector: Arc<Injector<Slot>>) {
    INJECTOR.with(|i| *i.borrow_mut() = Some(injector));
}

/// Scheduler ready-queue depth observed from this thread: this worker's
/// own-deque len plus the shared injector len (iamacoffeepot/aether#1134).
/// `0` off any pool worker (no own deque installed) — chassis-root
/// injects and other off-worker deposits report no backlog. Read at mail
/// deposit and carried on the envelope so the latency harness can split
/// queue residence into *wakeup* (depth 0) vs *wait-behind-N* (load).
///
/// Both `Worker::len` and `Injector::len` are cheap O(1)-ish reads; this
/// is a relaxed snapshot, not a synchronization point — a racing push by
/// a sibling may land just after the read, which is fine for a profiling
/// signal.
#[must_use]
pub fn pending_depth() -> u32 {
    let own = LOCAL.with(|w| w.borrow().as_ref().map_or(0, Worker::len));
    let injected = INJECTOR.with(|i| i.borrow().as_ref().map_or(0, |inj| inj.len()));
    u32::try_from(own.saturating_add(injected)).unwrap_or(u32::MAX)
}

/// Deque-length safety backstop (iamacoffeepot/aether#1160) — the max
/// slots a worker keeps on its own deque before [`try_push_local_budgeted`]
/// is forced to spill regardless of the mail/time budget, so a pathological
/// unbounded local cascade can't grow the deque without bound. Read once
/// from `AETHER_LOCAL_STICKY_MAX` (repurposed from the pre-#1160 stickiness
/// cap); values `< 1` and unparseable input fall back to `256`. This is a
/// backstop, not the primary governor — the mail/time budget is.
#[must_use]
pub fn hard_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        env::var("AETHER_LOCAL_STICKY_MAX")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&k| k >= 1)
            .unwrap_or(256)
    })
}

/// Keep-local mail budget per burst (iamacoffeepot/aether#1160). Read once
/// from `AETHER_LOCAL_MAIL_BUDGET`; default **0**, which makes
/// [`burst_over_budget`] always `true` so the decision collapses to "spill
/// any fan-out extra" — the default-preserving Phase 1 config that
/// reproduces the historical `cap == 1`. Set `> 0` to opt into keeping a
/// small local cascade on the producing worker (the keep-local win).
#[must_use]
pub fn mail_budget() -> u32 {
    static B: OnceLock<u32> = OnceLock::new();
    *B.get_or_init(|| {
        env::var("AETHER_LOCAL_MAIL_BUDGET")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
    })
}

/// Keep-local time budget per burst (iamacoffeepot/aether#1160). Read once
/// from `AETHER_LOCAL_TIME_BUDGET_US` (microseconds); default **0** =
/// disabled, so no wall clock is ever read (the default-preserving config).
/// When set this is roughly the parked-worker wakeup break-even (≈4–8µs, to
/// be pinned by the Phase 2 sweep): a burst that runs longer spills its
/// backlog even before the mail count trips.
#[must_use]
pub fn time_budget() -> Duration {
    static B: OnceLock<u64> = OnceLock::new();
    let us = *B.get_or_init(|| {
        env::var("AETHER_LOCAL_TIME_BUDGET_US")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    });
    Duration::from_micros(us)
}

/// Note one dispatched envelope against the current local-drain burst
/// (iamacoffeepot/aether#1160). Increments the burst mail counter and, when
/// time budgeting is on (`time_budget > 0`), anchors the burst start on the
/// first mail so `burst_over_budget` can measure elapsed at decision time.
/// With `time_budget == 0` (the default-preserving config) the clock is
/// never read — a single `Cell` increment. The clock is sampled at
/// *decision* time, not per mail (iamacoffeepot/aether#1160 fix): the prior
/// strided per-mail sample never fired for a sub-stride burst, so a narrow
/// *heavy* cascade (few mail × expensive handlers) never tripped the time
/// budget — exactly the case the valve exists to catch.
pub fn burst_note_mail(time_budget: Duration) {
    let n = BURST_MAIL.get().saturating_add(1);
    BURST_MAIL.set(n);
    // Anchor the burst start at its first mail (one clock read per burst,
    // only when time budgeting is on) so a heavy first handler's elapsed is
    // counted by the time path — not deferred to the first decision, which
    // would under-count the work that already ran.
    if !time_budget.is_zero() && n == 1 {
        BURST_START.set(Some(Instant::now()));
    }
}

/// Has the current burst exceeded its keep-local budget
/// (iamacoffeepot/aether#1160)? `true` once the burst has dispatched
/// `mail_budget` envelopes (`mail_budget == 0` ⇒ always over — the
/// default-preserving config reproducing the historical `cap == 1`) or has
/// run past `time_budget` since its first mail. The mail count short-
/// circuits, so the wall clock is read only when the count is under budget
/// *and* time budgeting is on — i.e. once per genuine keep-vs-spill
/// decision on a multi-blob backlog, never on the trivial path. Called by
/// [`try_push_local_budgeted`] only after the `depth > 0` guard, so a
/// single-blob fan-out or a chain (depth 0) reads no clock at all.
#[must_use]
pub fn burst_over_budget(mail_budget: u32, time_budget: Duration) -> bool {
    if mail_budget == 0 || BURST_MAIL.get() >= mail_budget {
        return true;
    }
    if time_budget.is_zero() {
        return false;
    }
    BURST_START
        .get()
        .is_some_and(|start| start.elapsed() >= time_budget)
}

/// Reset the local-drain burst counters (iamacoffeepot/aether#1160). Called
/// by `acquire_slot` the moment `pop_local` reports the own deque drained
/// empty, so each local cascade is one burst and any subsequently stolen
/// work starts a fresh budget.
pub fn burst_reset() {
    BURST_MAIL.set(0);
    BURST_START.set(None);
}

/// Budget-aware push of a just-produced blob onto this worker's own deque
/// (iamacoffeepot/aether#1160). Keeps it local unless the keep-local budget
/// says to spill:
///
/// ```text
/// spill  ⟺  local_deque_len >= hard_cap
///           || (local_deque_len > 0 && burst_over_budget(mail_budget, time_budget))
/// ```
///
/// The `local_deque_len > 0` guard is load-bearing **and short-circuits the
/// budget check (and thus the clock read)**: a serial relay chain or a
/// single-blob fan-out has an **empty** deque at schedule time (the current
/// blob was popped, nothing else queued), so it is kept local unconditionally
/// — no budget consulted, no clock read — because there is no independent
/// backlog to parallelize, so a spill would only buy a wakeup. Only a
/// multi-blob backlog (`len > 0`) reaches [`burst_over_budget`], where the
/// mail count + time budget decide: a trivial cascade stays local (under
/// budget — no wakeup, the measured win), a large *or heavy* one spills
/// (independent work + idle workers ⇒ parallelism amortizes the wakeup).
/// `hard_cap` is a deque-length backstop only.
///
/// With `mail_budget == 0` (default) [`burst_over_budget`] is always `true`,
/// so the rule collapses to "spill iff `len > 0`" — identical to the
/// pre-#1160 `try_push_local(slot, 1)`, and with no clock read.
///
/// Returns `Ok(())` when kept local (the caller skips injector + notify),
/// or `Err(slot)` to spill. Off a pool worker there is no own deque, so
/// always `Err` (spill).
pub fn try_push_local_budgeted(
    slot: Slot,
    mail_budget: u32,
    time_budget: Duration,
    hard_cap: usize,
) -> Result<(), Slot> {
    LOCAL.with(|w| {
        let w = w.borrow();
        match w.as_ref() {
            Some(worker) => {
                let len = worker.len();
                let spill =
                    len >= hard_cap || (len > 0 && burst_over_budget(mail_budget, time_budget));
                if spill {
                    Err(slot)
                } else {
                    worker.push(slot);
                    Ok(())
                }
            }
            None => Err(slot),
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
    use crate::scheduler::SpinPark;
    use crate::scheduler::slot::{BatchBudget, CycleResult, Drainable, WakeSink};
    use std::any::Any;
    use std::thread;

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
    fn budgeted_off_worker_always_spills() {
        // This test never calls `install`, so it isn't a pool worker:
        // every push must spill regardless of budget or backlog.
        assert!(try_push_local_budgeted(noop(), 1000, Duration::ZERO, 256).is_err());
        assert!(pop_local().is_none());
    }

    #[test]
    fn budgeted_default_reproduces_cap_one() {
        // `mail_budget == 0` ⇒ always over budget ⇒ spill whenever the own
        // deque already holds work — exactly the pre-#1160 `cap == 1`
        // "keep only when the deque is empty" shape.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();

        // Empty deque: kept local.
        assert!(try_push_local_budgeted(noop(), 0, Duration::ZERO, 256).is_ok());
        // Deque now at depth 1: the next spills.
        assert!(
            try_push_local_budgeted(noop(), 0, Duration::ZERO, 256).is_err(),
            "default (mail_budget 0) spills any fan-out extra, like cap 1"
        );
        drain_local();
    }

    #[test]
    fn budgeted_chain_never_spills_at_depth_zero() {
        // The load-bearing guard: a serial chain has an empty deque at
        // schedule time (the current blob was popped), so it stays local
        // even when the burst is well over budget — a chain has no
        // independent work to parallelize, so a spill would only buy a
        // wakeup.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        for _ in 0..100 {
            burst_note_mail(Duration::ZERO); // far past any small budget
        }
        assert!(
            burst_over_budget(1, Duration::ZERO),
            "burst should read over a 1-mail budget"
        );
        assert!(
            try_push_local_budgeted(noop(), 1, Duration::ZERO, 256).is_ok(),
            "depth 0 keeps local even over budget (the chain guard)"
        );
        drain_local();
    }

    #[test]
    fn budgeted_keeps_local_under_budget() {
        // Under budget (large mail_budget, small burst) a cascade stacks on
        // the own deque — the keep-local win the spill cost avoids.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        for _ in 0..5 {
            assert!(
                try_push_local_budgeted(noop(), 1000, Duration::ZERO, 256).is_ok(),
                "under budget keeps local"
            );
        }
        drain_local();
    }

    #[test]
    fn budgeted_spills_backlog_over_budget() {
        // Over budget (burst 10 > mail_budget 4) with real backlog
        // (depth > 0): spill the extra so an idle sibling can steal it.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        for _ in 0..10 {
            burst_note_mail(Duration::ZERO);
        }
        // Empty deque first push keeps (the depth-0 chain guard).
        assert!(try_push_local_budgeted(noop(), 4, Duration::ZERO, 256).is_ok());
        // Now depth 1 + over budget → spill.
        assert!(
            try_push_local_budgeted(noop(), 4, Duration::ZERO, 256).is_err(),
            "over budget with backlog spills"
        );
        drain_local();
    }

    #[test]
    fn budgeted_hard_cap_backstop() {
        // Even under the mail/time budget, the deque-length backstop forces
        // a spill once the own deque reaches `hard_cap`.
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        // hard_cap 2, large mail_budget (never trips by count).
        assert!(try_push_local_budgeted(noop(), 1000, Duration::ZERO, 2).is_ok()); // len 0 → 1
        assert!(try_push_local_budgeted(noop(), 1000, Duration::ZERO, 2).is_ok()); // len 1 → 2
        assert!(
            try_push_local_budgeted(noop(), 1000, Duration::ZERO, 2).is_err(),
            "len == hard_cap spills regardless of budget"
        );
        drain_local();
    }

    #[test]
    fn budgeted_time_valve_spills_few_mail_heavy() {
        // The core #1160 fix: a *few-mail* burst that has run past the time
        // budget spills its backlog (depth > 0), even though the mail count
        // is far under budget — the narrow-heavy case the strided per-mail
        // clock used to miss (it never sampled a sub-stride burst).
        install(Worker::new_lifo());
        drain_local();
        burst_reset();
        let tiny = Duration::from_nanos(1);
        burst_note_mail(tiny); // 1 mail, anchors the burst start
        thread::sleep(Duration::from_micros(50)); // now well past `tiny`
        // Depth 0 still keeps — the chain guard short-circuits before the
        // budget (and before any clock read).
        assert!(
            try_push_local_budgeted(noop(), 1000, tiny, 256).is_ok(),
            "depth 0 keeps regardless of elapsed time"
        );
        // Depth 1, only 1 mail (≪ 1000 budget), but over the time budget →
        // spill. Mail-only (the pre-fix behaviour) would have kept it.
        assert!(
            try_push_local_budgeted(noop(), 1000, tiny, 256).is_err(),
            "few mail but over the time budget spills the backlog (the valve fix)"
        );
        drain_local();
    }

    #[test]
    fn burst_counts_and_resets() {
        burst_reset();
        for _ in 0..5 {
            burst_note_mail(Duration::ZERO);
        }
        assert!(
            burst_over_budget(5, Duration::ZERO),
            "5 mail meets a 5-mail budget"
        );
        assert!(
            !burst_over_budget(6, Duration::ZERO),
            "5 mail is under a 6-mail budget"
        );
        burst_reset();
        assert!(
            !burst_over_budget(1, Duration::ZERO),
            "reset zeroes the counter"
        );
    }

    #[test]
    fn burst_mail_budget_zero_is_always_over() {
        burst_reset();
        assert!(
            burst_over_budget(0, Duration::ZERO),
            "mail_budget 0 trips at zero mail"
        );
    }

    #[test]
    fn burst_time_path_trips_over_budget() {
        // With time budgeting on, a burst that has run past the time budget
        // trips at decision time even when the mail count is nowhere near
        // its budget — the few-mail-heavy case the strided per-mail sample
        // used to miss (iamacoffeepot/aether#1160). The start is anchored on
        // the first mail; a tiny budget + a real sleep makes the elapsed
        // check at the decision deterministic.
        burst_reset();
        let tiny = Duration::from_nanos(1);
        burst_note_mail(tiny); // n == 1 anchors BURST_START
        thread::sleep(Duration::from_micros(50));
        assert!(
            burst_over_budget(u32::MAX, tiny),
            "only the time path can trip here (mail count is 1, mail budget u32::MAX)"
        );
        // Time budgeting off ⇒ the time path is never consulted, even though
        // the same elapsed time has passed.
        assert!(
            !burst_over_budget(u32::MAX, Duration::ZERO),
            "time_budget 0 never trips on elapsed"
        );
        burst_reset();
        assert!(
            !burst_over_budget(u32::MAX, tiny),
            "reset clears the burst start"
        );
    }

    #[test]
    fn schedule_default_reproduces_cap_one_on_worker() {
        // Drive the wired decision through `WakeSink::schedule` on a
        // simulated pool worker (own deque installed on this thread). At the
        // default config (mail_budget 0) the first schedule stays local
        // (empty deque) and the second spills — the pre-#1160 `cap == 1`
        // shape, now routed through the budget gate.
        //
        // `schedule` reads the env-cached `mail_budget()`, so this asserts
        // the *default* wiring; skip it when a keep-local budget is opted in
        // (e.g. the Phase 2 sweep sets `AETHER_LOCAL_MAIL_BUDGET`), where the
        // second schedule legitimately stays local instead of spilling.
        if mail_budget() != 0 {
            return;
        }
        install(Worker::new_lifo());
        drain_local();
        burst_reset();

        let injector = Arc::new(Injector::<Slot>::new());
        let sink = WakeSink::new(Arc::clone(&injector), Arc::new(SpinPark::new()), 8);

        sink.schedule(noop()); // empty deque → kept local
        sink.schedule(noop()); // depth 1 + always-over default → spills

        assert!(
            matches!(injector.steal(), Steal::Success(_)),
            "the second schedule must spill to the injector"
        );
        assert!(matches!(injector.steal(), Steal::Empty), "only one spills");
        assert!(pop_local().is_some(), "the first stays on the local deque");
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
